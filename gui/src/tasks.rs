//! GUI operations prefer the daemon and fall back only after proving absence.
use crate::daemon;
use coport::{config::Config, logger::Logger, server::Server};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

pub type AccountStates = [BTreeMap<String, String>; 2];
pub type CredentialLabels = BTreeMap<(String, String), String>;

pub struct Tasks {
    directory: PathBuf,
    config_path: PathBuf,
    config: Config,
    local: tokio::sync::Mutex<Option<Arc<Server>>>,
}

impl Tasks {
    pub fn new(directory: PathBuf, config_path: PathBuf, config: Config) -> Self {
        Self {
            directory,
            config_path,
            config,
            local: Default::default(),
        }
    }

    async fn backend(&self) -> Result<Option<daemon::Client>, String> {
        self.backend_for(false).await
    }

    async fn backend_for(&self, credentials: bool) -> Result<Option<daemon::Client>, String> {
        let Some((client, status)) = daemon::task_backend(self.directory.clone())
            .await
            .map_err(|e| e.to_string())?
        else {
            return Ok(None);
        };
        if credentials && !status.credential_checks_supported {
            return Err("Restart the proxy daemon to enable periodic credential checks.".into());
        }
        if !status.metadata_supported {
            return Err(
                "Restart the proxy daemon to enable account and traffic metadata queries.".into(),
            );
        }
        let canonical =
            |path: &std::path::Path| path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        if canonical(&status.config_path) != canonical(&self.config_path) {
            return Err("The running daemon uses a different configuration. Restart it before querying account metadata.".into());
        }
        daemon::check_task_config(&status).map_err(|e| e.to_string())?;
        Ok(Some(client))
    }

    pub async fn credential_reports(
        &self,
    ) -> Result<Vec<coport::server::CredentialReport>, String> {
        if let Some(client) = self.backend_for(true).await? {
            return client.credential_reports().await.map_err(|e| e.to_string());
        }
        Ok(self.local_server().await.credential_reports().await)
    }

    pub async fn account_states(&self) -> Result<AccountStates, String> {
        if let Some(client) = self.backend().await? {
            return client.account_states().await.map_err(|e| e.to_string());
        }
        let server = self.local_server().await;
        Ok(server.account_route_states().await.map(|states| {
            states
                .into_iter()
                .map(|(key, value)| (key, value.to_owned()))
                .collect()
        }))
    }

    async fn local_server(&self) -> Arc<Server> {
        {
            let mut local = self.local.lock().await;
            local
                .get_or_insert_with(|| {
                    Arc::new(Server::new(
                        self.config.clone(),
                        Arc::new(Logger::new(
                            self.directory.join("logs/account-probes.jsonl"),
                        )),
                    ))
                })
                .clone()
        }
    }

    pub async fn cached_credential_labels(&self) -> CredentialLabels {
        // Require the current daemon capability before using its cache API;
        // legacy metadata endpoints could perform an upstream lookup.
        match self.backend_for(true).await {
            Ok(Some(client)) => {
                if let Ok(labels) = client.cached_credential_labels().await {
                    return labels;
                }
            }
            Ok(None) => {
                return self
                    .local_server()
                    .await
                    .cached_traffic_credential_labels()
                    .await;
            }
            Err(_) => {}
        }
        self.config.local_traffic_credential_labels().await
    }
    pub async fn credential_labels(&self) -> Result<CredentialLabels, String> {
        if let Some(client) = self.backend().await? {
            return client.credential_labels().await.map_err(|e| e.to_string());
        }
        Ok(self
            .local_server()
            .await
            .cached_traffic_credential_labels()
            .await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn credential_checks_fall_back_only_when_daemon_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::parse("listen_port: 8787\nrequest_timeout_seconds: 3\ncodex:\n  homes: []\nclaude:\n  config_dirs: []\n").unwrap();
        let tasks = Tasks::new(dir.path().into(), dir.path().join("config.yaml"), config);
        let reports = tasks.credential_reports().await.unwrap();
        assert_eq!(reports.len(), 2);
        assert!(reports.iter().all(|report| report.ok));
        assert!(tasks.local.lock().await.is_some());
    }

    #[test]
    fn old_or_unresponsive_daemons_do_not_run_local_metadata_queries() {
        let dir = tempfile::tempdir().unwrap();
        let up = Arc::new(std::sync::atomic::AtomicBool::new(true));
        crate::daemon::tests::fake_daemon(dir.path(), up.clone());
        let config = Config::parse("listen_port: 8787\nrequest_timeout_seconds: 3\ncodex:\n  homes: []\nclaude:\n  config_dirs: []\n").unwrap();
        let tasks = Tasks::new(dir.path().into(), dir.path().join("config.yaml"), config);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            assert!(
                tasks
                    .account_states()
                    .await
                    .unwrap_err()
                    .contains("Restart")
            );
            assert!(
                tasks
                    .credential_labels()
                    .await
                    .unwrap_err()
                    .contains("Restart")
            );
            assert!(tasks.credential_reports().await.is_err());
            tasks.cached_credential_labels().await;
            assert!(tasks.local.lock().await.is_none());
            let lock = crate::daemon::lock_file(&dir.path().join("daemon.lock")).unwrap();
            lock.try_lock().unwrap();
            up.store(false, std::sync::atomic::Ordering::SeqCst);
            assert!(
                tasks
                    .account_states()
                    .await
                    .unwrap_err()
                    .contains("control channel")
            );
            assert!(
                tasks
                    .credential_labels()
                    .await
                    .unwrap_err()
                    .contains("control channel")
            );
            assert!(tasks.credential_reports().await.is_err());
            tasks.cached_credential_labels().await;
            assert!(tasks.local.lock().await.is_none());
        });
    }
}
