//! Activity traffic is read from retained logs, independently of the capped UI list.
use crate::logs::Entry;
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashSet},
    fs::File,
    io::{BufRead, BufReader},
    path::Path,
};

/// Endpoint category, not an assertion that the upstream actually charged quota.
#[derive(Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TrafficScope {
    #[default]
    All,
    Model,
}

fn is_model_request(entry: &Entry) -> bool {
    let raw = entry
        .get("path")
        .unwrap_or("")
        .split('?')
        .next()
        .unwrap_or("");
    let path = raw
        .strip_prefix("/codex/")
        .or_else(|| raw.strip_prefix("/anthropic/"))
        .or_else(|| raw.strip_prefix("/claude/"))
        .unwrap_or(raw)
        .trim_start_matches('/');
    let parsed = if path.starts_with("https://") || path.starts_with("http://") {
        reqwest::Url::parse(path).ok()
    } else {
        None
    };
    let path = parsed
        .as_ref()
        .map(|u| u.path())
        .unwrap_or(path)
        .trim_matches('/');
    let ends = |suffix: &str| path == suffix || path.ends_with(&format!("/{suffix}"));
    match entry.get("method") {
        Some("POST") => {
            ends("responses")
                || ends("responses/compact")
                || ends("messages")
                || ends("chat/completions")
                || ends("completions")
        }
        // Codex's streaming WebSocket upgrade uses GET on the model endpoint.
        Some("GET") => ends("responses"),
        _ => false,
    }
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
}

#[derive(Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialTraffic {
    service: String,
    credential: String,
    requests: u64,
    errors: u64,
    bytes: u64,
    avg_ms: Option<u64>,
    counts: Vec<u64>,
    error_counts: Vec<u64>,
    #[serde(skip)]
    latency_total: u64,
    #[serde(skip)]
    latency_count: u64,
}

pub fn read(
    path: &Path,
    minutes: u64,
    labels: &BTreeMap<(String, String), String>,
    scope: TrafficScope,
) -> Result<Traffic, String> {
    let bucket_minutes = match minutes {
        30 => 1,
        360 => 12,
        720 => 24,
        1440 => 48,
        10080 => 336,
        43200 => 1440,
        _ => return Err("Unsupported traffic range".into()),
    };
    let end = Local::now().timestamp_millis();
    let start = end - minutes as i64 * 60_000;
    let mut groups = BTreeMap::new();
    let mut entries = Vec::new();
    let mut labels = labels.clone();
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
    let current = [path, backup.as_path()].map(open);
    let archives = agent_router::logger::history_paths(path)
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
    let files = current.into_iter().chain(archives.map(|p| open(&p)));
    let mut seen_requests = HashSet::new();
    #[cfg(unix)]
    let mut identities = std::collections::HashSet::new();
    for file in files {
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
            let entry = Entry {
                seq: 0,
                event,
                time,
                fields,
            };
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
                labels
                    .entry((service.to_owned(), id.to_owned()))
                    .or_insert_with(|| label.to_owned());
            }
            if entry.is_request_end()
                && entry
                    .time
                    .is_some_and(|t| t.timestamp_millis() >= start && t.timestamp_millis() < end)
            {
                // Imported stderr history may overlap the live/rotated logs.
                let identity = entry
                    .get("request_id")
                    .filter(|id| !id.is_empty())
                    .map(|id| format!("{}:{id}", entry.event))
                    .unwrap_or_else(|| {
                        format!(
                            "{}:{:?}:{}",
                            entry.event,
                            entry.time,
                            serde_json::to_string(&entry.fields).unwrap_or_default()
                        )
                    });
                if seen_requests.insert(identity) {
                    entries.push(entry);
                }
            }
        }
    }
    for entry in &entries {
        if scope == TrafficScope::Model && !is_model_request(entry) {
            continue;
        }
        aggregate(&mut groups, entry, start, end, bucket_minutes, &labels);
    }
    let mut summary = CredentialTraffic {
        counts: vec![0; 30],
        error_counts: vec![0; 30],
        ..Default::default()
    };
    for group in groups.values() {
        summary.requests += group.requests;
        summary.errors += group.errors;
        summary.bytes += group.bytes;
        summary.latency_total += group.latency_total;
        summary.latency_count += group.latency_count;
        for i in 0..30 {
            summary.counts[i] += group.counts[i];
            summary.error_counts[i] += group.error_counts[i];
        }
    }
    summary.avg_ms =
        (summary.latency_count > 0).then(|| summary.latency_total / summary.latency_count);
    let mut credentials: Vec<_> = groups.into_values().collect();
    credentials.sort_by_key(|group| {
        (
            group.credential == "Unidentified",
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
    })
}

fn aggregate(
    groups: &mut BTreeMap<(String, String), CredentialTraffic>,
    e: &Entry,
    start: i64,
    end: i64,
    bucket_minutes: u64,
    labels: &BTreeMap<(String, String), String>,
) {
    if !e.is_request_end() {
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
    // Resolve account IDs to the exact selector used in routing, without exposing tokens.
    let credential = e
        .get("provider")
        .and_then(|name| labels.get(&(service.clone(), name.to_owned())).cloned())
        .or_else(|| {
            e.get("account_id")
                .and_then(|id| labels.get(&(service.clone(), id.to_owned())).cloned())
        })
        .or_else(|| {
            e.get("account_label")
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        })
        .or_else(|| {
            e.get("provider").filter(|s| !s.is_empty()).map(|name| {
                match name {
                    "openai-fallback" | "claude-api-key-fallback" => "api_key_fallback",
                    "claude-account-fallback" => "account_fallback",
                    _ => name,
                }
                .to_owned()
            })
        })
        .or_else(|| {
            e.get("account_id")
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "Unidentified".into());
    let group = groups
        .entry((service.clone(), credential.clone()))
        .or_insert_with(|| CredentialTraffic {
            service,
            credential,
            counts: vec![0; 30],
            error_counts: vec![0; 30],
            ..Default::default()
        });
    let slot = ((time - start) / (bucket_minutes as i64 * 60_000)) as usize;
    group.counts[slot] += 1;
    group.requests += 1;
    if e.is_error() {
        group.errors += 1;
        group.error_counts[slot] += 1;
    }
    group.bytes += e.bytes();
    if let Some(ms) = e.duration_ms() {
        group.latency_total += ms;
        group.latency_count += 1;
        group.avg_ms = Some(group.latency_total / group.latency_count);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quota_category_matches_inference_not_management_endpoints() {
        for (method, path, expected) in [
            ("POST", "/v1/responses", true),
            ("POST", "/backend-api/codex/responses/compact", true),
            (
                "GET",
                "/codex/https://example.invalid/v1/responses?model=x",
                true,
            ),
            ("POST", "/anthropic/v1/messages", true),
            (
                "POST",
                "/claude/https://example.invalid/api/v1/messages",
                true,
            ),
            ("POST", "/v1/chat/completions", true),
            ("POST", "/v1/completions", true),
            ("POST", "/anthropic/v1/messages/count_tokens", false),
            ("GET", "/v1/responses/response-id", false),
            ("GET", "/backend-api/wham/usage", false),
            ("GET", "/anthropic/api/oauth/usage", false),
            ("POST", "/anthropic/v1/oauth/token", false),
            ("POST", "/backend-api/codex/analytics-events/events", false),
            ("POST", "/mcp/openaiDeveloperDocs", false),
            ("GET", "/v1/models", false),
            ("CONNECT", "api.anthropic.com:443", false),
            ("POST", "/v1/not-responses", false),
        ] {
            let entry = Entry {
                seq: 0,
                time: None,
                event: "request_finished".into(),
                fields: serde_json::from_value(serde_json::json!({"method":method,"path":path}))
                    .unwrap(),
            };
            assert_eq!(is_model_request(&entry), expected, "{method} {path}");
        }
    }

    #[test]
    fn scope_filters_both_summary_and_credential_buckets_across_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("proxy.log");
        let timestamp = (Local::now() - chrono::Duration::minutes(1)).to_rfc3339();
        let row = |event, method, path, status, bytes, duration, provider| {
            serde_json::json!({
                "timestamp":timestamp, "event":event, "method":method, "path":path,
                "status":status, "received_bytes":bytes, "duration_ms":duration,
                "provider":provider, "service":"codex",
            })
            .to_string()
                + "\n"
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
        let all = read(&path, 30, &BTreeMap::new(), TrafficScope::All).unwrap();
        let model = read(&path, 30, &BTreeMap::new(), TrafficScope::Model).unwrap();
        assert_eq!(all.summary.requests, 6);
        assert_eq!(all.credentials.len(), 2);
        assert_eq!(model.summary.requests, 4);
        assert_eq!(model.summary.errors, 1);
        assert_eq!(model.summary.bytes, 160);
        assert_eq!(model.summary.avg_ms, Some(25));
        assert_eq!(model.summary.counts.iter().sum::<u64>(), 4);
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
        let result = read(&path, 30, &BTreeMap::new(), TrafficScope::All).unwrap();
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
        for range in [30, 360, 720, 1440, 10080, 43200] {
            let traffic = read(&path, range, &BTreeMap::new(), TrafficScope::All).unwrap();
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
        assert!(read(&path, 31, &BTreeMap::new(), TrafficScope::All).is_err());
        assert!(
            read(
                &dir.path().join("missing"),
                30,
                &BTreeMap::new(),
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
        let config = agent_router::config::Config::parse(&format!(
            "listen_port: 8787\nrequest_timeout_seconds: 3\ncodex:\n  homes: [{0}]\n  auth_file: auth.json\n  routing:\n    account: {{default: none}}\n    api_key: {{MY_API_KEY: none}}\nclaude:\n  config_dirs: [{0}]\n  auth_file: .credentials.json\n  routing:\n    account: {{'user@example.test': none, default: none}}\n",
            serde_json::to_string(dir.path()).unwrap()
        )).unwrap();
        let labels = config.traffic_credential_labels().await;
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
            aggregate(&mut groups, &e, 0, 1_800_000, 1, &labels);
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
        let traffic = read(&path, 30, &BTreeMap::new(), TrafficScope::All).unwrap();
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
            "123456"
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
            aggregate(&mut groups, &e, 0, 1_800_000, 1, &BTreeMap::new());
        }
        assert_eq!(groups.len(), 2);
        let codex = &groups[&("Codex".into(), "key".into())];
        assert_eq!(codex.counts[0], 2);
        assert_eq!(codex.counts[29], 1);
        assert_eq!(codex.errors, 3);
    }
}
