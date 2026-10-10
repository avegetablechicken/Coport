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

/// Returns immediately with the last account activation; a refresh of it runs
/// in the background and emits `state-changed` when the result differs. Settings
/// opts out of profile lookups and uses the cached result only.
#[tauri::command]
pub async fn get_state(
    app: AppHandle,
    state: State<'_, AppState>,
    refresh_accounts: Option<bool>,
) -> Result<Snapshot> {
    let daemon = local_daemon().await?.map(|(_, status)| status);
    let stopped_restore = if daemon.is_none() {
        let result = tauri::async_runtime::spawn_blocking(|| {
            coport_gui::remote_forward::Routing::stored_restore(&coport_gui::settings::app_dir())
        })
        .await
        .map_err(|error| error.to_string())?;
        Some(result)
    } else {
        None
    };
    let mut core = state.core.lock().unwrap();
    let mut snapshot = core.snapshot();
    if let Some(daemon) = daemon {
        snapshot.forwarding = daemon.forwarding;
        snapshot.forwarding_restore = daemon.forwarding_restore;
        snapshot.forwarding_restore_supported = daemon.forwarding_restore_supported;
    } else if let Some(restore) = stopped_restore {
        match restore {
            Ok(restore) => {
                snapshot.forwarding_restore = restore;
                snapshot.forwarding_restore_supported = true;
            }
            Err(error) => snapshot.forwarding_restore_error = Some(error),
        }
    }
    if snapshot.forwarding.is_some() {
        return Ok(snapshot);
    }
    if let Some(states) = core.account_states() {
        snapshot.set_account_route_states(states);
    }
    if refresh_accounts == Some(true)
        && let Some(probe) = core.begin_account_states()
    {
        tauri::async_runtime::spawn(async move {
            let states = probe.account_states().await;
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
) -> Result<crate::traffic::Traffic> {
    let (path, config) = {
        let mut core = state.core.lock().unwrap();
        core.refresh_config();
        (core.logs.path(), core.loaded_config().cloned())
    };
    // An unreadable file leaves every changed configuration to be reviewed.
    let assignments =
        Compatibility::load(&crate::settings::traffic_compatibility_path()).unwrap_or_default();
    let assignments = assignments.assignments;
    let identities = config
        .as_ref()
        .map(|config| crate::traffic_identity::Identities::from_config(config, &assignments))
        .unwrap_or_default();
    let labels = match &config {
        Some(config) => config.local_traffic_credential_labels().await,
        None => Default::default(),
    };
    tauri::async_runtime::spawn_blocking(move || {
        crate::traffic::read(
            &path,
            minutes,
            &labels,
            scope.unwrap_or_default(),
            &identities,
        )
    })
    .await
    .map_err(|_| "Cannot load traffic history".to_owned())?
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
    tray::sync(&app);
    result
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

#[tauri::command]
pub fn quit_app(app: AppHandle) {
    app.exit(0);
}

async fn local_daemon() -> Result<Option<(coport_gui::daemon::Client, coport_gui::daemon::Status)>>
{
    tauri::async_runtime::spawn_blocking(|| {
        coport_gui::daemon::Client::discover(&coport_gui::settings::app_dir())
    })
    .await
    .map_err(|error| error.to_string())
}
async fn stop_device_forwarding(id: &str) -> Result {
    match local_daemon().await? {
        Some((client, status))
            if status
                .forwarding
                .as_ref()
                .is_some_and(|target| target.device_id == id) =>
        {
            client
                .set_forwarding(None)
                .await
                .map_err(|e| e.to_string())?
        }
        None => {
            let id = id.to_owned();
            tauri::async_runtime::spawn_blocking(move || {
                coport_gui::remote_forward::Routing::configure_stopped(
                    &coport_gui::settings::app_dir(),
                    None,
                    Some(&id),
                )
            })
            .await
            .map_err(|e| e.to_string())??;
        }
        _ => {}
    }
    Ok(())
}
#[tauri::command]
pub async fn get_device_forwarders() -> Result<Vec<coport_gui::remote_forward::Status>> {
    Ok(local_daemon()
        .await?
        .and_then(|(_, status)| status.forwarding)
        .into_iter()
        .collect())
}
#[tauri::command]
pub async fn set_forwarding_restore(state: State<'_, AppState>, enabled: bool) -> Result {
    let _guard = state.forwarding_operations.lock().await;
    let Some((client, status)) = local_daemon().await? else {
        return tauri::async_runtime::spawn_blocking(move || {
            coport_gui::remote_forward::Routing::configure_stopped(
                &coport_gui::settings::app_dir(),
                Some(enabled),
                None,
            )
        })
        .await
        .map_err(|e| e.to_string())?;
    };
    if !status.forwarding_restore_supported {
        return Err("Update and restart the local daemon to configure startup restore.".into());
    }
    client
        .set_forwarding_restore(enabled)
        .await
        .map_err(|e| e.to_string())
}
#[tauri::command]
pub async fn get_device_capabilities() -> Result<Vec<coport_gui::device_capabilities::Status>> {
    match local_daemon().await? {
        Some((client, _)) => client
            .device_capabilities()
            .await
            .map_err(|e| e.to_string()),
        None => Ok(Vec::new()),
    }
}
async fn refresh_device_capabilities() {
    if let Ok(Some((client, _))) = local_daemon().await {
        let _ = client.refresh_device_capabilities().await;
    }
}
#[tauri::command]
pub async fn set_device_forwarding(
    state: State<'_, AppState>,
    id: String,
    enabled: bool,
) -> Result {
    let _guard = state.forwarding_operations.lock().await;
    if !enabled {
        return stop_device_forwarding(&id).await;
    }
    let target = {
        let core = state.core.lock().unwrap();
        let device = core
            .settings
            .managed_devices
            .iter()
            .find(|device| device.id == id)
            .ok_or("Device no longer exists")?;
        coport_gui::remote_forward::Target {
            device_id: id,
            name: device.name.clone(),
            connection: device
                .ssh_connection()
                .ok_or("Request forwarding requires SSH")?,
        }
    };
    let (client, status) = local_daemon()
        .await?
        .ok_or("Start the local proxy before enabling SSH forwarding.")?;
    if !status.forwarding_supported {
        return Err("Update and restart the local proxy to enable unified SSH forwarding.".into());
    }
    client
        .refresh_device_capabilities()
        .await
        .map_err(|e| e.to_string())?;
    let checks = client
        .device_capabilities()
        .await
        .map_err(|e| e.to_string())?;
    let supported = checks
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
        .map_err(|e| e.to_string())
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
    let _guard = state.forwarding_operations.lock().await;
    let (id, devices) = {
        let mut devices = state.core.lock().unwrap().settings.managed_devices.clone();
        let id = coport_gui::devices::save(&mut devices, device)?;
        (id, devices)
    };
    stop_device_forwarding(&id).await?;
    {
        // General preferences can change while the daemon operation is pending.
        // Commit only the device list against the latest settings snapshot.
        let mut core = state.core.lock().unwrap();
        let mut settings = core.settings.clone();
        settings.managed_devices = devices;
        settings.try_save().map_err(|e| e.to_string())?;
        core.settings = settings;
    }
    refresh_device_capabilities().await;
    Ok(id)
}
#[tauri::command]
pub async fn remove_device(state: State<'_, AppState>, id: String) -> Result {
    let _guard = state.forwarding_operations.lock().await;
    stop_device_forwarding(&id).await?;
    {
        let mut core = state.core.lock().unwrap();
        let mut settings = core.settings.clone();
        settings.managed_devices.retain(|device| device.id != id);
        settings.try_save().map_err(|e| e.to_string())?;
        core.settings = settings;
    }
    refresh_device_capabilities().await;
    Ok(())
}
#[tauri::command]
pub async fn get_merged_data(
    state: State<'_, AppState>,
    minutes: u64,
    scope: crate::traffic::TrafficScope,
    on_update: tauri::ipc::Channel<Vec<coport_gui::data_client::Merged>>,
    refresh_remote: Option<bool>,
) -> Result<Vec<coport_gui::data_client::Merged>> {
    coport_gui::data_api::bucket_minutes(minutes).ok_or("Unsupported traffic range")?;
    let (sources, config, log) = {
        let mut core = state.core.lock().unwrap();
        core.refresh_config();
        (
            core.settings.data_sources(),
            core.loaded_config()
                .cloned()
                .ok_or("Cannot read local configuration")?,
            core.logs.path(),
        )
    };
    // Statistics never request account metadata from the running proxy. Local
    // files plus already logged identities are sufficient; old daemons may
    // resolve missing metadata by contacting upstream providers.
    let labels = config.local_traffic_credential_labels().await;
    if refresh_remote != Some(true) {
        // Publish the selected range first; the 30-day views follow.
        let publish = |views| {
            on_update
                .send(views)
                .map_err(|_| "Traffic view closed".to_owned())
        };
        return coport_gui::data_client::MergeCache::views_shared(
            &state.merged_cache,
            sources,
            config,
            log,
            labels,
            Some((minutes, scope, &publish)),
        )
        .await;
    }
    let (cancel, cancelled) = tokio::sync::oneshot::channel();
    if let Some(previous) = state.traffic_request.lock().unwrap().replace(cancel) {
        let _ = previous.send(());
    }
    // Cached views remain readable while peers respond to this refresh.
    let operation = coport_gui::data_client::MergeCache::refresh_shared(
        &state.merged_cache,
        sources,
        config,
        log,
        labels,
        (minutes, scope),
        move |views| {
            on_update
                .send(views)
                .map_err(|_| "Traffic view closed".into())
        },
    );
    tokio::select! {result=operation=>result,_=cancelled=>Err("Traffic request superseded".into())}
}
