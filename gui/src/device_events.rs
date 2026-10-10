//! Local audit of outgoing statistics requests. Never records remote responses,
//! addresses, authentication material or raw transport errors.
use crate::data_client::{Source, Transport};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{LazyLock, Mutex},
    time::{Duration, Instant},
};

const INTERVAL: Duration = Duration::from_secs(300);
const CAPACITY: usize = 256;

#[derive(Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Protocol {
    Ssh,
    Http,
    Https,
}
#[derive(Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Outcome {
    Succeeded,
    Failed,
    Recovered,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct QueryEvent {
    pub device_id: String,
    pub transport: Protocol,
    pub outcome: Outcome,
    pub duration_ms: u64,
    pub query_count: u64,
}
impl QueryEvent {
    pub(crate) fn validate(&self) -> bool {
        (uuid::Uuid::parse_str(&self.device_id).is_ok()
            || (self.device_id.len() == 64
                && self.device_id.bytes().all(|c| c.is_ascii_hexdigit())))
            && self.duration_ms <= 120_000
            && (1..=1_000_000).contains(&self.query_count)
    }
    pub(crate) fn event(&self) -> &'static str {
        match self.outcome {
            Outcome::Succeeded => "device_statistics_succeeded",
            Outcome::Failed => "device_statistics_failed",
            Outcome::Recovered => "device_statistics_recovered",
        }
    }
    pub(crate) fn fields(&self) -> serde_json::Map<String, serde_json::Value> {
        let mut fields = serde_json::to_value(self)
            .unwrap()
            .as_object()
            .unwrap()
            .clone();
        // Existing Activity latency formatting reads the string representation.
        fields.insert("duration_ms".into(), self.duration_ms.to_string().into());
        fields
    }
}
struct LastQuery {
    success: bool,
    logged: Instant,
    count: u64,
}
#[derive(Default)]
struct History(BTreeMap<(PathBuf, String, &'static str), LastQuery>);
impl History {
    fn record(
        &mut self,
        log: &Path,
        source: &Source,
        success: bool,
        elapsed: Duration,
        now: Instant,
    ) -> Option<QueryEvent> {
        let transport = match source.transport {
            Transport::Ssh => Protocol::Ssh,
            Transport::Http if source.url.to_ascii_lowercase().starts_with("https:") => {
                Protocol::Https
            }
            Transport::Http => Protocol::Http,
        };
        let protocol = match transport {
            Protocol::Ssh => "ssh",
            Protocol::Http => "http",
            Protocol::Https => "https",
        };
        let id = source
            .device_id
            .as_ref()
            .filter(|id| uuid::Uuid::parse_str(id).is_ok())
            .cloned()
            .unwrap_or_else(|| {
                let destination = source
                    .ssh_connection
                    .as_ref()
                    .map_or(source.url.as_str(), |s| s.host.as_str());
                ring::digest::digest(&ring::digest::SHA256, destination.as_bytes())
                    .as_ref()
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect()
            });
        let key = (log.to_owned(), id.clone(), protocol);
        if self.0.len() >= CAPACITY
            && !self.0.contains_key(&key)
            && let Some(oldest) = self
                .0
                .iter()
                .min_by_key(|(_, v)| v.logged)
                .map(|(k, _)| k.clone())
        {
            self.0.remove(&oldest);
        }
        let previous = self.0.get_mut(&key);
        let (outcome, count) = if let Some(previous) = previous {
            previous.count = previous.count.saturating_add(1).min(1_000_000);
            if previous.success == success && now.duration_since(previous.logged) < INTERVAL {
                return None;
            }
            let outcome = if !success {
                Outcome::Failed
            } else if !previous.success {
                Outcome::Recovered
            } else {
                Outcome::Succeeded
            };
            let count = previous.count;
            previous.success = success;
            previous.logged = now;
            previous.count = 0;
            (outcome, count)
        } else {
            self.0.insert(
                key,
                LastQuery {
                    success,
                    logged: now,
                    count: 0,
                },
            );
            (
                if success {
                    Outcome::Succeeded
                } else {
                    Outcome::Failed
                },
                1,
            )
        };
        Some(QueryEvent {
            device_id: id,
            transport,
            outcome,
            duration_ms: elapsed.as_millis().min(120_000) as u64,
            query_count: count,
        })
    }
}
static HISTORY: LazyLock<Mutex<History>> = LazyLock::new(|| Mutex::new(History::default()));

pub(crate) async fn record(log: PathBuf, source: &Source, success: bool, elapsed: Duration) {
    let event = HISTORY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .record(&log, source, success, elapsed, Instant::now());
    if let Some(event) = event {
        let result = tokio::task::spawn_blocking(move || persist(&daemon_dir(), &log, event)).await;
        if !matches!(result, Ok(Ok(()))) {
            eprintln!("Cannot record device statistics query");
        }
    }
}
/// Where the live daemon is discovered. Unit tests query fake peers with
/// temporary logs and must never contact the user's running daemon.
fn daemon_dir() -> PathBuf {
    #[cfg(test)]
    return std::env::temp_dir().join("coport-unit-tests-without-daemon");
    #[cfg(not(test))]
    crate::settings::app_dir()
}
fn persist(app_dir: &Path, log: &Path, event: QueryEvent) -> io::Result<()> {
    // The live daemon owns log rotation: enqueue through its authenticated local
    // control channel instead of opening a second rotating writer.
    if let Some((client, status)) = crate::daemon::Client::discover(app_dir)
        && status.log_path == log
    {
        return client.record_device_query(event);
    }
    // Queries can run with the local proxy stopped. Append one bounded record,
    // without rotating a file that a concurrently starting daemon might own.
    if let Some(parent) = log.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut fields = event.fields();
    fields.insert("event".into(), event.event().into());
    fields.insert("timestamp".into(), chrono::Utc::now().to_rfc3339().into());
    let mut bytes = serde_json::to_vec(&fields)?;
    bytes.push(b'\n');
    coport::logger::open_private(log)?.write_all(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn source() -> Source {
        Source {
            name: "private-device-name".into(),
            device_id: Some(uuid::Uuid::new_v4().to_string()),
            transport: Transport::Http,
            url: "https://private.example".into(),
            ssh_connection: None,
            token_file: Some("/private/key".into()),
            token_env: Some("SECRET_KEY".into()),
            ca_certificate: None,
            ssh_device: None,
        }
    }
    #[test]
    fn repeated_results_are_bounded_and_transitions_are_immediate() {
        let mut history = History::default();
        let mut source = source();
        let path = Path::new("test.log");
        let now = Instant::now();
        let duration = Duration::from_millis(42);
        let first = history.record(path, &source, true, duration, now).unwrap();
        assert!(matches!(first.outcome, Outcome::Succeeded));
        assert_eq!(first.query_count, 1);
        assert!(
            history
                .record(path, &source, true, duration, now + Duration::from_secs(15))
                .is_none()
        );
        source.name = "renamed device".into();
        assert!(
            history
                .record(path, &source, true, duration, now + Duration::from_secs(30))
                .is_none()
        );
        let failed = history
            .record(
                path,
                &source,
                false,
                duration,
                now + Duration::from_secs(45),
            )
            .unwrap();
        assert!(matches!(failed.outcome, Outcome::Failed));
        assert_eq!(failed.query_count, 3);
        assert!(
            history
                .record(
                    path,
                    &source,
                    false,
                    duration,
                    now + Duration::from_secs(60)
                )
                .is_none()
        );
        let recovered = history
            .record(path, &source, true, duration, now + Duration::from_secs(75))
            .unwrap();
        assert!(matches!(recovered.outcome, Outcome::Recovered));
        assert_eq!(recovered.query_count, 2);
        assert!(
            history
                .record(path, &source, true, duration, now + Duration::from_secs(90))
                .is_none()
        );
        let summary = history
            .record(
                path,
                &source,
                true,
                duration,
                now + Duration::from_secs(375),
            )
            .unwrap();
        assert!(matches!(summary.outcome, Outcome::Succeeded));
        assert_eq!(summary.query_count, 2);
        for _ in 0..CAPACITY + 5 {
            source.device_id = Some(uuid::Uuid::new_v4().to_string());
            history.record(path, &source, true, duration, now);
        }
        assert_eq!(history.0.len(), CAPACITY);
    }
    #[test]
    fn unit_tests_never_discover_the_users_daemon() {
        assert_ne!(daemon_dir(), crate::settings::app_dir());
        assert!(crate::daemon::Client::discover(&daemon_dir()).is_none());
    }
    #[test]
    fn audit_records_exclude_private_fields_and_do_not_change_traffic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("proxy.log");
        let mut history = History::default();
        let source = source();
        let event = history
            .record(
                &path,
                &source,
                false,
                Duration::from_millis(42),
                Instant::now(),
            )
            .unwrap();
        assert!(event.validate());
        persist(dir.path(), &path, event.clone()).unwrap();
        let bytes = std::fs::read_to_string(&path).unwrap();
        for secret in [
            "private-device-name",
            "private.example",
            "/private/key",
            "SECRET_KEY",
            "url",
            "token",
        ] {
            assert!(!bytes.contains(secret));
        }
        assert!(bytes.contains("device_statistics_failed"));
        for scope in [
            crate::traffic::TrafficScope::All,
            crate::traffic::TrafficScope::Model,
        ] {
            let traffic =
                crate::traffic::read(&path, 30, &BTreeMap::new(), scope, &Default::default())
                    .unwrap();
            let traffic = serde_json::to_value(traffic).unwrap();
            for metric in ["requests", "errors", "bytes"] {
                assert_eq!(traffic["summary"][metric], 0);
            }
        }
        let mut entries = 0;
        crate::traffic::for_each_entry(&path, 0, |entry| {
            entries += 1;
            assert!(entry.matches_filter("all"));
            assert!(entry.matches_filter("errors"));
            assert!(!entry.matches_filter("requests"));
            assert!(!entry.matches_filter("models"));
            assert_eq!(entry.duration_ms(), Some(42));
        })
        .unwrap();
        assert_eq!(entries, 1);
        let mut forged = serde_json::to_value(&event).unwrap();
        forged["headers"] = "private".into();
        assert!(serde_json::from_value::<QueryEvent>(forged).is_err());
        let mut invalid = event;
        invalid.device_id = "https://private.example".into();
        assert!(!invalid.validate());
    }
}
