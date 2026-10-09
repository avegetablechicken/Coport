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

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub enum Service {
    Codex,
    Claude,
    Other,
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
/// Equality is supported only for random UUID account IDs (122 random bits),
/// never emails, names or URLs that a reader could recover with a dictionary.
pub fn account_reference(service: &str, account: &str) -> Option<String> {
    let id = uuid::Uuid::parse_str(account).ok()?;
    if id.get_version() != Some(uuid::Version::Random) || id.get_variant() != uuid::Variant::RFC4122
    {
        return None;
    }
    let value = format!("coport-known-account-v1\0{service}\0{id}");
    Some(
        ring::digest::digest(&ring::digest::SHA256, value.as_bytes())
            .as_ref()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
    )
}
impl Summary {
    pub fn validate(&self) -> Result<(), String> {
        let now = chrono::Utc::now().timestamp_millis();
        if !matches!(self.schema_version, 1..=4)
            || uuid::Uuid::parse_str(&self.node_id).is_err()
            || self.bucket_minutes != 1
            || self.window_end % 60_000 != 0
            || self.window_start.checked_add(30 * 60_000) != Some(self.window_end)
            || self.window_end > now + 60_000
            || self.window_end < now - 150_000
            || self.groups.len() > 259
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
    if groups.len() > 259 {
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
fn read_node_id(dir: &Path) -> io::Result<String> {
    let bytes = coport::external_access::read_limited(&dir.join("data-node-id"), false, 64)
        .map_err(io::Error::other)?;
    let id = String::from_utf8(bytes).map_err(|_| io::Error::other("Invalid node identity"))?;
    uuid::Uuid::parse_str(&id).map_err(|_| io::Error::other("Invalid node identity"))?;
    Ok(id)
}
/// Same processed DTO as HTTP. This command reads existing identity/config/log
/// files only; it does not start/stop services or open a network listener.
pub fn read_summary(dir: &Path) -> io::Result<Summary> {
    let (config_path, log) = crate::daemon::Client::discover(dir)
        .map(|(_, status)| (status.config_path, status.log_path))
        .unwrap_or_else(|| (dir.join("config.yaml"), dir.join("logs/proxy.log")));
    let config = Config::read(&config_path).map_err(io::Error::other)?;
    let key = identity_key(dir, false)?;
    let node = read_node_id(dir)?;
    let bytes = publish(&config, &log, &key, &node).map_err(io::Error::other)?;
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}

/// Flush the requested range before computing the remaining history.
pub fn stream_summary(
    dir: &Path,
    minutes: u64,
    scope: crate::traffic::TrafficScope,
    output: impl io::Write,
) -> io::Result<()> {
    let end = chrono::Utc::now().timestamp_millis() / 60_000 * 60_000;
    stream_summary_at(dir, minutes, scope, end, output)
}
fn stream_summary_at(
    dir: &Path,
    minutes: u64,
    scope: crate::traffic::TrafficScope,
    end: i64,
    mut output: impl io::Write,
) -> io::Result<()> {
    bucket_minutes(minutes).ok_or_else(|| io::Error::other("Unsupported traffic range"))?;
    let (config_path, log) = crate::daemon::Client::discover(dir)
        .map(|(_, status)| (status.config_path, status.log_path))
        .unwrap_or_else(|| (dir.join("config.yaml"), dir.join("logs/proxy.log")));
    let config = Config::read(&config_path).map_err(io::Error::other)?;
    let key = identity_key(dir, false)?;
    let node = read_node_id(dir)?;
    let first = publish_selected(&config, &log, &key, &node, minutes, scope, end)
        .map_err(io::Error::other)?;
    output.write_all(&first)?;
    output.write_all(b"\n")?;
    output.flush()?;
    let rest = publish_at(&config, &log, &key, &node, end).map_err(io::Error::other)?;
    output.write_all(&rest)?;
    output.write_all(b"\n")?;
    output.flush()
}

fn publish_selected(
    config: &Config,
    log: &Path,
    key: &DataKey,
    node: &str,
    minutes: u64,
    scope: crate::traffic::TrafficScope,
    end: i64,
) -> Result<Vec<u8>, String> {
    let bucket = bucket_minutes(minutes).ok_or("Unsupported traffic range")?;
    let identities = crate::traffic::ExportIdentities::from_config(config);
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
                    let (config,log,identity,node,shared)=(config.clone(),log.clone(),identity.clone(),node_id.clone(),shared.clone());
                    tasks.spawn(async move {
                        if let Ok(Ok(bytes))=tokio::task::spawn_blocking(move||publish(&config,&log,&identity,&node)).await {
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
) -> Result<Vec<u8>, String> {
    let end = chrono::Utc::now().timestamp_millis() / 60_000 * 60_000;
    publish_at(config, log, key, node, end)
}
fn publish_at(
    config: &Config,
    log: &Path,
    key: &DataKey,
    node: &str,
    end: i64,
) -> Result<Vec<u8>, String> {
    // Keep two previous boundaries so clock offsets and cached HTTP responses
    // can align without asking a user to refresh or changing a machine's clock.
    let identities = crate::traffic::ExportIdentities::from_config(config);
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
            for minutes in RANGES {
                for scope in [
                    crate::traffic::TrafficScope::All,
                    crate::traffic::TrafficScope::Model,
                ] {
                    let window = Window {
                        minutes,
                        scope,
                        window_start: at - minutes as i64 * 60_000,
                        window_end: at,
                        bucket_minutes: bucket_minutes(minutes).unwrap(),
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
                    };
                    if offset == 0 {
                        windows.push(window);
                    } else {
                        previous_windows.push(window);
                    }
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
    if let Ok(Some((minutes, scope))) = requested_window(&head) {
        let selected = (|| {
            let mut summary: Summary = serde_json::from_slice(&bytes).ok()?;
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
        reply(&mut socket, 200, &bytes).await;
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
                    minutes,
                    scope,
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
        let bytes = publish(&config, &log, &key, &uuid::Uuid::new_v4().to_string()).unwrap();
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
    fn full_windows_validate_ranges_shapes_and_default_consistency() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::parse("listen_port: 8787\nrequest_timeout_seconds: 30\ncodex:\n  homes: []\nclaude:\n  config_dirs: []\n").unwrap();
        let key = DataKey::new(KEY).unwrap();
        let bytes = publish(
            &config,
            &dir.path().join("missing.log"),
            &key,
            &uuid::Uuid::new_v4().to_string(),
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
        let missing_default = read_summary(&state);
        // A valid but different default configuration must not override the
        // live daemon's account mapping, nor its separate log path.
        std::fs::write(state.join("config.yaml"), "listen_port: 8787\nrequest_timeout_seconds: 30\ncodex:\n  homes: []\nclaude:\n  config_dirs: []\n").unwrap();
        let wrong_default = read_summary(&state);
        std::fs::write(state.join("config.yaml"), "invalid: [").unwrap();
        let invalid_default = read_summary(&state);
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
        let summary = read_summary(dir.path()).unwrap();
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
