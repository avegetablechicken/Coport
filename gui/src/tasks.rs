//! GUI operations prefer the daemon and fall back only after proving absence.
use crate::daemon;
use coport::{
    config::Config,
    logger::Logger,
    server::{ClaudeProfiles, Server},
};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

pub type AccountStates = [BTreeMap<String, String>; 2];
pub type CredentialLabels = BTreeMap<(String, String), String>;

pub struct Tasks {
    directory: PathBuf,
    config_path: PathBuf,
    config: Config,
    local: tokio::sync::Mutex<Option<Arc<Server>>>,
    claude_profiles: ClaudeProfiles,
}

impl Tasks {
    pub fn new(directory: PathBuf, config_path: PathBuf, config: Config) -> Self {
        Self {
            directory,
            config_path,
            config,
            local: Default::default(),
            claude_profiles: Default::default(),
        }
    }

    /// Keep Claude identities found by earlier configurations, so a reload does
    /// not repeat their profile lookups.
    pub fn with_claude_profiles(mut self, profiles: ClaudeProfiles) -> Self {
        self.claude_profiles = profiles;
        self
    }

    async fn backend(&self) -> Result<Option<daemon::Client>, String> {
        self.backend_for(false).await
    }

    async fn backend_for(&self, credentials: bool) -> Result<Option<daemon::Client>, String> {
        Ok(self
            .backend_status(credentials)
            .await?
            .map(|(client, _)| client))
    }

    async fn backend_status(
        &self,
        credentials: bool,
    ) -> Result<Option<(daemon::Client, daemon::Status)>, String> {
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
        Ok(Some((client, status)))
    }

    pub async fn credential_reports(
        &self,
    ) -> Result<Vec<coport::server::CredentialReport>, String> {
        if let Some(client) = self.backend_for(true).await? {
            return client.credential_reports().await.map_err(|e| e.to_string());
        }
        Ok(self.local_server().await.credential_reports().await)
    }

    /// Route activation from local data and identities already learned; this
    /// never probes upstream or runs a login shell.
    pub async fn account_states(&self) -> Result<AccountStates, String> {
        // Only file-only mode reports activation.
        if !self.config.codex.account_auth_file_only && !self.config.claude.account_auth_file_only {
            return Ok(Default::default());
        }
        if let Some((client, status)) = self.backend_status(false).await? {
            if !status.passive_account_states_supported {
                return Err("Restart the proxy daemon to show account status.".into());
            }
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
                    Arc::new(
                        Server::new(
                            self.config.clone(),
                            Arc::new(Logger::new(
                                self.directory.join("logs/account-probes.jsonl"),
                            )),
                        )
                        .with_claude_profiles(self.claude_profiles.clone()),
                    )
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

    #[tokio::test]
    async fn rebuilt_tasks_reuse_claude_identities_without_probing() {
        let dir = tempfile::tempdir().unwrap();
        let claude = dir.path().join("claude");
        std::fs::create_dir(&claude).unwrap();
        std::fs::write(
            claude.join(".credentials.json"),
            r#"{"claudeAiOauth":{"accessToken":"saved-secret"}}"#,
        )
        .unwrap();
        // Counts connections to the probe route and closes them at once.
        let trap = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = trap.local_addr().unwrap().port();
        let connections = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = connections.clone();
        std::thread::spawn(move || {
            for stream in trap.incoming() {
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                drop(stream);
            }
        });
        let config = Config::parse(&format!(
            "listen_port: 8787\nrequest_timeout_seconds: 3\nproxies:\n  trap: http://127.0.0.1:{port}\ncodex:\n  homes: []\nclaude:\n  config_dirs: [{}]\n  base_url: https://upstream.invalid\n  routing:\n    account:\n      remote@example.invalid: none\n    account_probe: trap\n",
            serde_json::to_string(&claude).unwrap()
        ))
        .unwrap();
        let profiles = ClaudeProfiles::default();
        profiles.lock().unwrap().insert(
            "saved-secret".into(),
            (
                std::time::Instant::now(),
                coport::claude::ClaudeIdentity {
                    account_id: "remote-account".into(),
                    usernames: vec!["remote@example.invalid".into()],
                },
            ),
        );
        let identity = ("Claude".to_owned(), "remote-account".to_owned());
        // Each configuration reload builds new tasks around the same cache.
        for _ in 0..3 {
            let tasks = Tasks::new(
                dir.path().into(),
                dir.path().join("config.yaml"),
                config.clone(),
            )
            .with_claude_profiles(profiles.clone());
            tasks.credential_reports().await.unwrap();
            assert_eq!(
                tasks
                    .cached_credential_labels()
                    .await
                    .get(&identity)
                    .map(String::as_str),
                Some("remote@example.invalid")
            );
        }
        assert_eq!(connections.load(std::sync::atomic::Ordering::SeqCst), 0);
        // Without the shared cache the same check does probe.
        let tasks = Tasks::new(dir.path().into(), dir.path().join("config.yaml"), config);
        tasks.credential_reports().await.unwrap();
        assert!(connections.load(std::sync::atomic::Ordering::SeqCst) > 0);
    }

    #[tokio::test]
    async fn account_states_never_query_daemons_that_would_probe() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::parse(&format!("listen_port: 8787\nrequest_timeout_seconds: 3\ncodex:\n  homes: []\nclaude:\n  config_dirs: [{}]\n  routing:\n    account: {{'remote@example.invalid': none}}\n", serde_json::to_string(dir.path()).unwrap())).unwrap();
        let tasks = Tasks::new(dir.path().into(), dir.path().join("config.yaml"), config);
        let actions = Arc::<std::sync::Mutex<Vec<String>>>::default();
        let up = Arc::new(std::sync::atomic::AtomicBool::new(true));
        // An older daemon answers metadata queries but probes for account states.
        crate::daemon::tests::fake_daemon_with(
            dir.path(),
            up,
            |status| status.metadata_supported = true,
            actions.clone(),
        );
        for _ in 0..2 {
            assert_eq!(
                tasks.account_states().await.unwrap_err(),
                "Restart the proxy daemon to show account status."
            );
        }
        assert!(
            actions
                .lock()
                .unwrap()
                .iter()
                .all(|action| action == "\"Status\""),
            "{actions:?}"
        );
        // Without file-only mode there is nothing to report, and nothing is asked.
        let mut config = tasks.config.clone();
        config.claude.account_auth_file_only = false;
        config.codex.account_auth_file_only = false;
        let before = actions.lock().unwrap().len();
        let tasks = Tasks::new(dir.path().into(), dir.path().join("config.yaml"), config);
        assert_eq!(
            tasks.account_states().await.unwrap(),
            AccountStates::default()
        );
        assert_eq!(actions.lock().unwrap().len(), before);
    }

    #[tokio::test]
    async fn local_account_states_read_learned_identities_without_probing() {
        let dir = tempfile::tempdir().unwrap();
        let claude = dir.path().join("claude");
        std::fs::create_dir(&claude).unwrap();
        std::fs::write(
            claude.join(".credentials.json"),
            r#"{"claudeAiOauth":{"accessToken":"saved-secret"}}"#,
        )
        .unwrap();
        // Counts connections to the probe route and closes them at once.
        let trap = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = trap.local_addr().unwrap().port();
        let connections = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = connections.clone();
        std::thread::spawn(move || {
            for stream in trap.incoming() {
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                drop(stream);
            }
        });
        let config = Config::parse(&format!(
            "listen_port: 8787\nrequest_timeout_seconds: 3\nproxies:\n  trap: http://127.0.0.1:{port}\ncodex:\n  homes: []\nclaude:\n  config_dirs: [{}]\n  base_url: https://upstream.invalid\n  routing:\n    account:\n      remote@example.invalid: none\n      other@example.invalid: none\n    account_probe: trap\n",
            serde_json::to_string(&claude).unwrap()
        ))
        .unwrap();
        let profiles = ClaudeProfiles::default();
        let tasks = Tasks::new(dir.path().into(), dir.path().join("config.yaml"), config)
            .with_claude_profiles(profiles.clone());
        // Unidentified until discovery or a request learns the token's account.
        for _ in 0..3 {
            let states = tasks.account_states().await.unwrap();
            assert_eq!(states[1]["remote@example.invalid"], "unknown");
        }
        profiles.lock().unwrap().insert(
            "saved-secret".into(),
            (
                std::time::Instant::now(),
                coport::claude::ClaudeIdentity {
                    account_id: "remote-account".into(),
                    usernames: vec!["remote@example.invalid".into()],
                },
            ),
        );
        for _ in 0..3 {
            let states = tasks.account_states().await.unwrap();
            assert_eq!(states[1]["remote@example.invalid"], "remote");
            assert_eq!(states[1]["other@example.invalid"], "inactive");
        }
        assert_eq!(connections.load(std::sync::atomic::Ordering::SeqCst), 0);
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
