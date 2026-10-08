//! Fetch only bounded, authenticated processed summaries. Labels come exclusively
//! from this device's configuration; remote values never become configuration.
#[cfg(test)]
use crate::data_api::canonical;
use crate::data_api::{Group, Service, Stats, Summary};
#[cfg(test)]
use coport::external_access::DataKey;
use coport::{config::Config, external_access::DataConfig};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    net::IpAddr,
    path::PathBuf,
    time::Duration,
};

#[derive(Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    #[default]
    Http,
    Ssh,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Source {
    pub name: String,
    #[serde(default, skip_serializing)]
    pub device_id: Option<String>,
    #[serde(default)]
    pub transport: Transport,
    pub ssh_connection: Option<crate::remote::Device>,
    pub url: String,
    pub token_env: Option<String>,
    pub token_file: Option<String>,
    pub ca_certificate: Option<String>,
    #[serde(default, skip_serializing)]
    pub ssh_device: Option<String>,
}
impl Source {
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty()
            || self.name.len() > 128
            || self.name.chars().any(char::is_control)
        {
            return Err("Provide a data source name (up to 128 bytes).".into());
        }
        if self.transport == Transport::Ssh {
            return self
                .ssh_connection
                .as_ref()
                .ok_or_else(|| "Configure the SSH data connection.".to_owned())?
                .validate();
        }
        if self.url.len() > 2048 {
            return Err("Data source URL is too long.".into());
        }
        let url = reqwest::Url::parse(&self.url).map_err(|_| "Invalid data source URL")?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || !matches!(url.path(), "" | "/")
        {
            return Err("Use an HTTP(S) origin without credentials, path or query.".into());
        }
        self.credentials().validate(0).map_err(|e| e.to_string())?;
        if self
            .ca_certificate
            .as_ref()
            .is_some_and(|path| !coport::config::expand(path).is_absolute())
        {
            return Err("CA certificate path must be absolute.".into());
        }
        Ok(())
    }
    fn credentials(&self) -> DataConfig {
        DataConfig {
            port: 1,
            token_env: self.token_env.clone(),
            token_file: self.token_file.clone(),
            trusted_lan: vec!["10.0.0.0/8".parse().unwrap()],
            tls: None,
        }
    }
}
async fn fetch(source: &Source) -> Result<Summary, String> {
    source.validate()?;
    if source.transport == Transport::Ssh {
        let summary = crate::remote::summary(
            source
                .ssh_connection
                .as_ref()
                .ok_or("Missing SSH data connection")?,
        )
        .await?;
        summary.validate()?;
        return Ok(summary);
    }
    let token = source
        .credentials()
        .load_token()
        .map_err(|_| "Cannot load this data source's private access key")?;
    let mut url = reqwest::Url::parse(&source.url).map_err(|_| "Invalid source URL")?;
    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(8));
    if url.scheme() == "http" {
        let host = url.host_str().ok_or("Missing data source host")?.to_owned();
        let port = url.port_or_known_default().ok_or("Missing source port")?;
        let addresses = tokio::net::lookup_host((host.as_str(), port))
            .await
            .map_err(|_| "Cannot resolve LAN data source")?
            .take(17)
            .collect::<Vec<_>>();
        if addresses.is_empty()
            || addresses.len() > 16
            || addresses.iter().any(|addr| !lan(addr.ip()))
        {
            return Err("HTTP data sources must resolve exclusively to private LAN or loopback addresses; use HTTPS for public sources.".into());
        }
        // Pin the validated resolution so a later DNS answer cannot redirect the key.
        builder = builder.resolve_to_addrs(&host, &addresses);
    }
    if let Some(path) = &source.ca_certificate {
        let bytes = coport::external_access::read_limited(
            &coport::config::expand(path),
            false,
            1024 * 1024,
        )
        .map_err(|_| "Cannot read data source CA certificate")?;
        let cert =
            reqwest::Certificate::from_pem(&bytes).map_err(|_| "Invalid source CA certificate")?;
        builder = builder.add_root_certificate(cert);
    }
    url.set_path("/v1/summary");
    let client = builder.build().map_err(|_| "Cannot create data client")?;
    let mut response = client
        .get(url)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|_| "Data connection failed; check connectivity and HTTPS certificate")?;
    if response.status() != reqwest::StatusCode::OK {
        return Err(
            "Data endpoint rejected the request; check its credentials and configuration.".into(),
        );
    }
    if response
        .headers()
        .get("content-type")
        .and_then(|h| h.to_str().ok())
        .is_none_or(|h| !h.starts_with("application/json"))
    {
        return Err("Unexpected data response type.".into());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "Cannot read data summary")?
    {
        if chunk.len() > (1024usize * 1024).saturating_sub(bytes.len()) {
            return Err("Data response exceeds size limit.".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    let summary: Summary =
        serde_json::from_slice(&bytes).map_err(|_| "Invalid processed summary schema")?;
    summary.validate()?;
    Ok(summary)
}
fn lan(ip: IpAddr) -> bool {
    ip.is_loopback() || matches!(ip,IpAddr::V4(ip) if ip.is_private())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MergedGroup {
    service: Service,
    proxy: String,
    upstream: String,
    anonymous_proxy: bool,
    anonymous_upstream: bool,
    stats: Stats,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceState {
    name: String,
    included: bool,
    error: Option<String>,
    traffic: Option<TrafficView>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Merged {
    pub window_start: i64,
    pub window_end: i64,
    pub sources: Vec<SourceState>,
    pub minutes: u64,
    pub scope: crate::traffic::TrafficScope,
    pub bucket_minutes: u64,
    pub traffic: TrafficView,
    pub local: TrafficView,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrafficView {
    #[serde(flatten)]
    stats: Stats,
    avg_ms: Option<u64>,
    cache_hit_rate: Option<f64>,
    credentials: Vec<CredentialView>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CredentialView {
    #[serde(skip)]
    anonymous: bool,
    service: Service,
    credential: String,
    #[serde(flatten)]
    stats: Stats,
    avg_ms: Option<u64>,
    cache_hit_rate: Option<f64>,
}
fn empty_stats(buckets: usize) -> Stats {
    Stats {
        counts: vec![0; buckets],
        error_counts: vec![0; buckets],
        token_counts: vec![0; buckets],
        ..Default::default()
    }
}
fn traffic_view(groups: &[MergedGroup], buckets: usize) -> TrafficView {
    let mut total = empty_stats(buckets);
    let mut credentials = BTreeMap::<(Service, String), (String, bool, Stats)>::new();
    for group in groups {
        add_stats(&mut total, &group.stats);
        let identity = if group.anonymous_upstream {
            "unidentified".into()
        } else {
            format!("known/{}", group.upstream)
        };
        let entry = credentials
            .entry((group.service, identity))
            .or_insert_with(|| {
                (
                    group.upstream.clone(),
                    group.anonymous_upstream,
                    empty_stats(buckets),
                )
            });
        add_stats(&mut entry.2, &group.stats);
    }
    let mut credentials: Vec<_> = credentials
        .into_iter()
        .map(
            |((service, _), (credential, anonymous, stats))| CredentialView {
                anonymous,
                service,
                credential,
                avg_ms: (stats.latency_samples > 0)
                    .then(|| stats.latency_total_ms / stats.latency_samples),
                cache_hit_rate: (stats.cache_prompt > 0)
                    .then(|| stats.cache_read as f64 / stats.cache_prompt as f64),
                stats,
            },
        )
        .collect();
    // Same ordering as Activity Traffic: known accounts by received bytes,
    // largest first; unidentified accounts follow all known accounts.
    credentials.sort_by_key(|c| (c.anonymous, std::cmp::Reverse(c.stats.bytes)));
    TrafficView {
        avg_ms: (total.latency_samples > 0).then(|| total.latency_total_ms / total.latency_samples),
        cache_hit_rate: (total.cache_prompt > 0)
            .then(|| total.cache_read as f64 / total.cache_prompt as f64),
        stats: total,
        credentials,
    }
}
fn selected_groups(
    summary: &Summary,
    minutes: u64,
    scope: crate::traffic::TrafficScope,
    end: i64,
) -> Result<&[Group], String> {
    if summary.schema_version == 1 {
        return if minutes == 30
            && scope == crate::traffic::TrafficScope::Model
            && end == summary.window_end
        {
            Ok(&summary.groups)
        } else {
            Err("Update this device's Coport to read this traffic range/category.".into())
        };
    }
    summary
        .windows
        .iter()
        .chain(&summary.previous_windows)
        .find(|w| w.minutes == minutes && w.scope == scope && w.window_end == end)
        .map(|w| w.groups.as_slice())
        .ok_or_else(|| "Missing traffic window".into())
}
fn common_end(
    summaries: &[&Summary],
    now: i64,
    minutes: u64,
    scope: crate::traffic::TrafficScope,
) -> Option<i64> {
    let Some(first) = summaries.first() else {
        return Some(now);
    };
    let candidates: BTreeSet<_> = std::iter::once(first.window_end)
        .chain(first.previous_windows.iter().map(|w| w.window_end))
        .filter(|end| *end <= now)
        .collect();
    candidates.into_iter().rev().find(|end| {
        summaries
            .iter()
            .all(|s| selected_groups(s, minutes, scope, *end).is_ok())
    })
}
async fn fetch_sources(
    sources: Vec<Source>,
    log: &std::path::Path,
) -> Result<Vec<(Source, Result<Summary, String>)>, String> {
    let mut tasks = tokio::task::JoinSet::new();
    for source in sources {
        let log = log.to_owned();
        tasks.spawn(async move {
            let started = std::time::Instant::now();
            let result = fetch(&source).await;
            crate::device_events::record(log, &source, result.is_ok(), started.elapsed()).await;
            (source, result)
        });
    }
    let mut results = Vec::new();
    while let Some(result) = tasks.join_next().await {
        results.push(result.map_err(|_| "Data reader stopped")?);
    }
    results.sort_by(|a, b| a.0.name.cmp(&b.0.name));
    Ok(results)
}
#[cfg(test)]
type KnownConfigs = (BTreeMap<String, String>, BTreeMap<String, String>);
type LabelMaps = (BTreeMap<String, String>, BTreeMap<String, String>);
#[cfg(test)]
fn known_configurations(config: &Config) -> KnownConfigs {
    let mut proxies = BTreeMap::from([("none".into(), "Direct".into())]);
    for (name, url) in &config.proxies {
        proxies
            .entry(canonical(url))
            .or_insert_with(|| name.clone());
    }
    let mut upstreams = BTreeMap::new();
    for (name, url) in [
        ("Codex account", &config.codex.base_url.account),
        ("Codex API", &config.codex.base_url.api_key),
        ("Claude", &config.claude.base_url),
    ] {
        upstreams.insert(canonical(url), name.into());
    }
    let targets = crate::traffic_identity::Identities::from_config(config, &[]).targets();
    if let Ok(serde_json::Value::Array(targets)) = serde_json::to_value(targets) {
        for target in targets {
            if let (Some(base), Some(name)) = (
                target.get("base").and_then(|v| v.as_str()),
                target.get("name").and_then(|v| v.as_str()),
            ) {
                upstreams
                    .entry(canonical(base))
                    .or_insert_with(|| name.into());
            }
        }
    }
    (proxies, upstreams)
}
#[cfg(test)]
fn labels(config: &Config, key: &DataKey) -> LabelMaps {
    let (proxies, upstreams) = known_configurations(config);
    (
        proxies
            .into_iter()
            .map(|(value, name)| (key.reference("proxy", &value), name))
            .collect(),
        upstreams
            .into_iter()
            .map(|(value, name)| (key.reference("upstream", &value), name))
            .collect(),
    )
}
fn add_stats(to: &mut Stats, from: &Stats) {
    to.requests = to.requests.saturating_add(from.requests);
    to.errors = to.errors.saturating_add(from.errors);
    to.bytes = to.bytes.saturating_add(from.bytes);
    for (to, from) in [
        (&mut to.input_tokens, from.input_tokens),
        (&mut to.output_tokens, from.output_tokens),
        (&mut to.cached_input_tokens, from.cached_input_tokens),
        (&mut to.uncached_input_tokens, from.uncached_input_tokens),
        (&mut to.cache_write_tokens, from.cache_write_tokens),
    ] {
        if let Some(value) = from {
            *to = Some(to.unwrap_or(0).saturating_add(value));
        }
    }
    to.cache_read = to.cache_read.saturating_add(from.cache_read);
    to.cache_prompt = to.cache_prompt.saturating_add(from.cache_prompt);
    to.latency_total_ms = to.latency_total_ms.saturating_add(from.latency_total_ms);
    to.latency_samples = to.latency_samples.saturating_add(from.latency_samples);
    for (to, from) in [
        (&mut to.counts, &from.counts),
        (&mut to.error_counts, &from.error_counts),
        (&mut to.token_counts, &from.token_counts),
    ] {
        if to.is_empty() {
            to.resize(from.len(), 0);
        }
        for (to, from) in to.iter_mut().zip(from) {
            *to = to.saturating_add(*from);
        }
    }
}
fn add_groups(
    merged: &mut BTreeMap<(Service, String, String), MergedGroup>,
    maps: &LabelMaps,
    node_id: &str,
    source_groups: &[Group],
    accounts: &BTreeMap<String, String>,
) {
    let (proxies, upstreams) = maps;
    for Group {
        service,
        proxy_ref,
        upstream_ref,
        stats,
        account_ref,
    } in source_groups
    {
        let proxy = proxies.get(proxy_ref);
        let upstream = account_ref
            .as_ref()
            .and_then(|r| accounts.get(r))
            .or_else(|| upstreams.get(upstream_ref));
        // Unknown references remain distinct within each source; displayed labels
        // disclose no names/addresses and never update this machine's configuration.
        let identity = (
            *service,
            proxy
                .cloned()
                .unwrap_or_else(|| format!("{node_id}/{proxy_ref}")),
            upstream
                .cloned()
                .unwrap_or_else(|| format!("{node_id}/{upstream_ref}")),
        );
        let group = merged.entry(identity).or_insert_with(|| MergedGroup {
            service: *service,
            proxy: proxy
                .cloned()
                .unwrap_or_else(|| "Unidentified proxy".into()),
            upstream: upstream.cloned().unwrap_or_else(|| "Unidentified".into()),
            anonymous_proxy: proxy.is_none(),
            anonymous_upstream: upstream.is_none(),
            stats: Stats::default(),
        });
        add_stats(&mut group.stats, stats);
    }
}
pub async fn merge(
    sources: Vec<Source>,
    config: Config,
    log: PathBuf,
    minutes: u64,
    scope: crate::traffic::TrafficScope,
) -> Result<Merged, String> {
    let labels = config.traffic_credential_labels().await;
    merge_with_labels(sources, config, log, minutes, scope, labels).await
}
pub async fn merge_with_labels(
    sources: Vec<Source>,
    config: Config,
    log: PathBuf,
    minutes: u64,
    scope: crate::traffic::TrafficScope,
    credential_labels: BTreeMap<(String, String), String>,
) -> Result<Merged, String> {
    crate::data_api::bucket_minutes(minutes).ok_or("Unsupported traffic range")?;
    if sources.len() > crate::devices::LIMIT {
        return Err("At most 32 data sources are supported.".into());
    }
    let context = std::sync::Arc::new(prepare_merge(config, log, credential_labels).await?);
    let (fetched, end) = align_sources(&context, sources, &[(minutes, scope)]).await?;
    let snapshot = load_snapshot(&context.log, end).await?;
    merge_fetched(context, &fetched, minutes, scope, end, snapshot).await
}

pub async fn merge_views_with_labels(
    sources: Vec<Source>,
    config: Config,
    log: PathBuf,
    credential_labels: BTreeMap<(String, String), String>,
) -> Result<Vec<Merged>, String> {
    if sources.len() > crate::devices::LIMIT {
        return Err("At most 32 data sources are supported.".into());
    }
    let context = std::sync::Arc::new(prepare_merge(config, log, credential_labels).await?);
    let selections: Vec<_> = crate::data_api::RANGES
        .into_iter()
        .flat_map(|minutes| {
            [
                crate::traffic::TrafficScope::Model,
                crate::traffic::TrafficScope::All,
            ]
            .into_iter()
            .map(move |scope| (minutes, scope))
        })
        .collect();
    let (fetched, end) = align_sources(&context, sources, &selections).await?;
    let snapshot = load_snapshot(&context.log, end).await?;
    let mut views = Vec::new();
    for (minutes, scope) in selections {
        views.push(
            merge_fetched(
                context.clone(),
                &fetched,
                minutes,
                scope,
                end,
                snapshot.clone(),
            )
            .await?,
        );
    }
    Ok(views)
}
struct MergeContext {
    config: Config,
    log: PathBuf,
    accounts: BTreeMap<String, String>,
    credential_labels: BTreeMap<(String, String), String>,
    local_id: String,
    at: i64,
}
async fn prepare_merge(
    config: Config,
    log: PathBuf,
    credential_labels: BTreeMap<(String, String), String>,
) -> Result<MergeContext, String> {
    let evidence_path = log.clone();
    let credential_labels = tokio::task::spawn_blocking(move || {
        crate::traffic::observed_identity_labels(&evidence_path, credential_labels)
    })
    .await
    .map_err(|_| "Cannot read local identity evidence")??;
    let mut accounts: BTreeMap<String, String> = credential_labels
        .clone()
        .into_iter()
        .filter_map(|((service, id), label)| {
            crate::data_api::account_reference(&service, &id).map(|r| (r, label))
        })
        .collect();
    let references = config.traffic_provider_references().await;
    let identities = crate::traffic_identity::Identities::from_config(&config, &[]);
    for provider in &references {
        if let Some(label) = identities.provider_label(provider) {
            accounts.entry(provider.reference.clone()).or_insert(label);
        }
    }
    Ok(MergeContext {
        config,
        log,
        accounts,
        credential_labels,
        local_id: std::fs::read_to_string(crate::settings::app_dir().join("data-node-id"))
            .unwrap_or_else(|_| "local".into()),
        at: chrono::Utc::now().timestamp_millis() / 60_000 * 60_000,
    })
}
type Fetched = Vec<(Source, Result<Summary, String>)>;
async fn load_snapshot(
    log: &std::path::Path,
    end: i64,
) -> Result<std::sync::Arc<crate::traffic::Snapshot>, String> {
    let log = log.to_owned();
    tokio::task::spawn_blocking(move || {
        crate::traffic::Snapshot::load(&log, end).map(std::sync::Arc::new)
    })
    .await
    .map_err(|_| "Cannot read local traffic snapshot")?
}
async fn align_sources(
    context: &MergeContext,
    sources: Vec<Source>,
    selections: &[(u64, crate::traffic::TrafficScope)],
) -> Result<(Fetched, i64), String> {
    let mut fetched = fetch_sources(sources, &context.log).await?;
    for attempt in 0..3 {
        let mut seen = BTreeSet::from([context.local_id.clone()]);
        let valid: Vec<_> = fetched
            .iter()
            .filter_map(|(_, r)| r.as_ref().ok())
            .filter(|s| seen.insert(s.node_id.clone()))
            .filter(|s| {
                selections.iter().any(|(minutes, scope)| {
                    selected_groups(s, *minutes, *scope, s.window_end).is_ok()
                })
            })
            .collect();
        let end = valid.first().map_or(Some(context.at), |first| {
            let candidates: BTreeSet<_> = std::iter::once(first.window_end)
                .chain(first.previous_windows.iter().map(|w| w.window_end))
                .filter(|at| *at <= context.at)
                .collect();
            candidates.into_iter().rev().find(|at| {
                selections.iter().all(|(minutes, scope)| {
                    let supported: Vec<_> = valid
                        .iter()
                        .copied()
                        .filter(|s| selected_groups(s, *minutes, *scope, s.window_end).is_ok())
                        .collect();
                    common_end(&supported, *at, *minutes, *scope) == Some(*at)
                })
            })
        });
        if let Some(end) = end {
            return Ok((fetched, end));
        }
        if attempt < 2 {
            tokio::time::sleep(Duration::from_millis(250 * (attempt + 1))).await;
            let sources = fetched.into_iter().map(|(s, _)| s).collect();
            fetched = fetch_sources(sources, &context.log).await?;
        }
    }
    Err("TRAFFIC_UPDATING".into())
}
async fn merge_fetched(
    context: std::sync::Arc<MergeContext>,
    fetched: &Fetched,
    minutes: u64,
    scope: crate::traffic::TrafficScope,
    end: i64,
    snapshot: std::sync::Arc<crate::traffic::Snapshot>,
) -> Result<Merged, String> {
    let bucket = crate::data_api::bucket_minutes(minutes).ok_or("Unsupported traffic range")?;
    let buckets = (minutes / bucket) as usize;
    let local_context = context.clone();
    let local = tokio::task::spawn_blocking(move || {
        crate::traffic::local_named_groups(
            crate::traffic::ReadSource::Snapshot(&snapshot),
            &local_context.config,
            &local_context.credential_labels,
            end,
            minutes,
            scope,
        )
    })
    .await
    .map_err(|_| "Cannot read local processed statistics")??;
    let mut groups = BTreeMap::new();
    for (service, credential, stats) in local {
        let anonymous = credential == crate::traffic_identity::UNIDENTIFIED;
        groups.insert(
            (service, "local".into(), credential.clone()),
            MergedGroup {
                service,
                proxy: "Unidentified proxy".into(),
                upstream: credential,
                anonymous_proxy: true,
                anonymous_upstream: anonymous,
                stats,
            },
        );
    }
    let local_view = traffic_view(
        &groups
            .values()
            .map(|g| MergedGroup {
                service: g.service,
                proxy: g.proxy.clone(),
                upstream: g.upstream.clone(),
                anonymous_proxy: g.anonymous_proxy,
                anonymous_upstream: g.anonymous_upstream,
                stats: g.stats.clone(),
            })
            .collect::<Vec<_>>(),
        buckets,
    );
    let mut seen = BTreeSet::from([context.local_id.clone()]);
    let mut states = Vec::new();
    for (source, result) in fetched {
        let name = source.name.clone();
        let mut source_traffic = None;
        let (included, error) = match result {
            Ok(summary) if !seen.insert(summary.node_id.clone()) => {
                (false, Some("Duplicate device; already counted.".into()))
            }
            Ok(summary) => match selected_groups(summary, minutes, scope, end) {
                Ok(selected) => {
                    let mut source_groups = BTreeMap::new();
                    add_groups(
                        &mut source_groups,
                        &(BTreeMap::new(), BTreeMap::new()),
                        &summary.node_id,
                        selected,
                        &context.accounts,
                    );
                    source_traffic = Some(traffic_view(
                        &source_groups.into_values().collect::<Vec<_>>(),
                        buckets,
                    ));
                    add_groups(
                        &mut groups,
                        &(BTreeMap::new(), BTreeMap::new()),
                        &summary.node_id,
                        selected,
                        &context.accounts,
                    );
                    (true, None)
                }
                Err(error) => (false, Some(error.clone())),
            },
            Err(error) => (false, Some(error.clone())),
        };
        states.push(SourceState {
            name,
            included,
            error,
            traffic: source_traffic,
        });
    }
    states.sort_by(|a, b| a.name.cmp(&b.name));
    let groups: Vec<_> = groups.into_values().collect();
    let traffic = traffic_view(&groups, buckets);
    Ok(Merged {
        window_start: end - minutes as i64 * 60_000,
        window_end: end,
        sources: states,
        minutes,
        scope,
        bucket_minutes: bucket,
        traffic,
        local: local_view,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{DATA_KEY as KEY, DATA_KEY_ENV, data_key_in_subprocess};
    fn sample(key: &DataKey, proxy: &str, upstream: &str) -> Summary {
        let mut counts = vec![0; 30];
        counts[0] = 1;
        Summary {
            schema_version: 1,
            node_id: uuid::Uuid::new_v4().to_string(),
            window_start: 0,
            window_end: 30 * 60_000,
            bucket_minutes: 1,
            windows: Vec::new(),
            previous_windows: Vec::new(),
            groups: vec![Group {
                service: Service::Codex,
                proxy_ref: key.reference("proxy", &canonical(proxy)),
                upstream_ref: key.reference("upstream", &canonical(upstream)),
                account_ref: None,
                stats: Stats {
                    requests: 1,
                    counts,
                    error_counts: vec![0; 30],
                    token_counts: vec![0; 30],
                    ..Default::default()
                },
            }],
        }
    }
    #[tokio::test]
    async fn all_views_share_alignment_retries_and_omit_unused_groups() {
        if data_key_in_subprocess(
            "data_client::tests::all_views_share_alignment_retries_and_omit_unused_groups",
        ) {
            return;
        }
        alignment_fixture(true).await;
    }
    #[tokio::test]
    async fn failed_alignment_stops_after_three_rounds_for_all_views() {
        if data_key_in_subprocess(
            "data_client::tests::failed_alignment_stops_after_three_rounds_for_all_views",
        ) {
            return;
        }
        alignment_fixture(false).await;
    }
    async fn alignment_fixture(recover: bool) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("proxy.log");
        std::fs::write(&log, "").unwrap();
        let now = chrono::Utc::now().timestamp_millis() / 60_000 * 60_000;
        let aligned = now - 60_000;
        let mut tasks = Vec::new();
        let mut counters = Vec::new();
        let mut sources = Vec::new();
        for peer in 0..2 {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            counters.push(count.clone());
            tasks.push(tokio::spawn(async move {
                let node = uuid::Uuid::new_v4().to_string();
                for attempt in 0..3 {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    let mut header = Vec::new();
                    while !header.ends_with(b"\r\n\r\n") { header.push(socket.read_u8().await.unwrap()); }
                    count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let end = if peer == 1 && (attempt < 2 || !recover) { now } else { aligned };
                    let windows = crate::data_api::RANGES.into_iter().flat_map(|minutes|
                        [crate::traffic::TrafficScope::All, crate::traffic::TrafficScope::Model].into_iter().map(move |scope|
                            crate::data_api::Window { minutes, scope, window_start: end-minutes as i64*60_000,
                                window_end: end, bucket_minutes: crate::data_api::bucket_minutes(minutes).unwrap(), groups: Vec::new() })).collect();
                    let summary = Summary { schema_version: 2, node_id: node.clone(), window_start: end-30*60_000,
                        window_end: end, bucket_minutes: 1, groups: Vec::new(), windows, previous_windows: Vec::new() };
                    summary.validate().unwrap();
                    let body = serde_json::to_vec(&summary).unwrap();
                    socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes()).await.unwrap();
                    socket.write_all(&body).await.unwrap();
                }
            }));
            sources.push(Source {
                name: format!("peer-{peer}"),
                device_id: None,
                transport: Transport::Http,
                ssh_connection: None,
                url,
                token_env: Some(DATA_KEY_ENV.into()),
                token_file: None,
                ca_certificate: None,
                ssh_device: None,
            });
        }
        let config = Config::parse("listen_port: 8787\nrequest_timeout_seconds: 30\ncodex:\n  homes: []\nclaude:\n  config_dirs: []\n").unwrap();
        let before = crate::traffic::live_scans(&log);
        let result = merge_views_with_labels(sources, config, log.clone(), BTreeMap::new()).await;
        if recover {
            let views = result.unwrap();
            assert_eq!(views.len(), 12);
            for view in views {
                assert_eq!(view.window_end, aligned);
                assert!(view.sources.iter().all(|source| source.included));
                let json = serde_json::to_value(view).unwrap();
                assert!(json.get("groups").is_none());
                assert!(json.get("traffic").is_some());
                assert!(json.get("local").is_some());
                assert!(json.get("sources").is_some());
            }
            // One identity-evidence scan plus two shared local scope scans.
            assert_eq!(crate::traffic::live_scans(&log) - before, 3);
        } else {
            assert!(matches!(result, Err(error) if error == "TRAFFIC_UPDATING"));
            assert_eq!(crate::traffic::live_scans(&log) - before, 1);
        }
        for counter in counters {
            assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 3);
        }
        for task in tasks {
            task.await.unwrap();
        }
    }

    #[tokio::test]
    async fn data_readers_without_an_explicit_ssh_link_cannot_resolve_private_configuration() {
        let source = Source {
            name: "reader".into(),
            transport: Transport::Http,
            ssh_connection: None,
            url: "http://10.42.0.196:8788".into(),
            token_env: Some("TEST_KEY".into()),
            token_file: None,
            ca_certificate: None,
            device_id: None,
            ssh_device: None,
        };
        let config=Config::parse("listen_port: 8787\nrequest_timeout_seconds: 30\nproxies:\n  known: http://10.1.1.1:7891\n").unwrap();
        let identity = DataKey::new(KEY).unwrap();
        let summary = sample(
            &identity,
            "http://10.1.1.1:7891",
            &config.codex.base_url.api_key,
        );
        let _ = source;
        let maps = (BTreeMap::new(), BTreeMap::new());
        let mut groups = BTreeMap::new();
        add_groups(
            &mut groups,
            &maps,
            &summary.node_id,
            &summary.groups,
            &BTreeMap::new(),
        );
        let group = groups.into_values().next().unwrap();
        assert!(group.anonymous_proxy && group.anonymous_upstream);
    }
    #[test]
    fn known_accounts_merge_across_private_namespaces_with_weighted_metrics() {
        let id = uuid::Uuid::new_v4().to_string();
        let reference = crate::data_api::account_reference("Codex", &id).unwrap();
        let accounts = BTreeMap::from([(reference.clone(), "My known account".into())]);
        let mut first = sample(
            &DataKey::new(KEY).unwrap(),
            "http://10.1.1.1",
            "https://first.example",
        );
        let mut second = sample(
            &DataKey::new(&"f".repeat(64)).unwrap(),
            "http://10.2.2.2",
            "https://second.example",
        );
        first.groups[0].account_ref = Some(reference.clone());
        second.groups[0].account_ref = Some(reference);
        first.groups[0].stats.latency_total_ms = 100;
        first.groups[0].stats.latency_samples = 1;
        first.groups[0].stats.cache_read = 2;
        first.groups[0].stats.cache_prompt = 10;
        second.groups[0].stats.requests = 3;
        second.groups[0].stats.counts[0] = 3;
        second.groups[0].stats.latency_total_ms = 900;
        second.groups[0].stats.latency_samples = 3;
        second.groups[0].stats.cache_read = 4;
        second.groups[0].stats.cache_prompt = 10;
        let maps = (BTreeMap::new(), BTreeMap::new());
        let mut groups = BTreeMap::new();
        for summary in [&first, &second] {
            add_groups(
                &mut groups,
                &maps,
                &summary.node_id,
                &summary.groups,
                &accounts,
            );
        }
        assert!(
            groups
                .values()
                .all(|g| g.anonymous_proxy && !g.anonymous_upstream)
        );
        let view = traffic_view(&groups.into_values().collect::<Vec<_>>(), 30);
        assert_eq!(view.credentials.len(), 1);
        assert_eq!(view.credentials[0].credential, "My known account");
        assert_eq!(view.stats.requests, 4);
        assert_eq!(view.avg_ms, Some(250));
        assert_eq!(view.cache_hit_rate, Some(0.3));
        let mut unknown = BTreeMap::new();
        add_groups(
            &mut unknown,
            &maps,
            &first.node_id,
            &first.groups,
            &BTreeMap::new(),
        );
        assert!(unknown.values().all(|g| g.anonymous_upstream));
    }

    #[test]
    fn minute_boundaries_and_different_peer_clocks_align_automatically() {
        let key = DataKey::new(KEY).unwrap();
        let now = chrono::Utc::now().timestamp_millis() / 60_000 * 60_000;
        let mut newer = sample(&key, "none", "https://known.example");
        let mut older = newer.clone();
        newer.schema_version = 3;
        newer.window_end = now;
        older.schema_version = 3;
        older.window_end = now - 60_000;
        for summary in [&mut newer, &mut older] {
            summary.windows = vec![crate::data_api::Window {
                minutes: 30,
                scope: crate::traffic::TrafficScope::Model,
                window_start: summary.window_end - 30 * 60_000,
                window_end: summary.window_end,
                bucket_minutes: 1,
                groups: summary.groups.clone(),
            }];
            summary.previous_windows = vec![crate::data_api::Window {
                minutes: 30,
                scope: crate::traffic::TrafficScope::Model,
                window_start: summary.window_end - 31 * 60_000,
                window_end: summary.window_end - 60_000,
                bucket_minutes: 1,
                groups: summary.groups.clone(),
            }];
        }
        assert_eq!(
            common_end(
                &[&newer, &older],
                now,
                30,
                crate::traffic::TrafficScope::Model
            ),
            Some(now - 60_000)
        );
        assert!(
            selected_groups(
                &newer,
                30,
                crate::traffic::TrafficScope::Model,
                now - 60_000
            )
            .is_ok()
        );
        assert_eq!(
            common_end(
                &[&newer],
                now - 60_000,
                30,
                crate::traffic::TrafficScope::Model
            ),
            Some(now - 60_000)
        );
    }
    #[test]
    fn proxy_and_endpoint_splits_do_not_invent_multiple_unknown_accounts() {
        let groups: Vec<_> = (0..4)
            .map(|i| MergedGroup {
                service: Service::Codex,
                proxy: format!("private-proxy-{i}"),
                upstream: "Unidentified".into(),
                anonymous_proxy: true,
                anonymous_upstream: true,
                stats: Stats {
                    requests: 1,
                    counts: vec![1],
                    error_counts: vec![0],
                    token_counts: vec![0],
                    ..Default::default()
                },
            })
            .collect();
        let view = traffic_view(&groups, 1);
        assert_eq!(view.credentials.len(), 1);
        assert_eq!(view.credentials[0].credential, "Unidentified");
        assert_eq!(view.credentials[0].stats.requests, 4);
        assert_eq!(view.stats.requests, 4);
    }
    #[tokio::test]
    async fn this_device_preserves_activity_names_for_legacy_and_logged_identities() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("proxy.log");
        let row = serde_json::json!({"timestamp":(chrono::Utc::now()-chrono::Duration::minutes(3)).to_rfc3339(),"event":"model_call_finished","service":"codex","model_call_id":"one","account_id":"legacy-non-uuid","account_label":"Known local account","status":"200","received_bytes":"17"});
        std::fs::write(&log, format!("{row}\n")).unwrap();
        let config = Config::parse(&format!("listen_port: 8787\nrequest_timeout_seconds: 30\ncodex:\n  homes: [{}]\n  routing:\n    account: {{'Known local account': none}}\nclaude:\n  config_dirs: []\n", serde_json::to_string(dir.path()).unwrap())).unwrap();
        let view = merge(vec![], config, log, 30, crate::traffic::TrafficScope::Model)
            .await
            .unwrap();
        assert_eq!(view.local.credentials.len(), 1);
        assert_eq!(view.local.credentials[0].credential, "Known local account");
        assert!(!view.local.credentials[0].anonymous);
        assert_eq!(view.traffic.stats.requests, 1);
    }
    #[test]
    fn credential_order_matches_activity_traffic_with_unknown_accounts_last() {
        let group = |service, name: &str, bytes, anonymous| MergedGroup {
            service,
            proxy: "Unidentified proxy".into(),
            upstream: name.into(),
            anonymous_proxy: true,
            anonymous_upstream: anonymous,
            stats: Stats {
                bytes,
                ..empty_stats(30)
            },
        };
        let views = traffic_view(
            &[
                group(Service::Codex, "A", 100, false),
                group(Service::Claude, "B", 500, false),
                group(Service::Codex, "Unidentified", 10000, true),
                group(Service::Claude, "C", 250, false),
            ],
            30,
        );
        assert_eq!(
            views
                .credentials
                .iter()
                .map(|c| c.credential.as_str())
                .collect::<Vec<_>>(),
            vec!["B", "C", "A", "Unidentified"]
        );
    }
    #[tokio::test]
    async fn same_provider_name_with_different_upstreams_matches_local_traffic_identities() {
        let dir = tempfile::tempdir().unwrap();
        let token = "sk-synthetic-0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ-abcdef";
        let bases = [
            "https://first-private.example/v1",
            "https://second-private.example/v1",
        ];
        let mut homes = Vec::new();
        let mut rows = String::new();
        for (i, base) in bases.iter().enumerate() {
            let home = dir.path().join(format!("home-{i}"));
            std::fs::create_dir(&home).unwrap();
            std::fs::write(home.join("config.toml"), format!("[model_providers.Shared]\nname = 'Shared'\nbase_url = '{base}'\nenv_key = 'DUPLICATE_PROVIDER_TEST_KEY'\n")).unwrap();
            std::fs::write(
                home.join(".env"),
                format!("DUPLICATE_PROVIDER_TEST_KEY={token}\n"),
            )
            .unwrap();
            homes.push(home);
            let row = serde_json::json!({"timestamp":(chrono::Utc::now()-chrono::Duration::minutes(2)).to_rfc3339(), "event":"model_call_finished", "model_call_id": format!("call-{i}"), "service":"codex", "provider":"Shared", "upstream_base_url":base, "status":"200", "received_bytes":format!("{}", (i+1)*11)});
            rows.push_str(&format!("{row}\n"));
        }
        let config = Config::parse(&format!("listen_port: 8787\nrequest_timeout_seconds: 30\ncodex:\n  homes: {}\n  routing:\n    api_key: {{Shared: none}}\nclaude:\n  config_dirs: []\n", serde_json::to_string(&homes).unwrap())).unwrap();
        let log = dir.path().join("proxy.log");
        std::fs::write(&log, rows).unwrap();
        let references = config.traffic_provider_references().await;
        assert_eq!(references.len(), 2);
        assert_ne!(references[0].reference, references[1].reference);
        let body = crate::data_api::publish(
            &config,
            &log,
            &DataKey::new(KEY).unwrap(),
            &uuid::Uuid::new_v4().to_string(),
        )
        .unwrap();
        let encoded = std::str::from_utf8(&body).unwrap();
        for secret in ["Shared", bases[0], bases[1], token] {
            assert!(!encoded.contains(secret));
        }
        let summary: Summary = serde_json::from_slice(&body).unwrap();
        assert_eq!(summary.groups.len(), 2);
        let labels = config.traffic_credential_labels().await;
        let context = prepare_merge(config.clone(), log.clone(), labels.clone())
            .await
            .unwrap();
        let local = crate::traffic::local_named_groups(
            crate::traffic::ReadSource::Path(&log),
            &config,
            &labels,
            context.at,
            30,
            crate::traffic::TrafficScope::Model,
        )
        .unwrap();
        assert_eq!(local.len(), 2);
        let mut groups = BTreeMap::new();
        add_groups(
            &mut groups,
            &(BTreeMap::new(), BTreeMap::new()),
            &summary.node_id,
            &summary.groups,
            &context.accounts,
        );
        let remote = traffic_view(&groups.into_values().collect::<Vec<_>>(), 30);
        assert_eq!(remote.credentials.len(), 2);
        for (_, label, stats) in local {
            assert!(label.starts_with("Shared (https://"));
            let matched = remote
                .credentials
                .iter()
                .find(|g| g.credential == label)
                .unwrap();
            assert!(!matched.anonymous);
            assert_eq!(matched.stats.requests, stats.requests);
            assert_eq!(matched.stats.bytes, stats.bytes);
        }
    }

    #[tokio::test]
    async fn sharecoder_provider_matches_without_exporting_names_urls_or_keys() {
        let dir = tempfile::tempdir().unwrap();
        let token = "sk-synthetic-0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ-abcdef";
        let base = "https://private.example/v1";
        std::fs::write(dir.path().join("config.toml"), format!("[model_providers.ShareCoder]\nname = \"ShareCoder\"\nbase_url = \"{base}\"\nenv_key = \"SHARECODER_TEST_KEY\"\n")).unwrap();
        std::fs::write(
            dir.path().join(".env"),
            format!("SHARECODER_TEST_KEY={token}\n"),
        )
        .unwrap();
        let config = Config::parse(&format!("listen_port: 8787\nrequest_timeout_seconds: 30\ncodex:\n  homes: [{}]\n  routing:\n    api_key: {{ShareCoder: none}}\nclaude:\n  config_dirs: []\n",serde_json::to_string(dir.path()).unwrap())).unwrap();
        let refs = config.traffic_provider_references().await;
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].name, "ShareCoder");
        let log = dir.path().join("proxy.log");
        let row = serde_json::json!({"timestamp":(chrono::Utc::now()-chrono::Duration::minutes(3)).to_rfc3339(),"event":"model_call_finished","model_call_id":"synthetic","service":"codex","provider":"ShareCoder","upstream_base_url":base,"status":"200"});
        // Sender resolves both a renamed provider and its old base URL to
        // the current configuration before exporting a single safe group.
        let mut renamed = row.clone();
        renamed["model_call_id"] = "renamed".into();
        renamed["provider"] = "Previous private name".into();
        let mut changed_base = row.clone();
        changed_base["model_call_id"] = "changed-base".into();
        changed_base["upstream_base_url"] = "https://previous-private.example/v1".into();
        let mut unknown = row.clone();
        unknown["model_call_id"] = "unknown".into();
        unknown["provider"] = "Removed private name".into();
        unknown["upstream_base_url"] = "https://removed-private.example/v1".into();
        unknown["credential_ref"] = refs[0].reference.clone().into();
        std::fs::write(
            &log,
            format!("{row}\n{renamed}\n{changed_base}\n{unknown}\n"),
        )
        .unwrap();
        let bytes = crate::data_api::publish(
            &config,
            &log,
            &DataKey::new(KEY).unwrap(),
            &uuid::Uuid::new_v4().to_string(),
        )
        .unwrap();
        let encoded = std::str::from_utf8(&bytes).unwrap();
        for secret in [
            "ShareCoder",
            "private.example",
            "Previous private name",
            "Removed private name",
            "previous-private.example",
            "removed-private.example",
            token,
        ] {
            assert!(!encoded.contains(secret));
        }
        let summary: Summary = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(summary.groups.len(), 2);
        let mapped = summary
            .groups
            .iter()
            .find(|g| g.account_ref.is_some())
            .unwrap();
        assert_eq!(
            mapped.account_ref.as_deref(),
            Some(refs[0].reference.as_str())
        );
        assert_eq!(mapped.stats.requests, 3);
        assert_eq!(
            summary
                .groups
                .iter()
                .find(|g| g.account_ref.is_none())
                .unwrap()
                .stats
                .requests,
            1
        );
        let maps = (BTreeMap::new(), BTreeMap::new());
        let known = BTreeMap::from([(refs[0].reference.clone(), "ShareCoder".into())]);
        let mut groups = BTreeMap::new();
        add_groups(
            &mut groups,
            &maps,
            &summary.node_id,
            &summary.groups,
            &known,
        );
        assert!(
            groups
                .values()
                .any(|g| g.upstream == "ShareCoder" && g.stats.requests == 3)
        );
        let mut unknown = BTreeMap::new();
        add_groups(
            &mut unknown,
            &maps,
            &summary.node_id,
            &summary.groups,
            &BTreeMap::new(),
        );
        assert!(unknown.values().all(|g| g.anonymous_upstream));
    }
    #[tokio::test]
    async fn one_http_snapshot_builds_all_views_and_matches_known_accounts_without_disclosing_identity()
     {
        if data_key_in_subprocess(
            "data_client::tests::one_http_snapshot_builds_all_views_and_matches_known_accounts_without_disclosing_identity",
        ) {
            return;
        }
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("codex");
        std::fs::create_dir(&home).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let claims = "eyJlbWFpbCI6Imtub3duQGV4YW1wbGUuaW52YWxpZCJ9";
        std::fs::write(home.join("auth.json"), serde_json::json!({"tokens":{"account_id":id,"access_token":"synthetic-token","id_token":format!("x.{claims}.x")}}).to_string()).unwrap();
        let config = Config::parse(&format!("listen_port: 8787\nrequest_timeout_seconds: 30\ncodex:\n  homes: [{}]\n  routing:\n    account: {{'known@example.invalid': none}}\nclaude:\n  config_dirs: []\n", serde_json::to_string(&home).unwrap())).unwrap();
        let log = dir.path().join("proxy.log");
        let timestamp = (chrono::Utc::now() - chrono::Duration::minutes(2)).to_rfc3339();
        let row = serde_json::json!({"timestamp":timestamp,"event":"model_call_finished","model_call_id":"test-call","account_id":id,"service":"codex","status":"200","input_tokens":"100","output_tokens":"50","cached_input_tokens":"30","duration_ms":"200","received_bytes":"123","upstream_base_url":"https://private.example/v1","proxy_endpoint":"http://10.9.8.7:7891"});
        std::fs::write(&log, format!("{row}\n")).unwrap();
        let body = crate::data_api::publish(
            &config,
            &log,
            &DataKey::new(&"e".repeat(64)).unwrap(),
            &uuid::Uuid::new_v4().to_string(),
        )
        .unwrap();
        let encoded = std::str::from_utf8(&body).unwrap();
        for secret in [
            &id,
            "known@example.invalid",
            "private.example",
            "10.9.8.7",
            "synthetic-token",
        ] {
            assert!(!encoded.contains(secret));
        }
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                head.push(socket.read_u8().await.unwrap());
                assert!(head.len() < 8192);
            }
            let header = String::from_utf8(head).unwrap();
            assert!(header.starts_with("GET /v1/summary HTTP/1.1"));
            assert!(header.contains(&format!("Bearer {KEY}")));
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes()).await.unwrap();
            socket.write_all(&body).await.unwrap();
        });
        let source = Source {
            name: "Peer".into(),
            transport: Transport::Http,
            ssh_connection: None,
            url: format!("http://127.0.0.1:{port}"),
            token_env: Some(DATA_KEY_ENV.into()),
            token_file: None,
            ca_certificate: None,
            device_id: None,
            ssh_device: None,
        };
        let labels = config.traffic_credential_labels().await;
        let views = merge_views_with_labels(vec![source], config, log.clone(), labels)
            .await
            .unwrap();
        let audit: Vec<serde_json::Value> = std::fs::read_to_string(&log)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .filter(|entry: &serde_json::Value| entry["event"] == "device_statistics_succeeded")
            .collect();
        assert_eq!(
            audit.len(),
            1,
            "All twelve views must share one query event"
        );
        assert_eq!(audit[0]["query_count"], 1);
        assert_eq!(audit[0]["transport"], "http");
        let encoded_audit = audit[0].to_string();
        for secret in [
            KEY,
            "private.example",
            "127.0.0.1",
            "Bearer",
            "known@example.invalid",
        ] {
            assert!(!encoded_audit.contains(secret));
        }
        assert_eq!(views.len(), 12);
        assert!(views.iter().all(|v| v.sources[0].included));
        let result = views
            .iter()
            .find(|v| v.minutes == 360 && v.scope == crate::traffic::TrafficScope::Model)
            .unwrap();
        server.await.unwrap();
        assert!(result.sources[0].included, "{:?}", result.sources[0].error);
        assert_eq!(result.bucket_minutes, 15);
        assert_eq!(result.local.stats.counts.len(), 24);
        assert_eq!(result.local.stats.requests, 1);
        let remote = result.sources[0].traffic.as_ref().unwrap();
        assert_eq!(remote.stats.requests, 1);
        assert_eq!(remote.avg_ms, Some(200));
        assert_eq!(remote.cache_hit_rate, Some(0.3));
        assert_eq!(remote.credentials[0].credential, "known@example.invalid");
        assert_eq!(result.traffic.stats.requests, 2);
        assert_eq!(result.traffic.credentials.len(), 1);
        assert_eq!(result.traffic.stats.input_tokens, Some(200));
    }
    #[test]
    fn unknown_proxy_and_upstream_remain_anonymous_and_do_not_change_config() {
        let config=Config::parse("listen_port: 8787\nrequest_timeout_seconds: 30\nproxies:\n  known: http://10.1.1.1:7891\n").unwrap();
        let key = DataKey::new(KEY).unwrap();
        let summary = sample(&key, "http://10.99.1.1:7891", "https://unknown.example/v1");
        let mut groups = BTreeMap::new();
        add_groups(
            &mut groups,
            &labels(&config, &key),
            &summary.node_id,
            &summary.groups,
            &BTreeMap::new(),
        );
        let result = serde_json::to_string(&groups.into_values().collect::<Vec<_>>()).unwrap();
        assert!(result.contains("Unidentified proxy") && result.contains("Unidentified"));
        assert!(!result.contains("10.99.1.1") && !result.contains("unknown.example"));
        assert_eq!(config.proxies.len(), 1);
    }
    #[test]
    fn only_existing_local_configurations_get_readable_names() {
        let config=Config::parse("listen_port: 8787\nrequest_timeout_seconds: 30\nproxies:\n  known: http://10.1.1.1:7891\n").unwrap();
        let key = DataKey::new(KEY).unwrap();
        let summary = sample(&key, "http://10.1.1.1:7891", &config.codex.base_url.api_key);
        let mut groups = BTreeMap::new();
        add_groups(
            &mut groups,
            &labels(&config, &key),
            &summary.node_id,
            &summary.groups,
            &BTreeMap::new(),
        );
        add_groups(
            &mut groups,
            &labels(&config, &key),
            &summary.node_id,
            &summary.groups,
            &BTreeMap::new(),
        );
        let group = groups.into_values().next().unwrap();
        assert_eq!(group.proxy, "known");
        assert_eq!(group.upstream, "Codex API");
        assert_eq!(group.stats.requests, 2);
        assert!(!group.anonymous_proxy && !group.anonymous_upstream);
    }
    #[test]
    fn url_configuration_cannot_embed_credentials_or_arbitrary_paths() {
        let make = |url: &str| Source {
            name: "test".into(),
            transport: Transport::Http,
            ssh_connection: None,
            url: url.into(),
            token_env: Some("TEST_KEY".into()),
            token_file: None,
            ca_certificate: None,
            device_id: None,
            ssh_device: None,
        };
        for url in [
            "file:///etc/passwd",
            "http://user:password@localhost",
            "https://example.com/logs",
            "https://example.com/?key=secret",
        ] {
            assert!(make(url).validate().is_err());
        }
        assert!(make("http://10.42.0.196:8788").validate().is_ok());
        assert!(make("https://example.com:8788").validate().is_ok());
        assert!(!lan("8.8.8.8".parse().unwrap()));
        assert!(!lan("169.254.169.254".parse().unwrap()));
    }
}
