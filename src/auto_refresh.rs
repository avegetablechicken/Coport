//! Opt-in, reactive renewal of saved OAuth logins for model requests only.
use super::*;
use crate::{
    config::AccountSource,
    oauth_credentials::{self as credentials, Store},
};
use serde_json::Value;

#[derive(Default)]
pub(super) struct RefreshState {
    // Keep routing aliases and persisted credentials consistent for in-flight requests.
    pub(super) rotation: tokio::sync::RwLock<()>,
    gates: Mutex<HashMap<Store, Arc<tokio::sync::Mutex<Attempt>>>>,
    aliases: Mutex<HashMap<String, Snapshot>>,
}
#[derive(Default)]
struct Attempt {
    // Mark before sending: cancellation and transport ambiguity must not cause
    // another request to immediately consume the same rotating refresh token.
    failed: Option<(String, Instant, bool)>,
}
#[derive(Clone)]
pub(super) struct Snapshot {
    source: AccountSource,
    store: Store,
    service: Service,
    value: Value,
}
fn fingerprint(service: Service, token: &str) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, token.as_bytes());
    format!("{}:{:02x?}", service.name(), digest.as_ref())
}
impl Server {
    pub(super) async fn refreshed_alias(&self, service: Service, token: &str) -> Option<String> {
        let key = fingerprint(service, token);
        let snapshot = self
            .auto_refresh
            .aliases
            .lock()
            .unwrap()
            .get(&key)
            .cloned()?;
        // An external logout, rotation, backend change or account switch revokes
        // this alias. Never silently follow a directory into a different login.
        let (store, value) = credentials::load(&snapshot.source, service).await.ok()?;
        if store != snapshot.store || value != snapshot.value {
            return None;
        }
        credentials::access(&value, service).map(String::from)
    }
    pub(super) async fn refresh_snapshot(&self, service: Service, token: &str) -> Option<Snapshot> {
        let sources = match service {
            Service::Codex => self.config.codex.account_sources(),
            Service::Claude => self.config.claude.account_sources(),
        };
        let mut found = None;
        for (_, source) in sources {
            let Ok((store, value)) = credentials::load(&source, service).await else {
                continue;
            };
            if credentials::access(&value, service) == Some(token)
                && credentials::refresh(&value, service).is_some()
            {
                if found.is_some() {
                    return None;
                }
                found = Some(Snapshot {
                    source,
                    store,
                    service,
                    value,
                });
            }
        }
        found
    }
    fn remember_rotation(&self, old: &Snapshot, updated: Value) {
        let mut aliases = self.auto_refresh.aliases.lock().unwrap();
        // Update aliases only for this exact generation, not other accounts
        // that happened to use the same storage path in the past.
        for alias in aliases.values_mut() {
            if alias.store == old.store && alias.service == old.service && alias.value == old.value
            {
                alias.value = updated.clone();
            }
        }
        if aliases.len() >= 256 {
            aliases.clear();
        }
        let key = fingerprint(
            old.service,
            credentials::access(&old.value, old.service).unwrap(),
        );
        aliases.insert(
            key,
            Snapshot {
                value: updated,
                ..old.clone()
            },
        );
    }
    pub(super) async fn renew_access(
        &self,
        snapshot: &Snapshot,
        choice: &Choice,
        deadline: Deadline,
        log: &mut RequestLog,
    ) -> Result<String> {
        let gate = self
            .auto_refresh
            .gates
            .lock()
            .unwrap()
            .entry(snapshot.store.clone())
            .or_default()
            .clone();
        let mut attempt = deadline.run(gate.lock()).await?;
        let (store, current) = credentials::load(&snapshot.source, snapshot.service).await?;
        if store != snapshot.store {
            return Err(Error::config("Login storage changed during request."));
        }
        if current != snapshot.value {
            // Only reuse a rotation performed by this server. An unrelated
            // external edit must be retried by the client with its current login.
            let old = credentials::access(&snapshot.value, snapshot.service).unwrap();
            return self
                .refreshed_alias(snapshot.service, old)
                .await
                .ok_or(Error::config("Saved login changed during request."));
        }
        let refresh = credentials::refresh(&current, snapshot.service)
            .ok_or(Error::config("Login has no refresh token."))?;
        let generation = fingerprint(snapshot.service, refresh);
        if attempt.failed.as_ref().is_some_and(|(key, at, permanent)| {
            key == &generation && (*permanent || at.elapsed() < Duration::from_secs(60))
        }) {
            return Err(Error::config("OAuth refresh is paused for this login."));
        }
        let mut request = refresh_request(snapshot.service, &current)?;
        // Never send incoming model headers, cookies or its access token here.
        request
            .headers_mut()
            .insert("accept", "application/json".parse().unwrap());
        attempt.failed = Some((generation.clone(), Instant::now(), false));
        let mut refresh_log = RequestLog { logger: self.logger.clone(), fields: json!({"request_id":uuid::Uuid::new_v4().to_string(), "service":snapshot.service.name(), "method":"POST", "path":"/oauth/token", "reason":"automatic_oauth_refresh"}).as_object().unwrap().clone(), started:Instant::now(), status:0, bytes:0, outcome:"oauth_refresh_finished" };
        let response = self
            .send_via(choice, request, false, deadline, &mut refresh_log)
            .await?;
        refresh_log.status = response.status().as_u16();
        if !response.status().is_success() {
            // Rejected generations stay blocked until the saved login changes.
            if matches!(response.status().as_u16(), 400 | 401 | 403) {
                attempt.failed = Some((generation, Instant::now(), true));
            }
            return Err(Error::config("OAuth refresh was rejected."));
        }
        let payload = deadline
            .run(async {
                let mut stream = response.bytes_stream();
                let mut bytes = Vec::new();
                while let Some(chunk) = stream.next().await {
                    let chunk =
                        chunk.map_err(|_| Error::config("Cannot read OAuth refresh response."))?;
                    if bytes.len() + chunk.len() > 65536 {
                        return Err(Error::config("OAuth refresh response exceeds limit."));
                    }
                    bytes.extend_from_slice(&chunk);
                }
                serde_json::from_slice::<Value>(&bytes)
                    .map_err(|_| Error::config("Invalid OAuth refresh response."))
            })
            .await??;
        let updated = merge_tokens(snapshot.service, &current, &payload)?;
        let _rotation = deadline.run(self.auto_refresh.rotation.write()).await?;
        // Recheck the active backend as well as its contents before writing.
        let (active_store, active) = credentials::load(&snapshot.source, snapshot.service).await?;
        if active_store != store || active != current {
            return Err(Error::config("Saved login changed during refresh."));
        }
        store.replace(&current, &updated).await?;
        let token = credentials::access(&updated, snapshot.service)
            .unwrap()
            .to_owned();
        self.remember_rotation(snapshot, updated);
        attempt.failed = None;
        log.field("oauth_refresh", "succeeded");
        Ok(token)
    }
}

fn refresh_request(service: Service, value: &Value) -> Result<reqwest::Request> {
    let refresh =
        credentials::refresh(value, service).ok_or(Error::config("Login has no refresh token."))?;
    let mut request = reqwest::Request::new(
        hyper::Method::POST,
        Url::parse(match service {
            Service::Codex => TOKEN_REFRESH_UPSTREAM,
            Service::Claude => crate::claude::TOKEN_REFRESH_UPSTREAM,
        })
        .unwrap(),
    );
    let (content_type, body) = match service {
        Service::Codex => {
            let claims =
                crate::identity::claims(value["tokens"]["id_token"].as_str().unwrap_or(""));
            if claims["iss"]
                .as_str()
                .is_some_and(|s| s.trim_end_matches('/') != "https://auth.openai.com")
            {
                return Err(Error::config("Unsupported OAuth issuer."));
            }
            let client = claims["aud"]
                .as_str()
                .or_else(|| claims["aud"].as_array()?.iter().find_map(Value::as_str))
                .filter(|s| crate::identity::valid_token(s))
                .unwrap_or("app_EMoamEEZ73f0CkXaXp7hrann");
            let body = url::form_urlencoded::Serializer::new(String::new())
                .append_pair("grant_type", "refresh_token")
                .append_pair("refresh_token", refresh)
                .append_pair("client_id", client)
                .finish();
            ("application/x-www-form-urlencoded", body)
        }
        Service::Claude => {
            let scopes: Vec<_> = value["claudeAiOauth"]["scopes"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .collect();
            let scope = if scopes.is_empty() {
                "user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload".into()
            } else {
                scopes.join(" ")
            };
            ("application/json", json!({"grant_type":"refresh_token", "refresh_token":refresh, "client_id":"9d1c250a-e61b-44d9-88ed-5944d1962f5e", "scope":scope}).to_string())
        }
    };
    request
        .headers_mut()
        .insert("content-type", content_type.parse().unwrap());
    *request.body_mut() = Some(body.into());
    Ok(request)
}
fn merge_tokens(service: Service, original: &Value, payload: &Value) -> Result<Value> {
    let access = payload["access_token"]
        .as_str()
        .filter(|s| crate::identity::valid_token(s))
        .ok_or(Error::config(
            "OAuth refresh returned no valid access token.",
        ))?;
    for field in ["refresh_token", "id_token"] {
        if !payload[field].is_null()
            && !payload[field]
                .as_str()
                .is_some_and(crate::identity::valid_token)
        {
            return Err(Error::config("OAuth refresh returned an invalid token."));
        }
    }
    let mut updated = original.clone();
    match service {
        Service::Codex => {
            // Do not allow an unexpected response to switch the routed account.
            if crate::identity::Identity::from_token(access).is_some_and(|id| {
                Some(id.account_id.as_str()) != original["tokens"]["account_id"].as_str()
            }) {
                return Err(Error::config("OAuth refresh changed the account."));
            }
            updated["tokens"]["access_token"] = access.into();
            for field in ["refresh_token", "id_token"] {
                if payload[field].is_string() {
                    updated["tokens"][field] = payload[field].clone();
                }
            }
            updated["last_refresh"] = time::OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap()
                .into();
        }
        Service::Claude => {
            updated["claudeAiOauth"]["accessToken"] = access.into();
            if payload["refresh_token"].is_string() {
                updated["claudeAiOauth"]["refreshToken"] = payload["refresh_token"].clone();
            }
            for (field, saved) in [
                ("expires_in", "expiresAt"),
                ("refresh_token_expires_in", "refreshTokenExpiresAt"),
            ] {
                if let Some(seconds) = payload[field].as_u64() {
                    let now = time::OffsetDateTime::now_utc().unix_timestamp().max(0) as u64;
                    updated["claudeAiOauth"][saved] =
                        now.saturating_add(seconds).saturating_mul(1000).into();
                }
            }
            if let Some(scope) = payload["scope"].as_str() {
                updated["claudeAiOauth"]["scopes"] =
                    scope.split_whitespace().collect::<Vec<_>>().into();
            }
        }
    }
    Ok(updated)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn renewal_preserves_optional_tokens_and_updates_both_claude_expiries() {
        let original =
            json!({"claudeAiOauth":{"accessToken":"old", "refreshToken":"keep", "other":true}});
        let updated = merge_tokens(Service::Claude, &original, &json!({"access_token":"new", "expires_in":60, "refresh_token_expires_in":120, "scope":"user:profile user:inference"})).unwrap();
        let oauth = &updated["claudeAiOauth"];
        assert_eq!(oauth["refreshToken"], "keep");
        assert_eq!(oauth["other"], true);
        assert_eq!(
            oauth["refreshTokenExpiresAt"].as_u64().unwrap() - oauth["expiresAt"].as_u64().unwrap(),
            60_000
        );
        assert_eq!(oauth["scopes"], json!(["user:profile", "user:inference"]));
        for payload in [
            json!({}),
            json!({"access_token":"bad token"}),
            json!({"access_token":"new", "refresh_token":42}),
        ] {
            assert!(merge_tokens(Service::Claude, &original, &payload).is_err());
        }
    }
}
