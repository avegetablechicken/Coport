//! Commands invoked by the panel's frontend. They run on the main thread, so
//! proxy start/stop (which briefly blocks on the proxy's own runtime) is safe.

use crate::{
    AppState,
    activity::{self, Cursor},
    core::{ActivityDto, Snapshot},
    panel, platform,
    settings::Appearance,
    tray,
};
use serde::Deserialize;
use tauri::{AppHandle, Manager, State};

type Result<T = ()> = std::result::Result<T, String>;

/// Returns immediately with the last account activation; a refresh of it runs
/// in the background and emits `state-changed` when the result differs.
#[tauri::command]
pub async fn get_state(app: AppHandle, state: State<'_, AppState>) -> Result<Snapshot> {
    let mut core = state.core.lock().unwrap();
    let mut snapshot = core.snapshot();
    if let Some(states) = core.account_states() {
        snapshot.set_account_route_states(states);
    }
    if let Some(probe) = core.begin_account_states() {
        tauri::async_runtime::spawn(async move {
            let states = probe.account_route_states().await;
            let state = app.state::<AppState>();
            state
                .core
                .lock()
                .unwrap()
                .finish_account_states(&probe, states);
        });
    }
    Ok(snapshot)
}

#[derive(Deserialize)]
pub struct ActivityRange {
    from: i64,
    to: i64,
}

/// Without `range`, the newest live events. With it, the range's entries
/// newest first, a page at a time older than `after`; the range is read once and
/// later filters, searches and pages reuse that read.
#[tauri::command]
pub async fn get_activity(
    state: State<'_, AppState>,
    filter: String,
    search: String,
    search_mode: Option<String>,
    range: Option<ActivityRange>,
    after: Option<Cursor>,
) -> Result<ActivityDto> {
    let search_mode = search_mode.unwrap_or_else(|| "keyword".into());
    let Some(range) = range else {
        let rows = state
            .core
            .lock()
            .unwrap()
            .activity(&filter, &search, &search_mode, 300);
        return Ok(ActivityDto::new(rows, None));
    };
    if range.from >= range.to {
        return Err("The start must be before the end.".into());
    }
    let query = activity::Query {
        from: range.from,
        to: range.to,
        filter,
        search_mode,
        needle: search.trim().to_lowercase(),
    };
    let mut rows = Vec::new();
    let mut after = after;
    loop {
        // One extra row tells whether another page follows.
        let (page, path) = {
            let core = state.core.lock().unwrap();
            let page = core.activity_page(&query, after, activity::PAGE + 1 - rows.len());
            (page, core.logs.path())
        };
        let begin = match page {
            Some((found, resume)) => {
                rows.extend(found);
                match resume {
                    Some(resume) if rows.len() <= activity::PAGE => Some(resume),
                    _ => break,
                }
            }
            None => after,
        };
        let (from, to) = (query.from, query.to);
        let scan = tauri::async_runtime::spawn_blocking(move || {
            activity::Scan::read(&path, from, to, begin)
        })
        .await
        .map_err(|_| "Cannot read the log".to_owned())??;
        state.core.lock().unwrap().store_activity_scan(scan);
        after = begin;
    }
    let next = (rows.len() > activity::PAGE)
        .then(|| rows.get(activity::PAGE - 1).and_then(activity::cursor))
        .flatten();
    rows.truncate(activity::PAGE);
    Ok(ActivityDto::new(rows, next))
}

#[tauri::command]
pub async fn get_traffic(
    state: State<'_, AppState>,
    minutes: u64,
    scope: Option<crate::traffic::TrafficScope>,
) -> Result<crate::traffic::Traffic> {
    let (path, config) = {
        let mut core = state.core.lock().unwrap();
        core.refresh_config();
        (core.logs.path(), core.loaded_config().cloned())
    };
    let labels = match config {
        Some(config) => config.traffic_credential_labels().await,
        None => Default::default(),
    };
    tauri::async_runtime::spawn_blocking(move || {
        crate::traffic::read(&path, minutes, &labels, scope.unwrap_or_default())
    })
    .await
    .map_err(|_| "Cannot load traffic history".to_owned())?
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

/// Replaces the configuration with a YAML file chosen in a file dialog, after
/// validating it. Returns false when the dialog is cancelled.
#[tauri::command]
pub async fn import_config(app: AppHandle) -> Result<bool> {
    use tauri_plugin_dialog::DialogExt;
    let picked = panel::with_modal(&app, || {
        app.dialog()
            .file()
            .set_title("Import Configuration")
            .add_filter("YAML", &["yaml", "yml"])
            .blocking_pick_file()
    });
    let Some(picked) = picked else {
        return Ok(false);
    };
    let path = picked.into_path().map_err(|e| e.to_string())?;
    let result = app
        .state::<AppState>()
        .core
        .lock()
        .unwrap()
        .import_config(&path);
    tray::sync(&app);
    result.map(|()| true)
}

/// Changes one supported YAML value; the proxy picks it up on restart.
#[tauri::command]
pub fn set_config_value(
    app: AppHandle,
    state: State<AppState>,
    key: String,
    value: serde_json::Value,
) -> Result {
    let result = {
        let mut core = state.core.lock().unwrap();
        let result = crate::config_edit::set_value(&core.config_path(), &key, &value);
        core.invalidate_config();
        result
    };
    tray::sync(&app);
    result
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

/// Opens or reveals a file: `config` (in the editor), `config-reveal`,
/// `log` or `log-reveal`. The log is the one the panel is reading.
#[tauri::command]
pub fn open_path(state: State<AppState>, target: String) {
    let (config, log) = {
        let core = state.core.lock().unwrap();
        (core.config_path(), core.logs.path())
    };
    match target.as_str() {
        "config" => platform::edit(&config),
        "config-reveal" => platform::reveal(&config),
        "log" if log.is_file() => platform::open(&log),
        // Nothing logged yet: show where the log will be written.
        "log" | "log-reveal" => platform::reveal(&log),
        _ => {}
    }
}

#[tauri::command]
pub fn copy_text(text: String) -> Result {
    platform::copy_text(&text)
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
