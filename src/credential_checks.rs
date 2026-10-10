//! Current credential diagnostics, independent of retained startup logs.
use super::Server;
use serde::{Deserialize, Serialize};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const INTERVAL: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialReport {
    pub name: String,
    pub ok: bool,
    pub reason: Option<String>,
    pub checked_at: u64,
}

impl Server {
    /// Serialize checks and reuse both successful and failed results equally.
    pub async fn credential_reports(&self) -> Vec<CredentialReport> {
        let mut cache = self.credential_checks.lock().await;
        if let Some((_, reports)) = cache.as_ref().filter(|(at, _)| at.elapsed() < INTERVAL) {
            return reports.clone();
        }
        let check = |name: &str, result: Result<crate::Result<()>, tokio::time::error::Elapsed>| {
            let reason = match result {
                Ok(Ok(())) => None,
                Ok(Err(error)) => Some(error.message.to_owned()),
                Err(_) => Some("Credential check timed out.".to_owned()),
            };
            CredentialReport {
                name: name.into(),
                ok: reason.is_none(),
                reason,
                checked_at: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
            }
        };
        let (codex, claude, _) = tokio::join!(
            tokio::time::timeout(
                Duration::from_secs(30),
                self.config.check_codex_credentials()
            ),
            tokio::time::timeout(
                Duration::from_secs(30),
                self.config.claude.check_credentials()
            ),
            // Only the periodic worker requests enrichment. Display readers use
            // cached_traffic_credential_labels and never initiate profile I/O.
            tokio::time::timeout(Duration::from_secs(30), self.traffic_credential_labels()),
        );
        let reports = vec![check("Codex", codex), check("Claude", claude)];
        *cache = Some((tokio::time::Instant::now(), reports.clone()));
        reports
    }

    pub(super) async fn monitor_credentials(&self) {
        loop {
            self.credential_reports().await;
            let next = self.credential_checks.lock().await.as_ref().unwrap().0 + INTERVAL;
            tokio::time::sleep_until(next).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Config, logger::Logger};
    use std::sync::Arc;

    #[tokio::test(start_paused = true)]
    async fn monitor_rechecks_failures_and_successes_without_gui_requests() {
        let dir = tempfile::tempdir().unwrap();
        let profile = dir.path().join("sample.json");
        std::fs::write(&profile, "{}").unwrap();
        let config = Config::parse(&format!(
            "listen_port: 8787\nrequest_timeout_seconds: 3\ncodex:\n  homes: []\nclaude:\n  config_dirs: [{}]\n  routing:\n    api_key:\n      sample.json: none\n",
            serde_json::to_string(dir.path()).unwrap()
        )).unwrap();
        let server = Arc::new(Server::new(
            config,
            Arc::new(Logger::new(dir.path().join("log"))),
        ));
        let monitor = tokio::spawn({
            let server = server.clone();
            async move { server.monitor_credentials().await }
        });
        tokio::task::yield_now().await;
        assert!(!server.credential_checks.lock().await.as_ref().unwrap().1[1].ok);
        std::fs::write(&profile, r#"{"env":{"ANTHROPIC_AUTH_TOKEN":"synthetic-test-token","ANTHROPIC_BASE_URL":"https://example.invalid"}}"#).unwrap();
        // Reads within the interval share the prior failure, without rechecking.
        assert!(!server.credential_reports().await[1].ok);
        tokio::time::advance(INTERVAL).await;
        tokio::task::yield_now().await;
        assert!(server.credential_checks.lock().await.as_ref().unwrap().1[1].ok);
        std::fs::write(&profile, "{}").unwrap();
        assert!(server.credential_reports().await[1].ok);
        tokio::time::advance(INTERVAL).await;
        tokio::task::yield_now().await;
        let cache = server.credential_checks.lock().await;
        let reports = &cache.as_ref().unwrap().1;
        assert!(
            reports[0].ok,
            "Claude failures must not mark Codex as failed"
        );
        assert!(!reports[1].ok);
        assert!(
            reports[1]
                .reason
                .as_ref()
                .unwrap()
                .contains("ANTHROPIC_AUTH_TOKEN")
        );
        monitor.abort();
    }
}
