//! Daemon-owned, bounded capability checks for configured SSH devices.
use crate::remote::{Capabilities, Device};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io::{self, Read},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub device_id: String,
    pub pending: bool,
    pub capabilities: Option<Capabilities>,
    pub error: Option<String>,
}
struct Entry {
    device: Device,
    result: Status,
    checked: bool,
}
#[derive(Default)]
pub struct Checks {
    file: Option<PathBuf>,
    entries: Mutex<BTreeMap<String, Entry>>,
    changed: tokio::sync::Notify,
}
impl Checks {
    pub fn new(dir: &Path) -> Self {
        let checks = Self {
            file: Some(dir.join("gui.json")),
            ..Self::default()
        };
        let _ = checks.reload();
        checks
    }
    fn devices(&self) -> io::Result<Vec<Device>> {
        let Some(file) = &self.file else {
            return Ok(Vec::new());
        };
        let file = match std::fs::File::open(file) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let mut bytes = Vec::new();
        file.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
        if bytes.len() > 1024 * 1024 {
            return Err(io::Error::other("Device preferences exceed the size limit"));
        }
        let settings: crate::settings::Settings =
            serde_json::from_slice(&bytes).map_err(io::Error::other)?;
        if settings.managed_devices.len() > crate::devices::LIMIT {
            return Err(io::Error::other("Too many configured devices"));
        }
        Ok(settings
            .managed_devices
            .iter()
            .filter_map(|device| device.ssh_connection())
            .collect())
    }
    pub fn reload(&self) -> io::Result<()> {
        // An unreadable file is not a configuration change: keep earlier results so
        // a transient read error cannot requeue SSH checks that were denied.
        let devices = self.devices()?;
        let mut entries = self.entries.lock().unwrap();
        entries.retain(|id, _| devices.iter().any(|device| &device.name == id));
        for device in devices {
            if entries
                .get(&device.name)
                .is_some_and(|entry| entry.device == device)
            {
                continue;
            }
            entries.insert(
                device.name.clone(),
                Entry {
                    result: Status {
                        device_id: device.name.clone(),
                        pending: true,
                        capabilities: None,
                        error: None,
                    },
                    device,
                    checked: false,
                },
            );
        }
        Ok(())
    }
    pub fn refresh(&self) -> io::Result<()> {
        self.reload()?;
        self.changed.notify_one();
        Ok(())
    }
    pub fn statuses(&self) -> Vec<Status> {
        self.entries
            .lock()
            .unwrap()
            .values()
            .map(|entry| entry.result.clone())
            .collect()
    }
    fn finish(&self, device: Device, result: Result<Capabilities, String>) {
        let mut entries = self.entries.lock().unwrap();
        if let Some(entry) = entries
            .get_mut(&device.name)
            .filter(|entry| entry.device == device)
        {
            entry.result.pending = false;
            entry.checked = true;
            match result {
                Ok(caps) => {
                    entry.result.capabilities = Some(caps);
                    entry.result.error = None;
                }
                Err(error) => {
                    entry.result.capabilities = None;
                    entry.result.error = Some(error.chars().take(128).collect());
                }
            }
        }
    }
    fn take_pending(&self) -> Vec<Device> {
        self.entries
            .lock()
            .unwrap()
            .values_mut()
            .filter_map(|entry| {
                if entry.checked {
                    return None;
                }
                entry.checked = true;
                Some(entry.device.clone())
            })
            .collect()
    }
    pub async fn monitor(self: &Arc<Self>) {
        loop {
            let _ = self.reload();
            let mut pending = self.take_pending().into_iter();
            let mut tasks = tokio::task::JoinSet::new();
            loop {
                while tasks.len() < 4 {
                    let Some(device) = pending.next() else {
                        break;
                    };
                    // A removed or changed configuration must not be contacted later
                    // just because it was waiting behind another device's check.
                    if !self
                        .entries
                        .lock()
                        .unwrap()
                        .get(&device.name)
                        .is_some_and(|entry| entry.device == device)
                    {
                        continue;
                    }
                    tasks.spawn(async move {
                        let result = crate::remote::capabilities(&device).await;
                        (device, result)
                    });
                }
                if tasks.is_empty() {
                    break;
                }
                if let Some(Ok((device, result))) = tasks.join_next().await {
                    self.finish(device, result);
                }
            }
            // This only watches local configuration. Completed/failed checks are
            // never retried because a timer fires or a GUI reads their results.
            tokio::select! { _=self.changed.notified()=>{}, _=tokio::time::sleep(Duration::from_secs(30))=>{} }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn successful_and_denied_checks_are_not_requeued_by_refresh_or_polling() {
        let dir = tempfile::tempdir().unwrap();
        let write = |host: &str| {
            std::fs::write(dir.path().join("gui.json"),format!(r#"{{"managed_devices":[{{"id":"device","name":"Device","ssh":{{"host":"{host}","binary":"coportd"}},"data":null}}]}}"#)).unwrap()
        };
        write("old-host");
        let checks = Checks::new(dir.path());
        let device = checks.take_pending().pop().unwrap();
        assert!(checks.take_pending().is_empty());
        checks.finish(device, Err("Access denied".into()));
        for _ in 0..10 {
            checks.refresh().unwrap();
            assert!(checks.take_pending().is_empty());
        }
        std::fs::write(dir.path().join("gui.json"), "{not json").unwrap();
        assert!(checks.refresh().is_err());
        assert_eq!(checks.statuses()[0].error.as_deref(), Some("Access denied"));
        write("old-host");
        checks.refresh().unwrap();
        assert!(checks.take_pending().is_empty());
        write("new-host");
        checks.refresh().unwrap();
        assert_eq!(checks.take_pending().len(), 1);
    }
    #[test]
    fn configuration_changes_discard_old_capabilities_and_late_results() {
        let dir = tempfile::tempdir().unwrap();
        let write = |host: &str| {
            std::fs::write(dir.path().join("gui.json"),format!(r#"{{"managed_devices":[{{"id":"device","name":"Test","ssh":{{"host":"{host}","binary":"coportd"}},"data":null}}]}}"#)).unwrap()
        };
        write("old-host");
        let checks = Checks::new(dir.path());
        assert!(checks.statuses()[0].pending);
        let old = checks.entries.lock().unwrap()["device"].device.clone();
        write("new-host");
        checks.refresh().unwrap();
        checks.finish(old, Err("stale".into()));
        let status = checks.statuses().remove(0);
        assert!(status.pending);
        assert!(status.error.is_none());
        std::fs::write(dir.path().join("gui.json"), "{}").unwrap();
        checks.refresh().unwrap();
        assert!(checks.statuses().is_empty());
    }
}
