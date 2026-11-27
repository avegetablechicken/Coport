//! Forwarding and the Devices it can forward to: changes to the Device list,
//! the forwarding target and its restore at startup, and the daemon's checks
//! of each SSH Device. Operations run one at a time. Without a running
//! daemon, the forwarding preferences are changed on disk for the next one.
use crate::{
    daemon,
    device_capabilities::Status as Checked,
    devices::{Device, Draft},
    remote_forward::{Routing, Status, Target},
};
use std::{io, path::PathBuf};

/// Where the Device list is kept.
pub trait DeviceStore {
    fn devices(&self) -> Vec<Device>;
    /// Keep `devices` as the list, with every other preference as it is now.
    fn commit(&self, devices: Vec<Device>) -> Result<(), String>;
}

/// Forwarding as the panel shows it.
#[derive(Debug, Default)]
pub struct State {
    pub target: Option<Status>,
    pub restore: bool,
    pub restore_supported: bool,
    /// Why the restore preference of a stopped daemon cannot be read.
    pub restore_error: Option<String>,
}

pub struct Forwarding {
    dir: PathBuf,
    operations: tokio::sync::Mutex<()>,
}

impl Forwarding {
    /// Forwarding by the daemon of the state directory `dir`.
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            operations: Default::default(),
        }
    }

    pub async fn state(&self) -> Result<State, String> {
        if let Some((_, status)) = self.daemon().await? {
            return Ok(State {
                target: status.forwarding,
                restore: status.forwarding_restore,
                restore_supported: status.forwarding_restore_supported,
                restore_error: None,
            });
        }
        let dir = self.dir.clone();
        let stored = tokio::task::spawn_blocking(move || Routing::stored_restore(&dir))
            .await
            .map_err(|error| error.to_string())?;
        Ok(match stored {
            Ok(restore) => State {
                restore,
                restore_supported: true,
                ..State::default()
            },
            Err(error) => State {
                restore_error: Some(error),
                ..State::default()
            },
        })
    }

    /// The forwarding target, while forwarding is on.
    pub async fn forwarders(&self) -> Result<Vec<Status>, String> {
        Ok(self
            .daemon()
            .await?
            .and_then(|(_, status)| status.forwarding)
            .into_iter()
            .collect())
    }

    /// The daemon's checks of the SSH Devices in `store`'s list; none
    /// without a daemon. The daemon is told the list as it is now: one read
    /// before a save or removal would bring back what it changed.
    pub async fn checks(&self, store: &impl DeviceStore) -> Result<Vec<Checked>, String> {
        let _operation = self.operations.lock().await;
        match self.daemon().await? {
            Some((client, status)) => checks(&client, &status, &store.devices(), false)
                .await
                .map_err(|error| error.to_string()),
            None => Ok(Vec::new()),
        }
    }

    pub async fn set_restore(&self, enabled: bool) -> Result<(), String> {
        let _operation = self.operations.lock().await;
        let Some((client, status)) = self.daemon().await? else {
            let dir = self.dir.clone();
            return tokio::task::spawn_blocking(move || {
                Routing::configure_stopped(&dir, Some(enabled), None)
            })
            .await
            .map_err(|error| error.to_string())?;
        };
        if !status.forwarding_restore_supported {
            return Err("Update and restart the local daemon to configure startup restore.".into());
        }
        client
            .set_forwarding_restore(enabled)
            .await
            .map_err(|error| error.to_string())
    }

    /// Forward to the Device `id` of `store`, or stop forwarding to it. A
    /// Device is forwarded to only once its check found forwarding available.
    pub async fn forward(
        &self,
        store: &impl DeviceStore,
        id: &str,
        enabled: bool,
    ) -> Result<(), String> {
        let _operation = self.operations.lock().await;
        if !enabled {
            return self.stop(id).await;
        }
        let devices = store.devices();
        let device = devices
            .iter()
            .find(|device| device.id == id)
            .ok_or("Device no longer exists")?;
        let target = Target {
            device_id: id.to_owned(),
            name: device.name.clone(),
            connection: device
                .ssh_connection()
                .ok_or("Request forwarding requires SSH")?,
        };
        let (client, status) = self
            .daemon()
            .await?
            .ok_or("Start the local proxy before enabling SSH forwarding.")?;
        if !status.forwarding_supported {
            return Err(
                "Update and restart the local proxy to enable unified SSH forwarding.".into(),
            );
        }
        let checked = checks(&client, &status, &devices, true)
            .await
            .map_err(|error| error.to_string())?;
        let supported = checked
            .iter()
            .find(|entry| entry.device_id == target.device_id)
            .and_then(|entry| entry.capabilities.as_ref())
            .is_some_and(|caps| caps.forwarding && caps.forwarding_available);
        if !supported {
            return Err(
                "Remote forwarding is unavailable or its capabilities have not been verified yet."
                    .into(),
            );
        }
        client
            .set_forwarding(Some(target))
            .await
            .map_err(|error| error.to_string())
    }

    /// Add `draft` to `store`'s list, or replace the Device it edits.
    /// Forwarding to that Device stops first, since its connection may change.
    pub async fn save(&self, store: &impl DeviceStore, draft: Draft) -> Result<String, String> {
        let _operation = self.operations.lock().await;
        let mut devices = store.devices();
        let id = crate::devices::save(&mut devices, draft)?;
        self.stop(&id).await?;
        self.keep(store, devices).await?;
        Ok(id)
    }

    /// Remove the Device `id` from `store`'s list, forwarding to it first.
    pub async fn remove(&self, store: &impl DeviceStore, id: &str) -> Result<(), String> {
        let _operation = self.operations.lock().await;
        self.stop(id).await?;
        let mut devices = store.devices();
        devices.retain(|device| device.id != id);
        self.keep(store, devices).await
    }

    /// Commit `devices` and have the daemon check the new or changed ones.
    async fn keep(&self, store: &impl DeviceStore, devices: Vec<Device>) -> Result<(), String> {
        store.commit(devices.clone())?;
        if let Ok(Some((client, status))) = self.daemon().await {
            let _ = checks(&client, &status, &devices, true).await;
        }
        Ok(())
    }

    /// Stop forwarding to the Device `id`, if it is the target.
    async fn stop(&self, id: &str) -> Result<(), String> {
        match self.daemon().await? {
            Some((client, status))
                if status
                    .forwarding
                    .as_ref()
                    .is_some_and(|target| target.device_id == id) =>
            {
                client
                    .set_forwarding(None)
                    .await
                    .map_err(|error| error.to_string())
            }
            Some(_) => Ok(()),
            None => {
                let (dir, id) = (self.dir.clone(), id.to_owned());
                tokio::task::spawn_blocking(move || {
                    Routing::configure_stopped(&dir, None, Some(&id))
                })
                .await
                .map_err(|error| error.to_string())?
            }
        }
    }

    async fn daemon(&self) -> Result<Option<(daemon::Client, daemon::Status)>, String> {
        daemon::find(&self.dir)
            .await
            .map_err(|error| error.to_string())
    }
}

/// The daemon's checks of `devices`, a new or changed Device checked at
/// once. An older daemon reads the list from the GUI's preferences instead,
/// which hold `devices` by then; with `reread`, it is first told to.
async fn checks(
    client: &daemon::Client,
    status: &daemon::Status,
    devices: &[Device],
    reread: bool,
) -> io::Result<Vec<Checked>> {
    if status.device_list_supported {
        let ssh = devices.iter().filter_map(Device::ssh_connection).collect();
        return client.device_checks(ssh).await;
    }
    if reread {
        client.refresh_device_capabilities().await?;
    }
    client.device_capabilities().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::{Arc, Mutex, atomic::AtomicBool};

    /// A Device list in memory that records each commit in `events`.
    struct Store {
        devices: Mutex<Vec<Device>>,
        events: Arc<Mutex<Vec<String>>>,
    }
    impl DeviceStore for Store {
        fn devices(&self) -> Vec<Device> {
            self.devices.lock().unwrap().clone()
        }
        fn commit(&self, devices: Vec<Device>) -> Result<(), String> {
            let hosts: Vec<_> = devices
                .iter()
                .filter_map(|device| Some(device.ssh.as_ref()?.host.clone()))
                .collect();
            self.events
                .lock()
                .unwrap()
                .push(format!("commit {}", hosts.join(",")));
            *self.devices.lock().unwrap() = devices;
            Ok(())
        }
    }

    fn device(id: &str, host: &str) -> Device {
        serde_json::from_value(
            json!({"id": id, "name": id, "ssh": {"host": host, "binary": ""}, "data": null}),
        )
        .unwrap()
    }

    /// A daemon serving `dir` that records each action in `events`, edited by
    /// `edit`, forwarding to `forwarding` if any.
    fn daemon(
        dir: &std::path::Path,
        events: &Arc<Mutex<Vec<String>>>,
        forwarding: Option<&str>,
        edit: impl Fn(&mut daemon::Status) + Send + 'static,
    ) {
        let forwarding = forwarding.map(|id| json!({"deviceId": id, "name": id, "error": null}));
        crate::daemon::tests::fake_daemon_answering(
            dir,
            Arc::new(AtomicBool::new(true)),
            move |status| {
                status.forwarding_supported = true;
                status.device_list_supported = true;
                status.forwarding = forwarding
                    .clone()
                    .map(|status| serde_json::from_value(status).unwrap());
                edit(status);
            },
            |action| match action.as_str() {
                Some("DeviceCapabilities") => Some(json!({"DeviceCapabilities": []})),
                Some("RefreshDeviceCapabilities") => Some(json!("Recorded")),
                _ if action.get("SetForwarding").is_some() => Some(json!("ForwardingSet")),
                _ if action.get("DeviceChecks").is_some() => {
                    Some(json!({"DeviceCapabilities": []}))
                }
                _ => None,
            },
            Arc::clone(events),
        );
    }

    fn requests(events: &Mutex<Vec<String>>) -> Vec<String> {
        events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| *event != "\"Status\"")
            .cloned()
            .collect()
    }

    const PEER: &str = "00000000-0000-4000-8000-000000000001";
    const OTHER: &str = "00000000-0000-4000-8000-000000000002";

    #[tokio::test]
    async fn saving_a_device_stops_forwarding_to_it_then_keeps_the_list_and_has_it_checked() {
        let dir = tempfile::tempdir().unwrap();
        let events = Arc::default();
        daemon(dir.path(), &events, Some(PEER), |_| {});
        let store = Store {
            devices: Mutex::new(vec![device(PEER, "old-host"), device(OTHER, "other")]),
            events: Arc::clone(&events),
        };
        let forwarding = Forwarding::new(dir.path().to_owned());
        let draft = serde_json::from_value(json!({
            "id": PEER, "name": "Peer", "ssh": {"host": "new-host", "binary": ""}, "data": null,
        }))
        .unwrap();
        assert_eq!(forwarding.save(&store, draft).await.unwrap(), PEER);
        let requests = requests(&events);
        assert_eq!(requests[0], r#"{"SetForwarding":{"target":null}}"#);
        assert_eq!(requests[1], "commit new-host,other");
        // The daemon is told the list itself and never reads the GUI's preferences.
        let checked: serde_json::Value = serde_json::from_str(&requests[2]).unwrap();
        assert_eq!(checked["DeviceChecks"]["devices"][0]["host"], "new-host");
        assert_eq!(requests.len(), 3);

        // Removing a Device not forwarded to leaves forwarding alone.
        forwarding.remove(&store, OTHER).await.unwrap();
        let requests = self::requests(&events);
        assert_eq!(requests[3], "commit new-host");
        assert!(requests[4].starts_with(r#"{"DeviceChecks""#));
    }

    #[tokio::test]
    async fn an_older_daemon_rereads_the_list_after_it_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let events = Arc::default();
        daemon(dir.path(), &events, None, |status| {
            status.device_list_supported = false;
        });
        let store = Store {
            devices: Mutex::new(vec![device("peer", "host")]),
            events: Arc::clone(&events),
        };
        let forwarding = Forwarding::new(dir.path().to_owned());
        forwarding.remove(&store, "peer").await.unwrap();
        forwarding.checks(&store).await.unwrap();
        assert_eq!(
            requests(&events),
            [
                "commit ",
                "\"RefreshDeviceCapabilities\"",
                "\"DeviceCapabilities\"",
                "\"DeviceCapabilities\"",
            ]
        );
    }

    #[tokio::test]
    async fn forwarding_needs_a_daemon_that_supports_it_and_a_checked_device() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store {
            devices: Mutex::new(vec![device("peer", "host")]),
            events: Arc::default(),
        };
        let forwarding = Forwarding::new(dir.path().to_owned());
        assert_eq!(
            forwarding.forward(&store, "peer", true).await.unwrap_err(),
            "Start the local proxy before enabling SSH forwarding."
        );
        let events = Arc::default();
        daemon(dir.path(), &events, None, |status| {
            status.forwarding_supported = false;
        });
        assert_eq!(
            forwarding.forward(&store, "peer", true).await.unwrap_err(),
            "Update and restart the local proxy to enable unified SSH forwarding."
        );
        assert!(requests(&events).is_empty());

        let events = Arc::default();
        daemon(dir.path(), &events, None, |_| {});
        assert!(
            forwarding
                .forward(&store, "peer", true)
                .await
                .unwrap_err()
                .contains("not been verified")
        );
        let requests = requests(&events);
        assert_eq!(requests.len(), 1, "{requests:?}");
        assert!(requests[0].starts_with(r#"{"DeviceChecks""#));
    }

    #[tokio::test]
    async fn without_a_daemon_a_removed_device_is_no_longer_restored() {
        let dir = tempfile::tempdir().unwrap();
        let target = json!({
            "deviceId": "peer", "name": "Peer",
            "connection": {"name": "peer", "host": "host", "binary": ""},
        });
        std::fs::write(
            dir.path().join("forwarding.json"),
            json!({"restore": true, "target": target}).to_string(),
        )
        .unwrap();
        let store = Store {
            devices: Mutex::new(vec![device("peer", "host")]),
            events: Arc::default(),
        };
        let forwarding = Forwarding::new(dir.path().to_owned());
        forwarding.remove(&store, "peer").await.unwrap();
        assert!(store.devices().is_empty());
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.path().join("forwarding.json")).unwrap())
                .unwrap();
        assert_eq!(saved["target"], serde_json::Value::Null);
        assert_eq!(saved["restore"], true);
        let state = forwarding.state().await.unwrap();
        assert!(state.restore && state.restore_supported && state.target.is_none());
    }
}
