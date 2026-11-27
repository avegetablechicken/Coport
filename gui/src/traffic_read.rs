//! Traffic reads: this device's Device traffic, the forwarded device's and
//! Merged Traffic, each answered from one snapshot of what it reads. A read
//! never looks an account up: it has only the credential labels already
//! known, and the forwarded device's traffic comes from the daemon's last
//! summary.
use crate::{
    daemon,
    data_client::{self, ForwardedTraffic, MergeCache, Merged, Source, ThisDevice},
    tasks::Tasks,
    traffic::TrafficScope,
    traffic_identity::{Compatibility, Identities, TrafficAssignment},
    traffic_stats::View,
};
use coport::config::Config;
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

/// What one traffic read reads, taken once so every part of the read sees
/// the same configuration, labels and choices.
pub struct Inputs {
    config: Option<Config>,
    log: PathBuf,
    sources: Vec<Source>,
    labels: BTreeMap<(String, String), String>,
    assignments: Vec<TrafficAssignment>,
    node_id: String,
    /// The running daemon, asked once per read.
    daemon: daemon::Lookup,
}

/// Peers' last summaries and the refresh reading them, kept across reads.
#[derive(Default)]
pub struct Peers {
    cache: tokio::sync::Mutex<MergeCache>,
    refreshing: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
}

impl Inputs {
    /// A read of the state directory `dir` with the panel's loaded `config`,
    /// request `log` and the devices it reads (`sources`). Credential labels
    /// are the ones `tasks` already knows.
    pub async fn take(
        dir: PathBuf,
        config: Option<Config>,
        log: PathBuf,
        sources: Vec<Source>,
        tasks: Option<Arc<Tasks>>,
    ) -> Self {
        let labels = match tasks {
            Some(tasks) => tasks.cached_credential_labels().await,
            None => Default::default(),
        };
        Self {
            config,
            log,
            sources,
            labels,
            assignments: Compatibility::assignments_in(&dir),
            node_id: crate::data_api::read_node_id(&dir).unwrap_or_else(|_| "local".into()),
            daemon: daemon::Lookup::new(dir),
        }
    }

    /// This device's traffic from its request log, with the user's choices
    /// for traffic of changed configurations.
    pub async fn local(&self, minutes: u64, scope: TrafficScope) -> Result<View, String> {
        let identities = self
            .config
            .as_ref()
            .map(|config| Identities::from_config(config, &self.assignments))
            .unwrap_or_default();
        let (log, labels) = (self.log.clone(), self.labels.clone());
        tokio::task::spawn_blocking(move || {
            crate::traffic::read(&log, minutes, &labels, scope, &identities)
        })
        .await
        .map_err(|_| "Cannot load traffic history".to_owned())?
    }

    /// The forwarded device's traffic from the daemon's last summary.
    /// Neither the device nor any upstream is contacted.
    pub async fn forwarded(
        &self,
        minutes: u64,
        scope: TrafficScope,
    ) -> Result<ForwardedTraffic, String> {
        crate::data_api::bucket_minutes(minutes).ok_or("Unsupported traffic range")?;
        let (client, status) = self
            .daemon
            .get()
            .await
            .ok_or("Start the local proxy to show forwarded traffic.")?;
        if !status.forwarding_traffic_supported {
            return Err("Restart the local proxy to show the forwarded device's traffic.".into());
        }
        self.forwarded_from(client, minutes, scope).await
    }

    /// Merged Traffic and every device's for all ranges, the selected one
    /// first. Each frame of the selected range carries this device's and the
    /// forwarded device's traffic, so its frames wait until both were read;
    /// the peers' last frame by then is published as they arrive. Peers are
    /// read again only with `refresh`, which supersedes an earlier refresh.
    pub async fn merged(
        &self,
        peers: &Peers,
        minutes: u64,
        scope: TrafficScope,
        refresh: bool,
        on_update: impl Fn(Vec<Merged>) -> Result<(), String> + Sync,
    ) -> Result<Vec<Merged>, String> {
        crate::data_api::bucket_minutes(minutes).ok_or("Unsupported traffic range")?;
        let device = self.device()?;
        let node_id = device.node_id.clone();
        let parts = std::sync::Mutex::new(DeviceParts::default());
        let selected = |view: &Merged| view.minutes == minutes && view.scope == scope;
        let complete = |views: &mut [Merged], parts: &DeviceParts| {
            let Some(local) = &parts.local else { return };
            for view in views.iter_mut().filter(|view| selected(view)) {
                view.with_devices(
                    &node_id,
                    local.as_ref().map_err(String::as_str),
                    parts.forwarded.as_ref(),
                );
            }
        };
        let publish = |mut views: Vec<Merged>| {
            {
                let mut parts = parts.lock().unwrap();
                if let Some(view) = views.iter().find(|view| selected(view)) {
                    parts.last = Some(view.clone());
                }
                if parts.local.is_some() {
                    complete(&mut views, &parts);
                } else {
                    // A frame without this device would replace a completed one.
                    views.retain(|view| !selected(view));
                    if views.is_empty() {
                        return Ok(());
                    }
                }
            }
            on_update(views)
        };
        let republish = || {
            let parts = parts.lock().unwrap();
            if let Some(mut view) = parts.last.clone() {
                complete(std::slice::from_mut(&mut view), &parts);
                drop(parts);
                let _ = on_update(vec![view]);
            }
        };
        let devices = async {
            let (local, forwarded) = tokio::join!(
                self.local(minutes, scope),
                self.live_forwarded(minutes, scope)
            );
            let mut parts = parts.lock().unwrap();
            parts.local = Some(local);
            parts.forwarded = forwarded;
            drop(parts);
            republish();
        };
        let sources = self.sources.clone();
        let peers_read = async {
            if !refresh {
                // Publish the selected range first; the 30-day views follow.
                return MergeCache::views_shared(
                    &peers.cache,
                    sources,
                    device,
                    Some((minutes, scope, &publish)),
                )
                .await;
            }
            let (cancel, cancelled) = tokio::sync::oneshot::channel();
            if let Some(previous) = peers.refreshing.lock().unwrap().replace(cancel) {
                let _ = previous.send(());
            }
            // The daemon's monitor has just read the forwarded device; reuse that
            // summary instead of opening another SSH connection to it.
            let refresh = data_client::Refresh {
                minutes,
                scope,
                reused: self.forwarded_summary().await,
            };
            // Cached views remain readable while peers respond to this refresh.
            let operation =
                MergeCache::refresh_shared(&peers.cache, sources, device, refresh, &publish);
            tokio::select! {
                result = operation => result,
                _ = cancelled => Err("Traffic request superseded".into()),
            }
        };
        // A superseded or failed read stops reading this device's parts too,
        // and publishes nothing more.
        let (mut views, ()) = tokio::try_join!(peers_read, async {
            devices.await;
            Ok(())
        })?;
        complete(&mut views, &parts.lock().unwrap());
        Ok(views)
    }

    /// This device as a merge reads it.
    fn device(&self) -> Result<ThisDevice, String> {
        Ok(ThisDevice {
            config: self
                .config
                .clone()
                .ok_or("Cannot read local configuration")?,
            log: self.log.clone(),
            labels: self.labels.clone(),
            node_id: self.node_id.clone(),
            daemon: self.daemon.clone(),
        })
    }

    async fn forwarded_from(
        &self,
        client: daemon::Client,
        minutes: u64,
        scope: TrafficScope,
    ) -> Result<ForwardedTraffic, String> {
        let window = client
            .forwarding_traffic(minutes, scope)
            .await
            .map_err(|error| error.to_string())?;
        data_client::forwarded_traffic(window, self.device()?).await
    }

    /// The forwarded device's live traffic while forwarding is on.
    async fn live_forwarded(&self, minutes: u64, scope: TrafficScope) -> Option<ForwardedTraffic> {
        let (client, status) = self.daemon.get().await?;
        if !status.forwarding_traffic_supported || status.forwarding.is_none() {
            return None;
        }
        self.forwarded_from(client, minutes, scope).await.ok()
    }

    /// The forwarded device's summary if the daemon read it within the last
    /// minute. Reading it never contacts the device.
    async fn forwarded_summary(&self) -> BTreeMap<String, crate::data_api::Summary> {
        let Some((client, status)) = self.daemon.get().await else {
            return Default::default();
        };
        if !status.forwarding_traffic_supported || status.forwarding.is_none() {
            return Default::default();
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_millis() as u64);
        match client.forwarding_summary().await {
            Ok(Some(reused)) if now.saturating_sub(reused.fetched_at) <= 60_000 => {
                [(reused.device_id, reused.summary)].into()
            }
            _ => Default::default(),
        }
    }
}

/// This device's and the forwarded device's traffic for the selected range,
/// and the peers' last frame of it.
#[derive(Default)]
struct DeviceParts {
    /// This device's read, once it finished.
    local: Option<Result<View, String>>,
    forwarded: Option<ForwardedTraffic>,
    last: Option<Merged>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::{Mutex, atomic::AtomicBool};

    const CONFIG: &str = "listen_port: 8787\nrequest_timeout_seconds: 30\ncodex:\n  homes: []\nclaude:\n  config_dirs: []\n";

    /// A state directory with a configuration and an empty request log,
    /// served by a daemon forwarding to "peer" that records every action
    /// asked of it.
    fn forwarding_daemon() -> (tempfile::TempDir, Config, Arc<Mutex<Vec<String>>>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        std::fs::write(&path, CONFIG).unwrap();
        std::fs::write(dir.path().join("proxy.log"), "").unwrap();
        let config = Config::read(&path).unwrap();
        let modified = std::fs::metadata(&path).unwrap().modified().ok();
        let end = chrono::Utc::now().timestamp_millis() / 60_000 * 60_000;
        let window = json!({
            "deviceId": "peer", "name": "Peer", "fetchedAt": 0, "error": null, "nodeId": "peer-node",
            "window": {
                "minutes": 30, "scope": TrafficScope::Model, "windowStart": end - 30 * 60_000,
                "windowEnd": end, "bucketMinutes": 1, "groups": [],
            },
        });
        let actions = Arc::default();
        crate::daemon::tests::fake_daemon_answering(
            dir.path(),
            Arc::new(AtomicBool::new(true)),
            move |status| {
                status.metadata_supported = true;
                status.credential_checks_supported = true;
                status.forwarding_traffic_supported = true;
                status.config_modified = modified;
                status.forwarding = Some(
                    serde_json::from_value(
                        json!({"deviceId": "peer", "name": "Peer", "error": null}),
                    )
                    .unwrap(),
                );
            },
            move |action| match action.as_str() {
                Some("CachedCredentialLabels") => Some(json!({"CredentialLabels": []})),
                Some("ForwardingSummary") => Some(json!({"ForwardingSummary": null})),
                _ => action
                    .get("ForwardingTraffic")
                    .map(|_| json!({"ForwardingTraffic": window.clone()})),
            },
            Arc::clone(&actions),
        );
        (dir, config, actions)
    }

    #[tokio::test]
    async fn traffic_reads_ask_the_daemon_once_and_only_for_known_labels() {
        let (dir, config, actions) = forwarding_daemon();
        let tasks = Tasks::new(
            dir.path().to_owned(),
            dir.path().join("config.yaml"),
            config.clone(),
        );
        let inputs = Inputs::take(
            dir.path().to_owned(),
            Some(config),
            dir.path().join("proxy.log"),
            Vec::new(),
            Some(Arc::new(tasks)),
        )
        .await;
        let model = TrafficScope::Model;
        inputs.local(30, model).await.unwrap();
        inputs.forwarded(30, model).await.unwrap();
        let views = inputs
            .merged(&Peers::default(), 30, model, true, |_| Ok(()))
            .await
            .unwrap();
        let selected = views
            .iter()
            .find(|view| view.minutes == 30 && view.scope == model)
            .unwrap();
        assert!(selected.local.is_some() && selected.traffic.is_some());
        assert_eq!(selected.forwarded.as_ref().unwrap().device, "peer");

        // Every read shares one label lookup and never asks for an account.
        let actions = actions.lock().unwrap();
        let cached = actions
            .iter()
            .filter(|action| *action == "\"CachedCredentialLabels\"")
            .count();
        assert_eq!(cached, 1, "{actions:?}");
        for action in actions.iter() {
            assert!(
                [
                    "\"Status\"",
                    "\"CachedCredentialLabels\"",
                    "\"ForwardingSummary\""
                ]
                .contains(&action.as_str())
                    || action.starts_with("{\"ForwardingTraffic\""),
                "{action}"
            );
        }
    }

    #[tokio::test]
    async fn a_read_takes_the_choices_and_node_id_of_its_state_directory() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::parse(CONFIG).unwrap();
        let id = "00000000-0000-4000-8000-000000000001";
        std::fs::write(dir.path().join("data-node-id"), id).unwrap();
        let mut choices = Compatibility::default();
        choices.assign("Codex".into(), "old".into(), None, Some(None));
        choices
            .save(&dir.path().join(crate::traffic_identity::FILE))
            .unwrap();
        let take = |dir: &std::path::Path| {
            Inputs::take(
                dir.to_owned(),
                Some(config.clone()),
                dir.join("proxy.log"),
                Vec::new(),
                None,
            )
        };
        let inputs = take(dir.path()).await;
        assert_eq!(inputs.device().unwrap().node_id, id);
        assert_eq!(inputs.assignments.len(), 1);

        // Without them there are no choices, and no node ID a peer could carry.
        let empty = tempfile::tempdir().unwrap();
        let inputs = take(empty.path()).await;
        assert_eq!(inputs.device().unwrap().node_id, "local");
        assert!(inputs.assignments.is_empty());
    }
}
