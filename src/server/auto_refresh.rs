//! Opt-in, reactive renewal of saved OAuth logins for model requests only.
use super::*;
use crate::{
    claude::refresh_lock::RefreshLock,
    config::AccountSource,
    saved_login::{self as login, SavedLogin, Store, with_login},
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
    /// The configured directory of a Codex or Claude login; none for
    /// Antigravity's one saved login.
    source: Option<AccountSource>,
    store: Store,
    service: Service,
    value: Value,
}
fn fingerprint(service: Service, token: &str) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, token.as_bytes());
    format!("{}:{:02x?}", service.name(), digest.as_ref())
}
/// What renewing a request's login needs: its saved login and a copy of the
/// request to send again.
pub(super) struct Renewal {
    snapshot: Snapshot,
    retry: reqwest::Request,
    claude_identity: Option<crate::claude::ClaudeIdentity>,
}
impl Renewal {
    pub(super) fn new(
        snapshot: Option<Snapshot>,
        request: &reqwest::Request,
        claude_identity: Option<crate::claude::ClaudeIdentity>,
    ) -> Option<Self> {
        Some(Self {
            snapshot: snapshot?,
            retry: request.try_clone()?,
            claude_identity,
        })
    }
}
impl Server {
    /// On an upstream 401, renews the request's saved login and sends the
    /// request once more with the new token; any other response, or one
    /// whose renewal fails, is returned as it came.
    pub(super) async fn retry_after_renewal(
        &self,
        response: reqwest::Response,
        renewal: Option<Renewal>,
        choice: &Choice,
        native_tls: bool,
        deadline: Deadline,
        log: &mut RequestLog,
    ) -> Result<reqwest::Response> {
        let Some(mut renewal) = renewal.filter(|_| response.status() == 401) else {
            return Ok(response);
        };
        let (snapshot, routed) = (&renewal.snapshot, renewal.claude_identity.as_ref());
        let renewed = with_login!(snapshot.service, L => {
            self.renew_access::<L>(snapshot, choice, routed, deadline, log)
                .await
        });
        match renewed {
            Ok(token) => {
                let headers = renewal.retry.headers_mut();
                headers.insert("authorization", format!("Bearer {token}").parse().unwrap());
                // OAuth routes always use Authorization, never an API key.
                headers.remove("x-api-key");
                // Only Model calls renew their login.
                record_upstream_response(log, response.status().as_u16(), Upstream::ModelCall);
                drop(response);
                self.send_via(
                    choice,
                    renewal.retry,
                    native_tls,
                    deadline,
                    Upstream::ModelCall,
                    log,
                )
                .await
            }
            Err(error) => {
                log.field("oauth_refresh", "failed");
                log.field("oauth_refresh_reason", error.message);
                Ok(response)
            }
        }
    }
    /// The current access token of the login this server rotated away from
    /// `token`, while that login is still the one it wrote.
    pub(super) async fn refreshed_alias(&self, service: Service, token: &str) -> Option<String> {
        with_login!(service, L => self.alias::<L>(token).await)
    }
    async fn alias<L: SavedLogin>(&self, token: &str) -> Option<String> {
        let key = fingerprint(L::APP, token);
        let snapshot = self
            .auto_refresh
            .aliases
            .lock()
            .unwrap()
            .get(&key)
            .cloned()?;
        // An external logout, rotation, backend change or account switch revokes
        // this alias. Never silently follow a directory into a different login.
        let (store, value) = L::load(snapshot.source.as_ref(), true).await.ok()?;
        if store != snapshot.store || value != snapshot.value {
            return None;
        }
        login::access::<L>(&value).map(String::from)
    }
    /// The one Saved login whose access token is `token`, if it can renew.
    pub(super) async fn refresh_snapshot(&self, service: Service, token: &str) -> Option<Snapshot> {
        with_login!(service, L => self.snapshot::<L>(token).await)
    }
    async fn snapshot<L: SavedLogin>(&self, token: &str) -> Option<Snapshot> {
        let mut found = None;
        for source in L::sources(&self.config) {
            let Ok((store, value)) = L::load(source.as_ref(), true).await else {
                continue;
            };
            if login::access::<L>(&value) == Some(token) && login::refresh::<L>(&value).is_some() {
                if found.is_some() {
                    return None;
                }
                found = Some(Snapshot {
                    source,
                    store,
                    service: L::APP,
                    value,
                });
            }
        }
        found
    }
    /// `replaced` is the saved login the rotation was written over: the
    /// request's snapshot, plus any unrelated edits made meanwhile.
    fn remember_rotation<L: SavedLogin>(&self, old: &Snapshot, replaced: &Value, updated: Value) {
        let mut aliases = self.auto_refresh.aliases.lock().unwrap();
        // Update aliases only for this exact generation, not other accounts
        // that happened to use the same storage path in the past.
        for alias in aliases.values_mut() {
            if alias.store == old.store && alias.service == old.service && &alias.value == replaced
            {
                alias.value = updated.clone();
            }
        }
        if aliases.len() >= 256 {
            aliases.clear();
        }
        let key = fingerprint(L::APP, login::access::<L>(&old.value).unwrap());
        aliases.insert(
            key,
            Snapshot {
                value: updated,
                ..old.clone()
            },
        );
    }
    async fn renew_access<L: SavedLogin>(
        &self,
        snapshot: &Snapshot,
        choice: &Choice,
        claude_identity: Option<&crate::claude::ClaudeIdentity>,
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
        let client = L::client().await?;
        // Claude Code and other renewers of this login take the same lock and
        // reread the login inside it, so one refresh token is spent only once.
        let lock = L::lock(snapshot.source.as_ref(), deadline.remaining()).await?;
        // Others wait for this lock: hold it for one bounded refresh only.
        let deadline = if lock.is_some() {
            deadline.cap(LOCKED_REFRESH_SECONDS)
        } else {
            deadline
        };
        let (store, current) = L::load(snapshot.source.as_ref(), false).await?;
        if store != snapshot.store {
            return Err(Error::config("Login storage changed during request."));
        }
        if !same_generation::<L>(&current, &snapshot.value) {
            // Only reuse a rotation performed by this server. Another renewal
            // or an account switch is left to the client, which rereads its
            // current login; it may name another account than this request's.
            let old = login::access::<L>(&snapshot.value).unwrap();
            return self
                .alias::<L>(old)
                .await
                .ok_or(Error::config("Saved login changed during request."));
        }
        let refresh =
            login::refresh::<L>(&current).ok_or(Error::config("Login has no refresh token."))?;
        let generation = fingerprint(L::APP, refresh);
        if attempt.failed.as_ref().is_some_and(|(key, at, permanent)| {
            key == &generation && (*permanent || at.elapsed() < Duration::from_secs(60))
        }) {
            return Err(Error::config("OAuth refresh is paused for this login."));
        }
        if lock.as_ref().is_some_and(RefreshLock::compromised) {
            return Err(Error::config("Login refresh lock was taken over."));
        }
        let mut request = login::request::<L>(&current, &client)?;
        // Never send incoming model headers, cookies or its access token here.
        request
            .headers_mut()
            .insert("accept", "application/json".parse().unwrap());
        attempt.failed = Some((generation.clone(), Instant::now(), false));
        let path = request.url().path().to_owned();
        let fields = json!({"request_id":uuid::Uuid::new_v4().to_string(), "service":snapshot.service.name(), "method":"POST", "path":path, "reason":"automatic_oauth_refresh"});
        // Its status, not its last event, says whether the renewal succeeded.
        let mut refresh_log = RequestLog::auxiliary(
            self.logger.clone(),
            "oauth_refresh",
            fields.as_object().unwrap().clone(),
            Outcome::Finished,
        );
        let response = self
            .send_via(
                choice,
                request,
                false,
                deadline,
                Upstream::Auxiliary,
                &mut refresh_log,
            )
            .await?;
        record_upstream_response(
            &mut refresh_log,
            response.status().as_u16(),
            Upstream::Auxiliary,
        );
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
        // Validate the response before touching the saved login.
        login::merge::<L>(&current, &payload)?;
        let _rotation = deadline.run(self.auto_refresh.rotation.write()).await?;
        // Recheck the active backend and generation before writing. Edits
        // that leave the spent refresh token in place (another field, an MCP
        // login) are kept, and the new tokens merged into them: dropping them
        // would leave only the spent token saved. A login whose refresh token
        // changed meanwhile belongs to someone else and is never overwritten.
        let (active_store, active) = L::load(snapshot.source.as_ref(), false).await?;
        if active_store != store || login::refresh::<L>(&active) != Some(refresh) {
            return Err(Error::config("Saved login changed during refresh."));
        }
        let updated = login::merge::<L>(&active, &payload)?;
        L::learn(self, refresh, &updated, &payload, claude_identity);
        store.replace(&active, &updated).await?;
        drop(lock);
        let token = login::access::<L>(&updated).unwrap().to_owned();
        self.remember_rotation::<L>(snapshot, &active, updated);
        attempt.failed = None;
        log.field("oauth_refresh", "succeeded");
        Ok(token)
    }
}

/// Both tokens unchanged: the same login generation, whatever else changed.
fn same_generation<L: SavedLogin>(a: &Value, b: &Value) -> bool {
    login::access::<L>(a) == login::access::<L>(b)
        && login::refresh::<L>(a) == login::refresh::<L>(b)
}

/// The longest a refresh holds another program's lock, which it waits for.
const LOCKED_REFRESH_SECONDS: f64 = 30.0;
