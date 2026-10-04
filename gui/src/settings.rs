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
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub config_path: String,
    pub log_path: String,
    pub appearance: Appearance,
    pub start_proxy_on_launch: bool,
    pub keep_proxy_running_on_quit: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            config_path: default_config_path().to_string_lossy().into_owned(),
            log_path: cache_dir()
                .join("logs/proxy.log")
                .to_string_lossy()
                .into_owned(),
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

    pub fn config_path(&self) -> PathBuf {
        coport::config::expand(&self.config_path)
    }

    /// GUI logs have their own location, independent of the proxy configuration.
    pub fn log_path(&self) -> PathBuf {
        coport::config::expand(&self.log_path)
    }
}

pub fn app_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("coport")
}

fn settings_file() -> PathBuf {
    app_dir().join("gui.json")
}

/// The GUI has one default location, independent of its working directory.
/// Explicit paths chosen in Settings or through --config still take precedence.
fn default_config_path() -> PathBuf {
    cache_dir().join("config.yaml")
}

fn cache_dir() -> PathBuf {
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
            cache_dir(),
            dirs::cache_dir().unwrap().join(BUNDLE_IDENTIFIER)
        );
    }

    #[test]
    fn log_path_defaults_to_cache_and_can_be_overridden_independently() {
        let settings: Settings =
            serde_json::from_str(r#"{"config_path":"/custom/config.yaml"}"#).unwrap();
        assert_eq!(
            settings.log_path(),
            dirs::cache_dir()
                .unwrap()
                .join("io.github.coport.gui/logs/proxy.log")
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("custom.log");
        let settings: Settings =
            serde_json::from_value(serde_json::json!({"log_path":path})).unwrap();
        assert_eq!(settings.log_path(), path);
        let restored: Settings =
            serde_json::from_slice(&serde_json::to_vec(&settings).unwrap()).unwrap();
        assert_eq!(restored.log_path(), path);
    }

    #[test]
    fn default_config_uses_only_the_application_cache() {
        let expected = dirs::cache_dir()
            .unwrap()
            .join("io.github.coport.gui/config.yaml");
        assert_eq!(Settings::default().config_path(), expected);
        let missing_path: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(missing_path.config_path(), expected);
    }

    #[test]
    fn explicitly_selected_yaml_paths_are_preserved() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["custom.yaml", "custom.yml"] {
            let path = dir.path().join(name);
            let settings: Settings = serde_json::from_value(serde_json::json!({
                "config_path": path,
            }))
            .unwrap();
            assert_eq!(settings.config_path(), path);
        }
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
