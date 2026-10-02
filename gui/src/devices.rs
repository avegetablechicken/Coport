//! One settings record per device for read-only statistics.
use serde::{Deserialize, Serialize};

pub const LIMIT: usize = 32;
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Ssh {
    pub host: String,
    #[serde(default)]
    pub binary: String,
    // Accepted only to migrate older preferences; no management capability exists.
    #[serde(default, skip_serializing)]
    pub management: bool,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Data {
    #[serde(default)]
    pub transport: crate::data_client::Transport,
    #[serde(default)]
    pub url: String,
    pub token_file: Option<String>,
    pub token_env: Option<String>,
    pub ca_certificate: Option<String>,
    #[serde(default, skip_serializing)]
    pub match_known_configurations: bool,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Device {
    pub id: String,
    pub name: String,
    pub ssh: Option<Ssh>,
    pub data: Option<Data>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Draft {
    pub id: Option<String>,
    pub name: String,
    pub ssh: Option<Ssh>,
    pub data: Option<Data>,
}
impl Device {
    pub fn ssh_connection(&self) -> Option<crate::remote::Device> {
        self.ssh.as_ref().map(|ssh| crate::remote::Device {
            name: self.id.clone(),
            host: ssh.host.clone(),
            binary: ssh.binary.clone(),
        })
    }
    pub fn data_source(&self) -> Option<crate::data_client::Source> {
        self.data.as_ref().map(|data| crate::data_client::Source {
            name: self.name.clone(),
            transport: data.transport,
            ssh_connection: self.ssh_connection(),
            url: data.url.clone(),
            token_file: data.token_file.clone(),
            token_env: data.token_env.clone(),
            ca_certificate: data.ca_certificate.clone(),
            device_id: Some(self.id.clone()),
            ssh_device: None,
        })
    }
    fn validate(&self) -> Result<(), String> {
        if uuid::Uuid::parse_str(&self.id).is_err() {
            return Err("Invalid device ID.".into());
        }
        if self.name.trim().is_empty()
            || self.name.len() > 128
            || self.name.chars().any(char::is_control)
        {
            return Err("Provide a device name (up to 128 bytes).".into());
        }
        if self.ssh.is_none() && self.data.is_none() {
            return Err("Enable an SSH connection or a statistics interface.".into());
        }
        if let Some(ssh) = self.ssh_connection() {
            ssh.validate()?;
        }
        if let Some(data) = self.data_source() {
            data.validate()?;
        }
        Ok(())
    }
}
pub fn save(devices: &mut Vec<Device>, draft: Draft) -> Result<String, String> {
    let editing = draft
        .id
        .as_ref()
        .map(|id| {
            devices
                .iter()
                .position(|d| &d.id == id)
                .ok_or_else(|| "Device no longer exists; reload Settings.".to_owned())
        })
        .transpose()?;
    if editing.is_none() && devices.len() >= LIMIT {
        return Err(format!("At most {LIMIT} devices are supported."));
    }
    let id = draft.id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let device = Device {
        id: id.clone(),
        name: draft.name,
        ssh: draft.ssh,
        data: draft.data,
    };
    device.validate()?;
    if devices.iter().any(|d| d.id != id && d.name == device.name) {
        return Err("A device already uses this name. Edit it or choose a different name.".into());
    }
    if let Some(index) = editing {
        devices[index] = device;
    } else {
        devices.push(device);
    }
    Ok(id)
}
/// Preserve legacy connections, including data-only entries and renamed links.
pub fn migrate(ssh: &[crate::remote::Device], data: &[crate::data_client::Source]) -> Vec<Device> {
    let mut result: Vec<_> = ssh
        .iter()
        .map(|d| Device {
            id: uuid::Uuid::new_v4().to_string(),
            name: d.name.clone(),
            ssh: Some(Ssh {
                host: d.host.clone(),
                binary: d.binary.clone(),
                management: true,
            }),
            data: None,
        })
        .collect();
    for source in data {
        let target = source.ssh_device.as_deref().unwrap_or(&source.name);
        let found = result
            .iter()
            .position(|d| d.name == target && d.data.is_none());
        let index = if let Some(index) = found {
            index
        } else {
            let mut name = source.name.clone();
            let mut suffix = 2;
            while result.iter().any(|d| d.name == name) {
                name = format!("{} ({suffix})", source.name);
                suffix += 1;
            }
            let connection = source
                .ssh_device
                .as_ref()
                .and_then(|name| ssh.iter().find(|d| &d.name == name));
            result.push(Device {
                id: uuid::Uuid::new_v4().to_string(),
                name,
                ssh: connection.map(|d| Ssh {
                    host: d.host.clone(),
                    binary: d.binary.clone(),
                    management: true,
                }),
                data: None,
            });
            result.len() - 1
        };
        result[index].data = Some(Data {
            transport: crate::data_client::Transport::Http,
            url: source.url.clone(),
            token_file: source.token_file.clone(),
            token_env: source.token_env.clone(),
            ca_certificate: source.ca_certificate.clone(),
            match_known_configurations: false,
        });
    }
    result
}
#[cfg(test)]
mod tests {
    use super::*;
    fn draft(name: &str) -> Draft {
        Draft {
            id: None,
            name: name.into(),
            ssh: Some(Ssh {
                host: name.into(),
                binary: "coportd".into(),
                management: true,
            }),
            data: None,
        }
    }
    #[test]
    fn multiple_devices_and_renames_do_not_overwrite_other_records() {
        let mut devices = Vec::new();
        let first = save(&mut devices, draft("first")).unwrap();
        let second = save(&mut devices, draft("second")).unwrap();
        assert_ne!(first, second);
        assert_eq!(devices.len(), 2);
        let mut edit = draft("renamed");
        edit.id = Some(first.clone());
        save(&mut devices, edit).unwrap();
        assert_eq!(devices[0].id, first);
        assert_eq!(devices[0].name, "renamed");
        assert_eq!(devices[1].id, second);
        assert!(save(&mut devices, draft("renamed")).is_err());
        assert_eq!(devices.len(), 2);
        devices.retain(|d| d.id != first);
        assert_eq!(devices[0].id, second);
    }
    #[test]
    fn each_device_may_use_ssh_data_or_both() {
        let mut devices = Vec::new();
        let mut data = draft("data-only");
        data.ssh = None;
        data.data = Some(Data {
            transport: crate::data_client::Transport::Http,
            url: "http://10.42.0.196:8788".into(),
            token_file: None,
            token_env: Some("DATA_KEY".into()),
            ca_certificate: None,
            match_known_configurations: false,
        });
        save(&mut devices, data).unwrap();
        assert!(devices[0].ssh_connection().is_none());
        assert!(devices[0].data_source().unwrap().ssh_device.is_none());
        let mut both = draft("both");
        both.data = devices[0].data.clone();
        both.data.as_mut().unwrap().match_known_configurations = true;
        let id = save(&mut devices, both).unwrap();
        assert_eq!(devices[1].id, id);
        assert!(devices[1].data_source().unwrap().ssh_device.is_none());
    }
    #[test]
    fn ssh_statistics_require_no_http_key_and_do_not_enable_management() {
        let mut draft = draft("reader");
        draft.ssh.as_mut().unwrap().management = false;
        draft.data = Some(Data {
            transport: crate::data_client::Transport::Ssh,
            url: String::new(),
            token_file: None,
            token_env: None,
            ca_certificate: None,
            match_known_configurations: false,
        });
        let mut devices = Vec::new();
        save(&mut devices, draft).unwrap();
        let source = devices[0].data_source().unwrap();
        source.validate().unwrap();
        assert!(!devices[0].ssh.as_ref().unwrap().management);
        assert!(source.token_file.is_none() && source.token_env.is_none() && source.url.is_empty());
        assert!(source.ssh_device.is_none());
    }
    #[test]
    fn alias_only_form_payload_saves_and_round_trips_as_ssh() {
        let draft: Draft = serde_json::from_value(serde_json::json!({
            "id": null, "name": "mbp16",
            "ssh": {"host": "mbp16", "binary": "coportd"},
            "data": {"transport": "ssh", "url": "", "tokenFile": null,
                     "tokenEnv": null, "caCertificate": null}
        }))
        .unwrap();
        let mut devices = Vec::new();
        save(&mut devices, draft).unwrap();
        let stored = serde_json::to_value(&devices).unwrap();
        assert!(stored[0]["ssh"].get("management").is_none());
        let loaded: Vec<Device> = serde_json::from_value(stored).unwrap();
        let source = loaded[0].data_source().unwrap();
        assert!(source.transport == crate::data_client::Transport::Ssh);
        assert!(source.url.is_empty());
        source.validate().unwrap();
        let mut http = source.clone();
        http.transport = crate::data_client::Transport::Http;
        assert_eq!(http.validate().unwrap_err(), "Invalid data source URL");
    }
    #[test]
    fn migration_preserves_linked_and_data_only_connections() {
        let ssh = vec![crate::remote::Device {
            name: "Mac".into(),
            host: "mbp16".into(),
            binary: "coportd".into(),
        }];
        let make = |name: &str, link: Option<&str>| crate::data_client::Source {
            transport: crate::data_client::Transport::Http,
            ssh_connection: None,
            name: name.into(),
            url: "https://data.example".into(),
            token_env: Some("DATA_KEY".into()),
            token_file: None,
            ca_certificate: None,
            device_id: None,
            ssh_device: link.map(str::to_owned),
        };
        let migrated = migrate(
            &ssh,
            &[make("Mac statistics", Some("Mac")), make("Other", None)],
        );
        assert_eq!(migrated.len(), 2);
        assert_eq!(migrated[0].name, "Mac");
        assert!(
            !migrated[0]
                .data
                .as_ref()
                .unwrap()
                .match_known_configurations
        );
        assert!(migrated[1].ssh.is_none());
        assert!(
            !migrated[1]
                .data
                .as_ref()
                .unwrap()
                .match_known_configurations
        );
    }
}
