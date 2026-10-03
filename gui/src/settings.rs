use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

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
    pub appearance: Appearance,
    pub start_proxy_on_launch: bool,
    pub keep_proxy_running_on_quit: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            config_path: default_config_path().to_string_lossy().into_owned(),
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
        let path = settings_file();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(bytes) = serde_json::to_vec_pretty(self) {
            let _ = write_private(&path, &bytes);
        }
    }

    pub fn config_path(&self) -> PathBuf {
        agent_router::config::expand(&self.config_path)
    }

    /// Same location the CLI uses: `logs/proxy.log` next to the config file.
    pub fn log_path(&self) -> PathBuf {
        self.config_path()
            .parent()
            .unwrap_or(Path::new("."))
            .join("logs/proxy.log")
    }
}

pub fn app_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("agent-router")
}

fn settings_file() -> PathBuf {
    app_dir().join("gui.json")
}

/// A `config.yaml` in the working directory wins, matching the CLI default;
/// otherwise the per-user application directory is used.
fn default_config_path() -> PathBuf {
    let local = PathBuf::from("config.yaml");
    if local.is_file()
        && let Ok(abs) = std::path::absolute(&local)
    {
        return abs;
    }
    app_dir().join("config.yaml")
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
