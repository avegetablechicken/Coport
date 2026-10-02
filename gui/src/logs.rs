//! Tails the proxy's JSON-lines log file and keeps recent events in memory.

use crate::proxy::Notify;
use chrono::{DateTime, Local};
use serde_json::{Map, Value};
use std::{
    collections::{BTreeMap, VecDeque},
    io::{Read, Seek, SeekFrom},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

const CAPACITY: usize = 5000;
/// How much existing history to load when a log file is first opened.
const BACKFILL: u64 = 512 * 1024;

/// A stable row key for a log line: the same line read by the tailer or by a
/// traffic scan gets the same key. Kept within JavaScript's exact integers.
pub fn line_key(line: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    line.hash(&mut hasher);
    hasher.finish() & ((1 << 53) - 1)
}

#[derive(Clone)]
pub struct Entry {
    pub seq: u64,
    pub time: Option<DateTime<Local>>,
    pub event: String,
    pub fields: Map<String, Value>,
}

impl Entry {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.fields.get(key).and_then(Value::as_str)
    }
    pub fn is_request_end(&self) -> bool {
        matches!(
            self.event.as_str(),
            "request_finished" | "request_rejected" | "request_failed" | "request_cancelled"
        )
    }
    pub fn status(&self) -> Option<u16> {
        self.get("status")?.parse().ok()
    }
    pub fn is_model_call_end(&self) -> bool {
        matches!(
            self.event.as_str(),
            "model_call_finished"
                | "model_call_incomplete"
                | "model_call_failed"
                | "model_call_cancelled"
                | "model_call_unknown"
        )
    }
    pub fn duration_ms(&self) -> Option<u64> {
        self.get("duration_ms")?.parse().ok()
    }
    pub fn bytes(&self) -> u64 {
        self.get("received_bytes")
            .and_then(|b| b.parse().ok())
            .unwrap_or(0)
    }
    /// Whether the entry belongs to an Activity list filter.
    pub fn matches_filter(&self, filter: &str) -> bool {
        match filter {
            "errors" => self.is_error(),
            "models" => self.is_model_call_end(),
            "all" => true,
            _ => self.is_request_end(),
        }
    }
    pub fn is_error(&self) -> bool {
        if self.event.starts_with("model_call_") {
            return self.event == "model_call_failed";
        }
        self.event == "device_statistics_failed"
            || self.event == "request_failed"
            || self.event == "request_rejected"
            || self.event == "route_unavailable"
            || self.status().is_some_and(|s| s >= 400)
    }
    /// Which client the request belongs to, inferred from its path.
    pub fn service(&self) -> Option<&'static str> {
        if let Some(service) = self.get("service") {
            return match service {
                "claude" => Some("Claude"),
                "codex" => Some("Codex"),
                _ => None,
            };
        }
        let path = self.get("path")?;
        if path.starts_with("/anthropic")
            || path.starts_with("/claude")
            || path.starts_with("/v1/messages")
            || path.starts_with("/api/oauth")
        {
            Some("Claude")
        } else {
            Some("Codex")
        }
    }
}

#[derive(Default)]
struct Store {
    path: PathBuf,
    entries: VecDeque<Entry>,
    /// Bumped when the file changes identity so the tailer restarts.
    generation: u64,
}

#[derive(Clone)]
pub struct LogFeed {
    store: Arc<Mutex<Store>>,
}

impl LogFeed {
    pub fn new(path: PathBuf, notify: Notify) -> Self {
        let store = Arc::new(Mutex::new(Store {
            path,
            ..Default::default()
        }));
        let feed = Self { store };
        let tail = feed.clone();
        std::thread::Builder::new()
            .name("log-tail".into())
            .spawn(move || tail.run(notify))
            .expect("log tail thread");
        feed
    }

    pub fn set_path(&self, path: PathBuf) {
        let mut s = self.store.lock().unwrap();
        if s.path != path {
            s.path = path;
            s.entries.clear();
            s.generation += 1;
        }
    }

    pub fn path(&self) -> PathBuf {
        self.store.lock().unwrap().path.clone()
    }

    /// Snapshot of entries matching `keep`, newest last.
    pub fn entries(&self, keep: impl Fn(&Entry) -> bool) -> Vec<Entry> {
        let s = self.store.lock().unwrap();
        s.entries.iter().filter(|e| keep(e)).cloned().collect()
    }

    /// Aggregates retained requests in the last 30 minutes since the latest start.
    pub fn stats(&self) -> Stats {
        let s = self.store.lock().unwrap();
        let start = s
            .entries
            .iter()
            .rposition(|e| e.event == "server_started")
            .map_or(0, |i| i + 1);
        let mut stats = Stats::default();
        let now = Local::now();
        let mut latency_total = 0u64;
        let mut latency_count = 0u64;
        for e in s.entries.range(start..) {
            match e.event.as_str() {
                "current_route" | "route_unavailable" => stats.routes.push(e.clone()),
                "route_health" => {
                    if let Some(endpoint) = e.get("proxy_endpoint") {
                        let route = (
                            endpoint.to_owned(),
                            e.get("origin").unwrap_or_default().to_owned(),
                            e.get("transport").unwrap_or_default().to_owned(),
                        );
                        stats.route_health.insert(route, e.clone());
                    }
                }
                _ => {}
            }
            if !e.is_request_end() {
                continue;
            }
            let Some(t) = e.time else { continue };
            let age = now - t;
            if age < chrono::Duration::zero()
                || age >= chrono::Duration::minutes(SPARK_MINUTES as i64)
            {
                continue;
            }
            stats.requests += 1;
            if e.is_error() {
                stats.errors += 1;
            }
            stats.bytes += e.bytes();
            if let Some(ms) = e.duration_ms() {
                latency_total += ms;
                latency_count += 1;
            }
            if let Some(t) = e.time {
                let age = (now - t).num_minutes();
                if (0..SPARK_MINUTES as i64).contains(&age) {
                    let slot = SPARK_MINUTES - 1 - age as usize;
                    stats.per_minute[slot] += 1;
                    if e.is_error() {
                        stats.errors_per_minute[slot] += 1;
                    }
                }
            }
        }
        stats.avg_latency_ms = (latency_count > 0).then(|| latency_total / latency_count);
        stats
    }

    fn run(&self, notify: Notify) {
        let mut file: Option<(std::fs::File, u64)> = None;
        let mut offset = 0u64;
        let mut partial = Vec::new();
        let mut generation = u64::MAX;
        loop {
            let (path, current_gen) = {
                let s = self.store.lock().unwrap();
                (s.path.clone(), s.generation)
            };
            if current_gen != generation {
                generation = current_gen;
                file = None;
            }
            // Reopen when the file appears, is rotated, or is truncated.
            let meta = std::fs::metadata(&path).ok();
            let identity = meta.as_ref().map(file_identity);
            let len = meta.as_ref().map_or(0, |m| m.len());
            // Lines written just before a rotation are read through the old handle.
            // Windows gives a file created soon after a rename the old one's
            // creation time, so a shorter file is also taken as a new one; a
            // file truncated in place has nothing past the offset to read.
            if let Some((mut f, id)) = file.take() {
                if identity == Some(id) && len >= offset {
                    file = Some((f, id));
                } else {
                    let mut buf = Vec::new();
                    if f.seek(SeekFrom::Start(offset)).is_ok() && f.read_to_end(&mut buf).is_ok() {
                        partial.extend_from_slice(&buf);
                        if self.ingest(&mut partial) {
                            notify();
                        }
                    }
                }
            }
            let reopen = file.is_none() && identity.is_some();
            if reopen && let Ok(f) = std::fs::File::open(&path) {
                // First open backfills history; later reopens mean rotation.
                let fresh = generation_is_empty(&self.store);
                offset = if fresh {
                    len.saturating_sub(BACKFILL)
                } else {
                    0
                };
                partial.clear();
                let skip_partial_line = offset > 0;
                file = Some((f, identity.unwrap_or(0)));
                if skip_partial_line {
                    partial.push(b'\0');
                }
            }
            if let Some((f, _)) = file.as_mut()
                && len > offset
                && f.seek(SeekFrom::Start(offset)).is_ok()
            {
                let mut buf = Vec::new();
                if let Ok(n) = f.take(len - offset).read_to_end(&mut buf) {
                    offset += n as u64;
                    partial.extend_from_slice(&buf);
                    if self.ingest(&mut partial) {
                        notify();
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(350));
        }
    }

    /// Parses complete lines out of `pending`; returns whether any were added.
    fn ingest(&self, pending: &mut Vec<u8>) -> bool {
        let Some(last_newline) = pending.iter().rposition(|&b| b == b'\n') else {
            return false;
        };
        let complete: Vec<u8> = pending.drain(..=last_newline).collect();
        let mut s = self.store.lock().unwrap();
        let mut added = false;
        for line in complete.split(|&b| b == b'\n') {
            // A leading NUL marks the truncated first line of a backfill.
            if line.first() == Some(&b'\0') || line.is_empty() {
                continue;
            }
            let Ok(Value::Object(mut fields)) = serde_json::from_slice(line) else {
                continue;
            };
            let event = match fields.remove("event") {
                Some(Value::String(e)) => e,
                _ => continue,
            };
            let time = fields
                .remove("timestamp")
                .and_then(|t| t.as_str().map(str::to_owned))
                .and_then(|t| DateTime::parse_from_rfc3339(&t).ok())
                .map(|t| t.with_timezone(&Local));
            s.entries.push_back(Entry {
                seq: line_key(line),
                time,
                event,
                fields,
            });
            if s.entries.len() > CAPACITY {
                s.entries.pop_front();
            }
            added = true;
        }
        added
    }
}

fn generation_is_empty(store: &Mutex<Store>) -> bool {
    store.lock().unwrap().entries.is_empty()
}

#[cfg(unix)]
fn file_identity(meta: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.ino()
}

#[cfg(not(unix))]
fn file_identity(meta: &std::fs::Metadata) -> u64 {
    // Rotation renames the file away; its creation time identifies a new one.
    meta.created()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos() as u64)
}

pub const SPARK_MINUTES: usize = 30;

#[derive(Default)]
pub struct Stats {
    pub requests: u64,
    pub errors: u64,
    pub bytes: u64,
    pub avg_latency_ms: Option<u64>,
    pub per_minute: [u32; SPARK_MINUTES],
    pub errors_per_minute: [u32; SPARK_MINUTES],
    pub routes: Vec<Entry>,
    /// The latest health of each route since the server started, by proxy
    /// endpoint, destination origin and transport, as the daemon tracks it.
    pub route_health: BTreeMap<(String, String, String), Entry>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_health_uses_latest_observation_from_current_server() {
        for newline in ["\n", "\r\n"] {
            let feed = LogFeed {
                store: Arc::new(Mutex::new(Store::default())),
            };
            let health = |endpoint: &str, origin: &str, health: &str| {
                serde_json::json!({"event":"route_health", "proxy_endpoint":endpoint,
                    "origin":origin, "transport":"connect", "health":health})
            };
            let events = [
                health("old", "https://a.example", "healthy"),
                serde_json::json!({"event":"server_started"}),
                health("used", "https://a.example", "unavailable"),
                health("used", "https://a.example", "healthy"),
                health("used", "https://b.example", "unavailable"),
            ];
            let mut lines = events
                .iter()
                .map(|e| format!("{e}{newline}"))
                .collect::<String>()
                .into_bytes();
            assert!(feed.ingest(&mut lines));
            let route = |origin: &str| ("used".to_owned(), origin.to_owned(), "connect".to_owned());
            let stats = feed.stats();
            // Each destination keeps its own latest observation.
            assert_eq!(stats.route_health.len(), 2);
            assert_eq!(
                stats.route_health[&route("https://a.example")].get("health"),
                Some("healthy")
            );
            assert_eq!(
                stats.route_health[&route("https://b.example")].get("health"),
                Some("unavailable")
            );
            let mut failure = format!(
                "{}{newline}",
                health("used", "https://a.example", "unavailable")
            )
            .into_bytes();
            feed.ingest(&mut failure);
            assert_eq!(
                feed.stats().route_health[&route("https://a.example")].get("health"),
                Some("unavailable")
            );
        }
    }

    #[test]
    fn tails_appended_and_rotated_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("proxy.log");
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = hits.clone();
        let feed = LogFeed::new(
            path.clone(),
            Arc::new(move || {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }),
        );
        let line = |event: &str, status: u16| {
            format!(
                "{{\"event\":\"{event}\",\"status\":\"{status}\",\"path\":\"/v1/messages\",\"duration_ms\":\"20\",\"received_bytes\":\"100\",\"timestamp\":\"{}\"}}\n",
                Local::now().to_rfc3339()
            )
        };
        std::fs::write(
            &path,
            format!(
                "{{\"event\":\"server_started\"}}\n{}",
                line("request_finished", 200)
            ),
        )
        .unwrap();
        wait_for(|| feed.stats().requests == 1);
        // Partial line is not parsed until its newline arrives.
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        let next = line("request_rejected", 403);
        f.write_all(&next.as_bytes()[..10]).unwrap();
        f.flush().unwrap();
        std::thread::sleep(Duration::from_millis(800));
        assert_eq!(feed.stats().requests, 1);
        f.write_all(&next.as_bytes()[10..]).unwrap();
        f.flush().unwrap();
        wait_for(|| feed.stats().requests == 2);
        let stats = feed.stats();
        assert_eq!(stats.errors, 1);
        assert_eq!(stats.bytes, 200);
        assert_eq!(stats.avg_latency_ms, Some(20));
        assert_eq!(
            feed.entries(|e| e.is_request_end())[0].service(),
            Some("Claude")
        );

        // Rotation: the logger renames to `.1` and starts a fresh file. A line
        // written just before is still read from the rotated file.
        f.write_all(line("request_finished", 200).as_bytes())
            .unwrap();
        f.flush().unwrap();
        std::fs::rename(&path, dir.path().join("proxy.log.1")).unwrap();
        std::fs::write(&path, line("request_failed", 502)).unwrap();
        wait_for(|| feed.stats().requests == 4);
        assert!(hits.load(std::sync::atomic::Ordering::SeqCst) >= 3);
    }

    #[test]
    fn stats_exclude_old_future_and_undated_requests() {
        let feed = LogFeed {
            store: Arc::new(Mutex::new(Store::default())),
        };
        let now = Local::now();
        let mut lines = Vec::new();
        for (time, status) in [
            (Some(now - chrono::Duration::minutes(31)), "502"),
            (Some(now - chrono::Duration::minutes(30)), "502"),
            (Some(now + chrono::Duration::seconds(10)), "502"),
            (None, "502"),
            (Some(now - chrono::Duration::minutes(29)), "200"),
            (Some(now - chrono::Duration::seconds(1)), "502"),
        ] {
            let value = serde_json::json!({"event":"request_finished", "timestamp":time.map(|t| t.to_rfc3339()),
                "status":status, "received_bytes":"100", "duration_ms":"20"});
            lines.extend_from_slice(format!("{value}\n").as_bytes());
        }
        feed.ingest(&mut lines);
        let stats = feed.stats();
        assert_eq!(stats.requests, 2);
        assert_eq!(stats.errors, 1);
        assert_eq!(stats.bytes, 200);
        assert_eq!(stats.avg_latency_ms, Some(20));
        assert_eq!(stats.per_minute.iter().sum::<u32>(), 2);
        assert_eq!(stats.errors_per_minute.iter().sum::<u32>(), 1);
    }

    #[test]
    fn cancellation_is_visible_without_counting_as_a_gateway_error() {
        let feed = LogFeed {
            store: Arc::new(Mutex::new(Store::default())),
        };
        let mut lines = format!(
            "{}\n",
            serde_json::json!({
                "event":"request_cancelled", "reason":"request_dropped",
                "timestamp":Local::now().to_rfc3339()
            })
        )
        .into_bytes();
        feed.ingest(&mut lines);
        assert_eq!(feed.entries(|e| e.is_request_end()).len(), 1);
        let stats = feed.stats();
        assert_eq!(stats.requests, 1);
        assert_eq!(stats.errors, 0);
        assert_eq!(feed.entries(|_| true)[0].status(), None);
    }

    fn wait_for(cond: impl Fn() -> bool) {
        for _ in 0..40 {
            if cond() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("condition not reached");
    }
}
