//! Claude and Codex logins by refresh token, learned from the token responses
//! the proxy relays. A refresh request carries nothing else that names its
//! account, and a refresh token cannot be looked up upstream; the response
//! names the account its new refresh token belongs to. Only a SHA-256 digest
//! of each refresh token is kept, so neither memory nor the file holds a
//! credential.
use crate::{claude::ClaudeIdentity, identity::Identity};
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
pub const CODEX_FILE: &str = "codex-refresh-routes.json";

/// The account a learned refresh token routes as.
pub trait Account: Clone + Send + 'static {
    /// The account a successful token response names.
    fn named_by(response: &serde_json::Value) -> Option<Self>;
    fn learned(account_id: String, usernames: Vec<String>) -> Self;
    fn account_id(&self) -> &str;
    fn usernames(&self) -> &[String];
}
impl Account for ClaudeIdentity {
    fn named_by(response: &serde_json::Value) -> Option<Self> {
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
    fn named_by(response: &serde_json::Value) -> Option<Self> {
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
/// A login rotates its refresh token on every renewal; a client may still
/// retry a recent generation.
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

    pub fn identity(&self, refresh_token: &str) -> Option<A> {
        let routes = self.routes.lock().ok()?;
        routes
            .get(&digest(refresh_token))
            .map(|learned| A::learned(learned.account_id.clone(), learned.usernames.clone()))
    }

    /// Records the account of a refresh token, keeping the newest few of
    /// each account, and saves the routes when they have a file.
    pub fn learn(&self, refresh_token: &str, identity: &A) -> std::io::Result<()> {
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
        routes.insert(
            digest(refresh_token),
            Learned {
                account_id: identity.account_id().into(),
                usernames: identity.usernames().to_vec(),
                learned_at,
            },
        );
        let mut generations: Vec<(u64, String)> = routes
            .iter()
            .filter(|(_, learned)| learned.account_id == identity.account_id())
            .map(|(key, learned)| (learned.learned_at, key.clone()))
            .collect();
        generations.sort_unstable();
        for (_, key) in generations.iter().rev().skip(PER_ACCOUNT) {
            routes.remove(key);
        }
        while routes.len() > LIMIT {
            let Some(oldest) = routes
                .iter()
                .min_by_key(|(_, learned)| learned.learned_at)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            routes.remove(&oldest);
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
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) else {
            return Ok(false);
        };
        let Some(refresh_token) = value["refresh_token"]
            .as_str()
            .filter(|token| crate::identity::valid_token(token))
        else {
            return Ok(false);
        };
        let Some(identity) = A::named_by(&value).or(previous.cloned()) else {
            return Ok(false);
        };
        self.learn(refresh_token, &identity)?;
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
}
