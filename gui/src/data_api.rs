//! Fixed, authenticated, read-only processed-data endpoint. Never forwards requests.
use coport::{config::Config, external_access::DataKey};
use serde::{Deserialize, Serialize};
use std::{
    io,
    net::Ipv4Addr,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpListener,
    sync::{RwLock, Semaphore},
};

/// Every group belongs to the app the proxy routed its requests to.
pub use coport::routing::Service;
/// Peers from before each logged request named its app also sent `Other`
/// groups: CONNECT tunnels, whose requests they also counted on their own,
/// and token refreshes. Neither is traffic of an app, so they are dropped.
fn app_groups<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Vec<Group>, D::Error> {
    Vec::<serde_json::Value>::deserialize(deserializer)?
        .into_iter()
        .filter(|group| group["service"] != "Other")
        .map(|group| serde_json::from_value(group).map_err(serde::de::Error::custom))
        .collect()
}
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Stats {
    pub requests: u64,
    pub errors: u64,
    pub bytes: u64,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    #[serde(default)]
    pub uncached_input_tokens: Option<u64>,
    #[serde(default)]
    pub cache_write_tokens: Option<u64>,
    #[serde(default)]
    pub cache_read: u64,
    #[serde(default)]
    pub cache_prompt: u64,
    pub latency_total_ms: u64,
    pub latency_samples: u64,
    /// Time to first streamed output, summed over successful model calls that
    /// streamed any. Each call uses its forwarded timing when available,
    /// otherwise its original request-start timing. Historic samples remain
    /// in both the total and denominator. Absent from Requests, older exporters,
    /// for readers that do not name [`FIRST_TOKEN`]; see [`SERVICES_HEADER`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_token_total_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_token_samples: Option<u64>,
    /// Independent measured timing populations, negotiated via FIRST_TOKEN_TIMING.
    /// Forwarded timing starts at the first upstream send. Only successful
    /// model calls contribute to either timing; cancelled waits stay in logs.
    /// Missing historic measurements never count as zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forwarded_first_token_total_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forwarded_first_token_samples: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_total_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_samples: Option<u64>,
    /// Paired totals: output tokens excluding the first token of each completed
    /// streaming call, and time from first output to request completion.
    /// Sent only to readers naming OUTPUT_TPS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tps_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tps_duration_ms: Option<u64>,
    pub counts: Vec<u64>,
    pub error_counts: Vec<u64>,
    pub token_counts: Vec<u64>,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Group {
    pub service: Service,
    pub proxy_ref: String,
    pub upstream_ref: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_ref: Option<String>,
    pub stats: Stats,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Summary {
    pub schema_version: u8,
    pub node_id: String,
    pub window_start: i64,
    pub window_end: i64,
    pub bucket_minutes: u8,
    #[serde(deserialize_with = "app_groups")]
    pub groups: Vec<Group>,
    #[serde(default)]
    pub windows: Vec<Window>,
    #[serde(default)]
    pub previous_windows: Vec<Window>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Window {
    pub minutes: u64,
    pub scope: crate::traffic::TrafficScope,
    pub window_start: i64,
    pub window_end: i64,
    pub bucket_minutes: u64,
    #[serde(deserialize_with = "app_groups")]
    pub groups: Vec<Group>,
}
pub const RANGES: [u64; 6] = [30, 360, 720, 1440, 10080, 43200];
pub fn bucket_minutes(minutes: u64) -> Option<u64> {
    match minutes {
        30 => Some(1),
        360 => Some(15),
        720 => Some(30),
        1440 => Some(60),
        10080 => Some(360),
        43200 => Some(1440),
        _ => None,
    }
}
/// Service-separated account equality, without exporting the account text.
/// Antigravity identifies Google accounts by case-insensitive email, not UUID.
/// Its email fingerprint permits guessing known candidate addresses; it is not
/// an anonymization mechanism. Other services retain their UUID-only policy.
pub fn account_reference(service: &str, account: &str) -> Option<String> {
    if service == Service::Antigravity.label() && account.contains('@') {
        let account = account.trim();
        let (local, domain) = account.split_once('@')?;
        if local.is_empty()
            || domain.is_empty()
            || domain.contains('@')
            || account.len() > 320
            || account.chars().any(|c| c.is_whitespace() || c.is_control())
            || account.contains([':', '/', '\\', '?', '#'])
        {
            return None;
        }
        return Some(account_digest(&format!(
            "coport-google-account-email-v1\0{service}\0{}",
            account.to_ascii_lowercase()
        )));
    }
    let id = uuid::Uuid::parse_str(account).ok()?;
    if id.get_version() != Some(uuid::Version::Random) || id.get_variant() != uuid::Variant::RFC4122
    {
        return None;
    }
    let value = format!("coport-known-account-v1\0{service}\0{id}");
    Some(account_digest(&value))
}

fn account_digest(value: &str) -> String {
    ring::digest::digest(&ring::digest::SHA256, value.as_bytes())
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
/// Every reader parses Codex and Claude groups; one from before Antigravity
/// or Grok rejects a whole summary naming it. Readers therefore name the
/// further services they parse, over HTTP in this header and over SSH in this
/// environment variable, both ignored by older exporters. Groups of a service
/// the reader did not name are left out. Statistics [`FIRST_TOKEN`],
/// [`OUTPUT_TPS`] and [`FIRST_TOKEN_TIMING`] are left out likewise for readers
/// that do not name them.
pub const SERVICES_HEADER: &str = "x-coport-services";
pub const SERVICES_ENV: &str = "COPORT_SUMMARY_SERVICES";
/// The further services and optional statistics this version parses.
pub const SERVICES: &str = "antigravity,grok,first-token,output-tps,first-token-timing";
/// Names the first token statistic in a reader's list.
pub const FIRST_TOKEN: &str = "first-token";
pub const FIRST_TOKEN_TIMING: &str = "first-token-timing";
pub const OUTPUT_TPS: &str = "output-tps";

pub fn output_tps(tokens: Option<u64>, duration_ms: Option<u64>) -> Option<f64> {
    let duration = duration_ms.filter(|ms| *ms > 0)?;
    Some(tokens? as f64 * 1000.0 / duration as f64)
}

/// The services beyond Codex and Claude a reader parses, as it names them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReaderServices {
    pub antigravity: bool,
    pub grok: bool,
    /// Parses `firstTokenTotalMs` and `firstTokenSamples`.
    pub first_token: bool,
    pub first_token_timing: bool,
    pub output_tps: bool,
}
impl ReaderServices {
    /// Every service this version parses, as [`SERVICES`] names them.
    pub const ALL: Self = Self {
        antigravity: true,
        grok: true,
        first_token: true,
        first_token_timing: true,
        output_tps: true,
    };
    /// The services a reader's comma-separated list names.
    pub fn named(list: &str) -> Self {
        let names = |service: &str| {
            list.split(',')
                .any(|name| name.trim().eq_ignore_ascii_case(service))
        };
        Self {
            antigravity: names("antigravity"),
            grok: names("grok"),
            first_token: names(FIRST_TOKEN),
            first_token_timing: names(FIRST_TOKEN_TIMING),
            output_tps: names(OUTPUT_TPS),
        }
    }
    /// The services the reader that started this helper names.
    pub fn of_helper() -> Self {
        std::env::var(SERVICES_ENV)
            .map(|list| Self::named(&list))
            .unwrap_or_default()
    }
    pub fn reads(self, service: Service) -> bool {
        match service {
            Service::Codex | Service::Claude => true,
            Service::Antigravity => self.antigravity,
            Service::Grok => self.grok,
        }
    }
}

/// A published summary as a reader receives it; see [`SERVICES_HEADER`].
fn for_reader(bytes: &[u8], services: ReaderServices) -> Result<Vec<u8>, String> {
    let named = |service: Service| {
        let quoted = format!("\"{}\"", service.label());
        bytes
            .windows(quoted.len())
            .any(|part| part == quoted.as_bytes())
    };
    let first_token = || {
        let key = b"\"firstTokenSamples\"";
        bytes.windows(key.len()).any(|part| part == key)
    };
    if [Service::Antigravity, Service::Grok]
        .into_iter()
        .all(|service| services.reads(service) || !named(service))
        && (services.first_token || !first_token())
        && (services.first_token_timing
            || !bytes
                .windows(b"\"forwardedFirstTokenSamples\"".len())
                .any(|part| part == b"\"forwardedFirstTokenSamples\"")
                && !bytes
                    .windows(b"\"cooldownSamples\"".len())
                    .any(|part| part == b"\"cooldownSamples\""))
        && (services.output_tps
            || !bytes
                .windows(b"\"outputTpsTokens\"".len())
                .any(|part| part == b"\"outputTpsTokens\""))
    {
        return Ok(bytes.to_vec());
    }
    let mut summary: Summary =
        serde_json::from_slice(bytes).map_err(|_| "Cannot decode processed summary")?;
    summary.only_for(services);
    summary.validate()?;
    serde_json::to_vec(&summary).map_err(|_| "Cannot encode processed summary".into())
}

impl Summary {
    /// Leaves out the groups of services, and the statistic, a reader cannot parse.
    pub fn only_for(&mut self, services: ReaderServices) {
        let keep = |group: &Group| services.reads(group.service);
        for groups in std::iter::once(&mut self.groups).chain(
            self.windows
                .iter_mut()
                .chain(self.previous_windows.iter_mut())
                .map(|window| &mut window.groups),
        ) {
            groups.retain(keep);
            if !services.output_tps {
                for group in groups.iter_mut() {
                    group.stats.output_tps_tokens = None;
                    group.stats.output_tps_duration_ms = None;
                }
            }
            if !services.first_token_timing {
                for group in groups.iter_mut() {
                    group.stats.forwarded_first_token_total_ms = None;
                    group.stats.forwarded_first_token_samples = None;
                    group.stats.cooldown_total_ms = None;
                    group.stats.cooldown_samples = None;
                }
            }
            if !services.first_token {
                for group in groups.iter_mut() {
                    group.stats.first_token_total_ms = None;
                    group.stats.first_token_samples = None;
                }
            }
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        let now = chrono::Utc::now().timestamp_millis();
        if !matches!(self.schema_version, 1..=4)
            || uuid::Uuid::parse_str(&self.node_id).is_err()
            || self.bucket_minutes != 1
            || self.window_end % 60_000 != 0
            || self.window_start.checked_add(30 * 60_000) != Some(self.window_end)
            || self.window_end > now + 60_000
            || self.window_end < now - 150_000
            || self.groups.len() > 260
        {
            return Err("Invalid or stale processed summary.".into());
        }
        validate_groups(&self.groups, 30)?;
        if self.schema_version == 1 && !self.windows.is_empty() {
            return Err("Unexpected version-one windows.".into());
        }
        if matches!(self.schema_version, 2 | 3) && self.windows.len() != 12 {
            return Err("Missing traffic windows.".into());
        }
        if self.schema_version == 4
            && (self.windows.is_empty()
                || self.windows.len() > 12
                || self.previous_windows.len() != self.windows.len() * 2)
        {
            return Err("Invalid partial traffic windows.".into());
        }
        let mut seen = std::collections::BTreeSet::new();
        if (self.schema_version < 3 && !self.previous_windows.is_empty())
            || (self.schema_version == 3 && self.previous_windows.len() != 24)
        {
            return Err("Invalid alignment windows.".into());
        }
        for (window, previous) in self
            .windows
            .iter()
            .map(|w| (w, false))
            .chain(self.previous_windows.iter().map(|w| (w, true)))
        {
            let bucket = bucket_minutes(window.minutes).ok_or("Unsupported traffic window")?;
            if window.bucket_minutes != bucket
                || ![
                    self.window_end,
                    self.window_end - 60_000,
                    self.window_end - 120_000,
                ]
                .contains(&window.window_end)
                || (!previous && window.window_end != self.window_end)
                || (previous && window.window_end == self.window_end)
                || window.window_start != window.window_end - window.minutes as i64 * 60_000
                || !seen.insert((
                    window.window_end,
                    window.minutes,
                    window.scope == crate::traffic::TrafficScope::Model,
                ))
            {
                return Err("Invalid traffic window.".into());
            }
            validate_groups(&window.groups, (window.minutes / bucket) as usize)?;
        }
        if self.schema_version == 4 {
            for window in &self.windows {
                for offset in [60_000, 120_000] {
                    if !seen.contains(&(
                        self.window_end - offset,
                        window.minutes,
                        window.scope == crate::traffic::TrafficScope::Model,
                    )) {
                        return Err("Missing partial alignment window.".into());
                    }
                }
            }
            if !self
                .windows
                .iter()
                .any(|w| w.minutes == 30 && w.scope == crate::traffic::TrafficScope::Model)
                && !self.groups.is_empty()
            {
                return Err("Unexpected default traffic groups.".into());
            }
        }
        if self.schema_version >= 2
            && (self.schema_version != 4
                || self
                    .windows
                    .iter()
                    .any(|w| w.minutes == 30 && w.scope == crate::traffic::TrafficScope::Model))
            && self
                .windows
                .iter()
                .find(|w| w.minutes == 30 && w.scope == crate::traffic::TrafficScope::Model)
                .is_none_or(|w| w.groups != self.groups)
        {
            return Err("Conflicting default traffic window.".into());
        }
        Ok(())
    }
}
fn validate_groups(groups: &[Group], buckets: usize) -> Result<(), String> {
    if groups.len() > 260 {
        return Err("Too many traffic groups.".into());
    }
    let mut seen = std::collections::BTreeSet::new();
    for group in groups {
        if !seen.insert((group.service, &group.proxy_ref, &group.upstream_ref)) {
            return Err("Duplicate configuration group in summary.".into());
        }
        if [&group.proxy_ref, &group.upstream_ref]
            .iter()
            .any(|r| r.len() != 64 || !r.bytes().all(|c| c.is_ascii_hexdigit()))
        {
            return Err("Invalid opaque configuration reference.".into());
        }
        if group
            .account_ref
            .as_ref()
            .is_some_and(|r| r.len() != 64 || !r.bytes().all(|c| c.is_ascii_hexdigit()))
        {
            return Err("Invalid account reference.".into());
        }
        let s = &group.stats;
        if [s.counts.len(), s.error_counts.len(), s.token_counts.len()] != [buckets; 3]
            || s.counts.iter().try_fold(0u64, |sum, n| sum.checked_add(*n)) != Some(s.requests)
            || s.error_counts
                .iter()
                .try_fold(0u64, |sum, n| sum.checked_add(*n))
                != Some(s.errors)
            || s.cache_read > s.cache_prompt
            || s.errors > s.requests
            || s.latency_samples > s.requests
            || match (s.output_tps_tokens, s.output_tps_duration_ms) {
                (None, None) => false,
                (Some(tokens), Some(duration)) => {
                    (tokens == 0) != (duration == 0)
                        || tokens > s.output_tokens.unwrap_or(0)
                        || duration > s.latency_total_ms
                }
                _ => true,
            }
            || [
                (s.first_token_total_ms, s.first_token_samples),
                (
                    s.forwarded_first_token_total_ms,
                    s.forwarded_first_token_samples,
                ),
                (s.cooldown_total_ms, s.cooldown_samples),
            ]
            .into_iter()
            .any(|pair| match pair {
                (None, None) => false,
                (Some(total), Some(samples)) => {
                    samples > s.requests || (samples == 0 && total != 0)
                }
                _ => true,
            })
            || s.error_counts.iter().zip(&s.counts).any(|(e, n)| e > n)
            || [
                s.requests,
                s.errors,
                s.bytes,
                s.latency_total_ms,
                s.latency_samples,
                s.cache_read,
                s.cache_prompt,
            ]
            .iter()
            .chain(s.counts.iter())
            .chain(s.error_counts.iter())
            .chain(s.token_counts.iter())
            .chain(s.input_tokens.iter())
            .chain(s.output_tokens.iter())
            .chain(s.cached_input_tokens.iter())
            .chain(s.uncached_input_tokens.iter())
            .chain(s.cache_write_tokens.iter())
            .chain(s.first_token_total_ms.iter())
            .chain(s.first_token_samples.iter())
            .chain(s.forwarded_first_token_total_ms.iter())
            .chain(s.forwarded_first_token_samples.iter())
            .chain(s.cooldown_total_ms.iter())
            .chain(s.cooldown_samples.iter())
            .chain(s.output_tps_tokens.iter())
            .chain(s.output_tps_duration_ms.iter())
            .any(|n| *n > 9_007_199_254_740_991)
        {
            return Err("Invalid summary statistics.".into());
        }
    }
    Ok(())
}
/// Canonical endpoints stay local; only their keyed fingerprints are exported.
pub fn canonical(value: &str) -> String {
    let Ok(mut url) = reqwest::Url::parse(value) else {
        return if value == "none" { "none" } else { "unknown" }.into();
    };
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_query(None);
    url.set_fragment(None);
    url.as_str().trim_end_matches('/').into()
}

/// Server-private identity key: reading the data API key cannot reveal or guess
/// configuration values. This key never leaves the device, even through SSH.
#[cfg(unix)]
fn identity_key(dir: &Path, create: bool) -> io::Result<DataKey> {
    let path = dir.join("data-identity.key");
    if create && !path.exists() {
        use ring::rand::SecureRandom;
        let mut bytes = [0; 32];
        ring::rand::SystemRandom::new()
            .fill(&mut bytes)
            .map_err(|_| io::Error::other("Cannot create private identity key"))?;
        let key = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
        crate::settings::create_private(&path, key.as_bytes())?;
    }
    let bytes =
        coport::external_access::read_limited(&path, true, 4096).map_err(io::Error::other)?;
    let key = std::str::from_utf8(&bytes)
        .map_err(|_| io::Error::other("Invalid private identity key"))?;
    DataKey::new(key).map_err(io::Error::other)
}
#[cfg(not(unix))]
fn identity_key(_dir: &Path, create: bool) -> io::Result<DataKey> {
    if !create {
        return Err(io::Error::other(
            "SSH configuration matching is unavailable on this platform",
        ));
    }
    use ring::rand::SecureRandom;
    let mut bytes = [0; 32];
    ring::rand::SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| io::Error::other("Cannot initialize private identities"))?;
    DataKey::new(&bytes.iter().map(|b| format!("{b:02x}")).collect::<String>())
        .map_err(io::Error::other)
}
pub fn prepare_identity(dir: &Path) -> io::Result<String> {
    let _ = identity_key(dir, true)?;
    let path = dir.join("data-node-id");
    if !path.exists() {
        crate::settings::create_private(&path, uuid::Uuid::new_v4().to_string().as_bytes())?;
    }
    read_node_id(dir)
}
pub(crate) fn read_node_id(dir: &Path) -> io::Result<String> {
    let bytes = coport::external_access::read_limited(&dir.join("data-node-id"), false, 64)
        .map_err(io::Error::other)?;
    let id = String::from_utf8(bytes).map_err(|_| io::Error::other("Invalid node identity"))?;
    uuid::Uuid::parse_str(&id).map_err(|_| io::Error::other("Invalid node identity"))?;
    Ok(id)
}
/// Same processed DTO as HTTP. This command reads existing identity/config/log
/// files only; it does not start/stop services or open a network listener.
/// `services`: the services whose groups the reader parses.
pub fn read_summary(dir: &Path, services: ReaderServices) -> io::Result<Summary> {
    let (config_path, log) = crate::daemon::Client::discover(dir)
        .map(|(_, status)| (status.config_path, status.log_path))
        .unwrap_or_else(|| (dir.join("config.yaml"), dir.join("logs/proxy.log")));
    let config = Config::read(&config_path).map_err(io::Error::other)?;
    let key = identity_key(dir, false)?;
    let node = read_node_id(dir)?;
    let assignments = crate::traffic_identity::Compatibility::assignments_in(dir);
    let bytes = publish(&config, &log, &key, &node, &assignments).map_err(io::Error::other)?;
    let mut summary: Summary = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    summary.only_for(services);
    Ok(summary)
}

/// Flush the requested range before computing the remaining history.
pub fn stream_summary(
    dir: &Path,
    minutes: u64,
    scope: crate::traffic::TrafficScope,
    services: ReaderServices,
    output: impl io::Write,
) -> io::Result<()> {
    let end = chrono::Utc::now().timestamp_millis() / 60_000 * 60_000;
    stream_summary_at(dir, minutes, scope, end, services, output)
}
fn stream_summary_at(
    dir: &Path,
    minutes: u64,
    scope: crate::traffic::TrafficScope,
    end: i64,
    services: ReaderServices,
    mut output: impl io::Write,
) -> io::Result<()> {
    bucket_minutes(minutes).ok_or_else(|| io::Error::other("Unsupported traffic range"))?;
    let (config_path, log) = crate::daemon::Client::discover(dir)
        .map(|(_, status)| (status.config_path, status.log_path))
        .unwrap_or_else(|| (dir.join("config.yaml"), dir.join("logs/proxy.log")));
    let config = Config::read(&config_path).map_err(io::Error::other)?;
    let key = identity_key(dir, false)?;
    let node = read_node_id(dir)?;
    let assignments = crate::traffic_identity::Compatibility::assignments_in(dir);
    let first = publish_selected(
        &config,
        &log,
        &key,
        &node,
        &assignments,
        (minutes, scope),
        end,
    )
    .and_then(|bytes| for_reader(&bytes, services))
    .map_err(io::Error::other)?;
    output.write_all(&first)?;
    output.write_all(b"\n")?;
    output.flush()?;
    let rest = publish_at(&config, &log, &key, &node, &assignments, end)
        .and_then(|bytes| for_reader(&bytes, services))
        .map_err(io::Error::other)?;
    output.write_all(&rest)?;
    output.write_all(b"\n")?;
    output.flush()
}

fn publish_selected(
    config: &Config,
    log: &Path,
    key: &DataKey,
    node: &str,
    assignments: &[crate::traffic_identity::TrafficAssignment],
    (minutes, scope): (u64, crate::traffic::TrafficScope),
    end: i64,
) -> Result<Vec<u8>, String> {
    let bucket = bucket_minutes(minutes).ok_or("Unsupported traffic range")?;
    let identities = crate::traffic::ExportIdentities::from_config(config, assignments);
    let snapshots = crate::traffic::Snapshot::load_many(
        log,
        &[end, end - 60_000, end - 120_000],
        minutes,
        &[scope],
    )?;
    for limit in [24, 8] {
        let mut windows = Vec::new();
        for (index, offset) in [0, 60_000, 120_000].into_iter().enumerate() {
            let at = end - offset;
            windows.push(Window {
                minutes,
                scope,
                window_start: at - minutes as i64 * 60_000,
                window_end: at,
                bucket_minutes: bucket,
                groups: crate::traffic::export_window_limit(
                    crate::traffic::ReadSource::Snapshot(&snapshots[index]),
                    config,
                    key,
                    crate::traffic::ExportOptions {
                        end: at,
                        minutes,
                        scope,
                        limit,
                    },
                    &identities,
                )?,
            });
        }
        let first = windows.remove(0);
        let groups = if minutes == 30 && scope == crate::traffic::TrafficScope::Model {
            first.groups.clone()
        } else {
            Vec::new()
        };
        let summary = Summary {
            schema_version: 4,
            node_id: node.into(),
            window_start: end - 30 * 60_000,
            window_end: end,
            bucket_minutes: 1,
            groups,
            windows: vec![first],
            previous_windows: windows,
        };
        summary.validate()?;
        let bytes = serde_json::to_vec(&summary).map_err(|_| "Cannot encode processed summary")?;
        if bytes.len() <= 1024 * 1024 {
            return Ok(bytes);
        }
    }
    Err("Processed summary exceeds size limit".into())
}

#[cfg(test)]
async fn ready_response(client: &reqwest::Client, url: &str, key: &str) -> reqwest::Response {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let response = client.get(url).bearer_auth(key).send().await.unwrap();
        if response.status() != 503 {
            return response;
        }
        assert!(
            Instant::now() < deadline,
            "Statistics snapshot did not become ready"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

type CachedSummary = Arc<RwLock<Option<(Instant, Arc<Vec<u8>>)>>>;

pub struct Running {
    task: tokio::task::JoinHandle<()>,
    pub port: u16,
}
impl Drop for Running {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub async fn start(config: Config, dir: &Path, log: PathBuf) -> io::Result<Running> {
    let data = config
        .external_data
        .clone()
        .ok_or_else(|| io::Error::other("Missing data API configuration"))?;
    let key = Arc::new(data.load_key().map_err(io::Error::other)?);
    let identity = Arc::new(identity_key(dir, true)?);
    // A configured TLS error is fatal; never silently fall back to HTTP.
    let tls = data
        .tls
        .as_ref()
        .map(coport::external_access::tls_acceptor)
        .transpose()
        .map_err(io::Error::other)?;
    let node_id = prepare_identity(dir)?;
    let dir = dir.to_owned();
    let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, data.port)).await?;
    let port = listener.local_addr()?.port();
    // A statistics scan must never delay the proxy listener. Authenticated
    // readers receive 503 until the first background snapshot is ready.
    let shared = Arc::new(RwLock::new(None));
    let task = tokio::spawn(async move {
        let mut tasks = tokio::task::JoinSet::new();
        let permits = Arc::new(Semaphore::new(16));
        let mut refresh = tokio::time::interval(Duration::from_secs(15));
        refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut updating = false;
        loop {
            tokio::select! {
                Some(result)=tasks.join_next(), if !tasks.is_empty()=> { if matches!(result,Ok(true)) {updating=false;} },
                _=refresh.tick(), if !updating=> {
                    updating=true;
                    let (config,log,identity,node,shared,dir)=(config.clone(),log.clone(),identity.clone(),node_id.clone(),shared.clone(),dir.clone());
                    tasks.spawn(async move {
                        // Read each time: the panel's choices apply to the next summary.
                        if let Ok(Ok(bytes))=tokio::task::spawn_blocking(move||publish(&config,&log,&identity,&node,&crate::traffic_identity::Compatibility::assignments_in(&dir))).await {
                            *shared.write().await=Some((Instant::now(),Arc::new(bytes)));
                        }
                        true
                    });
                },
                accepted=listener.accept()=> {
                    let Ok((socket,peer))=accepted else {continue};
                    let Ok(permit)=permits.clone().try_acquire_owned() else {continue};
                    let (key,data,tls,shared)=(key.clone(),data.clone(),tls.clone(),shared.clone());
                    tasks.spawn(async move {
                        let _permit=permit;
                        let _=tokio::time::timeout(Duration::from_secs(8),async move {
                            let mut first=[0];
                            if socket.peek(&mut first).await.unwrap_or(0)==0 {return;}
                            if first[0]==0x16 {
                                if let Some(tls)=tls && let Ok(socket)=tls.accept(socket).await {handle(socket,&key,shared).await;}
                            } else if data.permits_http(peer.ip()) {handle(socket,&key,shared).await;}
                        }).await;
                        false
                    });
                }
            }
        }
    });
    Ok(Running { task, port })
}
pub(crate) fn publish(
    config: &Config,
    log: &Path,
    key: &DataKey,
    node: &str,
    assignments: &[crate::traffic_identity::TrafficAssignment],
) -> Result<Vec<u8>, String> {
    let end = chrono::Utc::now().timestamp_millis() / 60_000 * 60_000;
    publish_at(config, log, key, node, assignments, end)
}
fn publish_at(
    config: &Config,
    log: &Path,
    key: &DataKey,
    node: &str,
    assignments: &[crate::traffic_identity::TrafficAssignment],
    end: i64,
) -> Result<Vec<u8>, String> {
    // Keep two previous boundaries so clock offsets and cached HTTP responses
    // can align without asking a user to refresh or changing a machine's clock.
    let identities = crate::traffic::ExportIdentities::from_config(config, assignments);
    let snapshots = crate::traffic::Snapshot::load_many(
        log,
        &[end, end - 60_000, end - 120_000],
        43200,
        &[
            crate::traffic::TrafficScope::All,
            crate::traffic::TrafficScope::Model,
        ],
    )?;
    for limit in [24, 8] {
        let mut windows = Vec::new();
        let mut previous_windows = Vec::new();
        for (index, offset) in [0, 60_000, 120_000].into_iter().enumerate() {
            let at = end - offset;
            let options: Vec<_> = RANGES
                .into_iter()
                .flat_map(|minutes| {
                    [
                        crate::traffic::TrafficScope::All,
                        crate::traffic::TrafficScope::Model,
                    ]
                    .into_iter()
                    .map(move |scope| crate::traffic::ExportOptions {
                        end: at,
                        minutes,
                        scope,
                        limit,
                    })
                })
                .collect();
            let exported = crate::traffic::export_windows_limit(
                crate::traffic::ReadSource::Snapshot(&snapshots[index]),
                config,
                key,
                &options,
                &identities,
            )?;
            for (option, groups) in options.into_iter().zip(exported) {
                let window = Window {
                    minutes: option.minutes,
                    scope: option.scope,
                    window_start: at - option.minutes as i64 * 60_000,
                    window_end: at,
                    bucket_minutes: bucket_minutes(option.minutes).unwrap(),
                    groups,
                };
                if offset == 0 {
                    windows.push(window);
                } else {
                    previous_windows.push(window);
                }
            }
        }

        let groups = windows
            .iter()
            .find(|w| w.minutes == 30 && w.scope == crate::traffic::TrafficScope::Model)
            .unwrap()
            .groups
            .clone();
        let summary = Summary {
            schema_version: 3,
            node_id: node.into(),
            window_start: end - 30 * 60_000,
            window_end: end,
            bucket_minutes: 1,
            groups,
            windows,
            previous_windows,
        };
        summary.validate()?;
        let bytes = serde_json::to_vec(&summary).map_err(|_| "Cannot encode processed summary")?;
        if bytes.len() <= 1024 * 1024 {
            return Ok(bytes);
        }
    }
    Err("Processed summary exceeds size limit".into())
}

async fn handle<S: AsyncRead + AsyncWrite + Unpin>(
    mut socket: S,
    key: &DataKey,
    shared: CachedSummary,
) {
    let mut head = Vec::with_capacity(1024);
    loop {
        match socket.read_u8().await {
            Ok(byte) => head.push(byte),
            Err(_) => return,
        }
        if head.len() > 8192 {
            reply(&mut socket, 431, b"{}").await;
            return;
        }
        if head.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let status = validate_request(&head, key);
    if let Err(status) = status {
        reply(&mut socket, status, b"{}").await;
        return;
    }
    let Some((at, bytes)) = shared.read().await.clone() else {
        reply(&mut socket, 503, b"{}").await;
        return;
    };
    if at.elapsed() > Duration::from_secs(90) {
        reply(&mut socket, 503, b"{}").await;
        return;
    }
    let services = reader_services(&head)
        .map(|list| ReaderServices::named(&list))
        .unwrap_or_default();
    if let Ok(Some((minutes, scope))) = requested_window(&head) {
        let selected = (|| {
            let mut summary: Summary = serde_json::from_slice(&bytes).ok()?;
            summary.only_for(services);
            summary
                .windows
                .retain(|w| w.minutes == minutes && w.scope == scope);
            summary
                .previous_windows
                .retain(|w| w.minutes == minutes && w.scope == scope);
            if minutes != 30 || scope != crate::traffic::TrafficScope::Model {
                summary.groups.clear();
            }
            summary.schema_version = 4;
            summary.validate().ok()?;
            serde_json::to_vec(&summary).ok()
        })();
        match selected {
            Some(bytes) => reply(&mut socket, 200, &bytes).await,
            None => reply(&mut socket, 503, b"{}").await,
        }
    } else {
        match for_reader(&bytes, services) {
            Ok(bytes) => reply(&mut socket, 200, &bytes).await,
            Err(_) => reply(&mut socket, 503, b"{}").await,
        }
    }
}
fn validate_request(head: &[u8], key: &DataKey) -> Result<(), u16> {
    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut request = httparse::Request::new(&mut headers);
    if !matches!(request.parse(head), Ok(httparse::Status::Complete(_))) {
        return Err(400);
    }
    let auth = request
        .headers
        .iter()
        .filter(|h| h.name.eq_ignore_ascii_case("authorization"))
        .collect::<Vec<_>>();
    if auth.len() != 1
        || !std::str::from_utf8(auth[0].value)
            .ok()
            .and_then(|s| s.strip_prefix("Bearer "))
            .is_some_and(|s| key.accepts(s))
    {
        return Err(401);
    }
    if request.method != Some("GET") {
        return Err(405);
    }
    if request.path != Some("/v1/summary") {
        return Err(404);
    }
    let mut names = std::collections::HashSet::new();
    for h in request.headers {
        if !names.insert(h.name.to_ascii_lowercase())
            || h.name.eq_ignore_ascii_case("transfer-encoding")
            || h.name.eq_ignore_ascii_case("upgrade")
            || h.name.eq_ignore_ascii_case("expect")
            || (h.name.eq_ignore_ascii_case("content-length") && h.value != b"0")
        {
            return Err(400);
        }
    }
    if !names.contains("host") {
        return Err(400);
    }
    requested_window(head)?;
    Ok(())
}
/// The services a reader names in [`SERVICES_HEADER`].
fn reader_services(head: &[u8]) -> Option<String> {
    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut request = httparse::Request::new(&mut headers);
    request.parse(head).ok()?;
    let header = request
        .headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case(SERVICES_HEADER))?;
    std::str::from_utf8(header.value).ok().map(str::to_owned)
}
fn requested_window(head: &[u8]) -> Result<Option<(u64, crate::traffic::TrafficScope)>, u16> {
    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut request = httparse::Request::new(&mut headers);
    request.parse(head).map_err(|_| 400u16)?;
    let Some(header) = request
        .headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case("x-coport-window"))
    else {
        return Ok(None);
    };
    let value = std::str::from_utf8(header.value).map_err(|_| 400u16)?;
    let (minutes, scope) = value.split_once(':').ok_or(400u16)?;
    let minutes = minutes.parse().map_err(|_| 400u16)?;
    bucket_minutes(minutes).ok_or(400u16)?;
    let scope = match scope {
        "model" => crate::traffic::TrafficScope::Model,
        "all" => crate::traffic::TrafficScope::All,
        _ => return Err(400),
    };
    Ok(Some((minutes, scope)))
}
async fn reply<S: AsyncWrite + Unpin>(socket: &mut S, status: u16, body: &[u8]) {
    let head = format!(
        "HTTP/1.1 {status} Response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n\r\n",
        body.len()
    );
    let _ = socket.write_all(head.as_bytes()).await;
    let _ = socket.write_all(body).await;
    let _ = socket.shutdown().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{DATA_KEY as KEY, DATA_KEY_ENV, data_key_in_subprocess};
    use serde_json::json;

    #[cfg(unix)]
    #[test]
    fn both_stream_frames_use_the_original_boundary_even_when_the_clock_advances() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("logs")).unwrap();
        std::fs::write(dir.path().join("logs/proxy.log"), "").unwrap();
        std::fs::write(dir.path().join("config.yaml"), "listen_port: 8787\nrequest_timeout_seconds: 30\ncodex:\n  homes: []\nclaude:\n  config_dirs: []\n").unwrap();
        prepare_identity(dir.path()).unwrap();
        let original_end = chrono::Utc::now().timestamp_millis() / 60_000 * 60_000 - 60_000;
        let mut output = Vec::new();
        stream_summary_at(
            dir.path(),
            1440,
            crate::traffic::TrafficScope::All,
            original_end,
            ReaderServices::ALL,
            &mut output,
        )
        .unwrap();
        let frames: Vec<Summary> = output
            .split(|b| *b == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).unwrap())
            .collect();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].schema_version, 4);
        assert_eq!(frames[1].schema_version, 3);
        for frame in frames {
            frame.validate().unwrap();
            assert_eq!(frame.window_end, original_end);
        }
    }

    #[test]
    fn selected_summary_contains_only_requested_range_with_alignment() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("proxy.log");
        std::fs::write(&log, "").unwrap();
        let config = Config::parse("listen_port: 8787\nrequest_timeout_seconds: 30\ncodex:\n  homes: []\nclaude:\n  config_dirs: []\n").unwrap();
        let key = DataKey::new(KEY).unwrap();
        for minutes in RANGES {
            for scope in [
                crate::traffic::TrafficScope::Model,
                crate::traffic::TrafficScope::All,
            ] {
                let before = crate::traffic::live_scans(&log);
                let bytes = publish_selected(
                    &config,
                    &log,
                    &key,
                    &uuid::Uuid::new_v4().to_string(),
                    &[],
                    (minutes, scope),
                    chrono::Utc::now().timestamp_millis() / 60_000 * 60_000,
                )
                .unwrap();
                assert_eq!(crate::traffic::live_scans(&log) - before, 1);
                let summary: Summary = serde_json::from_slice(&bytes).unwrap();
                summary.validate().unwrap();
                assert_eq!(summary.schema_version, 4);
                assert_eq!(summary.windows.len(), 1);
                assert_eq!(summary.previous_windows.len(), 2);
                assert!(
                    summary
                        .windows
                        .iter()
                        .chain(&summary.previous_windows)
                        .all(|w| w.minutes == minutes && w.scope == scope)
                );
                let mut invalid = summary.clone();
                invalid.previous_windows[0].scope = if scope == crate::traffic::TrafficScope::All {
                    crate::traffic::TrafficScope::Model
                } else {
                    crate::traffic::TrafficScope::All
                };
                assert!(invalid.validate().is_err());
                let mut invalid = summary;
                invalid.windows.clear();
                assert!(invalid.validate().is_err());
            }
        }
    }

    #[tokio::test]
    async fn http_selected_window_is_bounded_and_does_not_change_cached_snapshot() {
        let request = |method: &str, path: &str, extra: &str| {
            format!(
            "{method} {path} HTTP/1.1\r\nHost: data\r\nAuthorization: Bearer {KEY}\r\n{extra}\r\n"
        ).into_bytes()
        };
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("proxy.log");
        std::fs::write(&log, "").unwrap();
        let config = Config::parse("listen_port: 8787\nrequest_timeout_seconds: 30\ncodex:\n  homes: []\nclaude:\n  config_dirs: []\n").unwrap();
        let key = DataKey::new(KEY).unwrap();
        let bytes = publish(&config, &log, &key, &uuid::Uuid::new_v4().to_string(), &[]).unwrap();
        let shared = Arc::new(RwLock::new(Some((Instant::now(), Arc::new(bytes.clone())))));
        let (mut client, server) = tokio::io::duplex(1024 * 1024);
        let cache = shared.clone();
        let task = tokio::spawn(async move {
            handle(server, &key, cache).await;
        });
        client
            .write_all(request("GET", "/v1/summary", "X-Coport-Window: 1440:all\r\n").as_slice())
            .await
            .unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        task.await.unwrap();
        let split = response.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        let summary: Summary = serde_json::from_slice(&response[split..]).unwrap();
        summary.validate().unwrap();
        assert_eq!(summary.schema_version, 4);
        assert_eq!(summary.windows.len(), 1);
        assert_eq!(summary.windows[0].minutes, 1440);
        assert_eq!(summary.windows[0].scope, crate::traffic::TrafficScope::All);
        assert_eq!(shared.read().await.as_ref().unwrap().1.as_ref(), &bytes);
        for invalid in ["31:all", "1440:other", "1440:all:extra"] {
            assert_eq!(
                validate_request(
                    &request(
                        "GET",
                        "/v1/summary",
                        &format!("X-Coport-Window: {invalid}\r\n")
                    ),
                    &DataKey::new(KEY).unwrap()
                ),
                Err(400)
            );
        }
    }
    /// A state directory whose log holds one Antigravity, one Grok and one
    /// Codex model call; returns it with its log and configuration.
    fn antigravity_traffic() -> (tempfile::TempDir, PathBuf, &'static str) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("logs")).unwrap();
        let log = dir.path().join("logs/proxy.log");
        let at = (chrono::Utc::now() - chrono::Duration::minutes(2)).to_rfc3339();
        let row = |service: &str, path: &str| {
            json!({"timestamp":at, "event":"model_call_finished", "model_call_id":uuid::Uuid::new_v4().to_string(),
                "request_id":uuid::Uuid::new_v4().to_string(), "service":service, "method":"POST", "path":path,
                "account_id":"user@example.test", "status":"200", "first_token_ms":"420",
                "forwarded_first_token_ms":"120", "location_cooldown_actual_ms":"250",
                "output_tokens":"101", "duration_ms":"2420"})
            .to_string()
                + "\n"
        };
        std::fs::write(
            &log,
            row(
                "antigravity",
                "/antigravity/v1internal:streamGenerateContent",
            ) + &row("grok", "/grok/v1/responses")
                + &row("codex", "/v1/responses"),
        )
        .unwrap();
        let config: &str = "listen_port: 8787\nrequest_timeout_seconds: 30\ncodex:\n  homes: []\nclaude:\n  config_dirs: []\n";
        (dir, log, config)
    }
    /// The services any group of a summary belongs to.
    fn summary_services(summary: &Summary) -> Vec<Service> {
        let mut all: Vec<_> = summary
            .groups
            .iter()
            .chain(
                summary
                    .windows
                    .iter()
                    .chain(&summary.previous_windows)
                    .flat_map(|w| &w.groups),
            )
            .map(|g| g.service)
            .collect();
        all.sort();
        all.dedup();
        all
    }

    /// Readers from before Antigravity or Grok reject a summary naming it, so
    /// only an HTTP reader that names a service receives its groups.
    #[tokio::test]
    async fn antigravity_and_grok_groups_reach_only_http_readers_that_name_them() {
        let (_dir, log, config) = antigravity_traffic();
        let services = summary_services;
        let key = DataKey::new(KEY).unwrap();
        let bytes = publish(
            &Config::parse(config).unwrap(),
            &log,
            &key,
            &uuid::Uuid::new_v4().to_string(),
            &[],
        )
        .unwrap();
        let shared = Arc::new(RwLock::new(Some((Instant::now(), Arc::new(bytes)))));
        for (extra, antigravity, grok) in [
            ("", false, false),
            ("X-Coport-Window: 30:model\r\n", false, false),
            // A reader from before Grok.
            ("X-Coport-Services: codex, antigravity\r\n", true, false),
            (
                "X-Coport-Services: antigravity\r\nX-Coport-Window: 30:model\r\n",
                true,
                false,
            ),
            ("X-Coport-Services: Grok\r\n", false, true),
            (
                "X-Coport-Services: antigravity,grok\r\nX-Coport-Window: 30:model\r\n",
                true,
                true,
            ),
        ] {
            let (mut client, server) = tokio::io::duplex(1024 * 1024);
            let (key, cache) = (DataKey::new(KEY).unwrap(), shared.clone());
            let task = tokio::spawn(async move { handle(server, &key, cache).await });
            client
                .write_all(
                    format!("GET /v1/summary HTTP/1.1\r\nHost: data\r\nAuthorization: Bearer {KEY}\r\n{extra}\r\n")
                        .as_bytes(),
                )
                .await
                .unwrap();
            let mut response = Vec::new();
            client.read_to_end(&mut response).await.unwrap();
            task.await.unwrap();
            let split = response.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
            let body = &response[split..];
            let summary: Summary = serde_json::from_slice(body).unwrap();
            summary.validate().unwrap();
            let mut expected = vec![Service::Codex];
            if antigravity {
                expected.push(Service::Antigravity);
            }
            if grok {
                expected.push(Service::Grok);
            }
            assert_eq!(services(&summary), expected, "{extra:?}");
            let names = |name: &[u8]| body.windows(name.len()).any(|w| w == name);
            assert_eq!(names(b"Antigravity"), antigravity, "{extra:?}");
            assert_eq!(names(b"\"Grok\""), grok, "{extra:?}");
        }

        assert_eq!(
            ReaderServices::named(" Antigravity , GROK"),
            ReaderServices {
                first_token: false,
                first_token_timing: false,
                output_tps: false,
                ..ReaderServices::ALL
            }
        );
        assert_eq!(
            ReaderServices::named("antigravity-next,grok-x,codex,first-token-x"),
            ReaderServices::default()
        );
        assert_eq!(ReaderServices::named(SERVICES), ReaderServices::ALL);
        assert_eq!(
            ReaderServices::named(" First-Token "),
            ReaderServices {
                first_token: true,
                ..ReaderServices::default()
            }
        );
    }

    /// The first token statistic of a Models group, as a reader receives it.
    fn first_token(summary: &Summary, body: &[u8]) -> Vec<(Option<u64>, Option<u64>)> {
        let named = |name: &[u8]| body.windows(name.len()).any(|w| w == name);
        let stats: Vec<_> = summary
            .groups
            .iter()
            .chain(summary.windows.iter().flat_map(|w| &w.groups))
            .map(|g| (g.stats.first_token_total_ms, g.stats.first_token_samples))
            .collect();
        // A reader from before First Token rejects a summary naming it.
        assert_eq!(
            named(b"firstToken"),
            stats.iter().any(|s| s.0.is_some()),
            "{stats:?}"
        );
        stats
    }

    fn assert_first_token_timing(summary: &Summary, body: &[u8], named: bool) {
        let mut measured = false;
        for groups in std::iter::once(&summary.groups).chain(
            summary
                .windows
                .iter()
                .chain(summary.previous_windows.iter())
                .map(|w| &w.groups),
        ) {
            for group in groups {
                let s = &group.stats;
                let values = (
                    s.forwarded_first_token_total_ms,
                    s.forwarded_first_token_samples,
                    s.cooldown_total_ms,
                    s.cooldown_samples,
                );
                if !named {
                    assert_eq!(values, (None, None, None, None));
                }
                if values == (Some(120), Some(1), Some(250), Some(1)) {
                    measured = true;
                }
            }
        }
        assert_eq!(measured, named);
        for key in [
            "forwardedFirstTokenTotalMs",
            "forwardedFirstTokenSamples",
            "cooldownTotalMs",
            "cooldownSamples",
        ] {
            assert_eq!(body.windows(key.len()).any(|w| w == key.as_bytes()), named);
        }
    }

    /// Readers from before First Token reject a summary naming it, so only one
    /// that names it receives it, over HTTP and SSH alike.
    #[tokio::test]
    async fn first_token_reaches_only_readers_that_name_it() {
        // Keep the traffic fixture alive on every platform; only Unix reads
        // its directory directly for the SSH protocol checks below.
        let (_dir, log, config) = antigravity_traffic();
        let key = DataKey::new(KEY).unwrap();
        let bytes = publish(
            &Config::parse(config).unwrap(),
            &log,
            &key,
            &uuid::Uuid::new_v4().to_string(),
            &[],
        )
        .unwrap();
        let shared = Arc::new(RwLock::new(Some((Instant::now(), Arc::new(bytes)))));
        for (extra, named) in [
            ("", false),
            ("X-Coport-Window: 30:model\r\n", false),
            ("X-Coport-Services: antigravity,grok\r\n", false),
            (
                "X-Coport-Services: antigravity,grok\r\nX-Coport-Window: 30:model\r\n",
                false,
            ),
            ("X-Coport-Services: first-token\r\n", true),
            ("X-Coport-Services: output-tps\r\n", false),
            ("X-Coport-Services: first-token,output-tps\r\n", true),
            (
                "X-Coport-Services: first-token,first-token-timing\r\n",
                true,
            ),
            (
                "X-Coport-Services: first-token-timing\r\nX-Coport-Window: 30:model\r\n",
                false,
            ),
            (
                "X-Coport-Services: first-token,output-tps\r\nX-Coport-Window: 30:model\r\n",
                true,
            ),
            (
                "X-Coport-Services: antigravity,grok,first-token\r\nX-Coport-Window: 30:model\r\n",
                true,
            ),
        ] {
            let (mut client, server) = tokio::io::duplex(1024 * 1024);
            let (key, cache) = (DataKey::new(KEY).unwrap(), shared.clone());
            let task = tokio::spawn(async move { handle(server, &key, cache).await });
            client
                .write_all(
                    format!("GET /v1/summary HTTP/1.1\r\nHost: data\r\nAuthorization: Bearer {KEY}\r\n{extra}\r\n")
                        .as_bytes(),
                )
                .await
                .unwrap();
            let mut response = Vec::new();
            client.read_to_end(&mut response).await.unwrap();
            task.await.unwrap();
            let split = response.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
            let body = &response[split..];
            let summary: Summary = serde_json::from_slice(body).unwrap();
            summary.validate().unwrap();
            let stats = first_token(&summary, body);
            assert!(!stats.is_empty(), "{extra:?}");
            let expected = if named {
                (Some(120), Some(1))
            } else {
                (None, None)
            };
            assert!(stats.iter().all(|s| *s == expected), "{extra:?} {stats:?}");
            assert_output_tps(&summary, body, extra.contains("output-tps"));
            assert_first_token_timing(&summary, body, extra.contains("first-token-timing"));
        }
        #[cfg(unix)]
        {
            std::fs::write(_dir.path().join("config.yaml"), config).unwrap();
            prepare_identity(_dir.path()).unwrap();
            for (reader, named) in [("", false), ("antigravity,grok", false), (SERVICES, true)] {
                let services = ReaderServices::named(reader);
                let end = chrono::Utc::now().timestamp_millis() / 60_000 * 60_000;
                let mut output = Vec::new();
                stream_summary_at(
                    _dir.path(),
                    30,
                    crate::traffic::TrafficScope::Model,
                    end,
                    services,
                    &mut output,
                )
                .unwrap();
                let mut single =
                    serde_json::to_vec(&read_summary(_dir.path(), services).unwrap()).unwrap();
                single.push(b'\n');
                output.extend(single);
                for line in output.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
                    let frame: Summary = serde_json::from_slice(line).unwrap();
                    frame.validate().unwrap();
                    let stats = first_token(&frame, line);
                    assert_output_tps(&frame, line, services.output_tps);
                    assert_first_token_timing(&frame, line, services.first_token_timing);
                    assert!(!stats.is_empty(), "{reader:?}");
                    assert_eq!(
                        stats.contains(&(Some(120), Some(1))),
                        named,
                        "{reader:?} {stats:?}"
                    );
                }
            }
        }
    }

    fn assert_output_tps(summary: &Summary, bytes: &[u8], named: bool) {
        let stats: Vec<_> = summary
            .groups
            .iter()
            .chain(
                summary
                    .windows
                    .iter()
                    .chain(&summary.previous_windows)
                    .flat_map(|w| &w.groups),
            )
            .map(|g| (g.stats.output_tps_tokens, g.stats.output_tps_duration_ms))
            .collect();
        assert!(!stats.is_empty());
        assert_eq!(
            bytes.windows(b"outputTps".len()).any(|w| w == b"outputTps"),
            named
        );
        if named {
            assert!(stats.contains(&(Some(100), Some(2000))));
        } else {
            assert!(stats.iter().all(|s| *s == (None, None)));
        }
    }

    /// As over HTTP, for the SSH helper's stream and single summary.
    #[cfg(unix)]
    #[test]
    fn antigravity_and_grok_groups_reach_only_ssh_readers_that_name_them() {
        let (dir, _log, config) = antigravity_traffic();
        std::fs::write(dir.path().join("config.yaml"), config).unwrap();
        prepare_identity(dir.path()).unwrap();
        let services = summary_services;
        for reader in ["", "antigravity", "grok", SERVICES] {
            let reader = ReaderServices::named(reader);
            let end = chrono::Utc::now().timestamp_millis() / 60_000 * 60_000;
            let mut output = Vec::new();
            stream_summary_at(
                dir.path(),
                30,
                crate::traffic::TrafficScope::Model,
                end,
                reader,
                &mut output,
            )
            .unwrap();
            for line in output.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
                let frame: Summary = serde_json::from_slice(line).unwrap();
                frame.validate().unwrap();
                for service in [Service::Antigravity, Service::Grok] {
                    assert_eq!(
                        services(&frame).contains(&service),
                        reader.reads(service),
                        "{reader:?}"
                    );
                }
                assert!(services(&frame).contains(&Service::Codex));
            }
            let summary = read_summary(dir.path(), reader).unwrap();
            for service in [Service::Antigravity, Service::Grok] {
                assert_eq!(
                    services(&summary).contains(&service),
                    reader.reads(service),
                    "{reader:?}"
                );
            }
        }
    }
    #[tokio::test]
    async fn starting_data_listener_does_not_scan_logs_before_returning() {
        if data_key_in_subprocess(
            "data_api::tests::starting_data_listener_does_not_scan_logs_before_returning",
        ) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("proxy.log");
        std::fs::write(&log, "").unwrap();
        let mut config = Config::parse(&format!("listen_port: 8787\nrequest_timeout_seconds: 30\nallow_external_access: true\nexternal_data:\n  port: 8788\n  token_env: {DATA_KEY_ENV}\n  trusted_lan: [10.42.0.0/24]\ncodex:\n  homes: []\nclaude:\n  config_dirs: []\n")).unwrap();
        // Let the OS allocate the test port atomically. Reserving and releasing
        // an ephemeral port races concurrent client connections in other tests.
        config.external_data.as_mut().unwrap().port = 0;
        let before = crate::traffic::live_scans(&log);
        let running = start(config, dir.path(), log.clone()).await.unwrap();
        // On this current-thread runtime the background publisher has not been
        // polled yet. Startup must already have returned a bound listener.
        assert_ne!(running.port, 0);
        assert_eq!(crate::traffic::live_scans(&log), before);
        let socket = std::net::TcpStream::connect_timeout(
            &(Ipv4Addr::LOCALHOST, running.port).into(),
            Duration::from_secs(1),
        )
        .unwrap();
        drop(socket);
        drop(running);
    }

    #[tokio::test]
    async fn warming_cache_returns_503_and_still_enforces_authentication() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/summary", listener.local_addr().unwrap());
        let key = Arc::new(DataKey::new(KEY).unwrap());
        let shared = Arc::new(RwLock::new(None));
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (socket, _) = listener.accept().await.unwrap();
                handle(socket, &key, shared.clone()).await;
            }
        });
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        assert_eq!(client.get(&url).send().await.unwrap().status(), 401);
        assert_eq!(
            client
                .get(&url)
                .bearer_auth(KEY)
                .send()
                .await
                .unwrap()
                .status(),
            503
        );
        server.await.unwrap();
    }

    #[test]
    fn publishing_all_windows_reads_live_log_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("proxy.log");
        std::fs::write(&path, "").unwrap();
        let config = Config::parse("listen_port: 8787\nrequest_timeout_seconds: 30\ncodex:\n  homes: []\nclaude:\n  config_dirs: []\n").unwrap();
        let before = crate::traffic::live_scans(&path);
        let bytes = publish(
            &config,
            &path,
            &DataKey::new(KEY).unwrap(),
            &uuid::Uuid::new_v4().to_string(),
            &[],
        )
        .unwrap();
        let summary: Summary = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(summary.windows.len(), 12);
        assert_eq!(summary.previous_windows.len(), 24);
        assert_eq!(crate::traffic::live_scans(&path) - before, 1);
    }

    #[test]
    fn only_authenticated_exact_get_is_accepted() {
        let key = DataKey::new(KEY).unwrap();
        let request = |method: &str, path: &str, extra: &str| {
            format!("{method} {path} HTTP/1.1\r\nHost: data\r\nAuthorization: Bearer {KEY}\r\n{extra}\r\n").into_bytes()
        };
        assert!(validate_request(&request("GET", "/v1/summary", ""), &key).is_ok());
        for method in ["POST", "PUT", "DELETE", "CONNECT", "PATCH"] {
            assert_eq!(
                validate_request(&request(method, "/v1/summary", ""), &key),
                Err(405)
            );
        }
        for path in [
            "/config",
            "/logs",
            "/stop",
            "/restart",
            "/v1/summary?path=/etc/passwd",
            "/health",
        ] {
            assert_eq!(validate_request(&request("GET", path, ""), &key), Err(404));
        }
        assert_eq!(
            validate_request(
                &request(
                    "GET",
                    "/v1/summary",
                    &format!("Authorization: Bearer {KEY}\r\n")
                ),
                &key
            ),
            Err(401)
        );
        assert_eq!(
            validate_request(b"GET /v1/summary HTTP/1.1\r\nHost: data\r\n\r\n", &key),
            Err(401)
        );
        for header in [
            "Content-Length: 12\r\n",
            "Transfer-Encoding: chunked\r\n",
            "Upgrade: websocket\r\n",
            "Host: second\r\n",
        ] {
            assert_eq!(
                validate_request(&request("GET", "/v1/summary", header), &key),
                Err(400)
            );
        }
    }
    #[test]
    fn exported_results_never_include_raw_configuration_or_log_fields() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("proxy.log");
        let now = chrono::Utc::now() - chrono::Duration::minutes(2);
        let record = json!({"timestamp":now.to_rfc3339(),"event":"model_call_finished","model_call_id":"test-call", "request_id":"request-private", "service":"codex", "method":"POST", "path":"/responses", "proxy":"secret-proxy-name", "proxy_endpoint":"http://user:password@10.9.8.7:7891", "upstream_base_url":"https://secret-upstream.example/v1?token=secret", "account_id":"private-account", "account_label":"private@example.com", "provider":"private-provider", "input_tokens":"7", "output_tokens":"3", "status":"200"});
        std::fs::write(&log, format!("{record}\n")).unwrap();
        let config = Config::parse("listen_port: 8787\nrequest_timeout_seconds: 30\n").unwrap();
        let bytes = publish(
            &config,
            &log,
            &DataKey::new(KEY).unwrap(),
            &uuid::Uuid::new_v4().to_string(),
            &[],
        )
        .unwrap();
        let body = String::from_utf8(bytes.clone()).unwrap();
        for secret in [
            "10.9.8.7",
            "secret-upstream",
            "secret-proxy-name",
            "private-account",
            "private@example.com",
            "private-provider",
            "request-private",
            "password",
            "account_id",
            "proxy_endpoint",
            "upstream_base_url",
            KEY,
        ] {
            assert!(!body.contains(secret), "Leaked {secret}");
        }
        let summary: Summary = serde_json::from_slice(&bytes).unwrap();
        summary.validate().unwrap();
        assert_eq!(summary.groups.len(), 1);
        assert_eq!(summary.groups[0].stats.requests, 1);
        assert_eq!(summary.groups[0].stats.input_tokens, Some(7));
        assert_eq!(summary.groups[0].stats.output_tokens, Some(3));
    }
    #[test]
    fn groups_of_no_app_from_older_peers_are_dropped() {
        let end = chrono::Utc::now().timestamp_millis() / 60_000 * 60_000;
        let mut counts = vec![0; 30];
        counts[29] = 1;
        let group = |service: &str, upstream: &str| {
            json!({"service":service, "proxyRef":"a".repeat(64), "upstreamRef":upstream.repeat(64),
                "stats":Stats { requests: 1, counts: counts.clone(), error_counts: vec![0; 30], token_counts: vec![0; 30], ..Default::default() }})
        };
        let value = json!({"schemaVersion":1, "nodeId":uuid::Uuid::new_v4().to_string(), "windowStart":end-30*60_000,
            "windowEnd":end, "bucketMinutes":1, "groups":[group("Codex", "b"), group("Other", "c"), group("Claude", "d")]});
        let summary: Summary = serde_json::from_value(value).unwrap();
        summary.validate().unwrap();
        assert_eq!(
            summary.groups.iter().map(|g| g.service).collect::<Vec<_>>(),
            [Service::Codex, Service::Claude]
        );
        let unknown = json!({"schemaVersion":1, "nodeId":uuid::Uuid::new_v4().to_string(), "windowStart":end-30*60_000,
            "windowEnd":end, "bucketMinutes":1, "groups":[group("Unknown", "b")]});
        assert!(serde_json::from_value::<Summary>(unknown).is_err());
    }
    #[test]
    fn summary_parser_rejects_unexpected_sensitive_fields_and_stale_windows() {
        let end = chrono::Utc::now().timestamp_millis() / 60_000 * 60_000;
        let mut value = json!({"schemaVersion":1,"nodeId":uuid::Uuid::new_v4().to_string(),"windowStart":end-30*60_000,"windowEnd":end,"bucketMinutes":1,"groups":[]});
        let valid: Summary = serde_json::from_value(value.clone()).unwrap();
        valid.validate().unwrap();
        value["configuration"] = json!({"token":KEY});
        assert!(serde_json::from_value::<Summary>(value).is_err());
        let stale = Summary {
            window_start: valid.window_start - 10 * 60_000,
            window_end: valid.window_end - 10 * 60_000,
            ..valid
        };
        assert!(stale.validate().is_err());
    }
    #[test]
    fn first_token_statistic_is_optional_but_consistent() {
        let end = chrono::Utc::now().timestamp_millis() / 60_000 * 60_000;
        let reference = "a".repeat(64);
        let summary = |stats: serde_json::Value| {
            let mut group = json!({"requests":2,"errors":0,"bytes":0,"inputTokens":null,"outputTokens":null,"cachedInputTokens":null,
                "latencyTotalMs":0,"latencySamples":0,"counts":vec![0; 29].into_iter().chain([2]).collect::<Vec<u64>>(),
                "errorCounts":vec![0; 30],"tokenCounts":vec![0; 30]});
            group
                .as_object_mut()
                .unwrap()
                .extend(stats.as_object().unwrap().clone());
            serde_json::from_value::<Summary>(json!({"schemaVersion":1,"nodeId":uuid::Uuid::new_v4().to_string(),
                "windowStart":end-30*60_000,"windowEnd":end,"bucketMinutes":1,
                "groups":[{"service":"Codex","proxyRef":reference,"upstreamRef":reference,"stats":group}]}))
            .map_err(|e| e.to_string())
            .and_then(|summary| summary.validate().map(|_| summary))
        };
        for (total, samples) in [
            ("forwardedFirstTokenTotalMs", "forwardedFirstTokenSamples"),
            ("cooldownTotalMs", "cooldownSamples"),
        ] {
            assert!(summary(json!({total:900,samples:2})).is_ok());
            assert!(summary(json!({total:0,samples:0})).is_ok());
            for invalid in [
                json!({total:900}),
                json!({samples:1}),
                json!({total:900,samples:3}),
                json!({total:900,samples:0}),
                json!({total:9_007_199_254_740_992u64,samples:1}),
            ] {
                assert!(summary(invalid.clone()).is_err(), "{invalid}");
            }
        }
        // Exporters older than it omit it.
        let older = summary(json!({})).unwrap();
        assert_eq!(older.groups[0].stats.first_token_total_ms, None);
        assert_eq!(older.groups[0].stats.first_token_samples, None);
        let reported = summary(json!({"firstTokenTotalMs":900,"firstTokenSamples":2})).unwrap();
        assert_eq!(reported.groups[0].stats.first_token_total_ms, Some(900));
        assert!(summary(json!({"firstTokenTotalMs":0,"firstTokenSamples":0})).is_ok());
        assert!(summary(json!({"outputTpsTokens":0,"outputTpsDurationMs":0})).is_ok());
        assert!(
            summary(
                json!({"outputTokens":101,"latencyTotalMs":3000,"latencySamples":1,
            "outputTpsTokens":100,"outputTpsDurationMs":2000})
            )
            .is_ok()
        );
        for invalid in [
            json!({"outputTpsTokens":1}),
            json!({"outputTpsDurationMs":1}),
            json!({"outputTpsTokens":1,"outputTpsDurationMs":0}),
            json!({"outputTpsTokens":0,"outputTpsDurationMs":1}),
            json!({"outputTokens":5,"latencyTotalMs":100,"outputTpsTokens":6,"outputTpsDurationMs":50}),
            json!({"outputTokens":5,"latencyTotalMs":100,"outputTpsTokens":4,"outputTpsDurationMs":101}),
            json!({"outputTokens":9_007_199_254_740_992u64,"latencyTotalMs":100,"outputTpsTokens":9_007_199_254_740_992u64,"outputTpsDurationMs":50}),
            json!({"firstTokenTotalMs":900}),
            json!({"firstTokenSamples":1}),
            json!({"firstTokenTotalMs":900,"firstTokenSamples":3}),
            json!({"firstTokenTotalMs":900,"firstTokenSamples":0}),
            json!({"firstTokenTotalMs":9_007_199_254_740_992u64,"firstTokenSamples":1}),
        ] {
            assert!(summary(invalid.clone()).is_err(), "{invalid}");
        }
    }
    #[cfg(unix)]
    #[test]
    fn stable_account_references_match_only_random_account_ids() {
        let id = uuid::Uuid::new_v4().to_string();
        let reference = account_reference("Codex", &id).unwrap();
        assert_eq!(
            Some(reference.clone()),
            account_reference("Codex", &id.to_uppercase())
        );
        assert_ne!(Some(reference.clone()), account_reference("Claude", &id));
        assert_ne!(reference, id);
        for weak in [
            "user@example.com",
            "account",
            "https://upstream.example",
            "00000000-0000-1000-8000-000000000000",
        ] {
            assert!(account_reference("Codex", weak).is_none());
        }
    }
    #[test]
    fn google_email_references_are_normalized_and_service_specific() {
        let reference = account_reference("Antigravity", "Alice@Example.com").unwrap();
        assert_eq!(reference.len(), 64);
        assert_eq!(
            Some(reference.clone()),
            account_reference("Antigravity", " alice@example.COM ")
        );
        for other in [
            "bob@example.com",
            "alice+tag@example.com",
            "a.lice@example.com",
        ] {
            assert_ne!(
                Some(reference.clone()),
                account_reference("Antigravity", other)
            );
        }
        for invalid in [
            "default",
            "fallback",
            "",
            "@example.com",
            "alice@",
            "a@@b",
            "a b@c",
            "a\n@b",
        ] {
            assert!(account_reference("Antigravity", invalid).is_none());
        }
        for service in ["Codex", "Claude"] {
            assert!(account_reference(service, "alice@example.com").is_none());
        }
    }
    #[test]
    fn full_windows_validate_ranges_shapes_and_default_consistency() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::parse("listen_port: 8787\nrequest_timeout_seconds: 30\ncodex:\n  homes: []\nclaude:\n  config_dirs: []\n").unwrap();
        let key = DataKey::new(KEY).unwrap();
        let bytes = publish(
            &config,
            &dir.path().join("missing.log"),
            &key,
            &uuid::Uuid::new_v4().to_string(),
            &[],
        )
        .unwrap();
        let summary: Summary = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(summary.schema_version, 3);
        assert_eq!(summary.windows.len(), 12);
        summary.validate().unwrap();
        let mut invalid = summary.clone();
        invalid.windows[0].minutes = 31;
        assert!(invalid.validate().is_err());
        let mut invalid = summary.clone();
        invalid.windows[1] = invalid.windows[0].clone();
        assert!(invalid.validate().is_err());
        let mut invalid = summary;
        invalid.windows[0].window_end += 60_000;
        assert!(invalid.validate().is_err());
    }
    #[tokio::test]
    async fn network_endpoint_returns_only_cached_results_and_rejects_mutations() {
        if data_key_in_subprocess(
            "data_api::tests::network_endpoint_returns_only_cached_results_and_rejects_mutations",
        ) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let reserve = std::net::TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0)).unwrap();
        let port = reserve.local_addr().unwrap().port();
        drop(reserve);
        let config=Config::parse(&format!("listen_port: 8787\nrequest_timeout_seconds: 30\nallow_external_access: true\nexternal_data:\n  port: {port}\n  token_env: {DATA_KEY_ENV}\n  trusted_lan: [10.42.0.0/24]\n")).unwrap();
        let log = dir.path().join("proxy.log");
        let row = serde_json::json!({"timestamp":(chrono::Utc::now()-chrono::Duration::minutes(2)).to_rfc3339(), "event":"model_call_finished", "model_call_id":"private-call", "service":"codex", "method":"POST", "path":"/responses", "proxy_endpoint":"http://10.9.8.7:7891", "upstream_base_url":"https://private.example/v1", "status":"200"});
        std::fs::write(&log, format!("{row}\n")).unwrap();
        let running = start(config, dir.path(), log).await.unwrap();
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let url = format!("http://127.0.0.1:{port}/v1/summary");
        assert_eq!(client.get(&url).send().await.unwrap().status(), 401);
        assert_eq!(
            client
                .get(&url)
                .bearer_auth("f".repeat(64))
                .send()
                .await
                .unwrap()
                .status(),
            401
        );
        assert_eq!(
            client
                .post(&url)
                .bearer_auth(KEY)
                .send()
                .await
                .unwrap()
                .status(),
            405
        );
        let bytes = ready_response(&client, &url, KEY)
            .await
            .bytes()
            .await
            .unwrap();
        let body: Summary = serde_json::from_slice(&bytes).unwrap();
        body.validate().unwrap();
        assert_eq!(body.groups.len(), 1);
        let reader_key = DataKey::new(KEY).unwrap();
        assert_ne!(
            body.groups[0].proxy_ref,
            reader_key.reference("proxy", "http://10.9.8.7:7891")
        );
        assert_ne!(
            body.groups[0].upstream_ref,
            reader_key.reference("upstream", "https://private.example/v1")
        );
        // HTTP identities stay stable within the running server on every OS.
        let repeated = client
            .get(&url)
            .bearer_auth(KEY)
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        let repeated: Summary = serde_json::from_slice(&repeated).unwrap();
        assert_eq!(body.groups[0].proxy_ref, repeated.groups[0].proxy_ref);
        assert_eq!(body.groups[0].upstream_ref, repeated.groups[0].upstream_ref);
        // Unix persists the private key for SSH matching. Other platforms
        // deliberately keep it in memory and reject attempts to read it back.
        #[cfg(unix)]
        assert_eq!(
            body.groups[0].proxy_ref,
            identity_key(dir.path(), false)
                .unwrap()
                .reference("proxy", "http://10.9.8.7:7891")
        );
        #[cfg(not(unix))]
        {
            assert!(identity_key(dir.path(), false).is_err());
            assert!(!dir.path().join("data-identity.key").exists());
        }
        drop(running);
    }
}

#[cfg(all(test, unix))]
mod tls_tests {
    use super::*;
    #[tokio::test]
    async fn https_requires_a_verified_certificate_and_an_independent_access_key() {
        let dir = tempfile::tempdir().unwrap();
        let key = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let cert = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
        let certfile = dir.path().join("server.pem");
        let private = dir.path().join("server.key");
        let keyfile = dir.path().join("data.key");
        crate::settings::create_private(&certfile, cert.cert.pem().as_bytes()).unwrap();
        crate::settings::create_private(&private, cert.signing_key.serialize_pem().as_bytes())
            .unwrap();
        crate::settings::create_private(&keyfile, key.as_bytes()).unwrap();
        let socket = std::net::TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0)).unwrap();
        let port = socket.local_addr().unwrap().port();
        drop(socket);
        let config=Config::parse(&format!("listen_port: 8787\nrequest_timeout_seconds: 30\nallow_external_access: true\nexternal_data:\n  port: {port}\n  token_file: '{}'\n  tls:\n    certificate: '{}'\n    private_key: '{}'\n",keyfile.display(),certfile.display(),private.display())).unwrap();
        let running = start(config.clone(), dir.path(), dir.path().join("proxy.log"))
            .await
            .unwrap();
        let url = format!("https://127.0.0.1:{port}/v1/summary");
        let untrusted = reqwest::Client::builder().no_proxy().build().unwrap();
        assert!(untrusted.get(&url).bearer_auth(key).send().await.is_err());
        let trusted = reqwest::Client::builder()
            .no_proxy()
            .add_root_certificate(
                reqwest::Certificate::from_pem(cert.cert.pem().as_bytes()).unwrap(),
            )
            .build()
            .unwrap();
        assert_eq!(trusted.get(&url).send().await.unwrap().status(), 401);
        assert_eq!(ready_response(&trusted, &url, key).await.status(), 200);
        drop(running);
        std::fs::write(&certfile, "invalid certificate").unwrap();
        assert!(
            start(config, dir.path(), dir.path().join("proxy.log"))
                .await
                .is_err()
        );
    }
}

#[cfg(all(test, unix))]
mod ssh_summary_tests {
    use super::*;
    #[test]
    fn ssh_summary_uses_both_paths_from_the_running_daemon() {
        let _guard = crate::daemon::spawn_guard();
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        std::fs::create_dir(&state).unwrap();
        let config_path = dir.path().join("custom.yaml");
        let log_path = dir.path().join("custom.log");
        let home = dir.path().join("codex");
        std::fs::create_dir(&home).unwrap();
        let base = "https://custom-private.example/v1";
        let token = "sk-synthetic-0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ-abcdef";
        std::fs::write(home.join("config.toml"), format!("[model_providers.Shared]\nname = 'Shared'\nbase_url = '{base}'\nenv_key = 'CUSTOM_SUMMARY_TEST_KEY'\n")).unwrap();
        std::fs::write(
            home.join(".env"),
            format!("CUSTOM_SUMMARY_TEST_KEY={token}\n"),
        )
        .unwrap();
        let port = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        std::fs::write(&config_path, format!("listen_port: {port}\nrequest_timeout_seconds: 30\ncodex:\n  homes: [{}]\n  routing:\n    api_key: {{Shared: none}}\nclaude:\n  config_dirs: []\n", serde_json::to_string(&home).unwrap())).unwrap();
        let row = serde_json::json!({"timestamp": (chrono::Utc::now()-chrono::Duration::minutes(2)).to_rfc3339(), "event":"model_call_finished", "model_call_id":"custom-path-call", "service":"codex", "provider":"Shared", "upstream_base_url":base, "status":"200", "received_bytes":"42"});
        std::fs::write(&log_path, format!("{row}\n")).unwrap();
        let daemon = std::thread::spawn({
            let state = state.clone();
            let config = config_path.clone();
            let log = log_path.clone();
            move || {
                tokio::runtime::Runtime::new()
                    .unwrap()
                    .block_on(crate::daemon::serve(&state, &config, &log))
            }
        });
        let (client, status) = crate::daemon::tests::wait_for_daemon(&state);
        let missing_default = read_summary(&state, ReaderServices::ALL);
        // A valid but different default configuration must not override the
        // live daemon's account mapping, nor its separate log path.
        std::fs::write(state.join("config.yaml"), "listen_port: 8787\nrequest_timeout_seconds: 30\ncodex:\n  homes: []\nclaude:\n  config_dirs: []\n").unwrap();
        let wrong_default = read_summary(&state, ReaderServices::ALL);
        std::fs::write(state.join("config.yaml"), "invalid: [").unwrap();
        let invalid_default = read_summary(&state, ReaderServices::ALL);
        client.stop().unwrap();
        daemon.join().unwrap().unwrap();
        assert_eq!(status.config_path, config_path);
        assert_eq!(status.log_path, log_path);
        let proof = coport::identity::shared_api_reference("Codex", base, token).unwrap();
        for summary in [missing_default, wrong_default, invalid_default] {
            let summary = summary.unwrap();
            summary.validate().unwrap();
            assert_eq!(summary.groups.len(), 1);
            assert_eq!(summary.groups[0].stats.requests, 1);
            assert_eq!(summary.groups[0].stats.bytes, 42);
            assert_eq!(
                summary.groups[0].account_ref.as_deref(),
                Some(proof.as_str())
            );
        }
    }

    #[test]
    fn ssh_summary_reads_the_same_dto_without_an_http_listener_or_mutation() {
        let dir = tempfile::tempdir().unwrap();
        crate::settings::create_private(&dir.path().join("config.yaml"),b"listen_port: 8787\nrequest_timeout_seconds: 30\ncodex:\n  homes: []\nclaude:\n  config_dirs: []\n").unwrap();
        let node = prepare_identity(dir.path()).unwrap();
        let before = std::fs::read(dir.path().join("data-identity.key")).unwrap();
        let summary = read_summary(dir.path(), ReaderServices::ALL).unwrap();
        summary.validate().unwrap();
        assert_eq!(summary.node_id, node);
        assert!(summary.groups.is_empty());
        assert_eq!(
            std::fs::read(dir.path().join("data-identity.key")).unwrap(),
            before
        );
        assert!(!dir.path().join("daemon.json").exists());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 3);
    }
}
