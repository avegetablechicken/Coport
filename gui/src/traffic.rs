//! Activity traffic is read from retained logs, independently of the capped UI list.
use crate::logs::Entry;
use crate::traffic_identity::{Identities, Reason, Resolution, Source, Target, UNIDENTIFIED};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs::File,
    io::{BufRead, BufReader},
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

/// Endpoint category, not an assertion that the upstream actually charged quota.
#[derive(Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum TrafficScope {
    All,
    #[default]
    Model,
}

fn included_in_scope(entry: &Entry, scope: TrafficScope) -> bool {
    let model_event = entry.event.starts_with("model_call_");
    match scope {
        TrafficScope::All => !model_event,
        TrafficScope::Model => model_event || is_historical_http_call(entry),
    }
}

fn is_historical_http_call(entry: &Entry) -> bool {
    // One HTTP model request is one call even before per-call events existed.
    // Upgraded GET connections cannot tell us how many generations occurred.
    !entry.event.starts_with("model_call_")
        && !entry.fields.contains_key("model_call_id")
        && entry.get("method") == Some("POST")
        && coport::model_calls::is_model_endpoint("POST", entry.get("path").unwrap_or(""))
}

// Later lifecycle stages supply the outcome and credential, but a request is
// counted from its first recorded event, including an open WebSocket connection.
fn lifecycle_rank(entry: &Entry) -> u8 {
    if entry.is_request_end() || entry.is_model_call_end() {
        return 4;
    }
    match entry.event.as_str() {
        "upstream_response" | "model_call_updated" => 3,
        "route_selected" => 2,
        "request_received" | "model_call_started" => 1,
        _ => 0,
    }
}

/// The fields traffic reads from log records; file summaries keep only these.
const FIELDS: [&str; 19] = [
    "account_id",
    "credential_ref",
    "account_label",
    "cache_creation_input_tokens",
    "cached_input_tokens",
    "duration_ms",
    "input_tokens",
    "method",
    "model_call_id",
    "output_tokens",
    "path",
    "provider",
    "received_bytes",
    "request_id",
    "service",
    "status",
    "upstream_base_url",
    "proxy_endpoint",
    "proxy",
];

/// A request or model call merged from its lifecycle records. The most
/// advanced stage names the outcome, and each field comes from the most
/// advanced record holding it, the one read first on a tie. Merging is
/// associative, so per-file summaries combine to the same result.
#[derive(Clone)]
struct Record {
    time: Option<DateTime<Local>>,
    rank: u8,
    event: Arc<str>,
    /// Index in `FIELDS`, rank of the record supplying it, and value.
    fields: Vec<(u8, u8, Arc<str>)>,
}

impl Record {
    fn new(entry: &Entry, strings: &mut Strings) -> Self {
        let rank = lifecycle_rank(entry);
        Self {
            time: entry.time,
            rank,
            event: strings.get(&entry.event),
            fields: FIELDS
                .iter()
                .enumerate()
                .filter_map(|(key, name)| Some((key as u8, rank, strings.get(entry.get(name)?))))
                .collect(),
        }
    }

    /// Adds a record read after this one.
    fn merge(&mut self, later: &Record) {
        self.time = self.time.into_iter().chain(later.time).min();
        if later.rank > self.rank {
            self.rank = later.rank;
            self.event = later.event.clone();
        }
        for (key, rank, value) in &later.fields {
            match self.fields.iter_mut().find(|field| field.0 == *key) {
                Some(field) if field.1 >= *rank => {}
                Some(field) => *field = (*key, *rank, value.clone()),
                None => self.fields.push((*key, *rank, value.clone())),
            }
        }
    }

    fn entry(&self) -> Entry {
        Entry {
            seq: 0,
            time: self.time,
            event: self.event.to_string(),
            fields: self
                .fields
                .iter()
                .map(|(key, _, value)| {
                    (
                        FIELDS[*key as usize].to_owned(),
                        Value::String(value.to_string()),
                    )
                })
                .collect(),
        }
    }
}

/// Shares the values a file repeats, such as services, paths and upstreams,
/// so cached summaries stay small.
#[derive(Default)]
struct Strings(HashSet<Arc<str>>);

impl Strings {
    fn get(&mut self, value: &str) -> Arc<str> {
        if let Some(shared) = self.0.get(value) {
            return shared.clone();
        }
        let shared: Arc<str> = value.into();
        self.0.insert(shared.clone());
        shared
    }
}

/// What one log file contributes to traffic in one scope, before the time
/// window and the current configuration apply.
#[derive(Default)]
struct FileTraffic {
    scan_end: i64,
    latest_event: Option<i64>,
    /// Logged account labels by service and ID, each at its first sighting.
    evidence: Vec<(String, String, String)>,
    requests: BTreeMap<String, Record>,
    /// Request ends without an ID, with a hash of the whole logged record so
    /// a line read from two files counts once.
    uncorrelated: Vec<(u64, Record)>,
    explicit_call_requests: HashSet<String>,
}

impl FileTraffic {
    fn scan_many(file: File, selections: &[(TrafficScope, i64)]) -> Result<Vec<Self>, String> {
        let mut traffic: Vec<_> = selections
            .iter()
            .map(|(_, end)| Self {
                scan_end: *end,
                ..Self::default()
            })
            .collect();
        let mut sighted: Vec<HashSet<_>> = selections.iter().map(|_| HashSet::new()).collect();
        let mut strings = Strings::default();
        for_each_line(file, |entry| {
            for ((traffic, sighted), (scope, _)) in
                traffic.iter_mut().zip(&mut sighted).zip(selections)
            {
                traffic.observe(&entry, *scope, sighted, &mut strings);
            }
        })?;
        Ok(traffic)
    }
    fn observe(
        &mut self,
        entry: &Entry,
        scope: TrafficScope,
        sighted: &mut HashSet<(String, String, String)>,
        strings: &mut Strings,
    ) {
        // Retain safe identity evidence even when the corresponding request is
        // outside the selected window. Current configuration mappings win.
        if let (Some(id), Some(label)) = (entry.get("account_id"), entry.get("account_label"))
            && !id.is_empty()
            && !label.is_empty()
        {
            let service = entry
                .service()
                .or_else(|| entry.get("service"))
                .unwrap_or("Unknown");
            let evidence = (service.to_owned(), id.to_owned(), label.to_owned());
            if sighted.insert(evidence.clone()) {
                self.evidence.push(evidence);
            }
        }
        if !included_in_scope(entry, scope) || lifecycle_rank(entry) == 0 {
            return;
        }
        let Some(at) = entry.time.map(|t| t.timestamp_millis()) else {
            return;
        };
        // Even excluded events constrain reuse at another snapshot boundary.
        self.latest_event = Some(self.latest_event.map_or(at, |last| last.max(at)));
        if at >= self.scan_end {
            return;
        }
        let call = entry.event.starts_with("model_call_");
        if let Some(id) = entry
            .get(if call { "model_call_id" } else { "request_id" })
            .filter(|id| !id.is_empty())
        {
            if call && let Some(request_id) = entry.get("request_id").filter(|id| !id.is_empty()) {
                self.explicit_call_requests.insert(request_id.to_owned());
            }
            let id = format!("{}:{id}", if call { "call" } else { "request" });
            let record = Record::new(entry, strings);
            match self.requests.get_mut(&id) {
                Some(current) => current.merge(&record),
                None => {
                    self.requests.insert(id, record);
                }
            }
        } else if entry.is_request_end()
            && (scope == TrafficScope::All || is_historical_http_call(entry))
        {
            // Uncorrelated connection events can only be counted separately.
            // New model-call events always require their explicit call ID.
            use std::hash::{Hash, Hasher};
            let mut hash = std::hash::DefaultHasher::new();
            (
                &entry.event,
                entry.time,
                serde_json::to_string(&entry.fields).unwrap_or_default(),
            )
                .hash(&mut hash);
            self.uncorrelated
                .push((hash.finish(), Record::new(entry, strings)));
        }
    }
}

/// A log file that no longer changes, by its identity and contents' stamp.
#[derive(Clone, PartialEq, Eq, Hash)]
struct FileStamp {
    #[cfg(unix)]
    node: (u64, u64),
    #[cfg(not(unix))]
    path: std::path::PathBuf,
    len: u64,
    modified: Option<std::time::SystemTime>,
}

/// Summaries of rotated files, which do not change once written. Unused ones
/// are dropped, so a long range read once does not stay in memory.
type Summaries = HashMap<(FileStamp, bool), (Instant, Arc<FileTraffic>)>;
static SUMMARIES: Mutex<Option<Summaries>> = Mutex::new(None);
#[cfg(test)]
static LIVE_SCANS: Mutex<Option<HashMap<std::path::PathBuf, usize>>> = Mutex::new(None);
#[cfg(test)]
pub(crate) fn live_scans(path: &Path) -> usize {
    LIVE_SCANS
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|counts| counts.get(path))
        .copied()
        .unwrap_or(0)
}
const SUMMARY_TTL: Duration = Duration::from_secs(600);

struct SnapshotFile {
    summary: Arc<FileTraffic>,
    archived_modified: Option<i64>,
}

/// One immutable scan per scope/end shared by all ranges in a refresh.
pub(crate) struct Snapshot {
    end: i64,
    all: Vec<SnapshotFile>,
    model: Vec<SnapshotFile>,
}
impl Snapshot {
    pub(crate) fn load(path: &Path, end: i64) -> Result<Self, String> {
        Self::load_range(path, end, 43200)
    }
    pub(crate) fn load_range(path: &Path, end: i64, minutes: u64) -> Result<Self, String> {
        Ok(Self::load_many(
            path,
            &[end],
            minutes,
            &[TrafficScope::All, TrafficScope::Model],
        )?
        .remove(0))
    }
    /// Read and parse each file once while retaining separate lifecycle state
    /// for every scope/boundary. Later events must not leak into older windows.
    pub(crate) fn load_many(
        path: &Path,
        ends: &[i64],
        minutes: u64,
        scopes: &[TrafficScope],
    ) -> Result<Vec<Self>, String> {
        let Some(first) = ends.iter().min() else {
            return Ok(Vec::new());
        };
        let selections: Vec<_> = ends
            .iter()
            .flat_map(|end| scopes.iter().map(move |scope| (*scope, *end)))
            .collect();
        let mut files =
            file_summaries_many(path, first - minutes as i64 * 60_000, &selections)?.into_iter();
        Ok(ends
            .iter()
            .map(|end| {
                let mut snapshot = Self {
                    end: *end,
                    all: Vec::new(),
                    model: Vec::new(),
                };
                for scope in scopes {
                    let selected = files.next().unwrap();
                    match scope {
                        TrafficScope::All => snapshot.all = selected,
                        TrafficScope::Model => snapshot.model = selected,
                    }
                }
                snapshot
            })
            .collect())
    }
}
#[derive(Clone, Copy)]
pub(crate) enum ReadSource<'a> {
    Path(&'a Path),
    Snapshot(&'a Snapshot),
}
impl ReadSource<'_> {
    fn entries(
        self,
        start: i64,
        end: i64,
        scope: TrafficScope,
        labels: &mut BTreeMap<(String, String), String>,
    ) -> Result<Vec<Entry>, String> {
        match self {
            Self::Path(path) => processed_entries(path, start, end, scope, labels),
            Self::Snapshot(snapshot) => {
                if end != snapshot.end {
                    return Err("Traffic snapshot boundary differs".into());
                }
                let files = match scope {
                    TrafficScope::All => &snapshot.all,
                    TrafficScope::Model => &snapshot.model,
                };
                Ok(entries_from_files(files, start, end, scope, labels))
            }
        }
    }
}

/// Each file's contribution, in file order; see `for_each_file`. A summary
/// is reusable across boundaries only when both include every relevant event.
/// Keep one summary per file/scope so historical snapshots cannot grow the cache.
fn file_summaries(
    path: &Path,
    start: i64,
    scope: TrafficScope,
    end: i64,
) -> Result<Vec<SnapshotFile>, String> {
    Ok(file_summaries_many(path, start, &[(scope, end)])?.remove(0))
}
fn file_summaries_many(
    path: &Path,
    start: i64,
    selections: &[(TrafficScope, i64)],
) -> Result<Vec<Vec<SnapshotFile>>, String> {
    let mut summaries: Vec<Vec<SnapshotFile>> = selections.iter().map(|_| Vec::new()).collect();
    for_each_file(path, start, |file, live, file_path| {
        let meta = file
            .metadata()
            .map_err(|_| "Cannot read traffic history".to_owned())?;
        let backup = path.with_file_name(format!(
            "{}.1",
            path.file_name().unwrap_or_default().to_string_lossy()
        ));
        let archived_modified = if live || file_path == backup {
            None
        } else {
            meta.modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|t| t.as_millis() as i64)
        };
        let stamp = FileStamp {
            #[cfg(unix)]
            node: {
                use std::os::unix::fs::MetadataExt;
                (meta.dev(), meta.ino())
            },
            #[cfg(not(unix))]
            path: file_path.to_owned(),
            len: meta.len(),
            modified: meta.modified().ok(),
        };
        let mut results: Vec<Option<Arc<FileTraffic>>> = selections.iter().map(|_| None).collect();
        if !live {
            let mut cache = SUMMARIES.lock().unwrap();
            let cache = cache.get_or_insert_with(HashMap::new);
            cache.retain(|_, (used, _)| used.elapsed() < SUMMARY_TTL);
            for (result, (scope, end)) in results.iter_mut().zip(selections) {
                if let Some((used, summary)) =
                    cache.get_mut(&(stamp.clone(), *scope == TrafficScope::Model))
                {
                    let complete = summary
                        .latest_event
                        .is_none_or(|last| last < summary.scan_end && last < *end);
                    if summary.scan_end == *end || complete {
                        *used = Instant::now();
                        *result = Some(summary.clone());
                    }
                }
            }
        }
        let missing: Vec<_> = results
            .iter()
            .enumerate()
            .filter_map(|(i, value)| value.is_none().then_some(i))
            .collect();
        if !missing.is_empty() {
            #[cfg(test)]
            if live {
                *LIVE_SCANS
                    .lock()
                    .unwrap()
                    .get_or_insert_with(HashMap::new)
                    .entry(path.to_owned())
                    .or_default() += 1;
            }
            let pending: Vec<_> = missing.iter().map(|i| selections[*i]).collect();
            let scanned = FileTraffic::scan_many(file, &pending)?;
            for (i, summary) in missing.into_iter().zip(scanned) {
                let summary = Arc::new(summary);
                if !live {
                    SUMMARIES
                        .lock()
                        .unwrap()
                        .get_or_insert_with(HashMap::new)
                        .insert(
                            (stamp.clone(), selections[i].0 == TrafficScope::Model),
                            (Instant::now(), summary.clone()),
                        );
                }
                results[i] = Some(summary);
            }
        }
        for (summaries, summary) in summaries.iter_mut().zip(results) {
            summaries.push(SnapshotFile {
                summary: summary.unwrap(),
                archived_modified,
            });
        }
        Ok(())
    })?;
    Ok(summaries)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Traffic {
    scope: TrafficScope,
    start: i64,
    end: i64,
    bucket_minutes: u64,
    credentials: Vec<CredentialTraffic>,
    summary: CredentialTraffic,
    /// Current configurations that historical traffic can be assigned to.
    targets: Vec<Target>,
}

/// Logged configuration identity counted in a group without an exact match.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceTraffic {
    name: String,
    base: Option<String>,
    /// `None` when the user's choice applies exactly.
    reason: Option<Reason>,
    requests: u64,
}

#[derive(Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialTraffic {
    service: String,
    credential: String,
    requests: u64,
    errors: u64,
    bytes: u64,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cached_input_tokens: Option<u64>,
    /// Claude only: reported input without cache reads or writes.
    uncached_input_tokens: Option<u64>,
    /// Claude only: input written to the prompt cache.
    cache_write_tokens: Option<u64>,
    cache_hit_rate: Option<f64>,
    avg_ms: Option<u64>,
    counts: Vec<u64>,
    error_counts: Vec<u64>,
    /// Reported input plus output tokens per bucket; calls without usage add nothing.
    token_counts: Vec<u64>,
    sources: Vec<SourceTraffic>,
    #[serde(skip)]
    source_counts: BTreeMap<Source, (Option<Reason>, u64)>,
    #[serde(skip)]
    latency_total: u64,
    #[serde(skip)]
    latency_count: u64,
    #[serde(skip)]
    cache_read: u64,
    #[serde(skip)]
    cache_prompt: u64,
}

pub fn read(
    path: &Path,
    minutes: u64,
    labels: &BTreeMap<(String, String), String>,
    scope: TrafficScope,
    identities: &Identities,
) -> Result<Traffic, String> {
    read_at(
        ReadSource::Path(path),
        minutes,
        labels,
        scope,
        identities,
        Local::now().timestamp_millis(),
    )
}
fn read_at(
    source: ReadSource<'_>,
    minutes: u64,
    labels: &BTreeMap<(String, String), String>,
    scope: TrafficScope,
    identities: &Identities,
    end: i64,
) -> Result<Traffic, String> {
    let bucket_minutes = match minutes {
        30 => 1,
        360 => 15,
        720 => 30,
        1440 => 60,
        10080 => 360,
        43200 => 1440,
        _ => return Err("Unsupported traffic range".into()),
    };
    let bucket_count = (minutes / bucket_minutes) as usize;
    let start = end - minutes as i64 * 60_000;
    let mut groups = BTreeMap::new();
    let mut labels = labels.clone();
    for entry in source.entries(start, end, scope, &mut labels)? {
        let resolved = identities.resolve(&entry, entry.service().unwrap_or("Unknown"), &labels);
        aggregate(
            &mut groups,
            &entry,
            start,
            end,
            bucket_minutes,
            &labels,
            resolved,
        );
    }
    let mut summary = CredentialTraffic {
        counts: vec![0; bucket_count],
        error_counts: vec![0; bucket_count],
        token_counts: vec![0; bucket_count],
        ..Default::default()
    };
    for group in groups.values() {
        summary.requests += group.requests;
        summary.errors += group.errors;
        summary.bytes += group.bytes;
        add_tokens(&mut summary.input_tokens, group.input_tokens);
        add_tokens(&mut summary.output_tokens, group.output_tokens);
        add_tokens(&mut summary.cached_input_tokens, group.cached_input_tokens);
        summary.latency_total += group.latency_total;
        summary.latency_count += group.latency_count;
        summary.cache_read += group.cache_read;
        summary.cache_prompt += group.cache_prompt;
        for i in 0..bucket_count {
            summary.counts[i] += group.counts[i];
            summary.error_counts[i] += group.error_counts[i];
            summary.token_counts[i] += group.token_counts[i];
        }
    }
    summary.avg_ms =
        (summary.latency_count > 0).then(|| summary.latency_total / summary.latency_count);
    summary.cache_hit_rate = summary.hit_rate();
    let mut credentials: Vec<_> = groups.into_values().collect();
    for group in &mut credentials {
        group.cache_hit_rate = group.hit_rate();
        group.sources = std::mem::take(&mut group.source_counts)
            .into_iter()
            .map(|(source, (reason, requests))| SourceTraffic {
                name: source.name,
                base: source.base,
                reason,
                requests,
            })
            .collect();
    }
    credentials.sort_by_key(|group| {
        (
            group.credential == UNIDENTIFIED,
            std::cmp::Reverse(group.bytes),
        )
    });
    Ok(Traffic {
        scope,
        start,
        end,
        bucket_minutes,
        credentials,
        summary,
        targets: identities.targets(),
    })
}

impl CredentialTraffic {
    fn hit_rate(&self) -> Option<f64> {
        (self.cache_prompt > 0).then(|| self.cache_read as f64 / self.cache_prompt as f64)
    }
}

fn add_tokens(total: &mut Option<u64>, value: Option<u64>) {
    if let Some(value) = value {
        *total = Some(total.unwrap_or(0).saturating_add(value));
    }
}

fn credential_name(
    entry: &Entry,
    service: &str,
    labels: &BTreeMap<(String, String), String>,
    resolved: Option<&Resolution>,
) -> String {
    resolved.map(|r| r.label.clone()).unwrap_or_else(|| {
        ["provider", "account_id", "account_label"]
            .into_iter()
            .filter_map(|key| entry.get(key))
            .find_map(|name| labels.get(&(service.to_owned(), name.to_owned())).cloned())
            .unwrap_or_else(|| UNIDENTIFIED.into())
    })
}

fn aggregate(
    groups: &mut BTreeMap<(String, String), CredentialTraffic>,
    e: &Entry,
    start: i64,
    end: i64,
    bucket_minutes: u64,
    labels: &BTreeMap<(String, String), String>,
    resolved: Option<Resolution>,
) {
    if lifecycle_rank(e) == 0 {
        return;
    }
    let Some(time) = e.time.map(|t| t.timestamp_millis()) else {
        return;
    };
    if time < start || time >= end {
        return;
    }
    let service = e
        .service()
        .or_else(|| e.get("service"))
        .unwrap_or("Unknown")
        .to_owned();
    let credential = credential_name(e, &service, labels, resolved.as_ref());
    let source = resolved.and_then(|r| r.source);
    let bucket_count = ((end - start) as u64).div_ceil(bucket_minutes * 60_000) as usize;
    let claude = service == "Claude";
    let group = groups
        .entry((service.clone(), credential.clone()))
        .or_insert_with(|| CredentialTraffic {
            service,
            credential,
            counts: vec![0; bucket_count],
            error_counts: vec![0; bucket_count],
            token_counts: vec![0; bucket_count],
            ..Default::default()
        });
    if let Some((source, reason)) = source {
        group.source_counts.entry(source).or_insert((reason, 0)).1 += 1;
    }
    let slot = ((time - start) / (bucket_minutes as i64 * 60_000)) as usize;
    group.counts[slot] += 1;
    group.requests += 1;
    if e.is_error() {
        group.errors += 1;
        group.error_counts[slot] += 1;
    }
    group.bytes += e.bytes();
    let token = |key| e.get(key).and_then(|v| v.parse().ok());
    let cached = token("cached_input_tokens");
    // Input counts the whole prompt for every service, so cached input is part of it.
    // OpenAI input_tokens already includes cached tokens; Anthropic input_tokens
    // excludes cache reads and writes, which are added here.
    let input = if claude {
        [
            token("input_tokens"),
            cached,
            token("cache_creation_input_tokens"),
        ]
        .into_iter()
        .flatten()
        .reduce(u64::saturating_add)
    } else {
        token("input_tokens")
    };
    let output = token("output_tokens");
    add_tokens(&mut group.input_tokens, input);
    add_tokens(&mut group.output_tokens, output);
    group.token_counts[slot] = group.token_counts[slot]
        .saturating_add(input.unwrap_or(0).saturating_add(output.unwrap_or(0)));
    add_tokens(&mut group.cached_input_tokens, cached);
    if claude {
        add_tokens(&mut group.uncached_input_tokens, token("input_tokens"));
        add_tokens(
            &mut group.cache_write_tokens,
            token("cache_creation_input_tokens"),
        );
    }
    // Only calls reporting both counts contribute to the hit rate.
    if let (Some(input), Some(cached), Some(_)) = (input, cached, token("input_tokens")) {
        group.cache_read = group.cache_read.saturating_add(cached);
        group.cache_prompt = group.cache_prompt.saturating_add(input);
    }
    if let Some(ms) = e.duration_ms() {
        group.latency_total += ms;
        group.latency_count += 1;
        group.avg_ms = Some(group.latency_total / group.latency_count);
    }
}

fn processed_entries(
    path: &Path,
    start: i64,
    end: i64,
    scope: TrafficScope,
    labels: &mut BTreeMap<(String, String), String>,
) -> Result<Vec<Entry>, String> {
    let files = file_summaries(path, start, scope, end)?;
    Ok(entries_from_files(&files, start, end, scope, labels))
}
/// Evidence only adds ids to label values already present for a service, so
/// the result does not depend on file order or on the window's entries.
fn apply_identity_evidence(
    files: &[SnapshotFile],
    start: i64,
    labels: &mut BTreeMap<(String, String), String>,
) {
    for file in files
        .iter()
        .filter(|f| f.archived_modified.is_none_or(|modified| modified >= start))
    {
        for (service, id, label) in &file.summary.evidence {
            if labels
                .iter()
                .any(|((s, _), current)| s == service && current == label)
            {
                labels
                    .entry((service.clone(), id.clone()))
                    .or_insert_with(|| label.clone());
            }
        }
    }
}
fn entries_from_files(
    files: &[SnapshotFile],
    start: i64,
    end: i64,
    scope: TrafficScope,
    labels: &mut BTreeMap<(String, String), String>,
) -> Vec<Entry> {
    let mut seen_requests = HashSet::new();
    let mut entries = Vec::<&Record>::new();
    // Most requests lie within one file; only those in several are copied.
    let mut requests = BTreeMap::<&str, std::borrow::Cow<Record>>::new();
    let mut explicit_call_requests = HashSet::new();
    apply_identity_evidence(files, start, labels);
    for file in files
        .iter()
        .filter(|f| f.archived_modified.is_none_or(|modified| modified >= start))
    {
        let file = &file.summary;
        for (id, record) in &file.requests {
            match requests.get_mut(id.as_str()) {
                Some(current) => current.to_mut().merge(record),
                None => {
                    requests.insert(id, std::borrow::Cow::Borrowed(record));
                }
            }
        }
        for (identity, record) in &file.uncorrelated {
            if seen_requests.insert(*identity) {
                entries.push(record);
            }
        }
        explicit_call_requests.extend(file.explicit_call_requests.iter().map(String::as_str));
    }
    let in_window = |record: &&Record| {
        record
            .time
            .is_some_and(|t| (start..end).contains(&t.timestamp_millis()))
    };
    let mut result = Vec::new();
    for entry in entries
        .into_iter()
        .chain(requests.values().map(|record| record.as_ref()))
        .filter(in_window)
        .map(Record::entry)
    {
        if scope == TrafficScope::Model
            && is_historical_http_call(&entry)
            && entry
                .get("request_id")
                .is_some_and(|id| explicit_call_requests.contains(id))
        {
            continue;
        }
        result.push(entry);
    }
    result
}

/// Already observed identities belong to this device; matching them to a peer
/// does not import any peer configuration or expose names in a shared snapshot.
pub(crate) fn observed_identity_labels_range(
    path: &Path,
    mut labels: BTreeMap<(String, String), String>,
    minutes: u64,
) -> Result<BTreeMap<(String, String), String>, String> {
    let end = chrono::Utc::now().timestamp_millis();
    let start = end - minutes as i64 * 60_000;
    // Only identity evidence is needed; do not materialize the window's entries.
    let files = file_summaries(path, start, TrafficScope::Model, end)?;
    apply_identity_evidence(&files, start, &mut labels);
    Ok(labels)
}
/// This device uses exactly Activity Traffic's own identity resolution. It must
/// not be passed through the remote privacy projection and then re-identified.
pub(crate) struct LocalTraffic {
    pub groups: Vec<(
        coport_gui::data_api::Service,
        String,
        coport_gui::data_api::Stats,
    )>,
    pub reviews: BTreeMap<(String, String), Vec<SourceTraffic>>,
    pub targets: Vec<Target>,
}

pub(crate) fn local_named_groups(
    source: ReadSource<'_>,
    config: &coport::config::Config,
    labels: &BTreeMap<(String, String), String>,
    end: i64,
    minutes: u64,
    scope: TrafficScope,
) -> Result<LocalTraffic, String> {
    let assignments = crate::traffic_identity::Compatibility::load(
        &crate::settings::traffic_compatibility_path(),
    )
    .unwrap_or_default();
    let identities = Identities::from_config(config, &assignments.assignments);
    let traffic = read_at(source, minutes, labels, scope, &identities, end)?;
    let reviews = traffic
        .credentials
        .iter()
        .filter(|g| !g.sources.is_empty())
        .map(|g| ((g.service.clone(), g.credential.clone()), g.sources.clone()))
        .collect();
    let groups = traffic
        .credentials
        .into_iter()
        .map(|g| {
            (
                match g.service.as_str() {
                    "Codex" => coport_gui::data_api::Service::Codex,
                    "Claude" => coport_gui::data_api::Service::Claude,
                    _ => coport_gui::data_api::Service::Other,
                },
                g.credential,
                coport_gui::data_api::Stats {
                    requests: g.requests,
                    errors: g.errors,
                    bytes: g.bytes,
                    input_tokens: g.input_tokens,
                    output_tokens: g.output_tokens,
                    cached_input_tokens: g.cached_input_tokens,
                    uncached_input_tokens: g.uncached_input_tokens,
                    cache_write_tokens: g.cache_write_tokens,
                    cache_read: g.cache_read,
                    cache_prompt: g.cache_prompt,
                    latency_total_ms: g.latency_total,
                    latency_samples: g.latency_count,
                    counts: g.counts,
                    error_counts: g.error_counts,
                    token_counts: g.token_counts,
                },
            )
        })
        .collect();
    Ok(LocalTraffic {
        groups,
        reviews,
        targets: traffic.targets,
    })
}

/// Only numeric results and keyed opaque references leave the device.
#[derive(Clone, Copy)]
pub(crate) struct ExportOptions {
    pub end: i64,
    pub minutes: u64,
    pub scope: TrafficScope,
    pub limit: usize,
}
pub(crate) struct ExportIdentities {
    labels: BTreeMap<(String, String), String>,
    identities: Identities,
    references: Vec<coport::identity::TrafficProviderReference>,
}

impl ExportIdentities {
    pub(crate) fn from_config(config: &coport::config::Config) -> Self {
        let assignments = crate::traffic_identity::Compatibility::load(
            &crate::settings::traffic_compatibility_path(),
        )
        .unwrap_or_default();
        let identities = Identities::from_config(config, &assignments.assignments);
        let config = config.clone();
        let (labels, references) = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .ok()
                .map(|rt| {
                    rt.block_on(async {
                        (
                            config.local_traffic_credential_labels().await,
                            config.traffic_provider_references().await,
                        )
                    })
                })
                .unwrap_or_default()
        })
        .join()
        .unwrap_or_default();
        Self {
            labels,
            identities,
            references,
        }
    }
}
pub(crate) fn export_window_limit(
    source: ReadSource<'_>,
    config: &coport::config::Config,
    key: &coport::external_access::DataKey,
    options: ExportOptions,
    context: &ExportIdentities,
) -> Result<Vec<coport_gui::data_api::Group>, String> {
    let ExportOptions {
        end,
        minutes,
        scope,
        limit,
    } = options;
    let bucket =
        coport_gui::data_api::bucket_minutes(minutes).ok_or("Unsupported traffic range")?;
    let start = end - minutes as i64 * 60_000;
    let identities = &context.identities;
    let references = &context.references;
    let mut labels = context.labels.clone();
    let entries = source.entries(start, end, scope, &mut labels)?;
    let mut account_refs = BTreeMap::<_, BTreeSet<Option<String>>>::new();
    let mut groups = BTreeMap::new();
    for entry in entries {
        let service = entry.service().unwrap_or("Unknown").to_owned();
        // Group keys must match `aggregate`, which keeps raw services such as CONNECT.
        let group_service = entry
            .service()
            .or_else(|| entry.get("service"))
            .unwrap_or("Unknown")
            .to_owned();
        let resolved = identities.resolve(&entry, &service, &labels);
        let credential = credential_name(&entry, &service, &labels, resolved.as_ref());
        let proxy = entry
            .get("proxy_endpoint")
            .map(str::to_owned)
            .or_else(|| {
                entry
                    .get("proxy")
                    .and_then(|name| config.proxies.get(name))
                    .cloned()
            })
            .unwrap_or_else(|| {
                if entry.get("proxy") == Some("none") {
                    "none"
                } else {
                    "unknown"
                }
                .into()
            });
        let mut proxy_ref = key.reference("proxy", &coport_gui::data_api::canonical(&proxy));
        // Resolve/group locally first. Historical endpoints and names cannot
        // create extra upstream accounts in the exported result.
        let mut upstream_ref = key.reference("upstream", &format!("{service}\0{credential}"));
        let mut account_ref = identities
            .provider_reference(&service, &credential, references)
            .or_else(|| {
                coport_gui::data_api::account_reference(
                    &service,
                    entry.get("account_id").unwrap_or(""),
                )
            })
            .or_else(|| {
                if resolved.is_some() {
                    return None;
                }
                entry
                    .get("credential_ref")
                    .filter(|r| r.len() == 64 && r.bytes().all(|c| c.is_ascii_hexdigit()))
                    .map(str::to_owned)
            });
        let label = format!("{proxy_ref}/{upstream_ref}");
        if groups.len() >= limit && !groups.contains_key(&(group_service.clone(), label)) {
            proxy_ref = key.reference("proxy", "overflow");
            upstream_ref = key.reference("upstream", "overflow");
            account_ref = None;
        }
        let label = format!("{proxy_ref}/{upstream_ref}");
        account_refs
            .entry((group_service, label.clone()))
            .or_default()
            .insert(account_ref);
        aggregate(
            &mut groups,
            &entry,
            start,
            end,
            bucket,
            &labels,
            Some(Resolution {
                label,
                source: None,
            }),
        );
    }
    Ok(groups
        .into_values()
        .map(|g| {
            let (proxy_ref, upstream_ref) = g.credential.split_once('/').unwrap();
            coport_gui::data_api::Group {
                service: match g.service.as_str() {
                    "Codex" => coport_gui::data_api::Service::Codex,
                    "Claude" => coport_gui::data_api::Service::Claude,
                    _ => coport_gui::data_api::Service::Other,
                },
                proxy_ref: proxy_ref.into(),
                upstream_ref: upstream_ref.into(),
                account_ref: account_refs
                    .remove(&(g.service.clone(), g.credential.clone()))
                    .filter(|refs| refs.len() == 1)
                    .and_then(|refs| refs.into_iter().next().flatten()),
                stats: coport_gui::data_api::Stats {
                    requests: g.requests,
                    errors: g.errors,
                    bytes: g.bytes,
                    input_tokens: g.input_tokens,
                    output_tokens: g.output_tokens,
                    cached_input_tokens: g.cached_input_tokens,
                    uncached_input_tokens: g.uncached_input_tokens,
                    cache_write_tokens: g.cache_write_tokens,
                    cache_read: g.cache_read,
                    cache_prompt: g.cache_prompt,
                    latency_total_ms: g.latency_total,
                    latency_samples: g.latency_count,
                    counts: g.counts,
                    error_counts: g.error_counts,
                    token_counts: g.token_counts,
                },
            }
        })
        .collect())
}

/// Calls `visit` for every entry of the log at `path` and its rotated history,
/// in file order. Archives last written before `start` (epoch milliseconds)
/// cannot hold later entries and are skipped.
pub fn for_each_entry(path: &Path, start: i64, mut visit: impl FnMut(Entry)) -> Result<(), String> {
    for_each_file(path, start, |file, _, _| for_each_line(file, &mut visit))
}

/// Calls `visit` with each file of the log at `path`: the live log first
/// (marked `true`), then the rotated backup and archives, as for
/// `for_each_entry`. A file is never visited twice.
fn for_each_file(
    path: &Path,
    start: i64,
    mut visit: impl FnMut(File, bool, &Path) -> Result<(), String>,
) -> Result<(), String> {
    let backup = path.with_file_name(format!(
        "{}.1",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    // Open handles before reading so appends and rotations do not restart a scan.
    let open = |p: &Path| match File::open(p) {
        Ok(f) => Ok(Some(f)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err("Cannot read traffic history".to_owned()),
    };
    let current = [path, backup.as_path()].map(|p| (p.to_owned(), open(p)));
    let archives = coport::logger::history_paths(path)
        .map_err(|_| "Cannot read traffic archives".to_owned())?;
    // Archived files are immutable. Skip files last written before the selected
    // window, and open one at a time to avoid exhausting file descriptors.
    let archives = archives.into_iter().filter(|p| {
        p.metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .is_none_or(|t| t.as_millis() as i64 >= start)
    });
    let files = current
        .into_iter()
        .chain(archives.map(|p| (p.clone(), open(&p))));
    #[cfg(unix)]
    let mut identities = std::collections::HashSet::new();
    for (index, (file_path, file)) in files.enumerate() {
        let Some(file) = file? else { continue };
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let meta = file
                .metadata()
                .map_err(|_| "Cannot read traffic history".to_owned())?;
            // Rotation between the two opens can return the same file twice.
            if !identities.insert((meta.dev(), meta.ino())) {
                continue;
            }
        }
        visit(file, index == 0, &file_path)?;
    }
    Ok(())
}

fn for_each_line(file: File, mut visit: impl FnMut(Entry)) -> Result<(), String> {
    for line in BufReader::new(file).split(b'\n') {
        let line = line.map_err(|_| "Cannot read traffic history".to_owned())?;
        let Ok(Value::Object(mut fields)) = serde_json::from_slice(&line) else {
            continue;
        };
        let Some(Value::String(event)) = fields.remove("event") else {
            continue;
        };
        let time = fields
            .remove("timestamp")
            .and_then(|v| {
                v.as_str()
                    .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            })
            .map(|t| t.with_timezone(&Local));
        visit(Entry {
            seq: crate::logs::line_key(&line),
            event,
            time,
            fields,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn batched_boundaries_match_independent_scans_without_future_events() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("proxy.log");
        let end = chrono::Utc::now().timestamp_millis() / 60_000 * 60_000;
        let mut rows = String::new();
        for index in 0..3000 {
            for (event, offset) in [
                ("model_call_started", 180_000),
                ("model_call_finished", (index % 3) * 60_000 + 1000),
            ] {
                let mut row = serde_json::json!({"timestamp":chrono::DateTime::from_timestamp_millis(end-offset).unwrap().to_rfc3339(),"event":event,"request_id":format!("req-{index}"),"model_call_id":format!("call-{index}"),"service":"codex","method":"POST","path":"/v1/responses"});
                if event == "model_call_finished" {
                    row["input_tokens"] = "10".into();
                    row["output_tokens"] = "5".into();
                    row["status"] = "200".into();
                }
                rows.push_str(&row.to_string());
                rows.push('\n');
            }
        }
        std::fs::write(&path, rows).unwrap();
        let ends = [end, end - 60_000, end - 120_000];
        let scopes = [TrafficScope::Model, TrafficScope::All];
        let labels = BTreeMap::new();
        let identities = Identities::default();
        let before = live_scans(&path);
        let started = Instant::now();
        let mut expected = Vec::new();
        for at in ends {
            for scope in scopes {
                expected.push(
                    serde_json::to_value(
                        read_at(ReadSource::Path(&path), 30, &labels, scope, &identities, at)
                            .unwrap(),
                    )
                    .unwrap(),
                );
            }
        }
        let separate_time = started.elapsed();
        assert_eq!(live_scans(&path) - before, 6);
        let before = live_scans(&path);
        let started = Instant::now();
        let snapshots = Snapshot::load_many(&path, &ends, 30, &scopes).unwrap();
        let mut actual = Vec::new();
        for snapshot in &snapshots {
            for scope in scopes {
                actual.push(
                    serde_json::to_value(
                        read_at(
                            ReadSource::Snapshot(snapshot),
                            30,
                            &labels,
                            scope,
                            &identities,
                            snapshot.end,
                        )
                        .unwrap(),
                    )
                    .unwrap(),
                );
            }
        }
        let batch_time = started.elapsed();
        assert_eq!(live_scans(&path) - before, 1);
        assert_eq!(actual, expected);
        eprintln!("6000 lifecycle rows: separate={separate_time:?}, batched={batch_time:?}");
        // Finished lifecycle information after a boundary must stay excluded.
        assert_eq!(actual[0]["summary"]["inputTokens"], 30000);
        assert_eq!(actual[2]["summary"]["inputTokens"], 20000);
        assert_eq!(actual[4]["summary"]["inputTokens"], 10000);
    }

    #[test]
    fn rotated_cache_preserves_requests_after_earlier_snapshot_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("proxy.log");
        std::fs::write(&path, "").unwrap();
        let end = chrono::Utc::now().timestamp_millis() / 60_000 * 60_000 - 60_000;
        let row = |id: &str, at: i64| {
            serde_json::json!({
                "timestamp": chrono::DateTime::from_timestamp_millis(at).unwrap().to_rfc3339(),
                "event": "model_call_finished", "model_call_id": id, "request_id": id,
                "service": "claude", "path": "/v1/messages", "method": "POST", "status": "200"
            })
            .to_string()
                + "\n"
        };
        std::fs::write(
            path.with_file_name("proxy.log.1"),
            row("before", end - 1000) + &row("after", end + 1000),
        )
        .unwrap();
        let labels = BTreeMap::new();
        let identities = Identities::default();
        let first = Snapshot::load(&path, end).unwrap();
        assert_eq!(
            read_at(
                ReadSource::Snapshot(&first),
                30,
                &labels,
                TrafficScope::Model,
                &identities,
                end
            )
            .unwrap()
            .summary
            .requests,
            1
        );
        let next_end = end + 60_000;
        let next = Snapshot::load(&path, next_end).unwrap();
        let count = read_at(
            ReadSource::Snapshot(&next),
            30,
            &labels,
            TrafficScope::Model,
            &identities,
            next_end,
        )
        .unwrap()
        .summary
        .requests;
        assert_eq!(
            count, 2,
            "advancing the snapshot lost a request from the unchanged rotated log"
        );
        let older = Snapshot::load(&path, end).unwrap();
        assert_eq!(
            read_at(
                ReadSource::Snapshot(&older),
                30,
                &labels,
                TrafficScope::Model,
                &identities,
                end
            )
            .unwrap()
            .summary
            .requests,
            1
        );
        // A previously returned snapshot stays immutable after cache replacements.
        assert_eq!(
            read_at(
                ReadSource::Snapshot(&first),
                30,
                &labels,
                TrafficScope::Model,
                &identities,
                end
            )
            .unwrap()
            .summary
            .requests,
            1
        );
    }

    #[test]
    fn shared_snapshot_matches_every_range_and_remains_immutable_across_log_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("proxy.log");
        let end = chrono::Utc::now().timestamp_millis() / 60_000 * 60_000;
        let row = |id: &str| {
            serde_json::json!({"timestamp":chrono::DateTime::from_timestamp_millis(end - 120_000).unwrap().to_rfc3339(), "event":"request_finished", "request_id":id, "method":"POST", "path":"/v1/responses", "service":"codex", "status":"200", "received_bytes":"42", "input_tokens":"12", "duration_ms":"100"}).to_string()+"\n"
        };
        std::fs::write(&path, row("first")).unwrap();
        let before = live_scans(&path);
        let snapshot = Snapshot::load(&path, end).unwrap();
        assert_eq!(live_scans(&path) - before, 1);
        let labels = BTreeMap::new();
        let identities = Identities::default();
        for minutes in crate::data_api::RANGES {
            for scope in [TrafficScope::All, TrafficScope::Model] {
                let shared = read_at(
                    ReadSource::Snapshot(&snapshot),
                    minutes,
                    &labels,
                    scope,
                    &identities,
                    end,
                )
                .unwrap();
                let direct = read_at(
                    ReadSource::Path(&path),
                    minutes,
                    &labels,
                    scope,
                    &identities,
                    end,
                )
                .unwrap();
                assert_eq!(
                    serde_json::to_value(shared).unwrap(),
                    serde_json::to_value(direct).unwrap()
                );
            }
        }
        std::fs::write(&path, row("first") + &row("second")).unwrap();
        let frozen = read_at(
            ReadSource::Snapshot(&snapshot),
            30,
            &labels,
            TrafficScope::Model,
            &identities,
            end,
        )
        .unwrap();
        assert_eq!(frozen.summary.requests, 1);
        let fresh = Snapshot::load(&path, end).unwrap();
        assert_eq!(
            read_at(
                ReadSource::Snapshot(&fresh),
                30,
                &labels,
                TrafficScope::Model,
                &identities,
                end
            )
            .unwrap()
            .summary
            .requests,
            2
        );
        std::fs::rename(&path, path.with_file_name("proxy.log.1")).unwrap();
        std::fs::write(&path, row("third")).unwrap();
        let rotated = Snapshot::load(&path, end).unwrap();
        assert_eq!(
            read_at(
                ReadSource::Snapshot(&rotated),
                30,
                &labels,
                TrafficScope::Model,
                &identities,
                end
            )
            .unwrap()
            .summary
            .requests,
            3
        );
        std::fs::write(&path, "").unwrap();
        let truncated = Snapshot::load(&path, end).unwrap();
        assert_eq!(
            read_at(
                ReadSource::Snapshot(&truncated),
                30,
                &labels,
                TrafficScope::Model,
                &identities,
                end
            )
            .unwrap()
            .summary
            .requests,
            2
        );
    }

    #[test]
    fn changed_configurations_merge_with_reasons_and_unmatched_requests_are_unidentified() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.toml"), "[model_providers.myServer]\nbase_url = 'https://api.example.com/v1'\nenv_key = 'TEST_KEY'\n[model_providers.twinA]\nbase_url = 'https://shared.example.com/v1'\nenv_key = 'TEST_KEY'\n[model_providers.twinB]\nbase_url = 'https://shared.example.com/v1'\nenv_key = 'TEST_KEY'\n").unwrap();
        std::fs::write(dir.path().join("newSettings.json"), r#"{"env":{"ANTHROPIC_BASE_URL":"https://api.example.com/v1","ANTHROPIC_API_KEY":"test-only"}}"#).unwrap();
        let home = serde_json::to_string(dir.path()).unwrap();
        let config = coport::config::Config::parse(&format!("listen_port: 8787\nrequest_timeout_seconds: 3\ncodex:\n  homes: [{home}]\n  routing:\n    api_key: {{myServer: none, twinA: none, twinB: none}}\nclaude:\n  config_dirs: [{home}]\n  routing:\n    api_key: {{newSettings: none}}\n")).unwrap();
        let identities = Identities::from_config(&config, &[]);
        let timestamp = (Local::now() - chrono::Duration::minutes(1)).to_rfc3339();
        let row = |id, service, provider, base: Option<&str>| {
            let mut row = serde_json::json!({"event":"request_finished", "timestamp":timestamp, "request_id":id, "service":service, "provider":provider, "method":"POST", "path":"/v1/responses", "status":"200", "received_bytes":"10"});
            if let Some(base) = base {
                row["upstream_base_url"] = base.into();
            }
            row.to_string()
        };
        let current = [
            row(
                "new",
                "codex",
                "myServer",
                Some("https://api.example.com/v1"),
            ),
            row(
                "claude",
                "claude",
                "oldSettings",
                Some("https://api.example.com/v1"),
            ),
            row(
                "different-path",
                "codex",
                "myServer",
                Some("https://api.example.com/v2"),
            ),
            row("missing", "codex", "my", None),
            row("existing-name", "codex", "myServer", None),
            row(
                "deleted",
                "codex",
                "deleted",
                Some("https://deleted.example.com/v1"),
            ),
            row(
                "invalid",
                "codex",
                "myServer",
                Some("https://api.example.com/v1?secret=no"),
            ),
            row(
                "twin",
                "codex",
                "twinA",
                Some("https://shared.example.com/v1"),
            ),
            row(
                "shared",
                "codex",
                "gone",
                Some("https://shared.example.com/v1"),
            ),
        ];
        for newline in ["\n", "\r\n"] {
            let path = dir.path().join("proxy.log");
            let backup = dir.path().join("proxy.log.1");
            let current = current.join(newline) + newline;
            let old = row(
                "old",
                "codex",
                "my",
                Some("https://API.example.com:443/v1/"),
            ) + newline;
            std::fs::write(&path, &current).unwrap();
            std::fs::write(&backup, &old).unwrap();
            let labels = configured(&["myServer"]);
            for scope in [TrafficScope::All, TrafficScope::Model] {
                let traffic = super::read(&path, 30, &labels, scope, &identities).unwrap();
                assert_eq!(traffic.summary.requests, 10);
                assert_eq!(traffic.summary.bytes, 100);
                assert_eq!(traffic.credentials.len(), 4);
                assert_eq!(traffic.targets.len(), 4);
                use Reason::*;
                let api = || Some("https://api.example.com/v1");
                for (service, label, count, sources) in [
                    (
                        "Codex",
                        "myServer",
                        4,
                        vec![
                            ("my", api(), Some(Renamed)),
                            ("myServer", None, Some(Legacy)),
                            (
                                "myServer",
                                Some("https://api.example.com/v2"),
                                Some(BaseChanged),
                            ),
                        ],
                    ),
                    (
                        "Claude",
                        "newSettings",
                        1,
                        vec![("oldSettings", api(), Some(Renamed))],
                    ),
                    ("Codex", "twinA", 1, vec![]),
                    (
                        "Codex",
                        "Unidentified",
                        4,
                        vec![
                            (
                                "deleted",
                                Some("https://deleted.example.com/v1"),
                                Some(Unmatched),
                            ),
                            (
                                "gone",
                                Some("https://shared.example.com/v1"),
                                Some(Ambiguous),
                            ),
                            ("my", None, Some(Unmatched)),
                        ],
                    ),
                ] {
                    let group = traffic
                        .credentials
                        .iter()
                        .find(|g| g.service == service && g.credential == label)
                        .unwrap();
                    assert_eq!(group.requests, count);
                    assert_eq!(group.counts.iter().sum::<u64>(), count);
                    assert_eq!(logged_sources(group), sources);
                }
            }
            let unknown = super::read(
                &path,
                30,
                &BTreeMap::new(),
                TrafficScope::All,
                &Default::default(),
            )
            .unwrap();
            assert_eq!(unknown.summary.requests, 10);
            assert!(
                unknown
                    .credentials
                    .iter()
                    .all(|g| g.credential == "Unidentified")
            );
            assert_eq!(std::fs::read(&path).unwrap(), current.as_bytes());
            assert_eq!(std::fs::read(&backup).unwrap(), old.as_bytes());
        }
    }

    fn logged_sources(group: &CredentialTraffic) -> Vec<(&str, Option<&str>, Option<Reason>)> {
        group
            .sources
            .iter()
            .map(|s| (s.name.as_str(), s.base.as_deref(), s.reason))
            .collect()
    }

    #[test]
    fn user_assignments_override_inference_until_their_target_disappears() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.toml"), "[model_providers.main]\nbase_url = 'https://api.example.com/v1'\nenv_key = 'TEST_KEY'\n[model_providers.twinA]\nbase_url = 'https://shared.example.com/v1'\nenv_key = 'TEST_KEY'\n[model_providers.twinB]\nbase_url = 'https://shared.example.com/v1'\nenv_key = 'TEST_KEY'\n").unwrap();
        let home = serde_json::to_string(dir.path()).unwrap();
        let config = coport::config::Config::parse(&format!("listen_port: 8787\nrequest_timeout_seconds: 3\ncodex:\n  homes: [{home}]\n  routing:\n    api_key: {{main: none, twinA: none, twinB: none}}\n")).unwrap();
        let assign = |name: &str, base: Option<&str>, target: Option<(&str, &str)>| {
            crate::traffic_identity::TrafficAssignment {
                service: "Codex".into(),
                name: name.into(),
                base: base.map(Into::into),
                target: target.map(|(name, base)| crate::traffic_identity::TrafficTarget {
                    name: name.into(),
                    base: base.into(),
                }),
            }
        };
        let (api, shared) = (
            "https://api.example.com/v1",
            "https://shared.example.com/v1",
        );
        let identities = Identities::from_config(
            &config,
            &[
                assign("gone", Some(shared), Some(("twinB", shared))),
                assign("renamed", Some(api), None),
                assign(
                    "deleted",
                    Some("https://deleted.example.com"),
                    Some(("main", api)),
                ),
                assign(
                    "legacy",
                    None,
                    Some(("removed", "https://removed.example.com")),
                ),
            ],
        );
        let timestamp = (Local::now() - chrono::Duration::minutes(1)).to_rfc3339();
        let row = |provider: &str, base: Option<&str>| {
            let mut row = serde_json::json!({"event":"request_finished", "timestamp":timestamp, "service":"codex", "provider":provider, "method":"POST", "path":"/v1/responses", "status":"200"});
            if let Some(base) = base {
                row["upstream_base_url"] = base.into();
            }
            format!("{row}\n")
        };
        let path = dir.path().join("proxy.log");
        std::fs::write(
            &path,
            [
                row("gone", Some(shared)),
                row("renamed", Some(api)),
                row("deleted", Some("https://deleted.example.com")),
                row("legacy", None),
            ]
            .concat(),
        )
        .unwrap();
        let labels = BTreeMap::new();
        let traffic = super::read(&path, 30, &labels, TrafficScope::All, &identities).unwrap();
        let sources = |label| {
            let group = traffic
                .credentials
                .iter()
                .find(|g| g.credential == label)
                .unwrap();
            logged_sources(group)
        };
        assert_eq!(sources("twinB"), [("gone", Some(shared), None)]);
        assert_eq!(
            sources("main"),
            [("deleted", Some("https://deleted.example.com"), None)]
        );
        // A choice whose target no longer exists is reviewed again.
        assert_eq!(
            sources("Unidentified"),
            [
                ("legacy", None, Some(Reason::Unmatched)),
                ("renamed", Some(api), None),
            ]
        );
        let key = coport::external_access::DataKey::new(&"e".repeat(64)).unwrap();
        let context = ExportIdentities {
            labels,
            identities,
            references: Vec::new(),
        };
        let exported = export_window_limit(
            ReadSource::Path(&path),
            &config,
            &key,
            ExportOptions {
                end: traffic.end,
                minutes: 30,
                scope: TrafficScope::All,
                limit: 24,
            },
            &context,
        )
        .unwrap();
        assert_eq!(exported.len(), traffic.credentials.len());
        for local in &traffic.credentials {
            let reference = key.reference(
                "upstream",
                &format!("{}\0{}", local.service, local.credential),
            );
            let remote = exported
                .iter()
                .find(|g| g.upstream_ref == reference)
                .unwrap();
            assert_eq!(remote.stats.requests, local.requests);
            assert_eq!(remote.stats.bytes, local.bytes);
            assert_eq!(remote.stats.counts, local.counts);
            assert_eq!(remote.stats.token_counts, local.token_counts);
        }
    }

    fn configured(names: &[&str]) -> BTreeMap<(String, String), String> {
        names
            .iter()
            .map(|name| (("Codex".into(), (*name).into()), (*name).into()))
            .collect()
    }

    fn read(
        path: &Path,
        minutes: u64,
        labels: &BTreeMap<(String, String), String>,
        scope: TrafficScope,
    ) -> Result<Traffic, String> {
        super::read(path, minutes, labels, scope, &Default::default())
    }

    #[test]
    fn historical_http_calls_survive_rotation_without_double_counting_new_calls() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("proxy.log");
        let timestamp = (Local::now() - chrono::Duration::minutes(1)).to_rfc3339();
        let mut records = vec![
            serde_json::json!({"event":"request_received", "request_id":"responses", "method":"POST", "path":"/v1/responses", "provider":"old"}),
            serde_json::json!({"event":"request_finished", "request_id":"responses", "method":"POST", "path":"/v1/responses", "status":"200", "received_bytes":"100", "provider":"old"}),
            serde_json::json!({"event":"request_failed", "method":"POST", "path":"/anthropic/v1/messages", "status":"502", "provider":"old"}),
            serde_json::json!({"event":"request_finished", "request_id":"chat", "method":"POST", "path":"/codex/https://example.invalid/v1/chat/completions", "status":"200", "provider":"old"}),
            serde_json::json!({"event":"request_finished", "request_id":"compact", "method":"POST", "path":"/responses/compact", "status":"200", "provider":"old"}),
            serde_json::json!({"event":"request_finished", "request_id":"tokens", "method":"POST", "path":"/anthropic/v1/messages/count_tokens", "status":"200"}),
            serde_json::json!({"event":"request_finished", "request_id":"ws", "method":"GET", "path":"/v1/responses", "status":"101"}),
            serde_json::json!({"event":"request_finished", "request_id":"modern", "model_call_id":"modern-call", "method":"POST", "path":"/v1/responses", "status":"200"}),
            serde_json::json!({"event":"model_call_finished", "request_id":"modern", "model_call_id":"modern-call", "method":"POST", "path":"/v1/responses", "status":"200", "provider":"new", "input_tokens":"10"}),
        ];
        for row in &mut records {
            row["timestamp"] = serde_json::json!(timestamp);
        }
        let raw = records.iter().map(|r| format!("{r}\n")).collect::<String>();
        std::fs::write(&path, &raw).unwrap();
        std::fs::write(path.with_file_name("proxy.log.1"), &raw).unwrap();
        let model = read(
            &path,
            30,
            &BTreeMap::from([
                (("Codex".into(), "old".into()), "old".into()),
                (("Codex".into(), "new".into()), "new".into()),
                (("Claude".into(), "old".into()), "old".into()),
            ]),
            TrafficScope::Model,
        )
        .unwrap();
        assert_eq!(model.summary.requests, 5);
        assert_eq!(model.summary.errors, 1);
        assert_eq!(model.summary.bytes, 100);
        assert_eq!(model.summary.input_tokens, Some(10));
        assert_eq!(model.summary.counts.iter().sum::<u64>(), 5);
        let old = model
            .credentials
            .iter()
            .filter(|c| c.credential == "old")
            .collect::<Vec<_>>();
        assert_eq!(old.iter().map(|c| c.requests).sum::<u64>(), 4);
        assert!(old.iter().all(|c| c.input_tokens.is_none()));
    }

    #[test]
    fn export_limit_keeps_existing_connect_groups_out_of_overflow() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("proxy.log");
        let end = chrono::Utc::now().timestamp_millis() / 60_000 * 60_000;
        let timestamp = chrono::DateTime::from_timestamp_millis(end - 60_000)
            .unwrap()
            .to_rfc3339();
        let row = |id: &str| serde_json::json!({"event":"request_finished", "timestamp":timestamp, "request_id":id, "method":"CONNECT", "path":"example.com:443", "service":"connect", "proxy":"none", "status":"200"});
        std::fs::write(&path, format!("{}\n{}\n", row("first"), row("second"))).unwrap();
        let config = coport::config::Config::parse(
            "listen_port: 8787\nrequest_timeout_seconds: 30\ncodex:\n  homes: []\nclaude:\n  config_dirs: []\n",
        )
        .unwrap();
        let key = coport::external_access::DataKey::new(&"e".repeat(64)).unwrap();
        let context = ExportIdentities {
            labels: BTreeMap::new(),
            identities: Default::default(),
            references: Vec::new(),
        };
        let exported = export_window_limit(
            ReadSource::Path(&path),
            &config,
            &key,
            ExportOptions {
                end,
                minutes: 30,
                scope: TrafficScope::All,
                limit: 1,
            },
            &context,
        )
        .unwrap();
        assert_eq!(
            exported.len(),
            1,
            "The full group must not split into overflow"
        );
        assert_eq!(exported[0].stats.requests, 2);
        assert_ne!(
            exported[0].upstream_ref,
            key.reference("upstream", "overflow")
        );
    }

    #[test]
    fn model_scope_counts_historical_http_but_not_connections_or_orphan_call_events() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("proxy.log");
        let timestamp = (Local::now() - chrono::Duration::minutes(1)).to_rfc3339();
        let records = [
            serde_json::json!({"event":"request_finished", "timestamp":timestamp, "request_id":"http", "method":"POST", "path":"/v1/responses", "status":"200"}),
            serde_json::json!({"event":"request_finished", "timestamp":timestamp, "request_id":"ws", "method":"GET", "path":"/v1/responses", "status":"101"}),
            serde_json::json!({"event":"model_call_finished", "timestamp":timestamp, "request_id":"missing-call-id"}),
        ];
        std::fs::write(
            &path,
            records.iter().map(|r| format!("{r}\n")).collect::<String>(),
        )
        .unwrap();
        assert_eq!(
            read(&path, 30, &BTreeMap::new(), TrafficScope::Model)
                .unwrap()
                .summary
                .requests,
            1
        );
        assert_eq!(
            read(&path, 30, &BTreeMap::new(), TrafficScope::All)
                .unwrap()
                .summary
                .requests,
            2
        );
    }

    #[test]
    fn counts_websocket_turns_and_http_calls_without_counting_their_connections_twice() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("proxy.log");
        let timestamp = (Local::now() - chrono::Duration::minutes(1)).to_rfc3339();
        let mut rows = vec![
            serde_json::json!({"event":"request_received", "request_id":"ws", "method":"GET", "path":"/v1/responses"}),
            serde_json::json!({"event":"request_finished", "request_id":"ws", "method":"GET", "path":"/v1/responses", "status":"101"}),
            serde_json::json!({"event":"request_finished", "request_id":"http", "model_call_id":"http-call", "method":"POST", "path":"/v1/responses", "status":"200"}),
            serde_json::json!({"event":"request_finished", "request_id":"handshake-only", "method":"GET", "path":"/v1/responses", "status":"101"}),
        ];
        for (call, request, event) in [
            ("one", "ws", "model_call_finished"),
            ("two", "ws", "model_call_cancelled"),
            ("http-call", "http", "model_call_failed"),
        ] {
            rows.push(serde_json::json!({"event":"model_call_started", "request_id":request, "model_call_id":call, "provider":"account", "method":"GET", "path":"/v1/responses"}));
            rows.push(serde_json::json!({"event":event, "request_id":request, "model_call_id":call, "provider":"account", "method":"GET", "path":"/v1/responses", "input_tokens":"10", "output_tokens":"5", "cached_input_tokens":"2"}));
        }
        for row in &mut rows {
            row["timestamp"] = serde_json::json!(timestamp);
        }
        let raw = rows.iter().map(|r| format!("{r}\n")).collect::<String>();
        std::fs::write(&path, &raw).unwrap();
        std::fs::write(path.with_file_name("proxy.log.1"), &raw).unwrap();
        let all = read(&path, 30, &BTreeMap::new(), TrafficScope::All).unwrap();
        let model = read(&path, 30, &BTreeMap::new(), TrafficScope::Model).unwrap();
        assert_eq!(all.summary.requests, 3);
        assert_eq!(model.summary.requests, 3);
        assert_eq!(model.summary.errors, 1);
        assert_eq!(model.summary.input_tokens, Some(30));
        assert_eq!(model.summary.output_tokens, Some(15));
        assert_eq!(model.summary.token_counts.iter().sum::<u64>(), 45);
        assert_eq!(model.summary.cached_input_tokens, Some(6));
        assert_eq!(model.summary.cache_hit_rate, Some(0.2));
        assert_eq!(model.credentials.len(), 1);
    }

    #[test]
    fn active_model_requests_are_counted_once_and_updated_on_completion() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("proxy.log");
        let start = (Local::now() - chrono::Duration::seconds(150)).to_rfc3339();
        let finish = (Local::now() - chrono::Duration::minutes(1)).to_rfc3339();
        let received = serde_json::json!({
            "timestamp":start, "event":"model_call_started", "model_call_id":"call", "request_id":"ws",
            "method":"GET", "path":"/v1/responses",
        });
        let routed = serde_json::json!({
            "timestamp":start, "event":"model_call_updated", "model_call_id":"call", "request_id":"ws",
            "method":"GET", "path":"/v1/responses", "provider":"model-account",
        });
        let response = serde_json::json!({
            "timestamp":start, "event":"model_call_updated", "model_call_id":"call", "request_id":"ws",
            "method":"GET", "path":"/v1/responses", "status":"101",
        });
        let history = format!("{received}\n{routed}\n{response}\n");
        std::fs::write(&path, &history).unwrap();
        let active = read(
            &path,
            30,
            &configured(&["model-account"]),
            TrafficScope::Model,
        )
        .unwrap();
        assert_eq!(active.summary.requests, 1);
        assert_eq!(active.summary.errors, 0);
        assert_eq!(active.summary.avg_ms, None);
        assert_eq!(active.credentials[0].credential, "model-account");

        // The end is read before its start in the rotated file, and duplicated
        // history must not count a second request or overwrite its final result.
        std::fs::write(path.with_file_name("proxy.log.1"), &history).unwrap();
        let finished = serde_json::json!({
            "timestamp":finish, "event":"model_call_failed", "model_call_id":"call", "request_id":"ws",
            "method":"GET", "path":"/v1/responses", "status":"101",
            "received_bytes":"128", "duration_ms":"60000",
        });
        std::fs::write(&path, format!("{finished}\n{history}{finished}\n")).unwrap();
        let completed = read(
            &path,
            30,
            &configured(&["model-account"]),
            TrafficScope::Model,
        )
        .unwrap();
        assert_eq!(completed.summary.requests, 1);
        assert_eq!(completed.summary.errors, 1);
        assert_eq!(completed.summary.bytes, 128);
        assert_eq!(completed.summary.avg_ms, Some(60000));
        assert_eq!(completed.credentials[0].credential, "model-account");
        assert_eq!(completed.summary.counts, active.summary.counts);
    }

    #[test]
    fn requests_are_bucketed_by_start_and_management_is_excluded_from_model_scope() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("proxy.log");
        let old = (Local::now() - chrono::Duration::minutes(40)).to_rfc3339();
        let now = (Local::now() - chrono::Duration::minutes(1)).to_rfc3339();
        let records = [
            serde_json::json!({"timestamp":old, "event":"request_received",
                "request_id":"old", "method":"GET", "path":"/v1/responses"}),
            serde_json::json!({"timestamp":now, "event":"request_finished",
                "request_id":"old", "method":"GET", "path":"/v1/responses", "status":"101"}),
            serde_json::json!({"timestamp":now, "event":"request_received",
                "request_id":"new", "method":"POST", "path":"/anthropic/v1/messages"}),
            serde_json::json!({"timestamp":now, "event":"model_call_started",
                "request_id":"new", "model_call_id":"new-call", "method":"POST", "path":"/anthropic/v1/messages"}),
            serde_json::json!({"timestamp":now, "event":"request_received",
                "request_id":"usage", "method":"GET", "path":"/backend-api/wham/usage"}),
        ];
        std::fs::write(
            &path,
            records.iter().map(|r| format!("{r}\n")).collect::<String>(),
        )
        .unwrap();
        let model = read(&path, 30, &BTreeMap::new(), TrafficScope::Model).unwrap();
        let all = read(&path, 30, &BTreeMap::new(), TrafficScope::All).unwrap();
        assert_eq!(model.summary.requests, 1);
        assert_eq!(all.summary.requests, 2);
    }

    #[test]
    fn scope_filters_both_summary_and_credential_buckets_across_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("proxy.log");
        let timestamp = (Local::now() - chrono::Duration::minutes(1)).to_rfc3339();
        let row = |event: &str, method: &str, path: &str, status, bytes, duration, provider| {
            let mut record = serde_json::json!({
                "timestamp":timestamp, "event":event, "method":method, "path":path,
                "status":status, "received_bytes":bytes, "duration_ms":duration,
                "provider":provider, "service":"codex", "request_id":uuid::Uuid::new_v4().to_string(),
            });
            let model_event = match (method, path, event) {
                ("POST", "/v1/responses", "request_finished") => Some("model_call_finished"),
                ("POST", "/v1/responses", "request_failed") => Some("model_call_failed"),
                ("POST", "/v1/responses", "request_cancelled") => Some("model_call_cancelled"),
                _ => None,
            };
            if let Some(model_event) = model_event {
                record["model_call_id"] = serde_json::json!(uuid::Uuid::new_v4().to_string());
                let connection = format!("{record}\n");
                record["event"] = serde_json::json!(model_event);
                format!("{connection}{record}\n")
            } else {
                format!("{record}\n")
            }
        };
        std::fs::write(
            &path,
            row(
                "request_finished",
                "POST",
                "/v1/responses",
                "200",
                "100",
                "10",
                "model",
            ) + &row(
                "request_finished",
                "GET",
                "/backend-api/wham/usage",
                "200",
                "500",
                "100",
                "management",
            ) + &row(
                "request_failed",
                "POST",
                "/v1/responses",
                "502",
                "0",
                "40",
                "model",
            ),
        )
        .unwrap();
        std::fs::write(
            dir.path().join("proxy.log.1"),
            row(
                "request_finished",
                "GET",
                "/v1/responses",
                "101",
                "50",
                "20",
                "model",
            ) + &row(
                "request_cancelled",
                "POST",
                "/v1/responses",
                "200",
                "10",
                "30",
                "model",
            ) + &row(
                "request_finished",
                "POST",
                "/oauth/token",
                "200",
                "300",
                "100",
                "management",
            ),
        )
        .unwrap();
        let all = read(
            &path,
            30,
            &configured(&["model", "management"]),
            TrafficScope::All,
        )
        .unwrap();
        let model = read(
            &path,
            30,
            &configured(&["model", "management"]),
            TrafficScope::Model,
        )
        .unwrap();
        assert_eq!(all.summary.requests, 6);
        assert_eq!(all.credentials.len(), 2);
        assert_eq!(model.summary.requests, 3);
        assert_eq!(model.summary.errors, 1);
        assert_eq!(model.summary.bytes, 110);
        assert_eq!(model.summary.avg_ms, Some(26));
        assert_eq!(model.summary.counts.iter().sum::<u64>(), 3);
        assert_eq!(model.summary.error_counts.iter().sum::<u64>(), 1);
        assert_eq!(model.credentials.len(), 1);
        assert_eq!(model.credentials[0].credential, "model");
        assert_eq!(model.credentials[0].counts, model.summary.counts);
    }

    #[test]
    fn codex_docs_and_mcp_requests_share_codex_without_inventing_an_account() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("proxy.log");
        let timestamp = (Local::now() - chrono::Duration::minutes(1)).to_rfc3339();
        let records = [
            serde_json::json!({"event":"request_finished", "timestamp":timestamp,
                "service":"codex", "account_id":"account", "path":"/v1/responses"}),
            serde_json::json!({"event":"request_finished", "timestamp":timestamp,
                "service":"codex", "path":"/mcp/openaiDeveloperDocs"}),
            serde_json::json!({"event":"request_rejected", "timestamp":timestamp,
                "path":"/mcp/openaiDeveloperDocs/.well-known/openid-configuration"}),
        ];
        std::fs::write(
            &path,
            records
                .iter()
                .map(|r| r.to_string() + "\n")
                .collect::<String>(),
        )
        .unwrap();
        let result = read(&path, 30, &configured(&["account"]), TrafficScope::All).unwrap();
        assert_eq!(result.summary.requests, 3);
        assert_eq!(result.credentials.len(), 2);
        assert!(
            result
                .credentials
                .iter()
                .all(|group| group.service == "Codex")
        );
        assert_eq!(
            result
                .credentials
                .iter()
                .find(|group| group.credential == "Unidentified")
                .unwrap()
                .requests,
            2
        );
    }

    #[test]
    fn thirty_day_traffic_reads_archives_without_counting_imported_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("proxy.log");
        let history = dir.path().join("history");
        std::fs::create_dir(&history).unwrap();
        let row = |days: i64, id: &str| {
            serde_json::json!({
            "timestamp": (Local::now() - chrono::Duration::days(days) - chrono::Duration::minutes(1)).to_rfc3339(),
            "event":"request_finished", "service":"claude", "request_id":id,
            "account_id":"retained-account", "status":"200", "received_bytes":"100"
        }).to_string() + "\n"
        };
        let recent = row(0, "recent");
        std::fs::write(&path, &recent).unwrap();
        std::fs::write(
            history.join("proxy.log.1.imported.jsonl"),
            recent + &row(29, "history") + &row(31, "expired"),
        )
        .unwrap();
        let monthly = read(&path, 43200, &BTreeMap::new(), TrafficScope::All).unwrap();
        assert_eq!(monthly.summary.requests, 2);
        assert_eq!(monthly.credentials[0].service, "Claude");
        assert_eq!(
            read(&path, 30, &BTreeMap::new(), TrafficScope::All)
                .unwrap()
                .summary
                .requests,
            1
        );
    }
    #[test]
    fn rotated_files_are_summarized_once_until_they_change() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("proxy.log");
        let history = dir.path().join("history");
        std::fs::create_dir(&history).unwrap();
        let timestamp = (Local::now() - chrono::Duration::days(2)).to_rfc3339();
        let row = |status: &str, id: &str| {
            serde_json::json!({
                "timestamp": timestamp,
                "event":"request_finished", "service":"claude", "request_id":id,
                "status":status, "received_bytes":"100"
            })
            .to_string()
                + "\n"
        };
        let archive = history.join("proxy.log.1.archived.jsonl");
        let original = row("200", "a");
        let changed = row("500", "a");
        assert_eq!(original.len(), changed.len());
        std::fs::write(&archive, &original).unwrap();
        let modified = std::fs::metadata(&archive).unwrap().modified().unwrap();
        let errors = || {
            read(&path, 43200, &BTreeMap::new(), TrafficScope::All)
                .unwrap()
                .summary
                .errors
        };
        assert_eq!(errors(), 0);
        // Rotated files never change in place, so the summary is reused: an
        // edit keeping the length and modification time is not read.
        std::fs::write(&archive, &changed).unwrap();
        let file = std::fs::File::options().write(true).open(&archive).unwrap();
        file.set_modified(modified).unwrap();
        drop(file);
        let unchanged = std::fs::metadata(&archive).unwrap();
        assert_eq!(unchanged.len(), original.len() as u64);
        assert_eq!(unchanged.modified().unwrap(), modified);
        assert_eq!(errors(), 0);
        // A different stamp is a different file.
        std::fs::write(&archive, row("500", "ab")).unwrap();
        assert_eq!(errors(), 1);
    }

    #[test]
    fn merged_fields_do_not_depend_on_how_files_split_a_request() {
        let mut strings = Strings::default();
        let mut record = |event: &str, fields: serde_json::Value| {
            let entry = Entry {
                seq: 0,
                time: Some(Local::now()),
                event: event.into(),
                fields: fields.as_object().unwrap().clone(),
            };
            Record::new(&entry, &mut strings)
        };
        let records = [
            record("request_received", serde_json::json!({"provider":"first"})),
            record("upstream_response", serde_json::json!({})),
            record("route_selected", serde_json::json!({"provider":"routed"})),
            record("request_finished", serde_json::json!({"status":"200"})),
        ];
        let fold = |records: &[Record]| {
            let mut merged = records[0].clone();
            for record in &records[1..] {
                merged.merge(record);
            }
            merged
        };
        for split in 1..records.len() {
            let mut merged = fold(&records[..split]);
            merged.merge(&fold(&records[split..]));
            let entry = merged.entry();
            assert_eq!(entry.event, "request_finished");
            // The most advanced record holding a field supplies it.
            assert_eq!(entry.get("provider"), Some("routed"));
            assert_eq!(entry.get("status"), Some("200"));
        }
    }

    #[test]
    fn reads_rotated_history_groups_credentials_and_excludes_outside_range() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("proxy.log");
        let line = |age, account: &str, status| {
            serde_json::json!({
            "event": "request_finished", "timestamp": (Local::now() - chrono::Duration::minutes(age)).to_rfc3339(),
            "service": "codex", "account_id": account, "status": status,
            "duration_ms": "20", "received_bytes": "100"
        }).to_string() + "\n"
        };
        std::fs::write(
            &path,
            line(1, "a", "200") + &line(1, "b", "500") + &line(-10, "a", "200"),
        )
        .unwrap();
        std::fs::write(
            dir.path().join("proxy.log.1"),
            line(100, "a", "200") + &line(43201, "a", "200") + "invalid\n",
        )
        .unwrap();
        for (range, bucket_minutes, bars) in [
            (30, 1, 30),
            (360, 15, 24),
            (720, 30, 24),
            (1440, 60, 24),
            (10080, 360, 28),
            (43200, 1440, 30),
        ] {
            let traffic = read(&path, range, &configured(&["a", "b"]), TrafficScope::All).unwrap();
            assert_eq!(traffic.bucket_minutes, bucket_minutes);
            assert_eq!(traffic.summary.counts.len(), bars);
            assert_eq!(traffic.summary.error_counts.len(), bars);
            assert_eq!(
                traffic.summary.counts.iter().sum::<u64>(),
                traffic.summary.requests
            );
            for group in &traffic.credentials {
                assert_eq!(group.counts.len(), bars);
                assert_eq!(group.error_counts.len(), bars);
                assert_eq!(group.error_counts.iter().sum::<u64>(), group.errors);
            }
            assert_eq!(traffic.credentials.len(), 2);
            assert_eq!(traffic.summary.requests, if range == 30 { 2 } else { 3 });
            assert_eq!(traffic.summary.errors, 1);
            assert_eq!(traffic.summary.avg_ms, Some(20));
            let a = &traffic.credentials[0];
            assert_eq!(a.requests, if range == 30 { 1 } else { 2 });
            assert_eq!(a.counts.iter().sum::<u64>(), a.requests);
            assert_eq!(a.bytes, a.requests * 100);
            assert_eq!(a.avg_ms, Some(20));
            assert_eq!(traffic.credentials[1].errors, 1);
        }
        assert!(read(&path, 31, &configured(&["a", "b"]), TrafficScope::All).is_err());
        assert!(
            read(
                &dir.path().join("missing"),
                30,
                &configured(&["a", "b"]),
                TrafficScope::All
            )
            .unwrap()
            .credentials
            .is_empty()
        );
    }

    #[tokio::test]
    async fn shows_routing_selectors_for_saved_accounts_and_api_keys() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("auth.json"),
            r#"{"tokens":{"account_id":"opaque-id","access_token":"test-token"}}"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join(".claude.json"),
            r#"{"oauthAccount":{"accountUuid":"claude-id","emailAddress":"user@example.test"}}"#,
        )
        .unwrap();
        let config = coport::config::Config::parse(&format!(
            "listen_port: 8787\nrequest_timeout_seconds: 3\ncodex:\n  homes: [{0}]\n  routing:\n    account: {{default: none}}\n    api_key: {{MY_API_KEY: none}}\nclaude:\n  config_dirs: [{0}]\n  routing:\n    account: {{'user@example.test': none, default: none}}\n",
            serde_json::to_string(dir.path()).unwrap()
        )).unwrap();
        let labels = config.local_traffic_credential_labels().await;
        assert_eq!(labels[&("Codex".into(), "opaque-id".into())], "default");
        assert_eq!(
            labels[&("Claude".into(), "claude-id".into())],
            "user@example.test"
        );
        let mut groups = BTreeMap::new();
        for (service, account, provider) in [
            ("codex", "opaque-id", ""),
            ("claude", "claude-id", "personal"),
            ("codex", "opaque-id", "MY_API_KEY"),
        ] {
            let e = Entry { seq: 0, time: DateTime::from_timestamp_millis(1000).map(|t| t.with_timezone(&Local)), event: "request_finished".into(), fields: serde_json::from_value(serde_json::json!({"service": service, "account_id": account, "provider": provider})).unwrap() };
            aggregate(&mut groups, &e, 0, 1_800_000, 1, &labels, None);
        }
        assert!(groups.contains_key(&("Codex".into(), "default".into())));
        assert!(groups.contains_key(&("Codex".into(), "MY_API_KEY".into())));
        assert!(groups.contains_key(&("Claude".into(), "user@example.test".into())));
    }

    #[test]
    fn restores_old_ids_from_logged_routing_names_across_history() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("proxy.log");
        let line = |age, service, id, label| {
            serde_json::json!({
                "event": "request_finished",
                "timestamp": (Local::now() - chrono::Duration::minutes(age)).to_rfc3339(),
                "service": service, "account_id": id, "account_label": label, "status": "200"
            })
            .to_string()
                + "\n"
        };
        std::fs::write(
            &path,
            line(40, "codex", "123456", "user@example.test") + &line(1, "codex", "123456", ""),
        )
        .unwrap();
        std::fs::write(
            dir.path().join("proxy.log.1"),
            line(2, "codex", "123456", "") + &line(2, "claude", "123456", ""),
        )
        .unwrap();
        let traffic = read(
            &path,
            30,
            &configured(&["user@example.test"]),
            TrafficScope::All,
        )
        .unwrap();
        let codex = traffic
            .credentials
            .iter()
            .find(|g| g.service == "Codex")
            .unwrap();
        assert_eq!(codex.credential, "user@example.test");
        assert_eq!(codex.requests, 2);
        assert_eq!(
            traffic
                .credentials
                .iter()
                .find(|g| g.service == "Claude")
                .unwrap()
                .credential,
            "Unidentified"
        );
        let labels = BTreeMap::from([(("Codex".into(), "123456".into()), "current-name".into())]);
        let traffic = read(&path, 30, &labels, TrafficScope::All).unwrap();
        assert_eq!(
            traffic
                .credentials
                .iter()
                .find(|g| g.service == "Codex")
                .unwrap()
                .credential,
            "current-name"
        );
    }

    #[test]
    fn buckets_boundaries_and_keeps_services_separate() {
        let mut groups = BTreeMap::new();
        for (time, service) in [
            (0, "codex"),
            (59_999, "codex"),
            (60_000, "claude"),
            (1_799_999, "codex"),
            (1_800_000, "codex"),
            (-1, "codex"),
        ] {
            let e = Entry {
                seq: 0,
                time: DateTime::from_timestamp_millis(time).map(|t| t.with_timezone(&Local)),
                event: "request_failed".into(),
                fields: serde_json::from_value(
                    serde_json::json!({"service": service, "provider": "key"}),
                )
                .unwrap(),
            };
            aggregate(&mut groups, &e, 0, 1_800_000, 1, &BTreeMap::new(), None);
        }
        assert_eq!(groups.len(), 2);
        let codex = &groups[&("Codex".into(), "Unidentified".into())];
        assert_eq!(codex.counts[0], 2);
        assert_eq!(codex.counts[29], 1);
        assert_eq!(codex.errors, 3);
    }

    #[test]
    fn claude_input_counts_cache_reads_and_writes_like_openai() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("proxy.log");
        let timestamp = (Local::now() - chrono::Duration::minutes(1)).to_rfc3339();
        let mut raw = String::new();
        for (service, call, path, usage) in [
            (
                "claude",
                "claude-call",
                "/v1/messages",
                serde_json::json!({"input_tokens":"10", "cached_input_tokens":"30", "cache_creation_input_tokens":"10"}),
            ),
            (
                "codex",
                "codex-call",
                "/v1/responses",
                serde_json::json!({"input_tokens":"40", "cached_input_tokens":"10"}),
            ),
            ("codex", "no-usage", "/v1/responses", serde_json::json!({})),
        ] {
            let mut row = serde_json::json!({"timestamp":timestamp, "event":"model_call_finished", "request_id":call, "model_call_id":call, "service":service, "provider":"account", "method":"POST", "path":path, "status":"200"});
            row.as_object_mut()
                .unwrap()
                .extend(usage.as_object().unwrap().clone());
            raw.push_str(&format!("{row}\n"));
        }
        std::fs::write(&path, raw).unwrap();
        let model = read(&path, 30, &BTreeMap::new(), TrafficScope::Model).unwrap();
        let rate = |service| {
            model
                .credentials
                .iter()
                .find(|c| c.service == service)
                .unwrap()
                .cache_hit_rate
        };
        assert_eq!(rate("Claude"), Some(0.6));
        assert_eq!(rate("Codex"), Some(0.25));
        assert_eq!(model.summary.cache_hit_rate, Some(40.0 / 90.0));
        let input = |service| {
            let group = model
                .credentials
                .iter()
                .find(|c| c.service == service)
                .unwrap();
            (group.input_tokens, group.cached_input_tokens)
        };
        // Claude: 10 uncached + 30 read + 10 written; Codex input already includes its 10 cached.
        assert_eq!(input("Claude"), (Some(50), Some(30)));
        assert_eq!(input("Codex"), (Some(40), Some(10)));
        assert_eq!(model.summary.input_tokens, Some(90));
        assert_eq!(model.summary.cached_input_tokens, Some(40));
        // Bars use the same input plus output tokens; the call without usage adds nothing.
        assert_eq!(model.summary.token_counts.iter().sum::<u64>(), 90);
        let claude = model
            .credentials
            .iter()
            .find(|c| c.service == "Claude")
            .unwrap();
        assert_eq!(claude.token_counts.iter().sum::<u64>(), 50);
        // The parts of Claude input are kept for its tooltip; Codex has none.
        assert_eq!(
            (claude.uncached_input_tokens, claude.cache_write_tokens),
            (Some(10), Some(10))
        );
        let codex = model
            .credentials
            .iter()
            .find(|c| c.service == "Codex")
            .unwrap();
        assert_eq!(
            (codex.uncached_input_tokens, codex.cache_write_tokens),
            (None, None)
        );
        assert_eq!(model.summary.uncached_input_tokens, None);
    }
}
