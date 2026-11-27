//! Fetch only bounded, authenticated processed summaries. Labels come exclusively
//! from this device's configuration; remote values never become configuration.
#[cfg(test)]
use crate::data_api::canonical;
use crate::data_api::{Group, Service, Stats, Summary};
use coport::config::Config;
#[cfg(test)]
use coport::external_access::DataKey;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    time::Duration,
};

/// This device as a merge reads it, taken once per traffic read.
#[derive(Clone)]
pub struct ThisDevice {
    pub config: Config,
    /// Its request log, where its identity evidence and device queries are.
    pub log: PathBuf,
    /// Credential labels already known; a merge never looks an account up.
    pub labels: BTreeMap<(String, String), String>,
    /// The node ID its summaries carry: a peer answering with it is this device.
    pub node_id: String,
}

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
mod http;

/// A requested range and category of a Summary.
pub(crate) type Selection = (u64, crate::traffic::TrafficScope);

/// How Summaries are read from Devices: over each Source's own transport, or
/// in tests from a script.
#[derive(Clone, Default)]
pub(crate) enum Summaries {
    #[default]
    Network,
    /// Each read's replies, in the order a Device would send them.
    #[cfg(test)]
    Scripted(std::sync::Arc<Script>),
}
#[cfg(test)]
pub(crate) type Script = dyn Fn(&Source, Option<Selection>) -> tokio::sync::mpsc::UnboundedReceiver<Result<Summary, String>>
    + Send
    + Sync;

impl Summaries {
    /// Read `source`'s Summary. Without `selection`, `on_summary` gets the
    /// complete one; with it, the selected range, then the complete one
    /// unless that already was.
    async fn read(
        &self,
        source: &Source,
        selection: Option<Selection>,
        mut on_summary: impl FnMut(Summary) -> Result<(), String>,
    ) -> Result<(), String> {
        match self {
            Self::Network => {
                source.validate()?;
                if source.transport == Transport::Ssh {
                    let device = source
                        .ssh_connection
                        .as_ref()
                        .ok_or("Missing SSH data connection")?;
                    return match selection {
                        Some((minutes, scope)) => {
                            crate::remote::summary_progressive(device, minutes, scope, on_summary)
                                .await
                        }
                        None => {
                            let summary = crate::remote::summary(device).await?;
                            summary.validate()?;
                            on_summary(summary)
                        }
                    };
                }
                let first = http::read(source, selection).await?;
                let partial = selection.is_some() && first.schema_version == 4;
                on_summary(first)?;
                if partial {
                    on_summary(http::read(source, None).await?)?;
                }
                Ok(())
            }
            #[cfg(test)]
            Self::Scripted(script) => {
                let mut replies = script(source, selection);
                while let Some(reply) = replies.recv().await {
                    on_summary(reply?)?;
                }
                Ok(())
            }
        }
    }
    /// `source`'s complete Summary.
    async fn complete(&self, source: &Source) -> Result<Summary, String> {
        let mut last = None;
        self.read(source, None, |summary| {
            last = Some(summary);
            Ok(())
        })
        .await?;
        last.ok_or_else(|| "The device sent no summary.".into())
    }
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
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum SourceExclusion {
    Duplicate,
    NotRefreshed,
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceState {
    /// The configured device this source reads; never sent to the panel.
    #[serde(skip)]
    device_id: Option<String>,
    name: String,
    included: bool,
    exclusion: Option<SourceExclusion>,
    error: Option<String>,
    traffic: Option<crate::traffic_stats::View>,
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Merged {
    pub window_start: i64,
    pub window_end: i64,
    pub sources: Vec<SourceState>,
    pub minutes: u64,
    pub scope: crate::traffic::TrafficScope,
    pub bucket_minutes: u64,
    /// This device's traffic, in the frame of the range the panel asked for.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local: Option<crate::traffic_stats::View>,
    /// Why this device's traffic could not be read for that frame.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_error: Option<String>,
    /// The forwarded device's live traffic, shown in place of its last summary.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forwarded: Option<LiveDevice>,
    /// Merged Traffic: this device and every other device, each given once.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub traffic: Option<crate::traffic_stats::View>,
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveDevice {
    pub device: String,
    pub traffic: crate::traffic_stats::View,
}
impl Merged {
    /// Completes this range's frame with this device's traffic and the
    /// forwarded device's live traffic, and adds Merged Traffic: each device
    /// once, the forwarded one by its live view rather than its last summary,
    /// a duplicate entry not at all (ADR-0001). Without this device's traffic
    /// there is no Merged Traffic yet.
    pub fn with_devices(
        &mut self,
        local: Result<&crate::traffic_stats::View, &str>,
        forwarded: Option<&ForwardedTraffic>,
    ) {
        let live_id = forwarded.map(|live| live.device_id.as_str());
        let duplicate = self.sources.iter().any(|source| {
            source.device_id.as_deref() == live_id
                && source.exclusion == Some(SourceExclusion::Duplicate)
        });
        self.local = local.ok().cloned();
        self.local_error = local.err().map(str::to_owned);
        let local = local.ok();
        self.forwarded = forwarded.filter(|_| !duplicate).map(|live| LiveDevice {
            device: live.device_id.clone(),
            traffic: live.traffic.clone(),
        });
        let live = self.forwarded.as_ref();
        self.traffic = local.and_then(|local| {
            let peers = self
                .sources
                .iter()
                .filter(|source| live.is_none() || source.device_id.as_deref() != live_id)
                .filter_map(|source| source.traffic.as_ref());
            crate::traffic_stats::merge(
                std::iter::once(local)
                    .chain(live.map(|live| &live.traffic))
                    .chain(peers),
            )
        });
    }
}
/// One device's traffic from its merged groups. Accounts are told apart by
/// their label, and every unlabelled upstream of a service is one Unidentified.
fn traffic_view(
    groups: &[MergedGroup],
    window: crate::traffic_stats::Window,
    scope: crate::traffic::TrafficScope,
) -> crate::traffic_stats::View {
    let mut accounts = BTreeMap::<(Service, String), crate::traffic_stats::Account>::new();
    for group in groups {
        let identity = if group.anonymous_upstream {
            "unidentified".into()
        } else {
            format!("known/{}", group.upstream)
        };
        let account = accounts
            .entry((group.service, identity))
            .or_insert_with(|| {
                crate::traffic_stats::Account::new(
                    group.service,
                    group.upstream.clone(),
                    Vec::new(),
                    crate::traffic_stats::empty(window.buckets()),
                )
            });
        crate::traffic_stats::add(&mut account.stats, &group.stats);
    }
    crate::traffic_stats::View::reported_by_peer(scope, window, accounts.into_values().collect())
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
    summaries: &Summaries,
) -> Result<Vec<(Source, Result<Summary, String>)>, String> {
    let mut tasks = tokio::task::JoinSet::new();
    for source in sources {
        let (log, summaries) = (log.to_owned(), summaries.clone());
        tasks.spawn(async move {
            let started = std::time::Instant::now();
            let result = summaries.complete(&source).await;
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
        crate::traffic_stats::add(&mut group.stats, stats);
    }
}
/// The forwarded device's traffic, labelled as Devices labels that device.
/// Only this machine's configuration, cached labels and logs are read.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ForwardedTraffic {
    device_id: String,
    name: String,
    fetched_at: u64,
    error: Option<String>,
    scope: crate::traffic::TrafficScope,
    window_start: i64,
    window_end: i64,
    bucket_minutes: u64,
    traffic: crate::traffic_stats::View,
}
pub async fn forwarded_traffic(
    remote: crate::remote_forward::RemoteWindow,
    device: ThisDevice,
) -> Result<ForwardedTraffic, String> {
    let window = remote.window;
    let bucket = crate::data_api::bucket_minutes(window.minutes)
        .filter(|bucket| *bucket == window.bucket_minutes)
        .ok_or("Invalid remote traffic window")?;
    let context = prepare_merge_range(device, window.minutes).await?;
    let mut groups = BTreeMap::new();
    add_groups(
        &mut groups,
        &(BTreeMap::new(), BTreeMap::new()),
        &remote.node_id,
        &window.groups,
        &context.accounts,
    );
    Ok(ForwardedTraffic {
        device_id: remote.device_id,
        name: remote.name,
        fetched_at: remote.fetched_at,
        error: remote.error,
        scope: window.scope,
        window_start: window.window_start,
        window_end: window.window_end,
        bucket_minutes: bucket,
        traffic: traffic_view(
            &groups.into_values().collect::<Vec<_>>(),
            crate::traffic_stats::Window {
                start: window.window_start,
                end: window.window_end,
                bucket_minutes: bucket,
            },
            window.scope,
        ),
    })
}
/// One range and category of every Source's Summary, read from `summaries`.
#[cfg(test)]
async fn merge(
    sources: Vec<Source>,
    device: ThisDevice,
    minutes: u64,
    scope: crate::traffic::TrafficScope,
    summaries: &Summaries,
) -> Result<Merged, String> {
    crate::data_api::bucket_minutes(minutes).ok_or("Unsupported traffic range")?;
    if sources.len() > crate::devices::LIMIT {
        return Err("At most 32 data sources are supported.".into());
    }
    let context = std::sync::Arc::new(prepare_merge(device).await?);
    let (fetched, end) = align_sources(&context, sources, &[(minutes, scope)], summaries).await?;
    merge_fetched(&context, &fetched, minutes, scope, end)
}

/// Every range and category of every Source's Summary, read from `summaries`.
#[cfg(test)]
async fn merge_views(
    sources: Vec<Source>,
    device: ThisDevice,
    summaries: Summaries,
) -> Result<Vec<Merged>, String> {
    MergeCache::reading(summaries)
        .views(sources, device, true)
        .await
}

fn cache_source_key(source: &Source) -> Result<Vec<u8>, String> {
    let mut connection = source.clone();
    connection.name.clear();
    serde_json::to_vec(&(
        source.device_id.as_deref().unwrap_or(&source.name),
        connection,
    ))
    .map_err(|e| e.to_string())
}
#[derive(Default)]
pub struct MergeCache {
    summaries: Summaries,
    key: Vec<u8>,
    fetched: Option<(std::sync::Arc<Fetched>, i64)>,
    errors: BTreeMap<Vec<u8>, String>,
}
/// Cached peer results captured under the cache lock and rendered without it.
struct CachedState {
    fetched: Option<(std::sync::Arc<Fetched>, i64)>,
    errors: BTreeMap<Vec<u8>, String>,
    keys: Vec<(String, Vec<u8>)>,
}
impl MergeCache {
    /// A cache whose refreshes read Summaries from `summaries`.
    #[cfg(test)]
    pub(crate) fn reading(summaries: Summaries) -> Self {
        Self {
            summaries,
            ..Self::default()
        }
    }
    pub async fn refresh_progressive(
        &mut self,
        sources: Vec<Source>,
        device: ThisDevice,
        selection: (u64, crate::traffic::TrafficScope),
        on_update: impl Fn(Vec<Merged>) -> Result<(), String>,
    ) -> Result<Vec<Merged>, String> {
        let key = self.begin_refresh(&sources)?;
        let result = merge_progressive_snapshot(
            sources,
            device,
            selection.into(),
            on_update,
            &self.summaries,
        )
        .await?;
        self.commit_refresh(&key, result)
    }

    /// Like `refresh_progressive`, but the lock is held only to read and commit
    /// state, never across peer I/O, so cached views stay readable meanwhile.
    pub async fn refresh_shared(
        cache: &tokio::sync::Mutex<Self>,
        sources: Vec<Source>,
        device: ThisDevice,
        refresh: Refresh,
        on_update: impl Fn(Vec<Merged>) -> Result<(), String>,
    ) -> Result<Vec<Merged>, String> {
        let (key, summaries) = {
            let mut cache = cache.lock().await;
            (cache.begin_refresh(&sources)?, cache.summaries.clone())
        };
        let result =
            merge_progressive_snapshot(sources, device, refresh, on_update, &summaries).await?;
        cache.lock().await.commit_refresh(&key, result)
    }

    fn begin_refresh(&mut self, sources: &[Source]) -> Result<Vec<u8>, String> {
        let mut keys = sources
            .iter()
            .map(cache_source_key)
            .collect::<Result<Vec<_>, _>>()?;
        keys.sort();
        let key = serde_json::to_vec(&keys).map_err(|e| e.to_string())?;
        if key != self.key {
            self.key = key.clone();
            self.fetched = None;
            self.errors.clear();
        }
        Ok(key)
    }

    fn commit_refresh(
        &mut self,
        key: &[u8],
        result: ProgressiveResult,
    ) -> Result<Vec<Merged>, String> {
        // Devices changed while peers were read: never cache the old set's results.
        if self.key != key {
            return Ok(result.views);
        }
        self.errors.clear();
        for (source, _) in &result.fetched {
            if let Some(error) = result.errors.get(&source.name) {
                self.errors.insert(cache_source_key(source)?, error.clone());
            }
        }
        self.fetched = Some((std::sync::Arc::new(result.fetched), result.end));
        Ok(result.views)
    }

    /// Only an explicit remote refresh may open a connection. Cached snapshots
    /// are re-merged with current local assignments without refetching peers.
    pub async fn views(
        &mut self,
        sources: Vec<Source>,
        device: ThisDevice,
        refresh_remote: bool,
    ) -> Result<Vec<Merged>, String> {
        self.views_selected_first(sources, device, refresh_remote, None)
            .await
    }

    /// Like `views`, but publishes `first` through `on_update` before merging
    /// every range, so a page can render without waiting for 30-day views.
    pub async fn views_selected_first(
        &mut self,
        sources: Vec<Source>,
        device: ThisDevice,
        refresh_remote: bool,
        first: Option<FirstView<'_>>,
    ) -> Result<Vec<Merged>, String> {
        self.cached_state(&sources)?;
        let mut full = None;
        if refresh_remote {
            let context = std::sync::Arc::new(prepare_merge(device.clone()).await?);
            let (fetched, end) = align_sources(
                &context,
                sources.clone(),
                &all_selections(),
                &self.summaries,
            )
            .await?;
            self.fetched = Some((std::sync::Arc::new(fetched), end));
            self.errors.clear();
            full = Some(context);
        }
        let state = self.cached_state(&sources)?;
        render_views(state, sources, device, first, full).await
    }

    /// Passive reads hold the cache lock only to capture its state, so a
    /// later read's first frame never waits behind an earlier 30-day merge.
    pub async fn views_shared(
        cache: &tokio::sync::Mutex<Self>,
        sources: Vec<Source>,
        device: ThisDevice,
        first: Option<FirstView<'_>>,
    ) -> Result<Vec<Merged>, String> {
        let state = cache.lock().await.cached_state(&sources)?;
        render_views(state, sources, device, first, None).await
    }

    /// Reset the cache for a changed device set and map cached results to
    /// current device names.
    fn cached_state(&mut self, sources: &[Source]) -> Result<CachedState, String> {
        if sources.len() > crate::devices::LIMIT {
            return Err("At most 32 data sources are supported.".into());
        }
        let keys: Vec<_> = sources
            .iter()
            .map(|source| Ok((source.name.clone(), cache_source_key(source)?)))
            .collect::<Result<_, String>>()?;
        let mut sorted: Vec<_> = keys.iter().map(|(_, key)| key.clone()).collect();
        sorted.sort();
        let key = serde_json::to_vec(&sorted).map_err(|e| e.to_string())?;
        if key != self.key {
            self.fetched = None;
            self.errors.clear();
            self.key = key;
        }
        if let Some((fetched, _)) = &mut self.fetched {
            for (source, _) in std::sync::Arc::make_mut(fetched).iter_mut() {
                let key = cache_source_key(source)?;
                let current = sources
                    .iter()
                    .zip(&keys)
                    .find(|(_, (_, candidate))| candidate == &key)
                    .ok_or("Cached source is no longer configured")?
                    .0;
                *source = current.clone();
            }
        }
        Ok(CachedState {
            fetched: self.fetched.clone(),
            errors: self.errors.clone(),
            keys,
        })
    }
}
fn all_selections() -> Vec<(u64, crate::traffic::TrafficScope)> {
    crate::data_api::RANGES
        .into_iter()
        .flat_map(|minutes| {
            [
                crate::traffic::TrafficScope::Model,
                crate::traffic::TrafficScope::All,
            ]
            .into_iter()
            .map(move |scope| (minutes, scope))
        })
        .collect()
}
async fn render_views(
    state: CachedState,
    sources: Vec<Source>,
    device: ThisDevice,
    first: Option<FirstView<'_>>,
    full: Option<std::sync::Arc<MergeContext>>,
) -> Result<Vec<Merged>, String> {
    let selections = all_selections();
    let at = chrono::Utc::now().timestamp_millis() / 60_000 * 60_000;
    let pending = state.fetched.is_none();
    let empty: Fetched = Vec::new();
    let (fetched, end): (&Fetched, i64) = match &state.fetched {
        Some((fetched, end)) => (fetched, *end),
        None => (&empty, at),
    };
    let decorate = |view: &mut Merged| {
        if pending {
            view.sources = sources
                .iter()
                .map(|source| SourceState {
                    device_id: source.device_id.clone(),
                    name: source.name.clone(),
                    included: false,
                    exclusion: Some(SourceExclusion::NotRefreshed),
                    error: None,
                    traffic: None,
                })
                .collect();
            view.sources.sort_by(|a, b| a.name.cmp(&b.name));
        }
        for source in &mut view.sources {
            if let Some((_, key)) = state.keys.iter().find(|(name, _)| name == &source.name)
                && let Some(error) = state.errors.get(key)
            {
                source.error = Some(error.clone());
            }
        }
    };
    if let Some((minutes, scope, on_update)) =
        first.filter(|(minutes, scope, _)| selections.contains(&(*minutes, *scope)))
    {
        // The first frame reads only the files its range needs, like an
        // explicit refresh. A cold start must not parse 30 days of archives
        // first; the complete views below replace it with 30-day evidence.
        // Evidence spans from the window start to now, so a cached boundary
        // from an earlier refresh is still covered.
        let behind = u64::try_from(at - end).unwrap_or(0).div_ceil(60_000);
        let evidence = (minutes + behind).min(43200);
        let mut context = prepare_merge_range(device.clone(), evidence).await?;
        context.at = at;
        let mut view = merge_fetched(&context, fetched, minutes, scope, end)?;
        decorate(&mut view);
        on_update(vec![view])?;
    }
    let context = match full {
        Some(context) => context,
        None => {
            let mut context = prepare_merge(device).await?;
            context.at = at;
            std::sync::Arc::new(context)
        }
    };
    let mut views = merge_selections(&context, fetched, &selections, end)?;
    views.iter_mut().for_each(decorate);
    Ok(views)
}
/// A refresh of one range/category first. Devices whose summary was just read
/// elsewhere (the forwarded device, by the daemon's monitor) are not read again.
pub struct Refresh {
    pub minutes: u64,
    pub scope: crate::traffic::TrafficScope,
    /// Summaries by device id; one that no longer validates is read as usual.
    pub reused: BTreeMap<String, Summary>,
}
impl From<(u64, crate::traffic::TrafficScope)> for Refresh {
    fn from((minutes, scope): (u64, crate::traffic::TrafficScope)) -> Self {
        Self {
            minutes,
            scope,
            reused: BTreeMap::new(),
        }
    }
}
struct ProgressiveResult {
    views: Vec<Merged>,
    fetched: Fetched,
    end: i64,
    errors: BTreeMap<String, String>,
}
async fn merge_progressive_snapshot(
    sources: Vec<Source>,
    device: ThisDevice,
    refresh: Refresh,
    on_update: impl Fn(Vec<Merged>) -> Result<(), String>,
    summaries: &Summaries,
) -> Result<ProgressiveResult, String> {
    let Refresh {
        minutes,
        scope,
        reused,
    } = refresh;
    crate::data_api::bucket_minutes(minutes).ok_or("Unsupported traffic range")?;
    if sources.len() > crate::devices::LIMIT {
        return Err("At most 32 data sources are supported.".into());
    }
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut tasks = tokio::task::JoinSet::new();
    let mut fetched: Fetched = sources
        .iter()
        .cloned()
        .map(|s| (s, Err("TRAFFIC_UPDATING".into())))
        .collect();
    for (index, source) in sources.iter().cloned().enumerate() {
        // Nothing is queried for a reused summary, so there is no query to record.
        if let Some(summary) = source
            .device_id
            .as_ref()
            .and_then(|id| reused.get(id))
            .filter(|summary| summary.validate().is_ok())
        {
            tx.send((index, Ok(summary.clone())))
                .map_err(|_| "Data reader closed")?;
            continue;
        }
        let tx = tx.clone();
        let (log, summaries) = (device.log.clone(), summaries.clone());
        tasks.spawn(async move {
            let started = std::time::Instant::now();
            let result = summaries
                .read(&source, Some((minutes, scope)), |summary| {
                    tx.send((index, Ok(summary)))
                        .map_err(|_| "Data reader closed".into())
                })
                .await;
            crate::device_events::record(log, &source, result.is_ok(), started.elapsed()).await;
            if let Err(error) = result {
                let _ = tx.send((index, Err(error)));
            }
        });
    }
    drop(tx);
    // Remote I/O runs while local identity evidence is prepared.
    let context = std::sync::Arc::new(prepare_merge_range(device, minutes).await?);

    // Placeholders show at once; peers replace them as they answer.
    on_update(vec![merge_fetched(
        &context, &fetched, minutes, scope, context.at,
    )?])?;
    // While peers answer, parse this device's 30-day archives into the cache.
    // Archives never change, so the complete views below reuse them at any
    // boundary; a scan still running is awaited there, never repeated.
    let warm_log = context.device.log.clone();
    let _warm = tokio::task::spawn_blocking(move || {
        crate::traffic::observed_identity_labels_range(&warm_log, BTreeMap::new(), 43200)
    });
    let mut background_errors = BTreeMap::new();
    while let Some((index, result)) = rx.recv().await {
        if let Err(error) = &result {
            background_errors.insert(fetched[index].0.name.clone(), error.clone());
        }
        // A late background failure must not erase an already rendered window.
        if result.is_ok() || fetched[index].1.is_err() {
            fetched[index].1 = result;
        }
        let mut seen = BTreeSet::from([context.device.node_id.clone()]);
        let summaries: Vec<_> = fetched
            .iter()
            .filter_map(|(_, r)| r.as_ref().ok())
            .filter(|s| seen.insert(s.node_id.clone()))
            .filter(|s| selected_groups(s, minutes, scope, s.window_end).is_ok())
            .collect();
        let Some(end) = common_end(&summaries, context.at, minutes, scope) else {
            continue;
        };
        let mut view = merge_fetched(&context, &fetched, minutes, scope, end)?;
        for source in &mut view.sources {
            if let Some(error) = background_errors.get(&source.name) {
                source.error = Some(error.clone());
            }
        }
        on_update(vec![view])?;
    }
    while let Some(result) = tasks.join_next().await {
        result.map_err(|_| "Data reader stopped")?;
    }
    // Reuse the received snapshots. Only clock-boundary mismatches require retries.
    let selections: Vec<_> = crate::data_api::RANGES
        .into_iter()
        .flat_map(|m| {
            [
                (m, crate::traffic::TrafficScope::Model),
                (m, crate::traffic::TrafficScope::All),
            ]
        })
        .collect();
    let mut complete_context = prepare_merge(context.device.clone()).await?;
    complete_context.at = context.at;
    let context = std::sync::Arc::new(complete_context);
    let (fetched, end) = align_fetched(&context, fetched, &selections, None).await?;
    let mut views = merge_selections(&context, &fetched, &selections, end)?;
    for view in &mut views {
        for source in &mut view.sources {
            if let Some(error) = background_errors.get(&source.name) {
                source.error = Some(error.clone());
            }
        }
    }
    Ok(ProgressiveResult {
        views,
        fetched,
        end,
        errors: background_errors,
    })
}

/// `device` with the labels its identity evidence adds.
struct MergeContext {
    device: ThisDevice,
    accounts: BTreeMap<String, String>,
    at: i64,
}
async fn prepare_merge(device: ThisDevice) -> Result<MergeContext, String> {
    prepare_merge_range(device, 43200).await
}
async fn prepare_merge_range(device: ThisDevice, minutes: u64) -> Result<MergeContext, String> {
    let ThisDevice {
        config,
        log,
        labels: credential_labels,
        node_id,
    } = device;
    let evidence_path = log.clone();
    let credential_labels = tokio::task::spawn_blocking(move || {
        crate::traffic::observed_identity_labels_range(&evidence_path, credential_labels, minutes)
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
        device: ThisDevice {
            config,
            log,
            labels: credential_labels,
            node_id,
        },
        accounts,
        at: chrono::Utc::now().timestamp_millis() / 60_000 * 60_000,
    })
}
type Fetched = Vec<(Source, Result<Summary, String>)>;
async fn align_sources(
    context: &MergeContext,
    sources: Vec<Source>,
    selections: &[(u64, crate::traffic::TrafficScope)],
    summaries: &Summaries,
) -> Result<(Fetched, i64), String> {
    let fetched = fetch_sources(sources, &context.device.log, summaries).await?;
    align_fetched(context, fetched, selections, Some(summaries)).await
}
/// The boundary every Summary in `fetched` can be merged at. Devices that
/// disagree are read again from `refetch` up to twice, or without it marked.
async fn align_fetched(
    context: &MergeContext,
    mut fetched: Fetched,
    selections: &[(u64, crate::traffic::TrafficScope)],
    refetch: Option<&Summaries>,
) -> Result<(Fetched, i64), String> {
    for attempt in 0..3 {
        let mut seen = BTreeSet::from([context.device.node_id.clone()]);
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
        // Progressive refresh already has all frames from the authorized connection.
        // A boundary mismatch must not launch another SSH connection behind the UI;
        // keep the devices that agree and report the others instead.
        let Some(summaries) = refetch else {
            return Ok(align_partially(context, fetched, selections));
        };
        if attempt < 2 {
            tokio::time::sleep(Duration::from_millis(250 * (attempt + 1))).await;
            // Only boundary skew is retried; failed sources (e.g. SSH denial or
            // timeout) keep their result and are never reconnected.
            let (ok, mut failed): (Fetched, Fetched) =
                fetched.into_iter().partition(|(_, result)| result.is_ok());
            let sources = ok.into_iter().map(|(s, _)| s).collect();
            failed.extend(fetch_sources(sources, &context.device.log, summaries).await?);
            failed.sort_by(|a, b| a.0.name.cmp(&b.0.name));
            fetched = failed;
        }
    }
    Err("TRAFFIC_UPDATING".into())
}
/// Choose the boundary shared by the most devices, latest first, and mark the
/// devices that cannot provide it. Other devices stay visible and cached.
fn align_partially(
    context: &MergeContext,
    mut fetched: Fetched,
    selections: &[(u64, crate::traffic::TrafficScope)],
) -> (Fetched, i64) {
    let mut seen = BTreeSet::from([context.device.node_id.clone()]);
    let valid: Vec<(usize, &Summary)> = fetched
        .iter()
        .enumerate()
        .filter_map(|(index, (_, result))| {
            let summary = result.as_ref().ok()?;
            let usable = seen.insert(summary.node_id.clone())
                && selections.iter().any(|(minutes, scope)| {
                    selected_groups(summary, *minutes, *scope, summary.window_end).is_ok()
                });
            usable.then_some((index, summary))
        })
        .collect();
    let supports = |summary: &Summary, end: i64| {
        selections.iter().all(|(minutes, scope)| {
            selected_groups(summary, *minutes, *scope, summary.window_end).is_err()
                || selected_groups(summary, *minutes, *scope, end).is_ok()
        })
    };
    let candidates: BTreeSet<i64> = valid
        .iter()
        .flat_map(|(_, s)| {
            std::iter::once(s.window_end).chain(s.previous_windows.iter().map(|w| w.window_end))
        })
        .filter(|end| *end <= context.at)
        .collect();
    // `max_by_key` keeps the last maximum, so ascending order prefers the latest tie.
    let end = candidates
        .into_iter()
        .max_by_key(|end| valid.iter().filter(|(_, s)| supports(s, *end)).count())
        .unwrap_or(context.at);
    let mismatched: Vec<usize> = valid
        .iter()
        .filter(|(_, s)| !supports(s, end))
        .map(|(index, _)| *index)
        .collect();
    for index in mismatched {
        fetched[index].1 = Err(
            "This device reported a different time boundary; press Refresh to try again.".into(),
        );
    }
    (fetched, end)
}
/// A selected range/scope and where to publish it before the remaining views.
pub type FirstView<'a> = (
    u64,
    crate::traffic::TrafficScope,
    &'a (dyn Fn(Vec<Merged>) -> Result<(), String> + Sync),
);
/// Merge every range/scope from one set of fetched summaries; order is preserved.
fn merge_selections(
    context: &MergeContext,
    fetched: &Fetched,
    selections: &[(u64, crate::traffic::TrafficScope)],
    end: i64,
) -> Result<Vec<Merged>, String> {
    selections
        .iter()
        .map(|&(minutes, scope)| merge_fetched(context, fetched, minutes, scope, end))
        .collect()
}
/// Each device's view of one range/category at the shared boundary; see
/// `Merged::with_devices` for this device's and Merged Traffic.
fn merge_fetched(
    context: &MergeContext,
    fetched: &Fetched,
    minutes: u64,
    scope: crate::traffic::TrafficScope,
    end: i64,
) -> Result<Merged, String> {
    let bucket = crate::data_api::bucket_minutes(minutes).ok_or("Unsupported traffic range")?;
    let window = crate::traffic_stats::Window {
        start: end - minutes as i64 * 60_000,
        end,
        bucket_minutes: bucket,
    };
    let mut seen = BTreeSet::from([context.device.node_id.clone()]);
    let mut states = Vec::new();
    for (source, result) in fetched {
        let name = source.name.clone();
        let mut source_traffic = None;
        let mut exclusion = None;
        let (included, error) = match result {
            Ok(summary) if !seen.insert(summary.node_id.clone()) => {
                exclusion = Some(SourceExclusion::Duplicate);
                (false, None)
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
                        window,
                        scope,
                    ));
                    (true, None)
                }
                Err(error) => (false, Some(error.clone())),
            },
            Err(error) => (false, Some(error.clone())),
        };
        states.push(SourceState {
            device_id: source.device_id.clone(),
            name,
            included,
            exclusion,
            error,
            traffic: source_traffic,
        });
    }
    states.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Merged {
        local: None,
        local_error: None,
        forwarded: None,
        traffic: None,
        window_start: end - minutes as i64 * 60_000,
        window_end: end,
        sources: states,
        minutes,
        scope,
        bucket_minutes: bucket,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    /// This device with `config`, `log` and `labels`, and a node ID no peer has.
    fn this_device(
        config: Config,
        log: PathBuf,
        labels: BTreeMap<(String, String), String>,
    ) -> ThisDevice {
        ThisDevice {
            config,
            log,
            labels,
            node_id: "local".into(),
        }
    }
    /// A peer's Model traffic over `buckets` one-minute buckets.
    fn traffic_view(groups: &[MergedGroup], buckets: usize) -> crate::traffic_stats::View {
        super::traffic_view(
            groups,
            crate::traffic_stats::Window {
                start: 0,
                end: buckets as i64 * 60_000,
                bucket_minutes: 1,
            },
            crate::traffic::TrafficScope::Model,
        )
    }
    fn empty_stats(buckets: usize) -> Stats {
        crate::traffic_stats::empty(buckets)
    }
    fn add_stats(to: &mut Stats, from: &Stats) {
        crate::traffic_stats::add(to, from)
    }

    #[test]
    fn merged_traffic_counts_each_device_once_with_the_forwarded_one_live() {
        use crate::traffic_stats::{Account, View, Window};
        let window = Window {
            start: 0,
            end: 60_000,
            bucket_minutes: 1,
        };
        let traffic = |requests: u64| {
            View::reported_by_peer(
                crate::traffic::TrafficScope::Model,
                window,
                vec![Account::new(
                    Service::Codex,
                    "shared".into(),
                    Vec::new(),
                    Stats {
                        requests,
                        ..empty_stats(1)
                    },
                )],
            )
        };
        let source = |id: &str, requests: Option<u64>, exclusion| SourceState {
            device_id: Some(id.into()),
            name: id.into(),
            included: requests.is_some(),
            exclusion,
            error: None,
            traffic: requests.map(traffic),
        };
        let mut merged = Merged {
            window_start: window.start,
            window_end: window.end,
            sources: vec![
                source("peer", Some(10), None),
                source("forwarded", Some(100), None),
                source("copy", None, Some(SourceExclusion::Duplicate)),
            ],
            minutes: 1,
            scope: crate::traffic::TrafficScope::Model,
            bucket_minutes: 1,
            local: None,
            local_error: None,
            forwarded: None,
            traffic: None,
        };
        let live = |device: &str| ForwardedTraffic {
            device_id: device.into(),
            name: device.into(),
            fetched_at: 0,
            error: None,
            scope: crate::traffic::TrafficScope::Model,
            window_start: window.start,
            window_end: window.end,
            bucket_minutes: 1,
            traffic: traffic(1000),
        };
        let local = traffic(1);
        // Without this device's traffic there is no Merged Traffic, and the
        // frame says why.
        merged.with_devices(Err("Cannot read the request log"), Some(&live("forwarded")));
        assert!(merged.traffic.is_none());
        let json = serde_json::to_value(&merged).unwrap();
        assert_eq!(json["localError"], "Cannot read the request log");
        assert!(json.get("local").is_none() && json.get("traffic").is_none());
        // The forwarded device's live view replaces its last summary.
        merged.with_devices(Ok(&local), Some(&live("forwarded")));
        let total = merged.traffic.as_ref().unwrap();
        assert_eq!(total.stats.requests, 1 + 10 + 1000);
        assert_eq!(total.credentials[0].stats.requests, 1011);
        assert_eq!(merged.forwarded.as_ref().unwrap().device, "forwarded");
        let json = serde_json::to_value(&merged).unwrap();
        assert_eq!(json["local"]["requests"], 1);
        assert_eq!(json["forwarded"]["traffic"]["requests"], 1000);
        assert_eq!(json["traffic"]["requests"], 1011);
        assert!(json["sources"][0].get("deviceId").is_none());
        // A duplicate entry is never counted, live or not.
        merged.with_devices(Ok(&local), Some(&live("copy")));
        assert!(merged.forwarded.is_none());
        assert_eq!(
            merged.traffic.as_ref().unwrap().stats.requests,
            1 + 10 + 100
        );
        // Without forwarding every device's summary counts.
        merged.with_devices(Ok(&local), None);
        assert_eq!(merged.traffic.as_ref().unwrap().stats.requests, 111);
    }
    use crate::test_support::{DATA_KEY as KEY, DATA_KEY_ENV, data_key_in_subprocess};
    #[tokio::test]
    async fn selected_views_render_before_slow_peers_and_background_responses() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("proxy.log");
        std::fs::write(&log, "").unwrap();
        let config = Config::parse("listen_port: 8787\nrequest_timeout_seconds: 30\ncodex:\n  homes: []\nclaude:\n  config_dirs: []\n").unwrap();
        let release_slow = std::sync::Arc::new(tokio::sync::Notify::new());
        let release_background = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
        // Each Device sends the requested range, then the rest once released;
        // the second one is slow to send even the first.
        let replies: Vec<(Summary, Summary)> = (0..2)
            .map(|_| {
                let bytes = crate::data_api::publish(
                    &config,
                    &log,
                    &DataKey::new(KEY).unwrap(),
                    &uuid::Uuid::new_v4().to_string(),
                    &[],
                )
                .unwrap();
                let complete: Summary = serde_json::from_slice(&bytes).unwrap();
                let mut selected = complete.clone();
                selected.schema_version = 4;
                selected.groups.clear();
                selected
                    .windows
                    .retain(|w| w.minutes == 1440 && w.scope == crate::traffic::TrafficScope::All);
                selected
                    .previous_windows
                    .retain(|w| w.minutes == 1440 && w.scope == crate::traffic::TrafficScope::All);
                selected.validate().unwrap();
                (selected, complete)
            })
            .collect();
        let (slow, background) = (release_slow.clone(), release_background.clone());
        let summaries = Summaries::Scripted(std::sync::Arc::new(move |source, selection| {
            assert_eq!(selection, Some((1440, crate::traffic::TrafficScope::All)));
            let peer = usize::from(source.name == "peer-1");
            let (selected, complete) = replies[peer].clone();
            let (slow, background) = (slow.clone(), background.clone());
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            tokio::spawn(async move {
                if peer == 1 {
                    slow.notified().await;
                }
                let _ = tx.send(Ok(selected));
                background.acquire().await.unwrap().forget();
                let _ = tx.send(Ok(complete));
            });
            rx
        }));
        let sources = vec![device_source("peer-0"), device_source("peer-1")];
        let updates = std::sync::Mutex::new(Vec::new());
        let mut cache = MergeCache::reading(summaries);
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            cache.refresh_progressive(
                sources.clone(),
                this_device(config.clone(), log.clone(), BTreeMap::new()),
                (1440, crate::traffic::TrafficScope::All),
                |views| {
                    assert_eq!(views.len(), 1);
                    assert_eq!(views[0].minutes, 1440);
                    assert_eq!(views[0].scope, crate::traffic::TrafficScope::All);
                    let included = views[0].sources.iter().filter(|s| s.included).count();
                    let mut updates = updates.lock().unwrap();
                    if updates.is_empty() {
                        assert_eq!(included, 0, "This device renders before any peer");
                        assert!(
                            views[0]
                                .sources
                                .iter()
                                .all(|s| s.error.as_deref() == Some("TRAFFIC_UPDATING"))
                        );
                    } else if updates.len() == 1 {
                        assert_eq!(
                            included, 1,
                            "Fast peer must render while slow peer is blocked"
                        );
                        assert_eq!(
                            views[0].sources[1].error.as_deref(),
                            Some("TRAFFIC_UPDATING")
                        );
                        release_slow.notify_one();
                    }
                    if included == 2 && !updates.contains(&2) {
                        release_background.add_permits(2);
                    }
                    updates.push(included);
                    Ok(())
                },
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(result.len(), 12);
        assert!(
            result
                .iter()
                .all(|view| view.sources.iter().all(|s| s.included))
        );
        assert_eq!(updates.lock().unwrap()[..2], [0, 1]);
        // Cached page reads retain successful snapshots without reading again.
        let cached = cache
            .views(sources, this_device(config, log, BTreeMap::new()), false)
            .await
            .unwrap();
        assert_eq!(cached.len(), 12);
        assert!(cached.iter().all(|view| {
            view.sources
                .iter()
                .all(|source| source.included && source.error.is_none())
        }));
    }

    #[tokio::test]
    async fn cached_views_never_connect_and_keep_sources_matched_after_renaming() {
        let (summaries, reads) = scripted(|_, _, _| vec![Err("Data connection failed".into())]);
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("data.key");
        crate::settings::write_private(&key, KEY.as_bytes()).unwrap();
        let config=Config::parse("listen_port: 8787\nrequest_timeout_seconds: 3\ncodex:\n  homes: []\nclaude:\n  config_dirs: []\n").unwrap();
        let source = |id: &str, name: &str| Source {
            name: name.into(),
            device_id: Some(id.into()),
            transport: Transport::Http,
            ssh_connection: None,
            url: "http://127.0.0.1:1".into(),
            token_env: None,
            token_file: Some(key.to_string_lossy().into()),
            ca_certificate: None,
            ssh_device: None,
        };
        let mut sources = vec![source("z", "Z"), source("a", "A")];
        for source in &sources {
            source.validate().unwrap();
        }
        let log = dir.path().join("traffic.log");
        std::fs::write(&log, "").unwrap();
        let mut cache = MergeCache::reading(summaries);
        for _ in 0..3 {
            let views = tokio::time::timeout(
                Duration::from_secs(2),
                cache.views(
                    sources.clone(),
                    this_device(config.clone(), log.clone(), BTreeMap::new()),
                    false,
                ),
            )
            .await
            .unwrap()
            .unwrap();
            assert!(views[0].sources.iter().all(|source| source.error.is_none()
                && source.exclusion == Some(SourceExclusion::NotRefreshed)));
        }
        let end = chrono::Utc::now().timestamp_millis() / 60_000 * 60_000;
        cache.fetched = Some((
            std::sync::Arc::new(vec![
                (sources[1].clone(), Err("error A".into())),
                (sources[0].clone(), Err("error Z".into())),
            ]),
            end,
        ));
        sources[0].name = "First".into();
        sources[1].name = "Second".into();
        let views = cache
            .views(sources, this_device(config, log, BTreeMap::new()), false)
            .await
            .unwrap();
        assert_eq!(views[0].sources[0].name, "First");
        assert_eq!(views[0].sources[0].error.as_deref(), Some("error Z"));
        assert_eq!(views[0].sources[1].name, "Second");
        assert_eq!(views[0].sources[1].error.as_deref(), Some("error A"));
        assert!(
            reads.lock().unwrap().is_empty(),
            "Reading cached views read a Device"
        );
    }
    #[tokio::test]
    async fn progressive_alignment_mismatch_never_opens_another_connection() {
        use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("proxy.log");
        std::fs::write(&log, "").unwrap();
        let config = Config::parse("listen_port: 8787\nrequest_timeout_seconds: 30\ncodex:\n  homes: []\nclaude:\n  config_dirs: []\n").unwrap();
        let now = chrono::Utc::now().timestamp_millis() / 60_000 * 60_000;
        // The two Devices answer a minute apart.
        let turn = AtomicUsize::new(0);
        let (summaries, reads) = scripted(move |_, _, _| {
            let end = now - (turn.fetch_add(1, SeqCst) % 2) as i64 * 60_000;
            let mut summary = sample(&DataKey::new(KEY).unwrap(), "proxy", "upstream");
            summary.window_end = end;
            summary.window_start = end - 30 * 60_000;
            vec![Ok(summary)]
        });
        let sources = vec![device_source("peer-0"), device_source("peer-1")];
        let total = || reads.lock().unwrap().values().sum::<usize>();
        let mut cache = MergeCache::reading(summaries);
        let views = cache
            .refresh_progressive(
                sources.clone(),
                this_device(config.clone(), log.clone(), BTreeMap::new()),
                (30, crate::traffic::TrafficScope::Model),
                |_| Ok(()),
            )
            .await
            .unwrap();
        assert_eq!(
            total(),
            2,
            "One explicit refresh must contact each peer only once when boundaries disagree"
        );
        // The device on the latest boundary stays visible; the other is reported.
        let check = |views: &[Merged]| {
            let selected = views
                .iter()
                .find(|v| v.minutes == 30 && v.scope == crate::traffic::TrafficScope::Model)
                .unwrap();
            assert_eq!(selected.sources.iter().filter(|s| s.included).count(), 1);
            assert_eq!(
                selected
                    .sources
                    .iter()
                    .filter(|s| s
                        .error
                        .as_deref()
                        .is_some_and(|e| e.contains("different time boundary")))
                    .count(),
                1
            );
        };
        check(&views);
        // The partial result is cached; reading it never reconnects.
        check(
            &cache
                .views(sources, this_device(config, log, BTreeMap::new()), false)
                .await
                .unwrap(),
        );
        assert_eq!(total(), 2);
    }

    #[tokio::test]
    async fn first_frame_evidence_covers_a_cached_older_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("proxy.log");
        let now = chrono::Utc::now().timestamp_millis();
        // The last refresh ended two hours ago.
        let end = now / 60_000 * 60_000 - 120 * 60_000;
        let account = "00000000-0000-4000-8000-000000000123";
        std::fs::write(&log, "").unwrap();
        // The only evidence naming the peer's account lies in an archive
        // rotated an hour ago: after the cached boundary, before the last 30 minutes.
        let history = dir.path().join("history");
        std::fs::create_dir(&history).unwrap();
        let archive = history.join(format!(
            "proxy.log.{}.{}.jsonl",
            now - 60 * 60_000,
            uuid::Uuid::new_v4()
        ));
        let evidence = serde_json::json!({"timestamp":chrono::DateTime::from_timestamp_millis(now - 70 * 60_000).unwrap().to_rfc3339(),"event":"model_call_finished","service":"codex","model_call_id":"one","account_id":account,"account_label":"Known local account","status":"200"});
        std::fs::write(&archive, format!("{evidence}\n")).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&archive)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + Duration::from_millis((now - 60 * 60_000) as u64))
            .unwrap();
        let config = Config::parse(&format!("listen_port: 8787\nrequest_timeout_seconds: 30\ncodex:\n  homes: [{}]\n  routing:\n    account: {{'Known local account': none}}\nclaude:\n  config_dirs: []\n", serde_json::to_string(dir.path()).unwrap())).unwrap();
        let labels = config.local_traffic_credential_labels().await;
        let source = Source {
            name: "peer".into(),
            device_id: None,
            transport: Transport::Http,
            ssh_connection: None,
            url: "http://127.0.0.1:9".into(),
            token_env: Some(DATA_KEY_ENV.into()),
            token_file: None,
            ca_certificate: None,
            ssh_device: None,
        };
        let mut summary = sample(&DataKey::new(KEY).unwrap(), "proxy", "upstream");
        summary.window_end = end;
        summary.window_start = end - 30 * 60_000;
        summary.groups[0].account_ref = crate::data_api::account_reference("Codex", account);
        let mut cache = MergeCache::default();
        cache.cached_state(std::slice::from_ref(&source)).unwrap();
        cache.fetched = Some((
            std::sync::Arc::new(vec![(source.clone(), Ok(summary))]),
            end,
        ));
        let first = std::sync::Mutex::new(None);
        let publish = |views: Vec<Merged>| {
            *first.lock().unwrap() = Some(serde_json::to_value(&views[0]).unwrap());
            Ok(())
        };
        let views = cache
            .views_selected_first(
                vec![source],
                this_device(config, log, labels),
                false,
                Some((30, crate::traffic::TrafficScope::Model, &publish)),
            )
            .await
            .unwrap();
        let complete = views
            .iter()
            .find(|v| v.minutes == 30 && v.scope == crate::traffic::TrafficScope::Model)
            .unwrap();
        let complete = serde_json::to_value(complete).unwrap();
        assert!(complete.to_string().contains("Known local account"));
        assert_eq!(
            first.into_inner().unwrap().unwrap(),
            complete,
            "The first frame must use the evidence of the window it shows"
        );
    }

    #[tokio::test]
    async fn passive_reads_render_without_holding_the_cache_lock() {
        let (_dir, log, _) = archived_traffic_fixture();
        let config = Config::parse("listen_port: 8787\nrequest_timeout_seconds: 30\ncodex:\n  homes: []\nclaude:\n  config_dirs: []\n").unwrap();
        let cache = tokio::sync::Mutex::new(MergeCache::default());
        let free = std::sync::Mutex::new(None);
        // Another page read or a refresh must be able to take the lock while
        // this read is still merging its views.
        let publish = |_views: Vec<Merged>| {
            *free.lock().unwrap() = Some(cache.try_lock().is_ok());
            Ok(())
        };
        let views = MergeCache::views_shared(
            &cache,
            Vec::new(),
            this_device(config, log, BTreeMap::new()),
            Some((30, crate::traffic::TrafficScope::Model, &publish)),
        )
        .await
        .unwrap();
        assert_eq!(views.len(), 12);
        assert_eq!(*free.lock().unwrap(), Some(true));
    }

    #[tokio::test]
    async fn passive_reads_publish_the_selected_range_before_all_views() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("proxy.log");
        std::fs::write(&log, "").unwrap();
        let config = Config::parse("listen_port: 8787\nrequest_timeout_seconds: 30\ncodex:\n  homes: []\nclaude:\n  config_dirs: []\n").unwrap();
        let updates = std::sync::Mutex::new(Vec::new());
        let publish = |views: Vec<Merged>| {
            let mut updates = updates.lock().unwrap();
            updates.extend(views.iter().map(|view| (view.minutes, view.scope)));
            Ok(())
        };
        let views = MergeCache::default()
            .views_selected_first(
                Vec::new(),
                this_device(config, log, BTreeMap::new()),
                false,
                Some((1440, crate::traffic::TrafficScope::All, &publish)),
            )
            .await
            .unwrap();
        assert_eq!(
            *updates.lock().unwrap(),
            [(1440, crate::traffic::TrafficScope::All)]
        );
        let order: Vec<_> = views
            .iter()
            .map(|view| (view.minutes, view.scope))
            .collect();
        let expected: Vec<_> = crate::data_api::RANGES
            .into_iter()
            .flat_map(|m| {
                [
                    (m, crate::traffic::TrafficScope::Model),
                    (m, crate::traffic::TrafficScope::All),
                ]
            })
            .collect();
        assert_eq!(order, expected, "Parallel merging must keep view order");
    }

    /// This device's traffic exactly as its card reads it (`get_traffic`).
    fn local_traffic(
        config: &Config,
        log: &std::path::Path,
        labels: &BTreeMap<(String, String), String>,
        minutes: u64,
    ) -> serde_json::Value {
        let identities = crate::traffic_identity::Identities::from_config(config, &[]);
        serde_json::to_value(
            crate::traffic::read(
                log,
                minutes,
                labels,
                crate::traffic::TrafficScope::Model,
                &identities,
            )
            .unwrap(),
        )
        .unwrap()
    }
    fn archived_traffic_fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("proxy.log");
        let now = chrono::Utc::now();
        let row = |id: &str, at: chrono::DateTime<chrono::Utc>| {
            format!(
                "{}\n",
                serde_json::json!({"timestamp":at.to_rfc3339(),"event":"request_finished","request_id":id,"method":"POST","path":"/v1/responses","service":"codex","provider":"openai","status":"200","received_bytes":"42","input_tokens":"12","duration_ms":"100"})
            )
        };
        std::fs::write(&log, row("recent", now - chrono::Duration::minutes(2))).unwrap();
        let history = dir.path().join("history");
        std::fs::create_dir(&history).unwrap();
        let old = now - chrono::Duration::days(2);
        let archive = history.join(format!(
            "proxy.log.{}.{}.jsonl",
            old.timestamp_millis(),
            uuid::Uuid::new_v4()
        ));
        std::fs::write(&archive, row("archived", old)).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&archive)
            .unwrap()
            .set_modified(old.into())
            .unwrap();
        (dir, log, archive)
    }

    #[tokio::test]
    async fn identity_evidence_caches_archives_for_the_snapshot() {
        let (_dir, log, archive) = archived_traffic_fixture();
        // Evidence is read first here (it usually races the snapshot); the
        // snapshot must then reuse the archive for both scopes.
        crate::traffic::observed_identity_labels_range(&log, BTreeMap::new(), 43200).unwrap();
        let end = chrono::Utc::now().timestamp_millis() / 60_000 * 60_000;
        crate::traffic::Snapshot::load(&log, end).unwrap();
        assert_eq!(crate::traffic::archive_scans(&archive), 1);
    }

    #[tokio::test]
    async fn merged_views_leave_this_device_to_its_card() {
        // Peers' views never count this device: `Merged::with_devices` adds it
        // once, beside them, and only then Merged Traffic exists.
        let (_dir, log, _) = archived_traffic_fixture();
        let config = Config::parse("listen_port: 8787\nrequest_timeout_seconds: 30\ncodex:\n  homes: []\nclaude:\n  config_dirs: []\n").unwrap();
        let first = std::sync::Mutex::new(None);
        let publish = |views: Vec<Merged>| {
            *first.lock().unwrap() = Some(serde_json::to_value(&views[0]).unwrap());
            Ok(())
        };
        let views = MergeCache::default()
            .views_selected_first(
                Vec::new(),
                this_device(config, log, BTreeMap::new()),
                false,
                Some((30, crate::traffic::TrafficScope::Model, &publish)),
            )
            .await
            .unwrap();
        let complete = views
            .iter()
            .find(|v| v.minutes == 30 && v.scope == crate::traffic::TrafficScope::Model)
            .unwrap();
        let complete = serde_json::to_value(complete).unwrap();
        assert_eq!(first.into_inner().unwrap().unwrap(), complete);
        for key in ["traffic", "local"] {
            assert!(complete.get(key).is_none(), "{key} must not be merged");
        }
        assert_eq!(complete["sources"], serde_json::json!([]));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn first_frame_does_not_read_archives_outside_its_range() {
        use std::os::unix::fs::PermissionsExt;
        // SAFETY: geteuid has no pointers or side effects.
        if unsafe { libc::geteuid() } == 0 {
            return; // Root ignores file permissions.
        }
        let (_dir, log, archive) = archived_traffic_fixture();
        std::fs::set_permissions(&archive, std::fs::Permissions::from_mode(0o000)).unwrap();
        let config = Config::parse("listen_port: 8787\nrequest_timeout_seconds: 30\ncodex:\n  homes: []\nclaude:\n  config_dirs: []\n").unwrap();
        let first = std::sync::Mutex::new(None);
        let publish = |views: Vec<Merged>| {
            *first.lock().unwrap() = Some(views[0].minutes);
            Ok(())
        };
        let result = MergeCache::default()
            .views_selected_first(
                Vec::new(),
                this_device(config, log, BTreeMap::new()),
                false,
                Some((30, crate::traffic::TrafficScope::Model, &publish)),
            )
            .await;
        // Only the complete 30-day views need the old archive.
        assert_eq!(*first.lock().unwrap(), Some(30));
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn local_archives_are_parsed_while_peers_answer() {
        let (_dir, log, archive) = archived_traffic_fixture();
        let config = Config::parse("listen_port: 8787\nrequest_timeout_seconds: 30\ncodex:\n  homes: []\nclaude:\n  config_dirs: []\n").unwrap();
        let peer_dir = tempfile::tempdir().unwrap();
        let peer_log = peer_dir.path().join("proxy.log");
        std::fs::write(&peer_log, "").unwrap();
        let bytes = crate::data_api::publish(
            &config,
            &peer_log,
            &DataKey::new(KEY).unwrap(),
            &uuid::Uuid::new_v4().to_string(),
            &[],
        )
        .unwrap();
        let complete: Summary = serde_json::from_slice(&bytes).unwrap();
        let mut selected = complete.clone();
        selected.schema_version = 4;
        selected.groups.clear();
        selected
            .windows
            .retain(|w| w.minutes == 30 && w.scope == crate::traffic::TrafficScope::Model);
        selected
            .previous_windows
            .retain(|w| w.minutes == 30 && w.scope == crate::traffic::TrafficScope::Model);
        // The Device holds its answer until released.
        let release = std::sync::Arc::new(tokio::sync::Notify::new());
        let held = release.clone();
        let summaries = Summaries::Scripted(std::sync::Arc::new(move |_, _| {
            let (selected, complete, held) = (selected.clone(), complete.clone(), held.clone());
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            tokio::spawn(async move {
                held.notified().await;
                let _ = tx.send(Ok(selected));
                let _ = tx.send(Ok(complete));
            });
            rx
        }));
        let source = device_source("peer");
        let refresh = tokio::spawn(async move {
            MergeCache::reading(summaries)
                .refresh_progressive(
                    vec![source],
                    this_device(config, log, BTreeMap::new()),
                    (30, crate::traffic::TrafficScope::Model),
                    |_| Ok(()),
                )
                .await
        });
        // The peer is still holding its answer; the archive is parsed meanwhile.
        tokio::time::timeout(Duration::from_secs(10), async {
            while crate::traffic::archive_scans(&archive) == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("The 30-day archive was not parsed while peers answered");
        release.notify_one();
        let views = tokio::time::timeout(Duration::from_secs(10), refresh)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(views.len(), 12);
        assert!(views.iter().all(|view| view.sources[0].included));
        assert_eq!(
            crate::traffic::archive_scans(&archive),
            1,
            "The complete views reuse it"
        );
    }

    #[tokio::test]
    async fn cached_views_stay_readable_while_a_refresh_waits_for_peers() {
        use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("proxy.log");
        std::fs::write(&log, "").unwrap();
        let config = Config::parse("listen_port: 8787\nrequest_timeout_seconds: 30\ncodex:\n  homes: []\nclaude:\n  config_dirs: []\n").unwrap();
        let bytes = crate::data_api::publish(
            &config,
            &log,
            &DataKey::new(KEY).unwrap(),
            &uuid::Uuid::new_v4().to_string(),
            &[],
        )
        .unwrap();
        let complete: Summary = serde_json::from_slice(&bytes).unwrap();
        let mut selected = complete.clone();
        selected.schema_version = 4;
        selected.groups.clear();
        selected
            .windows
            .retain(|w| w.minutes == 30 && w.scope == crate::traffic::TrafficScope::Model);
        selected
            .previous_windows
            .retain(|w| w.minutes == 30 && w.scope == crate::traffic::TrafficScope::Model);
        let source = device_source("peer");
        // The first read holds its answer until released.
        let accepted = std::sync::Arc::new(AtomicUsize::new(0));
        let waiting = std::sync::Arc::new(tokio::sync::Notify::new());
        let release = std::sync::Arc::new(tokio::sync::Notify::new());
        let summaries = {
            let (accepted, waiting, release) = (accepted.clone(), waiting.clone(), release.clone());
            Summaries::Scripted(std::sync::Arc::new(move |_, _| {
                let first = accepted.fetch_add(1, SeqCst) == 0;
                let (selected, complete) = (selected.clone(), complete.clone());
                let (waiting, release) = (waiting.clone(), release.clone());
                let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                tokio::spawn(async move {
                    if first {
                        waiting.notify_one();
                        release.notified().await;
                    }
                    let _ = tx.send(Ok(selected));
                    let _ = tx.send(Ok(complete));
                });
                rx
            }))
        };
        let cache = std::sync::Arc::new(tokio::sync::Mutex::new(MergeCache::reading(summaries)));
        let refresh = {
            let (cache, config, log, source) =
                (cache.clone(), config.clone(), log.clone(), source.clone());
            tokio::spawn(async move {
                MergeCache::refresh_shared(
                    &cache,
                    vec![source],
                    this_device(config, log, BTreeMap::new()),
                    (30, crate::traffic::TrafficScope::Model).into(),
                    |_| Ok(()),
                )
                .await
            })
        };
        tokio::time::timeout(Duration::from_secs(10), waiting.notified())
            .await
            .unwrap();
        // The peer is still holding its response: a passive read must not wait.
        let cached = tokio::time::timeout(Duration::from_secs(5), async {
            cache
                .lock()
                .await
                .views(
                    vec![source.clone()],
                    this_device(config.clone(), log.clone(), BTreeMap::new()),
                    false,
                )
                .await
        })
        .await
        .expect("A passive read waited for an explicit refresh")
        .unwrap();
        assert!(cached.iter().all(|view| !view.sources[0].included));
        assert_eq!(accepted.load(SeqCst), 1, "The passive read read the Device");
        release.notify_one();
        let refreshed = tokio::time::timeout(Duration::from_secs(10), refresh)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(refreshed.iter().all(|view| view.sources[0].included));
        let cached = cache
            .lock()
            .await
            .views(
                vec![source],
                this_device(config, log, BTreeMap::new()),
                false,
            )
            .await
            .unwrap();
        assert!(cached.iter().all(|view| view.sources[0].included));
    }

    #[tokio::test]
    async fn alignment_retries_never_reconnect_failed_sources() {
        use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("proxy.log");
        std::fs::write(&log, "").unwrap();
        let config = Config::parse("listen_port: 8787\nrequest_timeout_seconds: 30\ncodex:\n  homes: []\nclaude:\n  config_dirs: []\n").unwrap();
        let now = chrono::Utc::now().timestamp_millis() / 60_000 * 60_000;
        // The skewed Devices take turns answering at two boundaries.
        let skewed = AtomicUsize::new(0);
        let (summaries, reads) = scripted(move |source, _, _| {
            if source.name == "denied" {
                return vec![Err("Data endpoint rejected the request.".into())];
            }
            let end = now - (skewed.fetch_add(1, SeqCst) % 2) as i64 * 60_000;
            let mut summary = sample(&DataKey::new(KEY).unwrap(), "proxy", "upstream");
            summary.window_end = end;
            summary.window_start = end - 30 * 60_000;
            vec![Ok(summary)]
        });
        let sources = vec![
            device_source("peer-0"),
            device_source("peer-1"),
            device_source("denied"),
        ];
        let result = merge(
            sources,
            this_device(config, log, BTreeMap::new()),
            30,
            crate::traffic::TrafficScope::Model,
            &summaries,
        )
        .await;
        assert!(matches!(result, Err(error) if error == "TRAFFIC_UPDATING"));
        let reads = reads.lock().unwrap();
        assert_eq!(
            reads["peer-0"] + reads["peer-1"],
            6,
            "Skewed peers are retried"
        );
        assert_eq!(
            reads["denied"], 1,
            "A failed source must not be reconnected by alignment retries"
        );
    }

    #[tokio::test]
    async fn an_http_device_sends_the_selected_range_then_the_rest() {
        if data_key_in_subprocess(
            "data_client::tests::an_http_device_sends_the_selected_range_then_the_rest",
        ) {
            return;
        }
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("proxy.log");
        std::fs::write(&log, "").unwrap();
        let config = Config::parse("listen_port: 8787\nrequest_timeout_seconds: 30\ncodex:\n  homes: []\nclaude:\n  config_dirs: []\n").unwrap();
        let bytes = crate::data_api::publish(
            &config,
            &log,
            &DataKey::new(KEY).unwrap(),
            &uuid::Uuid::new_v4().to_string(),
            &[],
        )
        .unwrap();
        let mut selected: Summary = serde_json::from_slice(&bytes).unwrap();
        selected.schema_version = 4;
        selected.groups.clear();
        selected
            .windows
            .retain(|w| w.minutes == 1440 && w.scope == crate::traffic::TrafficScope::All);
        selected
            .previous_windows
            .retain(|w| w.minutes == 1440 && w.scope == crate::traffic::TrafficScope::All);
        let selected = serde_json::to_vec(&selected).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let source = Source {
            url: format!("http://{}", listener.local_addr().unwrap()),
            token_env: Some(DATA_KEY_ENV.into()),
            ..device_source("peer")
        };
        let server = tokio::spawn(async move {
            for request in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut header = Vec::new();
                while !header.ends_with(b"\r\n\r\n") {
                    header.push(socket.read_u8().await.unwrap());
                }
                let header = String::from_utf8(header).unwrap().to_lowercase();
                assert!(header.contains("x-coport-services: antigravity,grok"));
                let body = if request == 0 {
                    assert!(header.contains("x-coport-window: 1440:all"));
                    &selected
                } else {
                    assert!(!header.contains("x-coport-window"));
                    &bytes
                };
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes()).await.unwrap();
                socket.write_all(body).await.unwrap();
            }
        });
        let mut versions = Vec::new();
        Summaries::Network
            .read(
                &source,
                Some((1440, crate::traffic::TrafficScope::All)),
                |summary| {
                    versions.push(summary.schema_version);
                    Ok(())
                },
            )
            .await
            .unwrap();
        server.await.unwrap();
        assert_eq!(versions.len(), 2);
        assert_eq!(versions[0], 4);
        assert_ne!(versions[1], 4);
    }

    /// A Device named `name`. A scripted read never connects to it.
    fn device_source(name: &str) -> Source {
        Source {
            name: name.into(),
            device_id: None,
            transport: Transport::Http,
            ssh_connection: None,
            url: "http://127.0.0.1:1".into(),
            token_env: None,
            token_file: None,
            ca_certificate: None,
            ssh_device: None,
        }
    }

    /// Summaries answered at once by `reply`, given the Device, the range
    /// asked for and how many reads of that Device came before. The reads of
    /// each Device are counted in the returned map.
    fn scripted(
        reply: impl Fn(&Source, Option<Selection>, usize) -> Vec<Result<Summary, String>>
        + Send
        + Sync
        + 'static,
    ) -> (
        Summaries,
        std::sync::Arc<std::sync::Mutex<BTreeMap<String, usize>>>,
    ) {
        let reads = std::sync::Arc::<std::sync::Mutex<BTreeMap<String, usize>>>::default();
        let counted = reads.clone();
        let summaries = Summaries::Scripted(std::sync::Arc::new(move |source, selection| {
            let attempt = {
                let mut reads = counted.lock().unwrap();
                let count = reads.entry(source.name.clone()).or_default();
                *count += 1;
                *count - 1
            };
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            for reply in reply(source, selection, attempt) {
                let _ = tx.send(reply);
            }
            rx
        }));
        (summaries, reads)
    }

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
        alignment_fixture(true).await;
    }
    #[tokio::test]
    async fn failed_alignment_stops_after_three_rounds_for_all_views() {
        alignment_fixture(false).await;
    }
    async fn alignment_fixture(recover: bool) {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("proxy.log");
        std::fs::write(&log, "").unwrap();
        let now = chrono::Utc::now().timestamp_millis() / 60_000 * 60_000;
        let aligned = now - 60_000;
        let nodes = [
            uuid::Uuid::new_v4().to_string(),
            uuid::Uuid::new_v4().to_string(),
        ];
        // The second Device answers a minute ahead until its third read, if ever.
        let (summaries, reads) = scripted(move |source, _, attempt| {
            let peer = usize::from(source.name == "peer-1");
            let end = if peer == 1 && (attempt < 2 || !recover) {
                now
            } else {
                aligned
            };
            let windows = crate::data_api::RANGES
                .into_iter()
                .flat_map(|minutes| {
                    [
                        crate::traffic::TrafficScope::All,
                        crate::traffic::TrafficScope::Model,
                    ]
                    .into_iter()
                    .map(move |scope| crate::data_api::Window {
                        minutes,
                        scope,
                        window_start: end - minutes as i64 * 60_000,
                        window_end: end,
                        bucket_minutes: crate::data_api::bucket_minutes(minutes).unwrap(),
                        groups: Vec::new(),
                    })
                })
                .collect();
            let summary = Summary {
                schema_version: 2,
                node_id: nodes[peer].clone(),
                window_start: end - 30 * 60_000,
                window_end: end,
                bucket_minutes: 1,
                groups: Vec::new(),
                windows,
                previous_windows: Vec::new(),
            };
            summary.validate().unwrap();
            vec![Ok(summary)]
        });
        let sources = vec![device_source("peer-0"), device_source("peer-1")];
        let config = Config::parse("listen_port: 8787\nrequest_timeout_seconds: 30\ncodex:\n  homes: []\nclaude:\n  config_dirs: []\n").unwrap();
        let before = crate::traffic::live_scans(&log);
        let result = merge_views(
            sources,
            this_device(config, log.clone(), BTreeMap::new()),
            summaries,
        )
        .await;
        if recover {
            let views = result.unwrap();
            assert_eq!(views.len(), 12);
            for view in views {
                assert_eq!(view.window_end, aligned);
                assert!(view.sources.iter().all(|source| source.included));
                let json = serde_json::to_value(view).unwrap();
                assert!(json.get("groups").is_none());
                // This device's traffic is its card's; merging only labels peers.
                assert!(json.get("traffic").is_none());
                assert!(json.get("local").is_none());
                assert!(json.get("sources").is_some());
            }
            // Only the identity-evidence scan reads this device's log.
            assert_eq!(crate::traffic::live_scans(&log) - before, 1);
        } else {
            assert!(matches!(result, Err(error) if error == "TRAFFIC_UPDATING"));
            assert_eq!(crate::traffic::live_scans(&log) - before, 1);
        }
        let reads = reads.lock().unwrap();
        assert_eq!(reads.len(), 2);
        assert!(reads.values().all(|count| *count == 3), "{reads:?}");
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
        assert_eq!(view.metrics.avg_ms, Some(250));
        assert_eq!(view.metrics.cache_hit_rate, Some(0.3));
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
    fn output_tps_merges_paired_totals_without_older_devices_diluting_them() {
        let mut total = empty_stats(30);
        for (tokens, duration) in [
            (Some(100), Some(2000)),
            (Some(300), Some(8000)),
            (None, None),
            (Some(0), Some(0)),
        ] {
            let mut stats = empty_stats(30);
            stats.output_tps_tokens = tokens;
            stats.output_tps_duration_ms = duration;
            stats.output_tokens = Some(9999);
            stats.latency_total_ms = 99999;
            add_stats(&mut total, &stats);
        }
        assert_eq!(total.output_tps_tokens, Some(400));
        assert_eq!(total.output_tps_duration_ms, Some(10000));
        assert_eq!(
            crate::data_api::output_tps(total.output_tps_tokens, total.output_tps_duration_ms),
            Some(40.0)
        );
        assert_eq!(crate::data_api::output_tps(None, None), None);
        assert_eq!(crate::data_api::output_tps(Some(0), Some(0)), None);
    }

    #[test]
    fn first_token_averages_the_devices_that_report_it() {
        let id = uuid::Uuid::new_v4().to_string();
        let reference = crate::data_api::account_reference("Codex", &id).unwrap();
        let accounts = BTreeMap::from([(reference.clone(), "work".into())]);
        let device = |key: &str, proxy, upstream, first_token: Option<(u64, u64)>| {
            let mut summary = sample(&DataKey::new(key).unwrap(), proxy, upstream);
            summary.groups[0].account_ref = Some(reference.clone());
            summary.groups[0].stats.requests = 2;
            summary.groups[0].stats.counts[0] = 2;
            summary.groups[0].stats.first_token_total_ms = first_token.map(|t| t.0);
            summary.groups[0].stats.first_token_samples = first_token.map(|t| t.1);
            summary.groups[0].stats.forwarded_first_token_total_ms = first_token.map(|t| t.0 / 3);
            summary.groups[0].stats.forwarded_first_token_samples = first_token.map(|t| t.1);
            summary.groups[0].stats.cooldown_total_ms = first_token.map(|t| t.0 / 2);
            summary.groups[0].stats.cooldown_samples = first_token.map(|t| t.1);
            summary
        };
        let current = device(KEY, "http://10.1.1.1", "https://a.example", Some((900, 2)));
        let quiet = device(
            &"e".repeat(64),
            "http://10.3.3.3",
            "https://c.example",
            Some((0, 0)),
        );
        // A device from before First Token reports nothing, not zero.
        let older = device(
            &"f".repeat(64),
            "http://10.2.2.2",
            "https://b.example",
            None,
        );
        let view = |summaries: &[&crate::data_api::Summary]| {
            let maps = (BTreeMap::new(), BTreeMap::new());
            let mut groups = BTreeMap::new();
            for summary in summaries {
                add_groups(
                    &mut groups,
                    &maps,
                    &summary.node_id,
                    &summary.groups,
                    &accounts,
                );
            }
            let view = traffic_view(&groups.into_values().collect::<Vec<_>>(), 30);
            serde_json::to_value(&view).unwrap()
        };
        let merged = view(&[&current, &older, &quiet]);
        assert_eq!(merged["requests"], 6);
        assert_eq!(merged["firstTokenMs"], 450);
        assert_eq!(merged["firstTokenSamples"], 2);
        for stats in [&merged, &merged["credentials"][0]] {
            assert_eq!(stats["forwardedFirstTokenTotalMs"], 300);
            assert_eq!(stats["forwardedFirstTokenSamples"], 2);
            assert_eq!(stats["cooldownTotalMs"], 450);
            assert_eq!(stats["cooldownSamples"], 2);
        }
        assert_eq!(merged["credentials"][0]["firstTokenMs"], 450);
        let older = view(&[&older]);
        assert!(older.get("firstTokenSamples").is_none());
        assert!(older.get("forwardedFirstTokenSamples").is_none());
        assert!(older.get("cooldownSamples").is_none());
        assert!(older["firstTokenMs"].is_null());
        assert!(older["credentials"][0].get("firstTokenSamples").is_none());
        let quiet = view(&[&quiet]);
        assert_eq!(quiet["firstTokenSamples"], 0);
        assert!(quiet["firstTokenMs"].is_null());
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
        let labels = config.local_traffic_credential_labels().await;
        let view = local_traffic(&config, &log, &labels, 30);
        let credentials = view["credentials"].as_array().unwrap();
        assert_eq!(credentials.len(), 1);
        assert_eq!(credentials[0]["credential"], "Known local account");
        assert_eq!(view["requests"], 1);
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
            &[],
        )
        .unwrap();
        let encoded = std::str::from_utf8(&body).unwrap();
        for secret in ["Shared", bases[0], bases[1], token] {
            assert!(!encoded.contains(secret));
        }
        let summary: Summary = serde_json::from_slice(&body).unwrap();
        assert_eq!(summary.groups.len(), 2);
        let labels = config.local_traffic_credential_labels().await;
        let context = prepare_merge(this_device(config.clone(), log.clone(), labels.clone()))
            .await
            .unwrap();
        // Merged Traffic adds accounts by name, so this device's card must
        // name them exactly as the peer's view does.
        let local = local_traffic(&config, &log, &labels, 30);
        let local = local["credentials"].as_array().unwrap();
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
        for group in local {
            let label = group["credential"].as_str().unwrap();
            assert!(label.starts_with("Shared (https://"));
            let matched = remote
                .credentials
                .iter()
                .find(|g| g.credential == label)
                .unwrap();
            assert_ne!(matched.credential, crate::traffic_identity::UNIDENTIFIED);
            assert_eq!(group["requests"], matched.stats.requests);
            assert_eq!(group["bytes"], matched.stats.bytes);
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
            &[],
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
            &[],
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
        let labels = config.local_traffic_credential_labels().await;
        let (config_for_views, labels_for_views) = (config.clone(), labels.clone());
        let views = merge_views(
            vec![source],
            this_device(config, log.clone(), labels),
            Summaries::Network,
        )
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
        let local = local_traffic(&config_for_views, &log, &labels_for_views, 360);
        assert_eq!(local["counts"].as_array().unwrap().len(), 24);
        assert_eq!(local["requests"], 1);
        let remote = result.sources[0].traffic.as_ref().unwrap();
        assert_eq!(remote.stats.requests, 1);
        assert_eq!(remote.metrics.avg_ms, Some(200));
        assert_eq!(remote.metrics.cache_hit_rate, Some(0.3));
        assert_eq!(remote.credentials[0].credential, "known@example.invalid");
        // The card names the account as the peer does, so Merged Traffic adds
        // both into one account.
        assert_eq!(
            local["credentials"][0]["credential"],
            "known@example.invalid"
        );
    }
    #[tokio::test]
    async fn renamed_claude_review_survives_on_this_device_card() {
        let dir = tempfile::tempdir().unwrap();
        let base = "https://renamed-review.invalid";
        std::fs::write(
            dir.path().join("myServer.json"),
            serde_json::json!({
                "env": {"ANTHROPIC_BASE_URL": base, "ANTHROPIC_AUTH_TOKEN": "test-key"}
            })
            .to_string(),
        )
        .unwrap();
        let config = Config::parse(&format!(
            "listen_port: 8787\nrequest_timeout_seconds: 30\ncodex:\n  homes: []\nclaude:\n  config_dirs: [{}]\n  routing:\n    api_key: {{myServer: none}}\n",
            serde_json::to_string(dir.path()).unwrap()
        )).unwrap();
        let end = chrono::Utc::now().timestamp_millis() / 60_000 * 60_000;
        let log = dir.path().join("traffic.log");
        std::fs::write(&log, serde_json::json!({
            "timestamp": chrono::DateTime::from_timestamp_millis(end - 1000).unwrap().to_rfc3339(),
            "event": "model_call_finished", "model_call_id": "renamed-call",
            "request_id": "renamed-request", "service": "claude", "provider": "api",
            "upstream_base_url": base, "path": "/v1/messages", "method": "POST", "status": "200"
        }).to_string() + "\n").unwrap();
        // This device's card carries the review that Merged Traffic keeps.
        let value = local_traffic(&config, &log, &BTreeMap::new(), 30);
        assert_eq!(value["requests"], 1);
        assert_eq!(value["credentials"][0]["credential"], "myServer");
        assert_eq!(value["credentials"][0]["sources"][0]["name"], "api");
        assert_eq!(value["credentials"][0]["sources"][0]["reason"], "renamed");
        assert_eq!(value["targets"][0]["name"], "myServer");
    }

    #[tokio::test]
    async fn duplicate_sources_are_excluded_without_connection_errors() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("empty.log");
        std::fs::write(&log, "").unwrap();
        let config = Config::parse("listen_port: 8787\nrequest_timeout_seconds: 30\ncodex:\n  homes: []\nclaude:\n  config_dirs: []\n").unwrap();
        let summary = sample(
            &DataKey::new(KEY).unwrap(),
            "none",
            "https://example.invalid",
        );
        let end = summary.window_end;
        let context = MergeContext {
            device: ThisDevice {
                config,
                log: log.clone(),
                labels: BTreeMap::new(),
                node_id: "local-test".into(),
            },
            accounts: BTreeMap::new(),
            at: end,
        };
        let source = |name: &str| Source {
            name: name.into(),
            transport: Transport::Http,
            url: "http://127.0.0.1:1".into(),
            token_env: None,
            token_file: None,
            ca_certificate: None,
            device_id: None,
            ssh_connection: None,
            ssh_device: None,
        };
        let fetched = vec![
            (source("first"), Ok(summary.clone())),
            (source("second"), Ok(summary)),
            (source("failed"), Err("connection refused".into())),
        ];
        let merged = merge_fetched(
            &context,
            &fetched,
            30,
            crate::traffic::TrafficScope::Model,
            end,
        )
        .unwrap();
        let first = merged.sources.iter().find(|s| s.name == "first").unwrap();
        assert!(first.included && first.error.is_none() && first.exclusion.is_none());
        assert_eq!(first.traffic.as_ref().unwrap().stats.requests, 1);
        let duplicate = merged.sources.iter().find(|s| s.name == "second").unwrap();
        assert!(!duplicate.included && duplicate.error.is_none());
        assert_eq!(duplicate.exclusion, Some(SourceExclusion::Duplicate));
        assert_eq!(
            serde_json::to_value(duplicate).unwrap()["exclusion"],
            "duplicate"
        );
        let failed = merged.sources.iter().find(|s| s.name == "failed").unwrap();
        assert!(failed.error.is_some() && failed.exclusion.is_none());
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
    #[tokio::test]
    async fn forwarded_traffic_labels_accounts_like_devices_without_connecting_upstream() {
        let dir = tempfile::tempdir().unwrap();
        // Every upstream route goes through a trap proxy: labelling must never connect.
        let trap = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = "https://private.example/v1";
        std::fs::write(dir.path().join("config.toml"), format!("[model_providers.Trap]\nname = \"Trap\"\nbase_url = \"{base}\"\nenv_key = \"FORWARDED_TEST_KEY\"\n")).unwrap();
        std::fs::write(
            dir.path().join(".env"),
            "FORWARDED_TEST_KEY=sk-synthetic-forwarded-0123456789ABCDEFGHIJKLMNOP\n",
        )
        .unwrap();
        let config = Config::parse(&format!("listen_port: 8787\nrequest_timeout_seconds: 30\nproxies:\n  trap: http://{}\ncodex:\n  homes: [{}]\n  routing:\n    account: {{'Known local account': trap}}\n    api_key: {{Trap: trap}}\nclaude:\n  config_dirs: []\n", trap.local_addr().unwrap(), serde_json::to_string(dir.path()).unwrap())).unwrap();
        let labels = config.local_traffic_credential_labels().await;
        let account = "00000000-0000-4000-8000-000000000123";
        let log = dir.path().join("proxy.log");
        let evidence = serde_json::json!({"timestamp":(chrono::Utc::now()-chrono::Duration::minutes(3)).to_rfc3339(),"event":"model_call_finished","service":"codex","model_call_id":"one","account_id":account,"account_label":"Known local account","status":"200"});
        std::fs::write(&log, format!("{evidence}\n")).unwrap();
        let provider = config
            .traffic_provider_references()
            .await
            .remove(0)
            .reference;
        let end = chrono::Utc::now().timestamp_millis() / 60_000 * 60_000;
        let group = |account_ref: Option<String>, requests: u64| {
            let mut counts = vec![0; 30];
            counts[29] = requests;
            Group {
                service: Service::Codex,
                proxy_ref: "a".repeat(64),
                upstream_ref: "b".repeat(64),
                account_ref,
                stats: Stats {
                    requests,
                    bytes: requests,
                    counts,
                    error_counts: vec![0; 30],
                    token_counts: vec![0; 30],
                    ..Default::default()
                },
            }
        };
        let remote = crate::remote_forward::RemoteWindow {
            device_id: "ms".into(),
            name: "MS".into(),
            fetched_at: 1,
            error: Some("statistics offline".into()),
            node_id: uuid::Uuid::new_v4().to_string(),
            window: crate::data_api::Window {
                minutes: 30,
                scope: crate::traffic::TrafficScope::Model,
                window_start: end - 30 * 60_000,
                window_end: end,
                bucket_minutes: 1,
                groups: vec![
                    group(crate::data_api::account_reference("Codex", account), 3),
                    group(Some(provider), 2),
                    group(None, 1),
                ],
            },
        };
        let view = forwarded_traffic(
            remote.clone(),
            this_device(config.clone(), log.clone(), labels.clone()),
        )
        .await
        .unwrap();
        let view = serde_json::to_value(view).unwrap();
        let credentials: Vec<_> = view["traffic"]["credentials"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| {
                (
                    c["credential"].as_str().unwrap(),
                    c["requests"].as_u64().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            credentials,
            [("Known local account", 3), ("Trap", 2), ("Unidentified", 1)]
        );
        assert_eq!(view["traffic"]["requests"], 6);
        assert_eq!(view["windowEnd"], end);
        assert_eq!(view["error"], "statistics offline");
        // A window whose buckets do not match its range is rejected.
        let mut invalid = remote;
        invalid.window.bucket_minutes = 10;
        assert!(
            forwarded_traffic(invalid, this_device(config, log, labels))
                .await
                .is_err()
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(100), trap.accept())
                .await
                .is_err(),
            "Forwarded traffic must not connect to an upstream"
        );
    }

    #[tokio::test]
    async fn antigravity_history_matches_known_accounts_in_devices_and_forwarding() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::parse("listen_port: 8787\nrequest_timeout_seconds: 30\ncodex:\n  homes: []\nclaude:\n  config_dirs: []\nantigravity:\n  saved_login: false\n  account_auth_file_only: false\n  routing:\n    account:\n      'Alice@Example.com': none\n").unwrap();
        let log = dir.path().join("remote.log");
        let end = chrono::Utc::now().timestamp_millis() / 60_000 * 60_000;
        let timestamp = chrono::DateTime::from_timestamp_millis(end - 60_000).unwrap();
        let mut lines = String::new();
        for (index, email) in [
            "alice@example.com",
            "ALICE@example.com",
            "unknown@example.com",
            "",
        ]
        .into_iter()
        .enumerate()
        {
            // Historical records: no credential proof or extra identity fields.
            let record = serde_json::json!({"timestamp":timestamp.to_rfc3339(), "event":"model_call_finished", "service":"antigravity", "model_call_id":format!("call-{index}"), "request_id":format!("request-{index}"), "account_id":email, "provider":"default", "status":"200", "received_bytes":"10", "input_tokens":"7", "output_tokens":"2"});
            lines.push_str(&format!("{record}\n"));
        }
        std::fs::write(&log, &lines).unwrap();
        let context = crate::traffic::ExportIdentities::from_config(&config, &[]);
        let key = DataKey::new(KEY).unwrap();
        let options = crate::traffic::ExportOptions {
            end,
            minutes: 30,
            scope: crate::traffic::TrafficScope::Model,
            limit: 24,
        };
        let groups = crate::traffic::export_window_limit(
            crate::traffic::ReadSource::Path(&log),
            &config,
            &key,
            options,
            &context,
        )
        .unwrap();
        let serialized = serde_json::to_string(&groups).unwrap();
        assert!(!serialized.contains("example.com"));
        assert!(!serialized.contains("default"));
        assert_eq!(std::fs::read_to_string(&log).unwrap(), lines);

        // The reader knows the email from configuration only, with different case.
        let local_log = dir.path().join("local.log");
        std::fs::write(&local_log, "").unwrap();
        let labels = config.local_traffic_credential_labels().await;
        let merge = prepare_merge_range(
            this_device(config.clone(), local_log.clone(), labels.clone()),
            30,
        )
        .await
        .unwrap();
        let mut device_groups = BTreeMap::new();
        add_groups(
            &mut device_groups,
            &(BTreeMap::new(), BTreeMap::new()),
            "remote",
            &groups,
            &merge.accounts,
        );
        let devices = traffic_view(&device_groups.into_values().collect::<Vec<_>>(), 30);
        let remote = crate::remote_forward::RemoteWindow {
            device_id: "remote".into(),
            name: "Remote".into(),
            fetched_at: 1,
            error: None,
            node_id: uuid::Uuid::new_v4().to_string(),
            window: crate::data_api::Window {
                minutes: 30,
                scope: options.scope,
                window_start: end - 30 * 60_000,
                window_end: end,
                bucket_minutes: 1,
                groups,
            },
        };
        let forwarded = forwarded_traffic(remote, this_device(config, local_log, labels))
            .await
            .unwrap();
        let mut devices = devices;
        devices.window = forwarded.traffic.window;
        assert_eq!(
            forwarded.traffic.window.end - forwarded.traffic.window.start,
            30 * 60_000
        );
        assert_eq!(
            serde_json::to_value(&devices).unwrap(),
            serde_json::to_value(&forwarded.traffic).unwrap()
        );
        let values = serde_json::to_value(devices).unwrap();
        let credentials = values["credentials"].as_array().unwrap();
        assert_eq!(credentials.len(), 2);
        assert_eq!(credentials[0]["credential"], "Alice@Example.com");
        assert_eq!(credentials[0]["requests"], 2);
        assert_eq!(credentials[1]["credential"], "Unidentified");
        assert_eq!(credentials[1]["requests"], 2);
        assert_eq!(values["requests"], 4);
    }

    #[tokio::test]
    async fn a_summary_reused_from_forwarding_opens_no_connection() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("proxy.log");
        std::fs::write(&log, "").unwrap();
        let config = Config::parse("listen_port: 8787\nrequest_timeout_seconds: 30\ncodex:\n  homes: []\nclaude:\n  config_dirs: []\n").unwrap();
        // Reading the forwarded device again fails, as an unreachable one would.
        let (summaries, reads) = scripted(|_, _, _| vec![Err("Data connection failed".into())]);
        let source = Source {
            device_id: Some("fw".into()),
            ..device_source("Forwarded")
        };
        let end = chrono::Utc::now().timestamp_millis() / 60_000 * 60_000;
        let mut summary = sample(&DataKey::new(KEY).unwrap(), "proxy", "upstream");
        summary.window_end = end;
        summary.window_start = end - 30 * 60_000;
        let refresh = |summary: Summary| Refresh {
            minutes: 30,
            scope: crate::traffic::TrafficScope::Model,
            reused: BTreeMap::from([("fw".to_owned(), summary)]),
        };
        let cache = tokio::sync::Mutex::new(MergeCache::reading(summaries));
        let views = MergeCache::refresh_shared(
            &cache,
            vec![source.clone()],
            this_device(config.clone(), log.clone(), BTreeMap::new()),
            refresh(summary.clone()),
            |_| Ok(()),
        )
        .await
        .unwrap();
        let view = views
            .iter()
            .find(|v| v.minutes == 30 && v.scope == crate::traffic::TrafficScope::Model)
            .unwrap();
        assert!(view.sources[0].included, "{:?}", view.sources[0].error);
        assert_eq!(view.sources[0].traffic.as_ref().unwrap().stats.requests, 1);
        assert!(
            reads.lock().unwrap().is_empty(),
            "A reused summary must not be read again"
        );
        // Nothing was queried, so no device query is recorded.
        assert!(
            !std::fs::read_to_string(&log)
                .unwrap()
                .contains("device_statistics")
        );
        // A summary too old to validate is not reused; the device is read as usual.
        let mut stale = summary;
        stale.window_end -= 10 * 60_000;
        stale.window_start -= 10 * 60_000;
        let result = MergeCache::refresh_shared(
            &cache,
            vec![source],
            this_device(config, log, BTreeMap::new()),
            refresh(stale),
            |_| Ok(()),
        )
        .await;
        assert_eq!(
            reads.lock().unwrap()["Forwarded"],
            1,
            "A stale summary must be read again"
        );
        assert!(!result.unwrap()[0].sources[0].included);
    }
}
