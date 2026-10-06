//! Application state shared by the tray, the panel and its commands, and the
//! snapshot the web frontend renders.

use crate::{
    describe,
    logs::{Entry, LogFeed},
    platform,
    proxy::{Controller, Notify, Phase, Probe},
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
    account_probe: Option<std::sync::Arc<coport::server::Server>>,
    /// Last account activation; kept across config changes until replaced.
    account_states: Option<AccountStates>,
    account_states_pending: bool,
    notify: Notify,
    /// Config file modification time when the proxy last started.
    started_stamp: Option<Option<SystemTime>>,
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
    crate::settings::write_private(target, text.as_bytes())
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
        let started_stamp = attached.map(|status| {
            if same_file(&status.config_path) {
                status.config_modified
            } else {
                Some(SystemTime::UNIX_EPOCH)
            }
        });
        let log_path = attached.map(|status| status.log_path.clone());
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
            account_states_pending: false,
            notify,
            started_stamp,
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
    ) -> Option<std::sync::Arc<coport::server::Server>> {
        if self.account_states_pending {
            return None;
        }
        let probe = self.account_probe()?;
        self.account_states_pending = true;
        Some(probe)
    }

    /// Stores the result of `begin_account_states`, notifying on change.
    /// Results for a configuration that has since been replaced are dropped.
    pub(crate) fn finish_account_states(
        &mut self,
        probe: &std::sync::Arc<coport::server::Server>,
        states: AccountStates,
    ) {
        if !self
            .account_probe
            .as_ref()
            .is_some_and(|current| std::sync::Arc::ptr_eq(current, probe))
        {
            return;
        }
        self.account_states_pending = false;
        if self.account_states.as_ref() != Some(&states) {
            self.account_states = Some(states);
            (self.notify)();
        }
    }

    fn account_probe(&mut self) -> Option<std::sync::Arc<coport::server::Server>> {
        if self.account_probe.is_none() {
            let config = self.loaded_config()?.clone();
            let logger = std::sync::Arc::new(coport::logger::Logger::new(
                crate::settings::log_path().with_file_name("account-probes.jsonl"),
            ));
            self.account_probe = Some(std::sync::Arc::new(coport::server::Server::new(
                config, logger,
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

    pub fn start(&mut self) {
        self.logs.set_path(crate::settings::log_path());
        self.started_stamp = Some(file_stamp(&self.config_path()));
        self.controller
            .start(&self.config_path(), crate::settings::log_path());
        self.invalidate_config();
    }

    pub fn stop(&mut self) -> Result<(), String> {
        self.controller.stop()
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
        let phase = self.controller.phase();
        let port = self.port();
        let stats = self.logs.stats();
        let base = format!("http://127.0.0.1:{port}");
        let changed_since_start = self.controller.is_running()
            && self
                .started_stamp
                .is_some_and(|started| started != file_stamp(&self.config_path()));
        let probes = self.controller.probes();
        Snapshot {
            version: env!("CARGO_PKG_VERSION"),
            phase: match &phase {
                Phase::Running { port, since } => PhaseDto {
                    state: "running",
                    port: *port,
                    uptime_secs: since.elapsed().as_secs(),
                    error: None,
                },
                Phase::Stopped => PhaseDto {
                    state: "stopped",
                    port,
                    uptime_secs: 0,
                    error: None,
                },
                Phase::Failed(message) => PhaseDto {
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
                details: self.loaded_config().map(|c| details(c, &probes)),
            },
            settings: SettingsDto {
                appearance: self.settings.appearance,
                start_proxy_on_launch: self.settings.start_proxy_on_launch,
                keep_proxy_running_on_quit: self.settings.keep_proxy_running_on_quit,
                launch_at_login: self.launch_at_login,
                log_bytes: std::fs::metadata(self.logs.path()).ok().map(|m| m.len()),
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
            .filter(|scan| scan.covers(&self.logs.path(), query.from, query.to, after))?;
        Some(scan.page(after, limit, |e| {
            e.matches_filter(&query.filter) && matches_search(e, &query.search_mode, &query.needle)
        }))
    }

    pub fn store_activity_scan(&mut self, scan: crate::activity::Scan) {
        self.activity_scan = Some(scan);
    }
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
        _ => {
            e.event.contains(needle)
                || e.fields.values().any(|v| {
                    v.as_str()
                        .is_some_and(|s| s.to_lowercase().contains(needle))
                })
        }
    }
}

fn details(config: &Config, probes: &BTreeMap<String, Probe>) -> ConfigDetails {
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
                local: crate::proxy::is_local(endpoint),
                probe: probes.get(name).map(|p| match p {
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
    version: &'static str,
    phase: PhaseDto,
    stats: StatsDto,
    routes: Vec<RouteReport>,
    urls: Urls,
    config: ConfigDto,
    settings: SettingsDto,
}

pub(crate) type AccountStates = [BTreeMap<String, &'static str>; 2];

impl Snapshot {
    pub fn set_account_route_states(&mut self, states: AccountStates) {
        if let Some(details) = &mut self.config.details {
            for (service, states) in [&mut details.codex, &mut details.claude]
                .into_iter()
                .zip(states)
            {
                for route in &mut service.account_routes {
                    route.activation = states.get(&route.selector).copied();
                }
            }
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PhaseDto {
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
struct Urls {
    base: String,
    claude: String,
    codex: String,
    /// Codex 0.160+ only accepts an HTTPS ChatGPT backend; the port also serves TLS.
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
    /// Listens on this machine; its exit address is looked up when probed.
    local: bool,
    probe: Option<ProbeDto>,
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
    activation: Option<&'static str>,
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
    appearance: crate::settings::Appearance,
    start_proxy_on_launch: bool,
    keep_proxy_running_on_quit: bool,
    launch_at_login: bool,
    /// Size of the request log in use, if it exists yet.
    log_bytes: Option<u64>,
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
    use super::{matches_search, parse_config, replace_config};
    use crate::logs::Entry;

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
        entry
            .fields
            .insert("proxy".into(), serde_json::json!("none"));
        assert!(matches_search(&entry, "proxy", "direct"));
        assert!(matches_search(&entry, "proxy", "none"));
        entry.fields.remove("proxy");
        entry.fields.remove("status");
        assert!(!matches_search(&entry, "proxy", "direct"));
        assert!(!matches_search(&entry, "status", "200"));
        for mode in ["keyword", "path", "proxy", "status"] {
            assert!(matches_search(&entry, mode, ""));
        }
    }

    const EXAMPLE: &str = include_str!("../../config.example.yaml");

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
}
