//! Bounded forwarding telemetry. Counters describe this machine's SSH connections;
//! remote request totals are explicitly a separate, whole-device snapshot.
use serde::{Deserialize, Serialize};
use std::{
    io,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll},
    time::{Instant, SystemTime, UNIX_EPOCH},
};
use tokio::io::AsyncWrite;

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Health {
    pub state: String,
    pub latency_ms: Option<u64>,
    pub last_checked_at: Option<u64>,
    pub last_connected_at: Option<u64>,
    pub disconnected_at: Option<u64>,
    pub recovered_at: Option<u64>,
    pub error: Option<String>,
    #[serde(skip)]
    sequence: u64,
}
impl Health {
    pub fn checking() -> Self {
        Self {
            state: "checking".into(),
            ..Self::default()
        }
    }
    pub fn observe(&mut self, sequence: u64, result: Result<u64, String>) {
        if sequence < self.sequence {
            return;
        }
        self.sequence = sequence;
        let now = now_ms();
        self.last_checked_at = Some(now);
        match result {
            Ok(latency) => {
                if self.state == "disconnected" {
                    self.recovered_at = Some(now);
                }
                self.state = "connected".into();
                self.latency_ms = Some(latency);
                self.last_connected_at = Some(now);
                self.error = None;
            }
            Err(error) => {
                if self.state != "disconnected" {
                    self.disconnected_at = Some(now);
                }
                self.state = "disconnected".into();
                self.latency_ms = None;
                self.error = Some(error);
            }
        }
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Traffic {
    pub connections: u64,
    pub active: u64,
    pub failures: u64,
    pub upload_bytes: u64,
    pub download_bytes: u64,
    pub upload_bytes_per_second: u64,
    pub download_bytes_per_second: u64,
}
pub struct Counters {
    pub connections: AtomicU64,
    pub active: AtomicU64,
    pub failures: AtomicU64,
    pub upload: Arc<AtomicU64>,
    pub download: Arc<AtomicU64>,
    sample: Mutex<(Instant, u64, u64, u64, u64)>,
}
impl Default for Counters {
    fn default() -> Self {
        Self {
            connections: 0.into(),
            active: 0.into(),
            failures: 0.into(),
            upload: Arc::new(0.into()),
            download: Arc::new(0.into()),
            sample: Mutex::new((Instant::now(), 0, 0, 0, 0)),
        }
    }
}
impl Counters {
    pub fn snapshot(&self) -> Traffic {
        let upload = self.upload.load(Ordering::Relaxed);
        let download = self.download.load(Ordering::Relaxed);
        let mut sample = self.sample.lock().unwrap();
        let elapsed = sample.0.elapsed().as_secs_f64();
        if elapsed >= 1.0 {
            *sample = (
                Instant::now(),
                upload,
                download,
                ((upload.saturating_sub(sample.1)) as f64 / elapsed) as u64,
                ((download.saturating_sub(sample.2)) as f64 / elapsed) as u64,
            );
        }
        Traffic {
            connections: self.connections.load(Ordering::Relaxed),
            active: self.active.load(Ordering::Relaxed),
            failures: self.failures.load(Ordering::Relaxed),
            upload_bytes: upload,
            download_bytes: download,
            upload_bytes_per_second: sample.3,
            download_bytes_per_second: sample.4,
        }
    }
}
pub struct Active(pub Arc<Counters>);
impl Active {
    pub fn new(counters: Arc<Counters>) -> Self {
        counters.connections.fetch_add(1, Ordering::Relaxed);
        counters.active.fetch_add(1, Ordering::Relaxed);
        Self(counters)
    }
}
impl Drop for Active {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::Relaxed);
    }
}
pub struct CountWriter<W>(pub W, pub Arc<AtomicU64>);
impl<W: AsyncWrite + Unpin> AsyncWrite for CountWriter<W> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let result = Pin::new(&mut self.0).poll_write(cx, buf);
        if let Poll::Ready(Ok(size)) = result {
            self.1.fetch_add(size as u64, Ordering::Relaxed);
        }
        result
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteTraffic {
    pub fetched_at: u64,
    pub window_start: i64,
    pub window_end: i64,
    pub requests: u64,
    pub errors: u64,
    pub bytes: u64,
}
impl RemoteTraffic {
    pub fn from_summary(summary: crate::data_api::Summary) -> Result<Self, String> {
        summary.validate()?;
        let window = summary
            .windows
            .iter()
            .find(|w| w.minutes == 30 && w.scope == crate::traffic::TrafficScope::All)
            .ok_or("Remote version does not provide the 30-minute All traffic window")?;
        if window.window_end <= window.window_start {
            return Err("Invalid remote traffic window".into());
        }
        let mut result = Self {
            fetched_at: now_ms(),
            window_start: window.window_start,
            window_end: window.window_end,
            requests: 0,
            errors: 0,
            bytes: 0,
        };
        for group in &window.groups {
            result.requests = result.requests.saturating_add(group.stats.requests);
            result.errors = result.errors.saturating_add(group.stats.errors);
            result.bytes = result.bytes.saturating_add(group.stats.bytes);
        }
        if result.errors > result.requests {
            return Err("Invalid remote error count".into());
        }
        Ok(result)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn remote_totals_use_all_requests_and_reject_stale_snapshots() {
        use crate::data_api::{Group, RANGES, Service, Stats, Summary, Window, bucket_minutes};
        use crate::traffic::TrafficScope;
        let end = chrono::Utc::now().timestamp_millis() / 60_000 * 60_000;
        let mut summary = Summary {
            schema_version: 2,
            node_id: uuid::Uuid::new_v4().to_string(),
            window_start: end - 30 * 60_000,
            window_end: end,
            bucket_minutes: 1,
            groups: Vec::new(),
            windows: Vec::new(),
            previous_windows: Vec::new(),
        };
        for minutes in RANGES {
            for scope in [TrafficScope::Model, TrafficScope::All] {
                summary.windows.push(Window {
                    minutes,
                    scope,
                    window_start: end - (minutes * 60_000) as i64,
                    window_end: end,
                    bucket_minutes: bucket_minutes(minutes).unwrap(),
                    groups: Vec::new(),
                });
            }
        }
        let mut stats = Stats {
            requests: 10,
            errors: 1,
            bytes: 2048,
            counts: vec![0; 30],
            error_counts: vec![0; 30],
            token_counts: vec![0; 30],
            ..Default::default()
        };
        stats.counts[0] = 10;
        stats.error_counts[0] = 1;
        summary
            .windows
            .iter_mut()
            .find(|w| w.minutes == 30 && w.scope == TrafficScope::All)
            .unwrap()
            .groups
            .push(Group {
                service: Service::Other,
                proxy_ref: "a".repeat(64),
                upstream_ref: "b".repeat(64),
                account_ref: None,
                stats,
            });
        let totals = RemoteTraffic::from_summary(summary.clone()).unwrap();
        assert_eq!(
            (totals.requests, totals.errors, totals.bytes),
            (10, 1, 2048)
        );
        summary.window_end -= 3_600_000;
        assert!(RemoteTraffic::from_summary(summary).is_err());
    }
    #[test]
    fn recovery_and_old_results_preserve_the_latest_observation() {
        let mut health = Health::checking();
        health.observe(1, Err("offline".into()));
        assert_eq!(health.state, "disconnected");
        assert!(health.last_connected_at.is_none());
        health.observe(3, Ok(23));
        assert!(health.recovered_at.is_some());
        assert!(health.error.is_none());
        health.observe(2, Err("old connection failed".into()));
        assert_eq!(health.state, "connected");
        assert_eq!(health.latency_ms, Some(23));
    }
    #[tokio::test]
    async fn counting_tracks_written_bytes_and_cancellation_releases_active_count() {
        use tokio::io::AsyncWriteExt;
        let counters = Arc::new(Counters::default());
        let active = Active::new(counters.clone());
        CountWriter(tokio::io::sink(), counters.upload.clone())
            .write_all(b"12345")
            .await
            .unwrap();
        assert_eq!(counters.snapshot().upload_bytes, 5);
        assert_eq!(counters.snapshot().active, 1);
        drop(active);
        assert_eq!(counters.snapshot().active, 0);
    }
}
