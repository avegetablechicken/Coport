use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const BUNDLE_IDENTIFIER: &str = "io.github.coport.gui";

#[derive(Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Appearance {
    #[default]
    System,
    Light,
    Dark,
}

/// Preferences of the GUI itself; proxy behavior stays in the YAML config.
/// Older `config_path`/`log_path` entries are ignored: both paths are fixed.
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub appearance: Appearance,
    pub start_proxy_on_launch: bool,
    pub keep_proxy_running_on_quit: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            appearance: Appearance::System,
            start_proxy_on_launch: true,
            keep_proxy_running_on_quit: false,
        }
    }
}

impl Settings {
    pub fn load() -> Self {
        std::fs::read(settings_file())
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let _ = self.try_save();
    }

    pub fn try_save(&self) -> std::io::Result<()> {
        write_private(&settings_file(), &serde_json::to_vec_pretty(self)?)
    }
}

pub fn app_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(BUNDLE_IDENTIFIER)
}

fn settings_file() -> PathBuf {
    app_dir().join("gui.json")
}

/// The GUI's only configuration file, independent of its working directory.
pub fn config_path() -> PathBuf {
    app_dir().join("config.yaml")
}

/// The GUI's request log, beside its configuration.
pub fn log_path() -> PathBuf {
    app_dir().join("logs/proxy.log")
}

pub(crate) fn legacy_cache_dir() -> PathBuf {
    dirs::cache_dir()
        .expect("Cannot locate the current user's application cache directory")
        .join(BUNDLE_IDENTIFIER)
}

/// Atomically replaces `path`, creating it with owner-only permissions.
pub fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    temp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_namespace_matches_the_application_bundle_identifier() {
        let config: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        assert_eq!(config["identifier"], BUNDLE_IDENTIFIER);
        assert_eq!(
            legacy_cache_dir(),
            dirs::cache_dir().unwrap().join(BUNDLE_IDENTIFIER)
        );
    }

    #[test]
    fn paths_are_fixed_in_the_application_support_directory() {
        let support = dirs::config_dir().unwrap().join(BUNDLE_IDENTIFIER);
        assert_eq!(config_path(), support.join("config.yaml"));
        assert_eq!(log_path(), support.join("logs/proxy.log"));
        // Paths saved by older versions are ignored and not written back.
        let settings: Settings = serde_json::from_str(
            r#"{"config_path":"/custom/config.yaml","log_path":"/custom.log","appearance":"Dark"}"#,
        )
        .unwrap();
        assert!(settings.appearance == Appearance::Dark);
        let saved = serde_json::to_value(&settings).unwrap();
        assert!(saved.get("config_path").is_none() && saved.get("log_path").is_none());
    }

    #[test]
    fn old_preferences_keep_existing_quit_behavior() {
        let settings: Settings =
            serde_json::from_str(r#"{"appearance":"Dark","start_proxy_on_launch":false}"#).unwrap();
        assert!(!settings.keep_proxy_running_on_quit);
        let enabled = Settings {
            keep_proxy_running_on_quit: true,
            ..settings
        };
        let restored: Settings =
            serde_json::from_slice(&serde_json::to_vec(&enabled).unwrap()).unwrap();
        assert!(restored.keep_proxy_running_on_quit);
    }
}
