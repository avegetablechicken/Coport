//! Claude and Codex logins by refresh token, learned from the token responses
//! the proxy relays. A refresh request carries nothing else that names its
//! account, and a refresh token cannot be looked up upstream; the response
//! names the account its new refresh token belongs to. Antigravity's saved
//! login keeps its refresh token while its access tokens rotate, so the
//! account a lookup named for it is kept the same way. Only a SHA-256 digest
//! of each refresh token is kept, so neither memory nor the file holds a
//! credential.
use crate::{antigravity::AntigravityIdentity, claude::ClaudeIdentity, identity::Identity};
use std::{
    collections::HashMap,
    io::Write,
    marker::PhantomData,
    path::PathBuf,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

/// The files, beside the configuration, that keep learned routes across restarts.
pub const FILE: &str = "claude-refresh-routes.json";
pub const GROK_FILE: &str = "grok-token-routes.json";
pub const CODEX_FILE: &str = "codex-refresh-routes.json";
pub const ANTIGRAVITY_FILE: &str = "antigravity-refresh-routes.json";

/// The account a learned refresh token routes as.
pub trait Account: Clone + Send + 'static {
    /// Grok clients can keep rotated opaque access tokens outside the saved login.
    const TRACK_ACCESS_TOKEN: bool = false;
    /// The account a successful token response names, filling in optional
    /// claims from the previous login when the subject matches.
    fn named_by(response: &serde_json::Value, previous: Option<&Self>) -> Option<Self>;
    fn learned(account_id: String, usernames: Vec<String>) -> Self;
    fn account_id(&self) -> &str;
    fn usernames(&self) -> &[String];
}
impl Account for ClaudeIdentity {
    fn named_by(response: &serde_json::Value, _: Option<&Self>) -> Option<Self> {
        Self::token_response(response)
    }
    fn learned(account_id: String, usernames: Vec<String>) -> Self {
        Self {
            account_id,
            usernames,
        }
    }
    fn account_id(&self) -> &str {
        &self.account_id
    }
    fn usernames(&self) -> &[String] {
        &self.usernames
    }
}
/// A learned Codex account carries no token of its own.
impl Account for Identity {
    fn named_by(response: &serde_json::Value, _: Option<&Self>) -> Option<Self> {
        Self::token_response(response)
    }
    fn learned(account_id: String, usernames: Vec<String>) -> Self {
        Self {
            account_id,
            token: String::new(),
            refresh_token: None,
            usernames,
        }
    }
    fn account_id(&self) -> &str {
        &self.account_id
    }
    fn usernames(&self) -> &[String] {
        &self.usernames
    }
}
/// An Antigravity account, keyed by email, is learned from Google's token
/// responses, whose ID token names it, or from a lookup.
impl Account for AntigravityIdentity {
    // Apps that renew a login themselves keep its new access token; learned
    // with its refresh token, it still names the account after a restart.
    const TRACK_ACCESS_TOKEN: bool = true;

    fn named_by(response: &serde_json::Value, _: Option<&Self>) -> Option<Self> {
        Self::token_response(response)
    }
    fn learned(account_id: String, _: Vec<String>) -> Self {
        Self { email: account_id }
    }
    fn account_id(&self) -> &str {
        &self.email
    }
    fn usernames(&self) -> &[String] {
        &[]
    }
}
/// A login rotates its refresh token on every renewal; a client may still
/// retry a recent generation. A generation is one renewal: its refresh
/// token and, for Grok, its access token, learned together.
const PER_ACCOUNT: usize = 4;
const LIMIT: usize = 256;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Learned {
    account_id: String,
    #[serde(default)]
    usernames: Vec<String>,
    /// Unix milliseconds, unique and increasing; the oldest go first.
    learned_at: u64,
}

pub struct RefreshRoutes<A = ClaudeIdentity> {
    /// Absent: learned routes last as long as the process.
    path: Option<PathBuf>,
    routes: Mutex<HashMap<String, Learned>>,
    account: PhantomData<fn() -> A>,
}
impl<A> Default for RefreshRoutes<A> {
    fn default() -> Self {
        Self {
            path: None,
            routes: Default::default(),
            account: PhantomData,
        }
    }
}

fn digest(refresh_token: &str) -> String {
    ring::digest::digest(&ring::digest::SHA256, refresh_token.as_bytes())
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

impl<A: Account> RefreshRoutes<A> {
    /// Routes saved at `path`; a missing or unreadable file starts empty.
    pub fn load(path: PathBuf) -> Self {
        let routes = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Self {
            path: Some(path),
            routes: Mutex::new(routes),
            account: PhantomData,
        }
    }

    /// Every learned account, for display labels.
    pub fn identities(&self) -> Vec<A> {
        let Ok(routes) = self.routes.lock() else {
            return Vec::new();
        };
        routes
            .values()
            .map(|learned| A::learned(learned.account_id.clone(), learned.usernames.clone()))
            .collect()
    }

    pub fn identity(&self, refresh_token: &str) -> Option<A> {
        let routes = self.routes.lock().ok()?;
        routes
            .get(&digest(refresh_token))
            .map(|learned| A::learned(learned.account_id.clone(), learned.usernames.clone()))
    }

    pub(crate) fn access_identity(&self, access: &str) -> Option<A> {
        self.identity(&format!("access\0{access}"))
    }

    /// Records the account of a refresh token, keeping the newest few of
    /// each account, and saves the routes when they have a file.
    pub fn learn(&self, refresh_token: &str, identity: &A) -> std::io::Result<()> {
        self.learn_tokens(refresh_token, None, identity)
    }

    fn learn_tokens(
        &self,
        refresh_token: &str,
        access: Option<&str>,
        identity: &A,
    ) -> std::io::Result<()> {
        let mut routes = self
            .routes
            .lock()
            .map_err(|_| std::io::Error::other("Refresh routes unavailable"))?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_millis() as u64);
        let learned_at = routes
            .values()
            .map(|learned| learned.learned_at + 1)
            .max()
            .map_or(now, |next| next.max(now));
        let learned = Learned {
            account_id: identity.account_id().into(),
            usernames: identity.usernames().to_vec(),
            learned_at,
        };
        if let Some(access) = access {
            routes.insert(digest(&format!("access\0{access}")), learned.clone());
        }
        routes.insert(digest(refresh_token), learned);
        // Whole generations go, so a renewal's access token never outlives
        // or crowds out its refresh token.
        let mut generations: Vec<u64> = routes
            .values()
            .filter(|learned| learned.account_id == identity.account_id())
            .map(|learned| learned.learned_at)
            .collect();
        generations.sort_unstable();
        generations.dedup();
        if let Some(&kept) = generations.iter().rev().nth(PER_ACCOUNT - 1) {
            routes.retain(|_, learned| {
                learned.account_id != identity.account_id() || learned.learned_at >= kept
            });
        }
        while routes.len() > LIMIT {
            let Some(oldest) = routes.values().map(|learned| learned.learned_at).min() else {
                break;
            };
            routes.retain(|_, learned| learned.learned_at != oldest);
        }
        let Some(path) = &self.path else {
            return Ok(());
        };
        let directory = path.parent().unwrap_or(std::path::Path::new("."));
        std::fs::create_dir_all(directory)?;
        // Owner-only and atomic, like the daemon's other state files.
        let mut temp = tempfile::NamedTempFile::new_in(directory)?;
        temp.write_all(&serde_json::to_vec(&*routes)?)?;
        temp.as_file().sync_all()?;
        temp.persist(path).map_err(|error| error.error)?;
        Ok(())
    }

    /// Learns from a successful token response: its new refresh token
    /// belongs to the account it names, else to `previous`, the account of
    /// the refresh token it replaces.
    pub fn learn_response(&self, body: &[u8], previous: Option<&A>) -> std::io::Result<bool> {
        self.learn_response_for(body, None, previous)
    }
    /// `learn_response`, where a response without a new refresh token, as
    /// Google's, keeps `sent`, the refresh token it renewed.
    pub fn learn_response_for(
        &self,
        body: &[u8],
        sent: Option<&str>,
        previous: Option<&A>,
    ) -> std::io::Result<bool> {
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) else {
            return Ok(false);
        };
        let Some(refresh_token) = value["refresh_token"]
            .as_str()
            .or(sent)
            .filter(|token| crate::identity::valid_token(token))
        else {
            return Ok(false);
        };
        let Some(identity) = A::named_by(&value, previous).or_else(|| previous.cloned()) else {
            return Ok(false);
        };
        let access = value["access_token"]
            .as_str()
            .filter(|s| A::TRACK_ACCESS_TOKEN && crate::identity::valid_token(s));
        self.learn_tokens(refresh_token, access, &identity)?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(id: &str) -> ClaudeIdentity {
        ClaudeIdentity {
            account_id: id.into(),
            usernames: vec![format!("{id}@example.invalid")],
        }
    }

    #[test]
    fn learned_routes_survive_a_restart_without_their_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        RefreshRoutes::<ClaudeIdentity>::load(path.clone())
            .learn_response(
                br#"{"access_token":"access-secret","refresh_token":"refresh-secret","account":{"uuid":"acct-1","email_address":"one@example.invalid"}}"#,
                None,
            )
            .unwrap();
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(!saved.contains("refresh-secret"));
        assert!(!saved.contains("access-secret"));
        let routes = RefreshRoutes::<ClaudeIdentity>::load(path);
        let learned = routes.identity("refresh-secret").unwrap();
        assert_eq!(learned.account_id, "acct-1");
        assert_eq!(learned.usernames, ["one@example.invalid"]);
        assert!(routes.identity("other").is_none());
    }

    #[test]
    fn google_responses_keep_the_sent_refresh_token_and_name_it_by_id_token() {
        let routes = RefreshRoutes::<AntigravityIdentity>::default();
        // {"email":"remote@example.invalid","sub":"1"}
        let body = br#"{"access_token":"a","id_token":"h.eyJlbWFpbCI6InJlbW90ZUBleGFtcGxlLmludmFsaWQiLCJzdWIiOiIxIn0.s"}"#;
        assert!(routes.learn_response_for(body, Some("sent"), None).unwrap());
        assert_eq!(
            routes.identity("sent").unwrap().email,
            "remote@example.invalid"
        );
        // Without a sent token, as for Codex and Claude, nothing is learned.
        assert!(!routes.learn_response(body, None).unwrap());
        // Nor without an account: no ID token and no previous one.
        assert!(
            !routes
                .learn_response_for(br#"{"access_token":"a"}"#, Some("other"), None)
                .unwrap()
        );
        assert!(routes.identity("other").is_none());
    }

    #[test]
    fn a_response_without_its_account_inherits_the_previous_one() {
        let routes = RefreshRoutes::default();
        let previous = identity("acct-1");
        assert!(
            routes
                .learn_response(br#"{"refresh_token":"next"}"#, Some(&previous))
                .unwrap()
        );
        assert_eq!(routes.identity("next").unwrap().account_id, "acct-1");
        for body in [
            &br#"{"refresh_token":"orphan"}"#[..],
            br#"{"account":{"uuid":"acct-2"}}"#,
            b"data: not json",
        ] {
            assert!(!routes.learn_response(body, None).unwrap());
        }
        assert!(routes.identity("orphan").is_none());
    }

    #[test]
    fn codex_responses_name_their_account_by_token_claims() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CODEX_FILE);
        // {"https://api.openai.com/auth":{"chatgpt_account_id":"acct-1"},"email":"one@example.invalid"}
        let access = "h.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjdC0xIn0sImVtYWlsIjoib25lQGV4YW1wbGUuaW52YWxpZCJ9.s";
        let routes = RefreshRoutes::<Identity>::load(path.clone());
        assert!(
            routes
                .learn_response(
                    format!(r#"{{"access_token":"{access}","refresh_token":"next"}}"#).as_bytes(),
                    None,
                )
                .unwrap()
        );
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(!saved.contains(access) && !saved.contains("next"));
        let learned = RefreshRoutes::<Identity>::load(path)
            .identity("next")
            .unwrap();
        assert_eq!(learned.account_id, "acct-1");
        assert_eq!(learned.usernames, ["one@example.invalid"]);
        assert!(learned.token.is_empty());
        assert!(
            !routes
                .learn_response(
                    br#"{"access_token":"opaque","refresh_token":"orphan"}"#,
                    None
                )
                .unwrap()
        );
    }

    #[test]
    fn only_recent_generations_of_each_account_are_kept() {
        let routes = RefreshRoutes::default();
        for n in 0..6 {
            routes.learn(&format!("one-{n}"), &identity("one")).unwrap();
            routes.learn(&format!("two-{n}"), &identity("two")).unwrap();
        }
        for account in ["one", "two"] {
            for n in 0..6 {
                assert_eq!(
                    routes.identity(&format!("{account}-{n}")).is_some(),
                    n >= 6 - PER_ACCOUNT,
                    "{account}-{n}"
                );
            }
        }
        for n in 0..LIMIT + 1 {
            routes
                .learn(&format!("many-{n}"), &identity(&format!("many-{n}")))
                .unwrap();
        }
        assert_eq!(routes.routes.lock().unwrap().len(), LIMIT);
        assert!(routes.identity("many-0").is_none());
        assert!(routes.identity(&format!("many-{LIMIT}")).is_some());
    }

    /// Grok keeps a renewal's access token beside its refresh token: they
    /// share a generation, and go together.
    #[test]
    fn grok_access_tokens_take_no_generation_of_their_own() {
        use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
        let access = |account: &str, n: usize| {
            let claims = serde_json::json!({"sub": account, "n": n}).to_string();
            format!("h.{}.s", URL_SAFE_NO_PAD.encode(claims))
        };
        let renew = |routes: &RefreshRoutes<crate::grok::GrokAccount>, account: &str, n: usize| {
            let body = serde_json::json!({"access_token": access(account, n), "refresh_token": format!("{account}-{n}")});
            assert!(
                routes
                    .learn_response(body.to_string().as_bytes(), None)
                    .unwrap()
            );
        };
        let routes = RefreshRoutes::<crate::grok::GrokAccount>::default();
        for n in 0..6 {
            renew(&routes, "alice", n);
        }
        for n in 0..6 {
            let kept = n >= 6 - PER_ACCOUNT;
            assert_eq!(
                routes.identity(&format!("alice-{n}")).is_some(),
                kept,
                "{n}"
            );
            assert_eq!(
                routes.access_identity(&access("alice", n)).is_some(),
                kept,
                "{n}"
            );
        }
        // Past the limit, the oldest renewals go whole.
        for n in 0..LIMIT {
            renew(&routes, &format!("user{n}"), 0);
        }
        assert!(routes.routes.lock().unwrap().len() <= LIMIT);
        for n in 0..LIMIT {
            let account = format!("user{n}");
            assert_eq!(
                routes.identity(&format!("{account}-0")).is_some(),
                routes.access_identity(&access(&account, 0)).is_some(),
                "{account}"
            );
        }
        assert!(routes.identity(&format!("user{}-0", LIMIT - 1)).is_some());
    }
}
