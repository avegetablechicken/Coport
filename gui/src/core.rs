//! Application state shared by the tray, the panel and its commands, and the
//! snapshot the web frontend renders.

use crate::{
    describe,
    logs::{Entry, LogFeed},
    platform,
    proxy::{Controller, Notify, Phase, Probe},
    settings::Settings,
};
use agent_router::config::{AccountSource, Choice, Config, Routing, redacted_endpoint};
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
    /// Config file modification time when the proxy last started.
    started_stamp: Option<Option<SystemTime>>,
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

fn file_stamp(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

impl Core {
    pub fn new(mut settings: Settings, notify: Notify) -> Self {
        let controller = Controller::new(notify.clone());
        let attached = controller.daemon_status();
        let started_stamp = attached.map(|status| status.config_modified);
        let log_path = attached.map(|status| status.log_path.clone());
        if let Some(status) = attached {
            // The live daemon is authoritative, even if auto-start is disabled.
            settings.config_path = status.config_path.to_string_lossy().into_owned();
            settings.save();
        }
        Self {
            logs: LogFeed::new(log_path.unwrap_or_else(|| settings.log_path()), notify),
            controller,
            launch_at_login: platform::launch_at_login(),
            settings,
            config: ConfigCache::default(),
            started_stamp,
        }
    }

    pub fn config_path(&self) -> PathBuf {
        self.settings.config_path()
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
        cache.parsed = Some(match std::fs::read_to_string(&path) {
            Ok(text) => parse_config(&text),
            Err(_) => Err("Cannot read configuration file.".to_owned()),
        });
        cache.path = path;
        cache.stamp = stamp;
    }

    pub fn invalidate_config(&mut self) {
        self.config = ConfigCache::default();
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
        self.logs.set_path(self.settings.log_path());
        self.started_stamp = Some(file_stamp(&self.config_path()));
        self.controller
            .start(&self.settings.config_path(), self.settings.log_path());
        self.invalidate_config();
    }

    pub fn stop(&mut self) -> Result<(), String> {
        self.controller.stop()
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
        let (checking, check) = self.controller.check_state();
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
                base,
            },
            config: ConfigDto {
                path: self.config_path().display().to_string(),
                exists: self.config.exists,
                error: match &self.config.parsed {
                    Some(Err(e)) if self.config.exists => Some(e.clone()),
                    _ => None,
                },
                changed_since_start,
                details: self.loaded_config().map(|c| details(c, &probes)),
            },
            check: CheckDto {
                running: checking,
                ok: check.as_ref().map(|c| c.ok),
                message: check.map(|c| c.message),
            },
            settings: SettingsDto {
                appearance: self.settings.appearance,
                start_proxy_on_launch: self.settings.start_proxy_on_launch,
                keep_proxy_running_on_quit: self.settings.keep_proxy_running_on_quit,
                launch_at_login: self.launch_at_login,
                log_path: self.settings.log_path().display().to_string(),
            },
        }
    }

    pub fn activity(&self, filter: &str, search: &str, limit: usize) -> Vec<EntryDto> {
        let needle = search.to_lowercase();
        let mut rows = self.logs.entries(|e| {
            let keep = match filter {
                "errors" => e.is_error(),
                "all" => true,
                _ => e.is_request_end(),
            };
            keep && (needle.is_empty() || matches_search(e, &needle))
        });
        rows.reverse();
        rows.truncate(limit);
        rows.into_iter().map(EntryDto::from).collect()
    }
}

fn matches_search(e: &Entry, needle: &str) -> bool {
    e.event.contains(needle)
        || e.fields.values().any(|v| {
            v.as_str()
                .is_some_and(|s| s.to_lowercase().contains(needle))
        })
}

fn details(config: &Config, probes: &BTreeMap<String, Probe>) -> ConfigDetails {
    ConfigDetails {
        listen_port: config.listen_port,
        timeout_secs: config.request_timeout_seconds,
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
            &config.codex.accounts,
            &config.codex.routing,
            true,
            |selector| describe::codex_api_key(config, selector),
        ),
        claude: service(
            vec![("upstream", config.claude.base_url.clone())],
            config.claude.account_auth_file_only,
            &config.claude.accounts,
            &config.claude.routing,
            false,
            |selector| describe::claude_api_key(config, selector),
        ),
    }
}

fn service(
    upstreams: Vec<(&'static str, String)>,
    file_only: bool,
    accounts: &BTreeMap<String, AccountSource>,
    routing: &Routing,
    codex: bool,
    api_key_detail: impl Fn(&str) -> Option<String>,
) -> ServiceDto {
    let rows = |routes: &BTreeMap<String, Choice>, detail: &dyn Fn(&str) -> Option<String>| {
        routes
            .iter()
            .map(|(selector, choice)| RouteRow {
                selector: selector.clone(),
                proxies: choice.names().to_vec(),
                detail: detail(selector),
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
        configured: !(accounts.is_empty()
            && routing.account.is_empty()
            && routing.api_key.is_empty()),
        upstreams: upstreams
            .into_iter()
            .map(|(key, value)| KeyValue {
                key: key.to_owned(),
                value,
            })
            .collect(),
        credentials: accounts
            .iter()
            .map(|(label, source)| KeyValue {
                key: label.clone(),
                value: match (&source.auth_file, &source.auth_env) {
                    (Some(file), _) => file.clone(),
                    (_, Some(env)) => format!("${env}"),
                    _ => "—".into(),
                },
            })
            .collect(),
        file_only,
        account_routes: rows(&routing.account, &|_| None),
        api_key_routes: rows(&routing.api_key, &api_key_detail),
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
    check: CheckDto,
    settings: SettingsDto,
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
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ConfigDto {
    path: String,
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
    credentials: Vec<KeyValue>,
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
    selector: String,
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
struct CheckDto {
    running: bool,
    ok: Option<bool>,
    message: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SettingsDto {
    appearance: crate::settings::Appearance,
    start_proxy_on_launch: bool,
    keep_proxy_running_on_quit: bool,
    launch_at_login: bool,
    log_path: String,
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
            bytes: e.is_request_end().then(|| e.bytes()),
            error: e.is_error(),
            reason: e.get("reason").map(str::to_owned),
            event: e.event,
            fields: e.fields,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse_config;

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
            assert!(agent_router::config::Config::read(&path).is_ok());
        }
    }
}
