//! Application state shared by the tray, the panel and its commands, and the
//! snapshot the web frontend renders.

use crate::{
    describe,
    logs::{Entry, LogFeed},
    platform,
    proxy::{BackgroundController as Controller, Completion, Notify, Phase, Probe},
    settings::Settings,
};
use coport::config::{Choice, Config, Routing, redacted_endpoint};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};

pub struct Core {
    pub controller: Controller,
    pub logs: LogFeed,
    pub settings: Settings,
    pub launch_at_login: bool,
    config: ConfigCache,
    account_probe: Option<std::sync::Arc<coport_gui::tasks::Tasks>>,
    /// Last account activation; kept across config changes until replaced.
    account_states: Option<AccountStates>,
    account_states_error: Option<String>,
    account_states_pending: bool,
    notify: Notify,
    /// Config file modification time when the proxy last started.
    started_stamp: Option<Option<SystemTime>>,
    attached_pid: Option<u32>,
    /// The Activity list's last read of its time range.
    activity_scan: Option<crate::activity::Scan>,
}

#[derive(Default)]
struct ConfigCache {
    path: PathBuf,
    stamp: Option<SystemTime>,
    checked: Option<Instant>,
    exists: bool,
    parsed: Option<Result<Config, String>>,
}

/// Parses configuration text; YAML syntax errors keep their location.
pub fn parse_config(text: &str) -> Result<Config, String> {
    if let Err(e) = serde_yaml_ng::from_str::<serde_yaml_ng::Value>(text) {
        return Err(match e.location() {
            Some(loc) => format!(
                "YAML syntax error at line {}, column {}",
                loc.line(),
                loc.column()
            ),
            None => "Invalid YAML configuration.".to_owned(),
        });
    }
    Config::parse(text).map_err(|e| e.message.to_owned())
}

/// Copies a YAML file over `target` byte for byte, only if it is a valid configuration.
fn replace_config(source: &Path, target: &Path) -> Result<(), String> {
    let text = std::fs::read_to_string(source)
        .map_err(|e| format!("Cannot read the selected file: {e}"))?;
    Config::parse(&text).map_err(|e| e.message.to_string())?;
    // Replace the link target, as editing does, so a linked configuration stays
    // linked. A link to a missing file is reported rather than replaced.
    let target = match target.canonicalize() {
        Ok(resolved) => resolved,
        Err(_)
            if target
                .symlink_metadata()
                .is_ok_and(|m| m.file_type().is_symlink()) =>
        {
            return Err(format!(
                "Cannot replace the configuration: {} links to a missing file.",
                target.display()
            ));
        }
        Err(_) => target.to_path_buf(),
    };
    crate::settings::write_private(&target, text.as_bytes())
        .map_err(|e| format!("Cannot replace the configuration: {e}"))
}

fn file_stamp(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

impl Core {
    pub fn new(settings: Settings, notify: Notify) -> Self {
        let controller = Controller::new(notify.clone());
        let attached = controller.daemon_status();
        // A daemon started from another file by an older version keeps running
        // until restarted; it is reported as out of date, not adopted.
        let same_file = |path: &Path| {
            let canonical = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
            canonical(path) == canonical(&crate::settings::config_path())
        };
        let started_stamp = attached.as_ref().map(|status| {
            if same_file(&status.config_path) {
                status.config_modified
            } else {
                Some(SystemTime::UNIX_EPOCH)
            }
        });
        let log_path = attached.as_ref().map(|status| status.log_path.clone());
        Self {
            logs: LogFeed::new(
                log_path.unwrap_or_else(crate::settings::log_path),
                notify.clone(),
            ),
            controller,
            launch_at_login: platform::launch_at_login(),
            settings,
            config: ConfigCache::default(),
            account_probe: None,
            account_states: None,
            account_states_error: None,
            account_states_pending: false,
            notify,
            started_stamp,
            attached_pid: attached.map(|s| s.pid),
            activity_scan: None,
        }
    }

    pub fn config_path(&self) -> PathBuf {
        crate::settings::config_path()
    }

    pub fn config_exists(&mut self) -> bool {
        self.refresh_config();
        self.config.exists
    }

    /// Re-reads the config file when it changes on disk (checked at most 1/s).
    pub(crate) fn refresh_config(&mut self) {
        let path = self.config_path();
        let cache = &mut self.config;
        let due = cache
            .checked
            .is_none_or(|t| t.elapsed() > Duration::from_secs(1));
        if cache.path == path && !due {
            return;
        }
        cache.checked = Some(Instant::now());
        let stamp = file_stamp(&path);
        if cache.path == path && cache.stamp == stamp && cache.parsed.is_some() {
            return;
        }
        cache.exists = path.is_file();
        self.account_probe = None;
        self.account_states_pending = false;
        cache.parsed = Some(match std::fs::read_to_string(&path) {
            Ok(text) => parse_config(&text),
            Err(_) => Err("Cannot read configuration file.".to_owned()),
        });
        cache.path = path;
        cache.stamp = stamp;
    }

    pub fn invalidate_config(&mut self) {
        self.config = ConfigCache::default();
        self.reset_account_probe();
    }

    fn reset_account_probe(&mut self) {
        self.account_probe = None;
        self.account_states_pending = false;
    }

    pub(crate) fn account_states(&self) -> Option<AccountStates> {
        self.account_states.clone()
    }

    /// Starts an account activation refresh unless one is already running.
    /// Profile lookups can take seconds, so snapshots never wait for them.
    pub(crate) fn begin_account_states(
        &mut self,
    ) -> Option<std::sync::Arc<coport_gui::tasks::Tasks>> {
        if self.account_states_pending {
            return None;
        }
        let probe = self.account_tasks()?;
        self.account_states_pending = true;
        Some(probe)
    }

    /// Stores the result of `begin_account_states`, notifying on change.
    /// Results for a configuration that has since been replaced are dropped.
    pub(crate) fn finish_account_states(
        &mut self,
        probe: &std::sync::Arc<coport_gui::tasks::Tasks>,
        states: Result<AccountStates, String>,
    ) {
        if !self
            .account_probe
            .as_ref()
            .is_some_and(|current| std::sync::Arc::ptr_eq(current, probe))
        {
            return;
        }
        self.account_states_pending = false;
        let (states, error) = match states {
            Ok(states) => (Some(states), None),
            Err(error) => (None, Some(error)),
        };
        if self.account_states != states || self.account_states_error != error {
            self.account_states = states;
            self.account_states_error = error;
            (self.notify)();
        }
    }

    pub(crate) fn account_tasks(&mut self) -> Option<std::sync::Arc<coport_gui::tasks::Tasks>> {
        if self.account_probe.is_none() {
            self.account_probe = Some(std::sync::Arc::new(coport_gui::tasks::Tasks::new(
                crate::settings::app_dir(),
                self.config_path(),
                self.loaded_config()?.clone(),
            )));
        }
        self.account_probe.clone()
    }

    pub(crate) fn loaded_config(&self) -> Option<&Config> {
        self.config.parsed.as_ref()?.as_ref().ok()
    }

    /// Port clients should use: the live listener, else the configured one.
    pub fn port(&self) -> u16 {
        match self.controller.phase() {
            Phase::Running { port, .. } => port,
            _ => self.loaded_config().map_or(8787, |c| c.listen_port),
        }
    }

    pub fn start(&mut self) -> Result<Completion, String> {
        let result = self
            .controller
            .start(&self.config_path(), crate::settings::log_path());
        self.invalidate_config();
        result
    }

    pub fn start_if_stopped(&self) -> Result<Completion, String> {
        self.controller
            .start_if_stopped(&self.config_path(), crate::settings::log_path())
    }

    pub fn stop(&mut self) -> Result<Completion, String> {
        self.controller.stop()
    }

    fn sync_daemon(&mut self) {
        let status = self.controller.daemon_status();
        let pid = status.as_ref().map(|s| s.pid);
        if pid != self.attached_pid {
            if let Some(status) = status {
                let canonical = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
                self.started_stamp = Some(
                    if canonical(&status.config_path) == canonical(&self.config_path()) {
                        status.config_modified
                    } else {
                        Some(SystemTime::UNIX_EPOCH)
                    },
                );
                self.logs.set_path(status.log_path);
            }
            self.attached_pid = pid;
            self.reset_account_probe();
            self.account_states = None;
            self.account_states_error = None;
        }
    }

    /// Replaces the configuration with a validated YAML file; a running proxy
    /// picks it up on restart.
    pub fn import_config(&mut self, source: &Path) -> Result<(), String> {
        let result = replace_config(source, &self.config_path());
        self.invalidate_config();
        result
    }

    pub fn set_launch_at_login(&mut self, enable: bool) -> Result<(), String> {
        let result = platform::set_launch_at_login(enable).map_err(|e| e.to_string());
        self.launch_at_login = platform::launch_at_login();
        result
    }

    /// Probes proxies whose last result is missing or older than `max_age`.
    pub fn probe_stale(&self, max_age: Duration) {
        if let Some(config) = self.loaded_config() {
            self.controller.probe_stale(&config.proxies, max_age);
        }
    }

    pub fn probe(&self, name: Option<&str>) {
        let Some(config) = self.loaded_config() else {
            return;
        };
        for (proxy, endpoint) in &config.proxies {
            if name.is_none_or(|n| n == proxy) {
                self.controller.probe(proxy, endpoint);
            }
        }
    }

    pub fn snapshot(&mut self) -> Snapshot {
        self.refresh_config();
        self.sync_daemon();
        let phase = self.controller.phase();
        let port = self.port();
        let stats = self.logs.stats();
        let base = format!("http://127.0.0.1:{port}");
        let changed_since_start = self.controller.is_running()
            && self
                .started_stamp
                .is_some_and(|started| started != file_stamp(&self.config_path()));
        Snapshot {
            forwarding: None,
            account_states_error: self.account_states_error.clone(),
            version: env!("CARGO_PKG_VERSION"),
            phase: match &phase {
                Phase::Running { port, since } => PhaseDto {
                    busy: self.controller.busy(),
                    state: "running",
                    port: *port,
                    uptime_secs: since.elapsed().as_secs(),
                    error: self.controller.error(),
                },
                Phase::Stopped => PhaseDto {
                    busy: self.controller.busy(),
                    state: "stopped",
                    port,
                    uptime_secs: 0,
                    error: self.controller.error(),
                },
                Phase::Failed(message) => PhaseDto {
                    busy: self.controller.busy(),
                    state: "failed",
                    port,
                    uptime_secs: 0,
                    error: Some(message.clone()),
                },
            },
            stats: StatsDto {
                requests: stats.requests,
                errors: stats.errors,
                bytes: stats.bytes,
                avg_ms: stats.avg_latency_ms,
                per_minute: stats.per_minute.to_vec(),
                errors_per_minute: stats.errors_per_minute.to_vec(),
            },
            routes: stats
                .routes
                .iter()
                .map(|e| RouteReport {
                    name: e
                        .get("provider")
                        .or_else(|| e.get("service"))
                        .unwrap_or_default()
                        .to_owned(),
                    proxies: e
                        .get("proxy")
                        .map(|p| p.split(", ").map(str::to_owned).collect()),
                    ok: e.event == "current_route",
                    reason: e.get("reason").map(str::to_owned),
                })
                .collect(),
            urls: Urls {
                claude: format!("{base}/anthropic"),
                codex: format!("{base}/v1"),
                chatgpt: format!("https://127.0.0.1:{port}/backend-api"),
                ca_certificate: coport::local_tls::dir_for(&self.config_path())
                    .join(coport::local_tls::CA_FILE)
                    .display()
                    .to_string(),
                base,
            },
            config: ConfigDto {
                exists: self.config.exists,
                error: match &self.config.parsed {
                    Some(Err(e)) if self.config.exists => Some(e.clone()),
                    _ => None,
                },
                changed_since_start,
                details: self.loaded_config().map(|c| {
                    let mut result = details(c, &self.controller.probes_for(&c.proxies));
                    if matches!(phase, Phase::Running { .. }) && !changed_since_start {
                        sync_route_health(&mut result, &stats.route_health, SystemTime::now());
                    }
                    result
                }),
            },
            settings: SettingsDto {
                device_count: self.settings.managed_devices.len(),
                appearance: self.settings.appearance,
                start_proxy_on_launch: self.settings.start_proxy_on_launch,
                keep_proxy_running_on_quit: self.settings.keep_proxy_running_on_quit,
                launch_at_login: self.launch_at_login,
                log_bytes: std::fs::metadata(self.logs.path()).ok().map(|m| m.len()),
                traffic_compatibility: TrafficCompatibilityDto::read(),
            },
        }
    }

    pub fn activity(
        &self,
        filter: &str,
        search: &str,
        search_mode: &str,
        limit: usize,
    ) -> Vec<Entry> {
        let needle = search.trim().to_lowercase();
        let mut rows = self
            .logs
            .entries(|e| e.matches_filter(filter) && matches_search(e, search_mode, &needle));
        rows.reverse();
        rows.truncate(limit);
        rows
    }
}

impl Core {
    /// A page of the Activity range from the last read, if that read covers
    /// it; see `activity::Scan::page`.
    pub fn activity_page(
        &self,
        query: &crate::activity::Query,
        after: Option<crate::activity::Cursor>,
        limit: usize,
    ) -> Option<(Vec<Entry>, Option<crate::activity::Cursor>)> {
        let scan = self
            .activity_scan
            .as_ref()
            .filter(|scan| scan.covers(&self.logs.path(), query, after))?;
        Some(scan.page(after, limit, |e| activity_matches(query, e)))
    }

    pub fn store_activity_scan(&mut self, scan: crate::activity::Scan) {
        self.activity_scan = Some(scan);
    }
}

/// Whether `e` is listed for the Activity `query`.
pub fn activity_matches(query: &crate::activity::Query, e: &Entry) -> bool {
    e.matches_filter(&query.filter) && matches_search(e, &query.search_mode, &query.needle)
}

fn matches_search(e: &Entry, mode: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    match mode {
        "proxy" => e.get("proxy").is_some_and(|proxy| {
            proxy.to_lowercase() == needle || (proxy == "none" && needle == "direct")
        }),
        "status" => {
            needle.len() == 3
                && needle.bytes().all(|b| b.is_ascii_digit())
                && needle.parse::<u16>().ok().is_some_and(|status| {
                    (100..=599).contains(&status) && e.status() == Some(status)
                })
        }
        "path" => e
            .get("path")
            .is_some_and(|path| path.to_lowercase().contains(needle)),
        // Configuration names: the matched account route, or the API Key
        // variable, provider ID, Claude settings name or URL route.
        "credential" => ["account_label", "provider"].iter().any(|key| {
            e.get(key)
                .is_some_and(|name| name.to_lowercase().contains(needle))
        }),
        _ => {
            e.event.contains(needle)
                || e.fields.values().any(|v| {
                    v.as_str()
                        .is_some_and(|s| s.to_lowercase().contains(needle))
                })
        }
    }
}

/// Business destinations are independent of the standalone proxy test.
/// Refreshing the test must not erase route warnings or let traffic rewrite
/// its pending state, measured latency, exit address, or failure.
fn sync_route_health(
    details: &mut ConfigDetails,
    health: &BTreeMap<(String, String, String), Entry>,
    now: SystemTime,
) {
    for proxy in &mut details.proxies {
        let mut destinations = BTreeMap::<String, bool>::new();
        for ((_, origin, _), entry) in health
            .range((proxy.endpoint.clone(), String::new(), String::new())..)
            .take_while(|((endpoint, _, _), _)| *endpoint == proxy.endpoint)
            .filter(|(_, entry)| {
                entry
                    .time
                    .map(SystemTime::from)
                    .is_some_and(|time| time <= now)
            })
        {
            let available = match entry.get("available") {
                Some("true") => true,
                Some("false") => false,
                _ => continue,
            };
            // As with route selection, any working transport for a destination
            // keeps that destination available.
            *destinations.entry(origin.clone()).or_default() |= available;
        }
        proxy.route_failures = destinations
            .into_iter()
            .filter_map(|(origin, available)| (!available).then_some(origin))
            .collect();
    }
}

fn details(config: &Config, probes: &BTreeMap<String, (Probe, SystemTime)>) -> ConfigDetails {
    ConfigDetails {
        listen_port: config.listen_port,
        timeout_secs: config.request_timeout_seconds,
        editable: crate::config_edit::values(config),
        proxies: config
            .proxies
            .iter()
            .map(|(name, endpoint)| ProxyDto {
                name: name.clone(),
                endpoint: redacted_endpoint(endpoint),
                local: crate::proxy::is_local_network(endpoint),
                route_failures: Vec::new(),
                probe: probes.get(name).map(|(p, _)| match p {
                    Probe::Pending => ProbeDto {
                        state: "pending",
                        ..Default::default()
                    },
                    Probe::Reachable { latency, exit } => ProbeDto {
                        state: "ok",
                        ms: Some(latency.as_millis() as u64),
                        exit_ip: exit.as_ref().map(|e| e.ip.clone()),
                        country: exit.as_ref().and_then(|e| e.country.clone()),
                        ..Default::default()
                    },
                    Probe::Unavailable(e) => ProbeDto {
                        state: "unavailable",
                        error: Some(e.clone()),
                        ..Default::default()
                    },
                    Probe::Unreachable(e) => ProbeDto {
                        state: "error",
                        error: Some(e.clone()),
                        ..Default::default()
                    },
                }),
            })
            .collect(),
        codex: service(
            vec![
                ("accountUpstream", config.codex.base_url.account.clone()),
                ("apiKeyUpstream", config.codex.base_url.api_key.clone()),
            ],
            config.codex.account_auth_file_only,
            config.codex.auth_env.is_some(),
            &config.codex.routing,
            true,
            |selector| describe::codex_api_key(config, selector),
            |selector| config.codex.api_key_kind(selector).unwrap_or("unknown"),
        ),
        claude: service(
            vec![("upstream", config.claude.base_url.clone())],
            config.claude.account_auth_file_only,
            config.claude.auth_env.is_some(),
            &config.claude.routing,
            false,
            |selector| describe::claude_api_key(config, selector),
            |selector| config.claude.api_key_kind(selector).unwrap_or("unknown"),
        ),
    }
}

fn service(
    upstreams: Vec<(&'static str, String)>,
    file_only: bool,
    auth_env: bool,
    routing: &Routing,
    codex: bool,
    api_key_detail: impl Fn(&str) -> Option<String>,
    api_key_kind: impl Fn(&str) -> &'static str,
) -> ServiceDto {
    let rows = |routes: &BTreeMap<String, Choice>,
                detail: &dyn Fn(&str) -> Option<String>,
                kind: &dyn Fn(&str) -> &'static str| {
        routes
            .iter()
            .map(|(selector, choice)| RouteRow {
                selector: selector.clone(),
                kind: kind(selector),
                proxies: choice.names().to_vec(),
                detail: detail(selector),
                activation: None,
            })
            .collect()
    };
    let fallback = |key: &'static str, choice: &Option<Choice>| Fallback {
        key,
        proxies: choice.as_ref().map(|c| c.names().to_vec()),
    };
    let mut fallbacks = vec![
        fallback("accountFallback", &routing.account_fallback),
        fallback("apiKeyFallback", &routing.api_key_fallback),
    ];
    fallbacks.push(if codex {
        fallback("mcpFallback", &routing.mcp_fallback)
    } else {
        fallback("accountProbe", &routing.account_probe)
    });
    ServiceDto {
        configured: auth_env || !routing.account.is_empty() || !routing.api_key.is_empty(),
        upstreams: upstreams
            .into_iter()
            .map(|(key, value)| KeyValue {
                key: key.to_owned(),
                value,
            })
            .collect(),
        file_only,
        account_routes: rows(&routing.account, &|_| None, &|_| "account"),
        api_key_routes: rows(&routing.api_key, &api_key_detail, &api_key_kind),
        fallbacks,
    }
}

// ---------------------------------------------------------------------------
// Data sent to the frontend

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub forwarding: Option<coport_gui::remote_forward::Status>,
    account_states_error: Option<String>,
    version: &'static str,
    phase: PhaseDto,
    stats: StatsDto,
    routes: Vec<RouteReport>,
    urls: Urls,
    config: ConfigDto,
    settings: SettingsDto,
}

pub(crate) type AccountStates = coport_gui::tasks::AccountStates;

impl Snapshot {
    pub fn set_account_route_states(&mut self, states: AccountStates) {
        if let Some(details) = &mut self.config.details {
            for (service, states) in [&mut details.codex, &mut details.claude]
                .into_iter()
                .zip(states)
            {
                for route in &mut service.account_routes {
                    route.activation = states.get(&route.selector).cloned();
                }
            }
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PhaseDto {
    busy: bool,
    state: &'static str,
    port: u16,
    uptime_secs: u64,
    error: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StatsDto {
    requests: u64,
    errors: u64,
    bytes: u64,
    avg_ms: Option<u64>,
    per_minute: Vec<u32>,
    errors_per_minute: Vec<u32>,
}

#[derive(Serialize)]
struct RouteReport {
    name: String,
    proxies: Option<Vec<String>>,
    ok: bool,
    reason: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Urls {
    base: String,
    claude: String,
    codex: String,
    /// Codex 0.156+ only accepts an HTTPS ChatGPT backend; the port also serves TLS.
    chatgpt: String,
    ca_certificate: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ConfigDto {
    exists: bool,
    error: Option<String>,
    changed_since_start: bool,
    details: Option<ConfigDetails>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ConfigDetails {
    listen_port: u16,
    timeout_secs: f64,
    /// Values the Settings page edits, by dotted YAML key.
    editable: BTreeMap<&'static str, serde_json::Value>,
    proxies: Vec<ProxyDto>,
    codex: ServiceDto,
    claude: ServiceDto,
}

#[derive(Serialize)]
struct ProxyDto {
    name: String,
    endpoint: String,
    /// Loopback or local-network proxy; its exit address is looked up when probed.
    local: bool,
    probe: Option<ProbeDto>,
    /// Failed business destinations; never used as standalone test results.
    #[serde(rename = "routeFailures")]
    route_failures: Vec<String>,
}

#[derive(Serialize, Default)]
#[serde(rename_all = "camelCase")]
struct ProbeDto {
    state: &'static str,
    ms: Option<u64>,
    error: Option<String>,
    exit_ip: Option<String>,
    country: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ServiceDto {
    configured: bool,
    upstreams: Vec<KeyValue>,
    file_only: bool,
    account_routes: Vec<RouteRow>,
    api_key_routes: Vec<RouteRow>,
    fallbacks: Vec<Fallback>,
}

#[derive(Serialize)]
struct KeyValue {
    key: String,
    value: String,
}

#[derive(Serialize)]
struct RouteRow {
    activation: Option<String>,
    selector: String,
    kind: &'static str,
    proxies: Vec<String>,
    /// Hover text: the base URL an API key selector resolves to.
    detail: Option<String>,
}

#[derive(Serialize)]
struct Fallback {
    key: &'static str,
    proxies: Option<Vec<String>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SettingsDto {
    device_count: usize,
    appearance: crate::settings::Appearance,
    start_proxy_on_launch: bool,
    keep_proxy_running_on_quit: bool,
    launch_at_login: bool,
    /// Size of the request log in use, if it exists yet.
    log_bytes: Option<u64>,
    traffic_compatibility: TrafficCompatibilityDto,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TrafficCompatibilityDto {
    exists: bool,
    choices: usize,
    error: Option<String>,
}

impl TrafficCompatibilityDto {
    fn read() -> Self {
        let path = crate::settings::traffic_compatibility_path();
        let loaded = crate::traffic_identity::Compatibility::load(&path);
        Self {
            exists: path.is_file(),
            choices: loaded.as_ref().map_or(0, |file| file.assignments.len()),
            error: loaded.err(),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivityDto {
    rows: Vec<EntryDto>,
    /// Where the next page starts, when there is one.
    next: Option<crate::activity::Cursor>,
}

impl ActivityDto {
    pub fn new(rows: Vec<Entry>, next: Option<crate::activity::Cursor>) -> Self {
        Self {
            rows: rows.into_iter().map(EntryDto::from).collect(),
            next,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EntryDto {
    seq: u64,
    /// Milliseconds since the Unix epoch.
    time: Option<i64>,
    event: String,
    status: Option<u16>,
    method: Option<String>,
    path: Option<String>,
    service: Option<&'static str>,
    proxy: Option<String>,
    duration_ms: Option<u64>,
    bytes: Option<u64>,
    error: bool,
    reason: Option<String>,
    fields: serde_json::Map<String, serde_json::Value>,
}

impl From<Entry> for EntryDto {
    fn from(e: Entry) -> Self {
        Self {
            seq: e.seq,
            time: e.time.map(|t| t.timestamp_millis()),
            status: e.status(),
            method: e.get("method").map(str::to_owned),
            path: e.get("path").map(str::to_owned),
            service: e.get("path").and(e.service()),
            proxy: e.get("proxy").map(str::to_owned),
            duration_ms: e.duration_ms(),
            bytes: (e.is_request_end() || e.is_model_call_end()).then(|| e.bytes()),
            error: e.is_error(),
            reason: e.get("reason").map(str::to_owned),
            event: e.event,
            fields: e.fields,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Urls, matches_search, parse_config, replace_config};
    use crate::logs::Entry;

    #[test]
    fn lan_proxy_details_include_exit_address() {
        use crate::proxy::{Exit, Probe};
        use std::{collections::BTreeMap, time::SystemTime};

        let config = parse_config("listen_port: 8787\nrequest_timeout_seconds: 30\nproxies:\n  jp_lab: http://10.156.232.107:10810\n").unwrap();
        let probes = BTreeMap::from([(
            "jp_lab".into(),
            (
                Probe::Reachable {
                    latency: std::time::Duration::from_millis(42),
                    exit: Some(Exit {
                        ip: "203.0.113.5".into(),
                        country: Some("JP".into()),
                    }),
                },
                SystemTime::now(),
            ),
        )]);
        let dto = serde_json::to_value(super::details(&config, &probes)).unwrap();
        assert_eq!(dto["proxies"][0]["local"], true);
        assert_eq!(dto["proxies"][0]["probe"]["exitIp"], "203.0.113.5");
        assert_eq!(dto["proxies"][0]["probe"]["country"], "JP");
    }

    #[test]
    fn route_observations_never_replace_standalone_test_results() {
        use crate::proxy::{Exit, Probe};
        use std::{
            collections::BTreeMap,
            time::{Duration, SystemTime},
        };
        let config = parse_config("listen_port: 8787\nrequest_timeout_seconds: 30\nproxies:\n  jp_lab: http://10.156.232.107:10810\n").unwrap();
        let now = SystemTime::now();
        let endpoint = "http://10.156.232.107:10810";
        let origin = "https://chatgpt.com/";
        for probe in [
            None,
            Some(Probe::Pending),
            Some(Probe::Unreachable("cannot connect".into())),
            Some(Probe::Reachable {
                latency: Duration::from_millis(42),
                exit: Some(Exit {
                    ip: "203.0.113.5".into(),
                    country: Some("JP".into()),
                }),
            }),
        ] {
            // Both older and newer traffic must leave all test fields intact.
            for checked in [now - Duration::from_secs(10), now + Duration::from_secs(10)] {
                let probes = probe
                    .clone()
                    .map(|p| BTreeMap::from([("jp_lab".into(), (p, checked))]))
                    .unwrap_or_default();
                let mut result = super::details(&config, &probes);
                let expected = serde_json::to_value(&result.proxies[0].probe).unwrap();
                for available in ["false", "true", "false"] {
                    let health = BTreeMap::from([(
                        (endpoint.into(), origin.into(), "connect".into()),
                        Entry {
                            seq: 1,
                            time: Some(now.into()),
                            event: "route_health".into(),
                            fields: serde_json::from_value(
                                serde_json::json!({"available": available}),
                            )
                            .unwrap(),
                        },
                    )]);
                    super::sync_route_health(&mut result, &health, now);
                    assert_eq!(
                        serde_json::to_value(&result.proxies[0].probe).unwrap(),
                        expected
                    );
                    let failures = &result.proxies[0].route_failures;
                    if available == "false" {
                        assert_eq!(failures, &[origin]);
                    } else {
                        assert!(failures.is_empty());
                    }
                }
            }
        }
    }

    #[test]
    fn route_warnings_track_destinations_and_ignore_invalid_observations() {
        use std::{
            collections::BTreeMap,
            time::{Duration, SystemTime},
        };
        let config = parse_config("listen_port: 8787\nrequest_timeout_seconds: 30\nproxies:\n  used: http://127.0.0.1:12345\n  other: http://127.0.0.1:12346\n").unwrap();
        let now = SystemTime::now();
        let mut health = BTreeMap::new();
        for (origin, phase, available, time) in [
            ("https://a.example", "connect", "false", Some(now)),
            ("https://a.example", "request", "true", Some(now)),
            ("https://b.example", "connect", "false", Some(now)),
            (
                "https://future.example",
                "connect",
                "false",
                Some(now + Duration::from_secs(1)),
            ),
            ("https://undated.example", "connect", "false", None),
            ("https://unknown.example", "connect", "unknown", Some(now)),
        ] {
            health.insert(
                ("http://127.0.0.1:12345".into(), origin.into(), phase.into()),
                Entry {
                    seq: 1,
                    time: time.map(Into::into),
                    event: "route_health".into(),
                    fields: serde_json::from_value(serde_json::json!({"available": available}))
                        .unwrap(),
                },
            );
        }
        let mut result = super::details(&config, &BTreeMap::new());
        super::sync_route_health(&mut result, &health, now);
        let used = result.proxies.iter().find(|p| p.name == "used").unwrap();
        assert_eq!(used.route_failures, ["https://b.example"]);
        assert!(used.probe.is_none());
        assert!(
            result
                .proxies
                .iter()
                .find(|p| p.name == "other")
                .unwrap()
                .route_failures
                .is_empty()
        );
        health.clear();
        super::sync_route_health(&mut result, &health, now);
        assert!(result.proxies.iter().all(|p| p.route_failures.is_empty()));
    }

    #[test]
    fn activity_search_modes_target_only_the_selected_field() {
        let mut entry = Entry {
            seq: 1,
            time: None,
            event: "request_finished".into(),
            fields: serde_json::from_value(serde_json::json!({
                "status": "200", "duration_ms": "1500", "proxy": "Office",
                "path": "/v1/responses", "request_id": "other-proxy-500"
            }))
            .unwrap(),
        };
        assert!(matches_search(&entry, "keyword", "500"));
        assert!(!matches_search(&entry, "status", "500"));
        assert!(matches_search(&entry, "status", "200"));
        for invalid in ["20", "2xx", "0200", "200 500", "abc"] {
            assert!(!matches_search(&entry, "status", invalid));
        }
        assert!(matches_search(&entry, "proxy", "office"));
        assert!(!matches_search(&entry, "proxy", "off"));
        assert!(!matches_search(&entry, "proxy", "other-proxy"));
        assert!(matches_search(&entry, "path", "responses"));
        assert!(!matches_search(&entry, "path", "office"));
        assert!(!matches_search(&entry, "credential", "office"));
        for (key, name) in [
            ("account_label", "Work@Example.com"),
            ("provider", "OPENAI_API_KEY"),
        ] {
            entry.fields.insert(key.into(), serde_json::json!(name));
            assert!(matches_search(&entry, "credential", &name.to_lowercase()));
            assert!(matches_search(&entry, "credential", "work") == (key == "account_label"));
            assert!(matches_search(&entry, "credential", "openai") == (key == "provider"));
            entry.fields.remove(key);
        }
        // Other fields, such as the account ID, are not configuration names.
        entry
            .fields
            .insert("account_id".into(), serde_json::json!("acct-openai"));
        assert!(!matches_search(&entry, "credential", "openai"));
        entry.fields.remove("account_id");
        entry
            .fields
            .insert("proxy".into(), serde_json::json!("none"));
        assert!(matches_search(&entry, "proxy", "direct"));
        assert!(matches_search(&entry, "proxy", "none"));
        entry.fields.remove("proxy");
        entry.fields.remove("status");
        assert!(!matches_search(&entry, "proxy", "direct"));
        assert!(!matches_search(&entry, "status", "200"));
        for mode in ["keyword", "path", "proxy", "status", "credential"] {
            assert!(matches_search(&entry, mode, ""));
        }
    }

    const EXAMPLE: &str = include_str!("../../config.example.yaml");

    #[test]
    fn connect_urls_use_the_field_names_the_panel_reads() {
        let urls = serde_json::to_value(Urls {
            base: "http://127.0.0.1:8787".into(),
            claude: "claude".into(),
            codex: "codex".into(),
            chatgpt: "chatgpt".into(),
            ca_certificate: "/tls/ca.pem".into(),
        })
        .unwrap();
        // The panel's setup snippets read `urls.caCertificate`.
        assert_eq!(urls["caCertificate"], "/tls/ca.pem");
        for key in ["base", "claude", "codex", "chatgpt"] {
            assert!(urls[key].is_string());
        }
    }

    #[test]
    fn example_config_is_valid() {
        assert!(parse_config(EXAMPLE).is_ok());
        assert!(parse_config("listen_port: [").is_err_and(|e| e.contains("line")));
    }

    #[test]
    fn yaml_and_yml_files_support_the_same_proxy_configuration() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["config.yaml", "config.yml"] {
            let path = dir.path().join(name);
            std::fs::write(&path, EXAMPLE).unwrap();
            let text = std::fs::read_to_string(&path).unwrap();
            assert!(parse_config(&text).is_ok());
            assert!(coport::config::Config::read(&path).is_ok());
        }
    }

    #[test]
    fn import_replaces_only_with_a_valid_configuration_and_keeps_its_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("cache/config.yaml");
        let source = dir.path().join("picked.yml");
        std::fs::write(&source, "listen_port: [").unwrap();
        assert!(replace_config(&source, &target).is_err());
        assert!(!target.exists());
        // Line endings and comments are copied as they are, on every platform.
        for newline in ["\n", "\r\n"] {
            let text = format!(
                "# imported{newline}listen_port: 9797{newline}request_timeout_seconds: 30{newline}"
            );
            std::fs::write(&source, &text).unwrap();
            replace_config(&source, &target).unwrap();
            assert_eq!(std::fs::read_to_string(&target).unwrap(), text);
            assert_eq!(
                coport::config::Config::read(&target).unwrap().listen_port,
                9797
            );
        }
        std::fs::write(&source, "listen_port: 0\nrequest_timeout_seconds: 30\n").unwrap();
        assert!(replace_config(&source, &target).is_err());
        assert!(std::fs::read_to_string(&target).unwrap().contains("9797"));
        assert!(replace_config(&dir.path().join("missing.yaml"), &target).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn import_writes_through_a_linked_configuration() {
        let dir = tempfile::tempdir().unwrap();
        let linked = dir.path().join("dotfiles.yaml");
        let target = dir.path().join("config.yaml");
        let source = dir.path().join("picked.yml");
        std::fs::write(&linked, "listen_port: 8787\nrequest_timeout_seconds: 30\n").unwrap();
        std::os::unix::fs::symlink(&linked, &target).unwrap();
        std::fs::write(&source, "listen_port: 9797\nrequest_timeout_seconds: 30\n").unwrap();
        replace_config(&source, &target).unwrap();
        assert!(target.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(
            coport::config::Config::read(&linked).unwrap().listen_port,
            9797
        );
        // A link whose file is missing is kept, not replaced by a copy.
        std::fs::remove_file(&linked).unwrap();
        assert!(replace_config(&source, &target).is_err());
        assert!(target.symlink_metadata().unwrap().file_type().is_symlink());
        assert!(!linked.exists());
    }
}
