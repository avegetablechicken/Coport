//! Commands invoked by the panel's frontend. They run on the main thread, so
//! proxy start/stop (which briefly blocks on the proxy's own runtime) is safe.

use crate::{
    AppState,
    core::{EntryDto, Snapshot},
    panel, platform,
    settings::Appearance,
    tray,
};
use serde::Deserialize;
use tauri::{AppHandle, State};

type Result<T = ()> = std::result::Result<T, String>;

#[tauri::command]
pub fn get_state(state: State<AppState>) -> Snapshot {
    state.core.lock().unwrap().snapshot()
}

#[tauri::command]
pub fn get_activity(state: State<AppState>, filter: String, search: String) -> Vec<EntryDto> {
    state.core.lock().unwrap().activity(&filter, &search, 300)
}

#[tauri::command]
pub fn set_running(app: AppHandle, state: State<AppState>, running: bool) -> Result {
    {
        let mut core = state.core.lock().unwrap();
        if running {
            core.start();
        } else {
            core.stop()?;
        }
    }
    tray::sync(&app);
    Ok(())
}

#[tauri::command]
pub fn restart_proxy(app: AppHandle, state: State<AppState>) {
    state.core.lock().unwrap().start();
    tray::sync(&app);
}

#[tauri::command]
pub fn check_credentials(state: State<AppState>) {
    let core = state.core.lock().unwrap();
    core.controller.check(&core.config_path());
}

/// Tests one proxy, or all of them when `name` is absent. With `stale_only`,
/// only proxies without a recent result are tested (used when the panel opens).
#[tauri::command]
pub fn probe_proxy(state: State<AppState>, name: Option<String>, stale_only: Option<bool>) {
    let mut core = state.core.lock().unwrap();
    if stale_only == Some(true) {
        core.refresh_config();
        core.probe_stale(std::time::Duration::from_secs(120));
    } else {
        core.probe(name.as_deref());
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsPatch {
    appearance: Option<Appearance>,
    start_proxy_on_launch: Option<bool>,
    keep_proxy_running_on_quit: Option<bool>,
}

#[tauri::command]
pub fn update_settings(app: AppHandle, state: State<AppState>, patch: SettingsPatch) {
    {
        let mut core = state.core.lock().unwrap();
        if let Some(appearance) = patch.appearance {
            core.settings.appearance = appearance;
            crate::apply_appearance(&app, appearance);
        }
        if let Some(start) = patch.start_proxy_on_launch {
            core.settings.start_proxy_on_launch = start;
        }
        if let Some(keep_running) = patch.keep_proxy_running_on_quit {
            core.settings.keep_proxy_running_on_quit = keep_running;
        }
        core.settings.save();
    }
    tray::sync(&app);
}

#[tauri::command]
pub fn set_launch_at_login(state: State<AppState>, enabled: bool) -> Result {
    state.core.lock().unwrap().set_launch_at_login(enabled)
}

#[tauri::command]
pub fn set_config_path(app: AppHandle, state: State<AppState>, path: String) {
    {
        let mut core = state.core.lock().unwrap();
        core.settings.config_path = path.trim().to_owned();
        core.settings.save();
        core.invalidate_config();
        let log = core.settings.log_path();
        core.logs.set_path(log);
        if core.controller.is_running() {
            core.start();
        }
    }
    tray::sync(&app);
}

const EXAMPLE: &str = include_str!("../../config.example.yaml");

#[tauri::command]
pub fn create_example_config(state: State<AppState>) -> Result {
    let mut core = state.core.lock().unwrap();
    let path = core.config_path();
    crate::settings::write_private(&path, EXAMPLE.as_bytes()).map_err(|e| e.to_string())?;
    core.invalidate_config();
    platform::edit(&path);
    Ok(())
}

/// Opens a file or folder: `config`, `config-folder`, `log-folder` or `log`.
#[tauri::command]
pub fn open_path(state: State<AppState>, target: String) {
    let core = state.core.lock().unwrap();
    let config = core.config_path();
    let log = core.settings.log_path();
    match target.as_str() {
        "config" => platform::edit(&config),
        "config-folder" => config.parent().into_iter().for_each(platform::open),
        "log" => platform::open(&log),
        "log-folder" => {
            if let Some(dir) = log.parent() {
                let _ = std::fs::create_dir_all(dir);
                platform::open(dir);
            }
        }
        _ => {}
    }
}

#[tauri::command]
pub fn copy_text(text: String) -> Result {
    platform::copy_text(&text)
}

#[tauri::command]
pub fn clear_activity(state: State<AppState>) {
    state.core.lock().unwrap().logs.clear();
}

#[tauri::command]
pub fn fit_panel(app: AppHandle, height: f64) {
    panel::fit(&app, height);
}

#[tauri::command]
pub fn hide_panel(app: AppHandle) {
    panel::hide(&app);
}

#[tauri::command]
pub fn quit_app(app: AppHandle) {
    app.exit(0);
}
