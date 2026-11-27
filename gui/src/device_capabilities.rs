//! Daemon-owned, bounded capability checks for the SSH devices the GUI
//! manages. The GUI names them with every request; the daemon keeps them in
//! memory only and never reads the GUI's preferences.
use crate::remote::{Capabilities, Device};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub device_id: String,
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub binary: String,
    #[serde(default)]
    pub checked_at: Option<u64>,
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
    entries: Mutex<BTreeMap<String, Entry>>,
    changed: tokio::sync::Notify,
}
impl Checks {
    /// Check `devices` from now on: a device that is new or changed is checked
    /// at once, one no longer named is forgotten with its result, and the
    /// others keep theirs.
    pub fn set(&self, devices: Vec<Device>) -> io::Result<()> {
        if devices.len() > crate::devices::LIMIT {
            return Err(io::Error::other("Too many configured devices"));
        }
        let mut entries = self.entries.lock().unwrap();
        entries.retain(|id, _| devices.iter().any(|device| &device.name == id));
        for device in devices {
            if entries
                .get(&device.name)
                .is_some_and(|entry| entry.device == device)
            {
                continue;
            }
            self.changed.notify_one();
            entries.insert(
                device.name.clone(),
                Entry {
                    result: Status {
                        device_id: device.name.clone(),
                        host: device.host.clone(),
                        binary: device.binary.clone(),
                        checked_at: None,
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
    /// Check now whatever is waiting to be checked.
    pub fn refresh(&self) {
        self.changed.notify_one();
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
            entry.result.checked_at = Some(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis()
                    .min(u64::MAX as u128) as u64,
            );
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
            // Only a new or changed device is checked. Completed/failed checks are
            // never retried because a timer fires or a GUI reads their results.
            tokio::select! { _=self.changed.notified()=>{}, _=tokio::time::sleep(Duration::from_secs(30))=>{} }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn device(host: &str) -> Device {
        Device {
            name: "device".into(),
            host: host.into(),
            binary: "coportd".into(),
        }
    }
    #[test]
    fn successful_and_denied_checks_are_not_requeued_by_refresh_or_polling() {
        let checks = Checks::default();
        checks.set(vec![device("old-host")]).unwrap();
        let checked = checks.take_pending().pop().unwrap();
        assert!(checks.take_pending().is_empty());
        checks.finish(checked, Err("Access denied".into()));
        let observed = checks.statuses().remove(0);
        assert!(observed.checked_at.is_some());
        assert_eq!(observed.host, "old-host");
        assert_eq!(observed.binary, "coportd");
        for _ in 0..10 {
            checks.set(vec![device("old-host")]).unwrap();
            checks.refresh();
            assert!(checks.take_pending().is_empty());
            assert_eq!(checks.statuses()[0].checked_at, observed.checked_at);
        }
        // A list too long to check changes nothing.
        assert!(checks.set(vec![device("old-host"); 33]).is_err());
        assert_eq!(checks.statuses()[0].error.as_deref(), Some("Access denied"));
        checks.set(vec![device("new-host")]).unwrap();
        assert_eq!(checks.take_pending().len(), 1);
    }
    #[test]
    fn configuration_changes_discard_old_capabilities_and_late_results() {
        let checks = Checks::default();
        checks.set(vec![device("old-host")]).unwrap();
        assert!(checks.statuses()[0].pending);
        let old = checks.entries.lock().unwrap()["device"].device.clone();
        checks.set(vec![device("new-host")]).unwrap();
        checks.finish(old, Err("stale".into()));
        let status = checks.statuses().remove(0);
        assert!(status.pending);
        assert!(status.error.is_none());
        checks.set(Vec::new()).unwrap();
        assert!(checks.statuses().is_empty());
    }
}
