//! Commands invoked by the panel. Daemon lifecycle operations run on their own
//! worker; commands await completion without holding the application-state lock.

use crate::{
    AppState,
    activity::{self, Cursor},
    core::{ActivityDto, Snapshot},
    panel, platform,
    settings::Appearance,
    traffic_identity::{Compatibility, TrafficTarget},
    tray,
};
use serde::Deserialize;
use tauri::{AppHandle, Manager, State};

type Result<T = ()> = std::result::Result<T, String>;

/// Detect pending edits without applying them or running any network checks.
pub(crate) async fn monitor_config(app: AppHandle) {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        app.state::<AppState>()
            .core
            .lock()
            .unwrap()
            .refresh_config();
    }
}

/// Runs independently of WebView visibility, including GUI-only operation.
pub(crate) async fn monitor_credentials(app: AppHandle) {
    loop {
        let task = {
            let state = app.state::<AppState>();
            let mut core = state.core.lock().unwrap();
            core.begin_credential_check()
        };
        if let Some(task) = task {
            let result = task.credential_reports().await;
            app.state::<AppState>()
                .core
                .lock()
                .unwrap()
                .finish_credential_check(&task, result);
        }
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
}

/// Refreshes account route activation independently of the panel. It reads
/// local data and identities already learned, never probing upstream; it
/// notifies only when the result changes.
pub(crate) async fn monitor_account_states(app: AppHandle) {
    loop {
        let task = {
            let state = app.state::<AppState>();
            let mut core = state.core.lock().unwrap();
            core.refresh_config();
            core.begin_account_states()
        };
        if let Some(task) = task {
            let states = task.account_states().await;
            app.state::<AppState>()
                .core
                .lock()
                .unwrap()
                .finish_account_states(&task, states);
        }
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
    }
}

/// Proxy test results older than this are retested.
const PROXY_TEST_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(120);

/// Retests proxies whose last result has aged, independently of the panel and
/// its page. Tests run in the daemon when one is running, else in the GUI.
pub(crate) async fn monitor_proxies(app: AppHandle) {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        let state = app.state::<AppState>();
        let mut core = state.core.lock().unwrap();
        core.refresh_config();
        core.probe_stale(PROXY_TEST_MAX_AGE);
    }
}

/// Returns immediately with the last account activation, which
/// `monitor_account_states` refreshes; reading state starts no account work.
#[tauri::command]
pub async fn get_state(state: State<'_, AppState>) -> Result<Snapshot> {
    let forwarding = state.forwarding.state().await?;
    let mut core = state.core.lock().unwrap();
    let mut snapshot = core.snapshot();
    snapshot.forwarding = forwarding.target;
    snapshot.forwarding_restore = forwarding.restore;
    snapshot.forwarding_restore_supported = forwarding.restore_supported;
    snapshot.forwarding_restore_error = forwarding.restore_error;
    if snapshot.forwarding.is_some() {
        return Ok(snapshot);
    }
    if let Some(states) = core.account_states() {
        snapshot.set_account_route_states(states);
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
/// later filters, searches and pages reuse that read unless `fresh` asks for a
/// re-read.
#[tauri::command]
pub async fn get_activity(
    state: State<'_, AppState>,
    filter: String,
    search: String,
    search_mode: Option<String>,
    range: Option<ActivityRange>,
    after: Option<Cursor>,
    fresh: Option<bool>,
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
    // A fresh listing re-reads the log for lines written since the last read.
    let mut cached = !fresh.unwrap_or(false);
    // A first read keeps every entry for later filters and searches. Once a
    // read runs out before a page is full, the next keeps only matches, so a
    // sparse query does not reread the range once per capacity of entries.
    let mut narrow = false;
    loop {
        // One extra row tells whether another page follows.
        let (page, path) = {
            let core = state.core.lock().unwrap();
            let page = cached
                .then(|| core.activity_page(&query, after, activity::PAGE + 1 - rows.len()))
                .flatten();
            (page, core.logs.path())
        };
        let begin = match page {
            Some((found, resume)) => {
                rows.extend(found);
                match resume {
                    Some(resume) if rows.len() <= activity::PAGE => {
                        narrow = true;
                        Some(resume)
                    }
                    _ => break,
                }
            }
            None => after,
        };
        let query = query.clone();
        let scan = tauri::async_runtime::spawn_blocking(move || {
            if narrow {
                activity::Scan::read_matching(&path, &query, begin, |e| {
                    crate::core::activity_matches(&query, e)
                })
            } else {
                activity::Scan::read(&path, query.from, query.to, begin)
            }
        })
        .await
        .map_err(|_| "Cannot read the log".to_owned())??;
        state.core.lock().unwrap().store_activity_scan(scan);
        after = begin;
        cached = true;
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
) -> Result<coport_gui::traffic_stats::View> {
    traffic_inputs(&state)
        .await
        .local(minutes, scope.unwrap_or_default())
        .await
}

/// One snapshot of what a traffic read reads. Only credential labels
/// already known are read; reading traffic never starts an account probe.
async fn traffic_inputs(state: &AppState) -> coport_gui::traffic_read::Inputs {
    let (config, log, sources, tasks) = {
        let mut core = state.core.lock().unwrap();
        core.refresh_config();
        (
            core.loaded_config().cloned(),
            core.logs.path(),
            core.settings.data_sources(),
            core.account_tasks(),
        )
    };
    coport_gui::traffic_read::Inputs::take(
        coport_gui::settings::app_dir(),
        config,
        log,
        sources,
        tasks,
    )
    .await
}

/// Records where traffic logged under `name` and `base` is counted: a current
/// configuration, Unidentified without `target`, or the automatic match.
#[tauri::command]
pub fn set_traffic_assignment(
    state: State<AppState>,
    service: String,
    name: String,
    base: Option<String>,
    target: Option<TrafficTarget>,
    automatic: bool,
) -> Result {
    // The panel's lock serializes read-modify-write of the file.
    let _core = state.core.lock().unwrap();
    let path = crate::settings::traffic_compatibility_path();
    let mut file = Compatibility::load(&path)?;
    file.assign(service, name, base, (!automatic).then_some(target));
    file.save(&path)
}

/// Removes every traffic compatibility choice after confirmation. Returns
/// false when cancelled.
#[tauri::command]
pub async fn clear_traffic_compatibility(app: AppHandle) -> Result<bool> {
    use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
    let confirmed = panel::with_modal(&app, || {
        app.dialog()
            .message("Requests from renamed or changed configurations will be matched automatically again and marked for review. Request logs are not changed.")
            .title("Clear Traffic Compatibility Choices?")
            .kind(MessageDialogKind::Warning)
            .buttons(MessageDialogButtons::OkCancelCustom(
                "Clear".into(),
                "Cancel".into(),
            ))
            .blocking_show()
    });
    if !confirmed {
        return Ok(false);
    }
    let state = app.state::<AppState>();
    let _core = state.core.lock().unwrap();
    match std::fs::remove_file(crate::settings::traffic_compatibility_path()) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err("Cannot clear the traffic compatibility file".to_owned())
        }
        _ => Ok(true),
    }
}

/// Keeps the current proxy CA: the hosts it does not cover stay undecrypted.
#[tauri::command]
pub fn keep_proxy_ca(state: State<AppState>) {
    state.core.lock().unwrap().keep_proxy_ca();
}

/// Replaces the proxy CA after confirmation: the new CA is trusted first, so
/// a refused trust prompt changes nothing, and a running proxy restarts to use
/// it. Returns false when cancelled, otherwise whether trust was automatic.
#[tauri::command]
pub async fn replace_proxy_ca(app: AppHandle) -> Result<Option<bool>> {
    use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
    let confirmed = panel::with_modal(&app, || {
        app.dialog()
            .message(if cfg!(any(target_os = "macos", target_os = "windows")) {
                "A new proxy CA covering every configured host is added to your trust store; the system asks you to confirm. The proxy then restarts.\n\nApps that loaded the current CA may need a restart. The current CA is kept in tls/backup."
            } else {
                "A new proxy CA covering every configured host replaces the current one, and the proxy restarts. Add it to your trust store afterwards.\n\nThe current CA is kept in tls/backup."
            })
            .title("Replace the Proxy CA?")
            .kind(MessageDialogKind::Warning)
            .buttons(MessageDialogButtons::OkCancelCustom(
                "Replace".into(),
                "Cancel".into(),
            ))
            .blocking_show()
    });
    if !confirmed {
        return Ok(None);
    }
    let config_path = app.state::<AppState>().core.lock().unwrap().config_path();
    // Not under the core lock: the trust prompt waits for the user.
    let trusted = tauri::async_runtime::spawn_blocking(move || {
        let config =
            coport::config::Config::read(&config_path).map_err(|e| e.message.to_owned())?;
        let mut trusted = false;
        coport::credential_proxy::CredentialProxy::replace_ca(
            &config,
            &coport::local_tls::dir_for(&config_path),
            |path, probe| {
                trusted = platform::trust_certificate(path, probe)?;
                Ok(())
            },
        )
        .map_err(|e| format!("The proxy CA was not replaced: {e}"))?;
        Ok::<_, String>(trusted)
    })
    .await
    .map_err(|_| "The proxy CA was not replaced.".to_owned())??;
    let completion = {
        let state = app.state::<AppState>();
        let mut core = state.core.lock().unwrap();
        core.settings.kept_proxy_ca.clear();
        core.settings.save();
        core.is_running().then(|| core.start()).transpose()?
    };
    if let Some(completion) = completion {
        completion
            .await
            .map_err(|_| "The proxy control worker stopped.".to_owned())??;
    }
    tray::sync(&app);
    Ok(Some(trusted))
}

#[tauri::command]
pub async fn set_running(app: AppHandle, state: State<'_, AppState>, running: bool) -> Result {
    let completion = {
        let mut core = state.core.lock().unwrap();
        if running { core.start()? } else { core.stop()? }
    };
    let result = completion
        .await
        .map_err(|_| "The proxy control worker stopped.".to_owned())?;
    tray::sync(&app);
    result
}

#[tauri::command]
pub async fn restart_proxy(app: AppHandle, state: State<'_, AppState>) -> Result {
    let completion = state.core.lock().unwrap().start()?;
    let result = completion
        .await
        .map_err(|_| "The proxy control worker stopped.".to_owned())?;
    if result.is_ok() {
        state.core.lock().unwrap().refresh_after_restart();
    }
    tray::sync(&app);
    result
}

/// Tests one proxy, or all of them when `name` is absent. With `stale_only`,
/// only proxies without a recent result are tested (used at startup).
#[tauri::command]
pub fn probe_proxy(state: State<AppState>, name: Option<String>, stale_only: Option<bool>) {
    let mut core = state.core.lock().unwrap();
    if stale_only == Some(true) {
        core.refresh_config();
        core.probe_stale(PROXY_TEST_MAX_AGE);
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
    mask_account_names: Option<bool>,
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
        if let Some(mask) = patch.mask_account_names {
            core.settings.mask_account_names = mask;
        }
        core.settings.save();
    }
    tray::sync(&app);
}

/// Runs a login item call off the UI thread and outside the core lock:
/// Windows starts `reg.exe` and macOS waits on `SMAppService`, which would
/// stall the panel, the tray and the monitors. Calls run one at a time, and
/// the item they leave is recorded.
async fn login_item<T: Send + 'static>(
    state: &State<'_, AppState>,
    call: impl FnOnce() -> T + Send + 'static,
) -> Result<(T, platform::LoginItem)> {
    static CALLS: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let (value, item) = tauri::async_runtime::spawn_blocking(move || {
        let _one = CALLS.lock().unwrap_or_else(|e| e.into_inner());
        let value = call();
        (value, platform::launch_at_login())
    })
    .await
    .map_err(|error| error.to_string())?;
    state.core.lock().unwrap().launch_at_login = item;
    Ok((value, item))
}

#[tauri::command]
pub async fn set_launch_at_login(state: State<'_, AppState>, enabled: bool) -> Result {
    login_item(&state, move || platform::set_launch_at_login(enabled))
        .await?
        .0
        .map_err(|error| error.to_string())
}

/// Rereads the login item, which System Settings can switch off or allow.
#[tauri::command]
pub async fn refresh_launch_at_login(state: State<'_, AppState>) -> Result<platform::LoginItem> {
    Ok(login_item(&state, || ()).await?.1)
}

/// Opens System Settings where macOS lists login items it blocks.
#[tauri::command]
pub fn open_login_items() {
    #[cfg(target_os = "macos")]
    platform::open_login_items();
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

const EXAMPLE: &str = include_str!("../../../config.example.yaml");

#[tauri::command]
pub fn create_example_config(state: State<AppState>) -> Result {
    let mut core = state.core.lock().unwrap();
    let path = core.config_path();
    // The panel may still offer this after the file, or a link in its place,
    // was created elsewhere.
    crate::settings::create_private(&path, EXAMPLE.as_bytes()).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            "A configuration file already exists.".to_owned()
        } else {
            e.to_string()
        }
    })?;
    core.invalidate_config();
    platform::edit(&path);
    Ok(())
}

/// Opens or reveals a file: `config` (in the editor), `config-reveal`,
/// `log`, `log-reveal`, `traffic-compatibility` or its `-reveal`. The log is the one the panel is reading.
#[tauri::command]
pub fn open_path(state: State<AppState>, target: String) {
    let (config, log) = {
        let core = state.core.lock().unwrap();
        (core.config_path(), core.logs.path())
    };
    let compatibility = crate::settings::traffic_compatibility_path();
    match target.as_str() {
        "traffic-compatibility" => platform::edit(&compatibility),
        "traffic-compatibility-reveal" => platform::reveal(&compatibility),
        "config" => platform::edit(&config),
        "config-reveal" => platform::reveal(&config),
        "log" if log.is_file() => platform::open(&log),
        // Nothing logged yet: show where the log will be written.
        "log" | "log-reveal" => platform::reveal(&log),
        _ => {}
    }
}

#[tauri::command]
pub fn copy_text(clipboard: State<'_, platform::Clipboard>, text: String) -> Result {
    clipboard.copy_text(&text)
}

#[tauri::command]
pub fn fit_panel(app: AppHandle, height: f64) {
    panel::fit(&app, height);
}

#[tauri::command]
pub fn hide_panel(app: AppHandle) {
    panel::hide(&app);
}

/// Moves menu bar keyboard navigation from the open panel to the neighbouring
/// item (macOS); elsewhere the panel is not part of such navigation.
#[tauri::command]
pub fn navigate_menu_bar(app: AppHandle, right: bool) {
    #[cfg(target_os = "macos")]
    crate::tray::navigate(&app, right);
    #[cfg(not(target_os = "macos"))]
    let _ = (app, right);
}

#[tauri::command]
pub fn quit_app(app: AppHandle) {
    app.exit(0);
}

#[tauri::command]
pub async fn get_device_forwarders(
    state: State<'_, AppState>,
) -> Result<Vec<coport_gui::remote_forward::Status>> {
    state.forwarding.forwarders().await
}
#[tauri::command]
pub async fn set_forwarding_restore(state: State<'_, AppState>, enabled: bool) -> Result {
    state.forwarding.set_restore(enabled).await
}
#[tauri::command]
pub async fn get_device_capabilities(
    state: State<'_, AppState>,
) -> Result<Vec<coport_gui::device_capabilities::Status>> {
    state.forwarding.checks(&*state).await
}
#[tauri::command]
pub async fn set_device_forwarding(
    state: State<'_, AppState>,
    id: String,
    enabled: bool,
) -> Result {
    state.forwarding.forward(&*state, &id, enabled).await
}
#[tauri::command]
pub fn get_devices(state: State<AppState>) -> Vec<coport_gui::devices::Device> {
    state.core.lock().unwrap().settings.managed_devices.clone()
}
#[tauri::command]
pub async fn check_ssh_device(state: State<'_, AppState>, id: String) -> Result {
    let device = state
        .core
        .lock()
        .unwrap()
        .settings
        .managed_devices
        .iter()
        .find(|device| device.id == id)
        .and_then(|device| device.ssh_connection())
        .ok_or("SSH device no longer exists; reload Settings.")?;
    coport_gui::remote::check_connection(&device).await
}
#[tauri::command]
pub async fn save_device(
    state: State<'_, AppState>,
    device: coport_gui::devices::Draft,
) -> Result<String> {
    state.forwarding.save(&*state, device).await
}
#[tauri::command]
pub async fn remove_device(state: State<'_, AppState>, id: String) -> Result {
    state.forwarding.remove(&*state, &id).await
}
/// While forwarding, Home shows the destination's traffic from the daemon's
/// last summary. Neither the device nor any upstream is contacted.
#[tauri::command]
pub async fn get_forwarding_traffic(
    state: State<'_, AppState>,
    minutes: u64,
    scope: crate::traffic::TrafficScope,
) -> Result<coport_gui::data_client::ForwardedTraffic> {
    traffic_inputs(&state).await.forwarded(minutes, scope).await
}

#[tauri::command]
pub async fn get_merged_data(
    state: State<'_, AppState>,
    minutes: u64,
    scope: crate::traffic::TrafficScope,
    on_update: tauri::ipc::Channel<Vec<coport_gui::data_client::Merged>>,
    refresh_remote: Option<bool>,
) -> Result<Vec<coport_gui::data_client::Merged>> {
    traffic_inputs(&state)
        .await
        .merged(
            &state.peers,
            minutes,
            scope,
            refresh_remote == Some(true),
            |views| {
                on_update
                    .send(views)
                    .map_err(|_| "Traffic view closed".to_owned())
            },
        )
        .await
}
