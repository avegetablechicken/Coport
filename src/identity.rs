//! Credential primitives every App shares: token checks, JWT claims,
//! API Keys read from the environment or a login shell, and the traffic
//! labels and shared-API proofs built from all of them.
use crate::{
    Error, Result,
    codex::Identity,
    config::{Config, expand, unwrap_upstream},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};

/// Whether a credential a request carries is the one a route holds,
/// compared in time that depends on their lengths, not on where they differ.
pub(crate) fn same_secret(held: Option<&str>, sent: &str) -> bool {
    held.is_some_and(|held| {
        held.len() == sent.len()
            && std::hint::black_box(
                held.bytes()
                    .zip(sent.bytes())
                    .fold(0u8, |diff, (a, b)| diff | (a ^ b)),
            ) == 0
    })
}

pub fn valid_token(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b > 32 && b < 127)
}
pub(crate) fn claims(token: &str) -> Value {
    let parts: Vec<_> = token.split('.').collect();
    if parts.len() != 3 {
        return Value::Null;
    }
    URL_SAFE_NO_PAD
        .decode(parts[1].trim_end_matches('='))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null)
}
pub(crate) fn key(raw: &str) -> Result<String> {
    let k = raw.trim();
    if !valid_token(k) {
        return Err(Error::config(
            "API Key must be a nonempty ASCII token without whitespace or control characters.",
        ));
    }
    Ok(k.into())
}
pub(crate) async fn environment_key_with_shell(name: &str, shell: bool) -> Result<String> {
    let raw = match std::env::var(name) {
        Ok(value) => value,
        Err(_) if shell => cached_shell_value(name, shell_value).await?,
        Err(_) => {
            return Err(Error::config(
                "API Key environment variable is unavailable.",
            ));
        }
    };
    key(&raw)
}
/// Login-shell lookups, failures included, are reused briefly: each one runs
/// the user's shell startup files and can take seconds, once per request.
const SHELL_LOOKUP_TTL: std::time::Duration = std::time::Duration::from_secs(30);

type ShellLookups = BTreeMap<String, (std::time::Instant, Result<String>)>;

async fn cached_shell_value<'a, F, Fut>(name: &'a str, lookup: F) -> Result<String>
where
    F: FnOnce(&'a str) -> Fut,
    Fut: std::future::Future<Output = Result<String>>,
{
    static CACHE: std::sync::Mutex<ShellLookups> = std::sync::Mutex::new(BTreeMap::new());
    // Concurrent requests for a missing key wait for one shell, not one each.
    static LOOKUP: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let cached = || {
        let mut cache = CACHE.lock().unwrap();
        cache.retain(|_, (at, _)| at.elapsed() < SHELL_LOOKUP_TTL);
        cache.get(name).map(|(_, value)| value.clone())
    };
    if let Some(value) = cached() {
        return value;
    }
    let _lookup = LOOKUP.lock().await;
    if let Some(value) = cached() {
        return value;
    }
    let value = lookup(name).await;
    CACHE
        .lock()
        .unwrap()
        .insert(name.to_owned(), (std::time::Instant::now(), value.clone()));
    value
}

#[cfg(unix)]
async fn shell_value(name: &str) -> Result<String> {
    use std::{os::unix::process::CommandExt, process::Stdio};
    let bad =
        || Error::config("API Key environment variable is unavailable or shell lookup timed out.");
    if name.is_empty()
        || !name
            .bytes()
            .enumerate()
            .all(|(i, b)| b == b'_' || b.is_ascii_alphabetic() || i > 0 && b.is_ascii_digit())
    {
        return Err(bad());
    }
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let shell_path = std::path::Path::new(&shell);
    if !shell_path.is_absolute()
        || !matches!(
            shell_path.file_name().and_then(|s| s.to_str()),
            Some("zsh" | "bash" | "sh")
        )
    {
        return Err(bad());
    }
    let dir = tempfile::tempdir().map_err(|_| bad())?;
    let output = dir.path().join("value");
    let mut cmd = std::process::Command::new(shell);
    cmd.args([
        "-l",
        "-i",
        "-c",
        "umask 077; exec /usr/bin/printenv \"$1\" > \"$2\"",
        "coport",
        name,
    ])
    .arg(&output)
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .process_group(0);
    let mut cmd = tokio::process::Command::from(cmd);
    cmd.kill_on_drop(true);
    let mut child = cmd.spawn().map_err(|_| bad())?;
    // Kill the process group even on cancellation, including shell startup descendants.
    struct Group(u32);
    impl Drop for Group {
        fn drop(&mut self) {
            unsafe {
                libc::kill(-(self.0 as i32), libc::SIGKILL);
            }
        }
    }
    let group = Group(child.id().ok_or_else(bad)?);
    let status = tokio::time::timeout(std::time::Duration::from_secs(3), child.wait()).await;
    drop(group);
    if !matches!(status, Ok(Ok(s)) if s.success()) {
        let _ = child.wait().await;
        return Err(bad());
    }
    if std::fs::metadata(&output).map_err(|_| bad())?.len() > 65536 {
        return Err(bad());
    }
    std::fs::read_to_string(output).map_err(|_| bad())
}
#[cfg(not(unix))]
async fn shell_value(_: &str) -> Result<String> {
    Err(Error::config(
        "API Key must be exported into the service process environment on Windows.",
    ))
}

/// Processed proof that two local configurations use the same high-entropy API
/// credential and upstream. Neither the credential nor endpoint is exported.
pub fn shared_api_reference(service: &str, upstream: &str, token: &str) -> Option<String> {
    if token.len() < 32 || !valid_token(token) || token.bytes().collect::<HashSet<_>>().len() < 16 {
        return None;
    }
    let upstream = unwrap_upstream(upstream).ok()?;
    let base = upstream.trim_end_matches('/');
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, token.as_bytes());
    let message = format!("coport-shared-api-v1\0{service}\0{base}");
    Some(
        ring::hmac::sign(&key, message.as_bytes())
            .as_ref()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
    )
}
#[derive(Clone)]
pub struct TrafficProviderReference {
    pub service: String,
    pub name: String,
    pub upstream: String,
    pub reference: String,
}
impl Config {
    /// Used locally for matching processed traffic; never invokes a shell or
    /// exports raw credentials/configuration to a peer.
    pub async fn traffic_provider_references(&self) -> Vec<TrafficProviderReference> {
        let mut result = Vec::new();
        for provider in self.codex.providers() {
            if let Ok(credentials) = provider
                .credentials(&self.codex.base_url.api_key, false, &self.codex)
                .await
            {
                for credential in credentials {
                    if let Some(reference) =
                        shared_api_reference("Codex", &credential.upstream, &credential.token)
                    {
                        result.push(TrafficProviderReference {
                            service: "Codex".into(),
                            name: provider.label().into(),
                            upstream: credential.upstream,
                            reference,
                        });
                    }
                }
            }
        }
        for name in self
            .claude
            .routing
            .api_key
            .keys()
            .filter(|name| !crate::claude::settings::is_url(name))
        {
            let pair = match crate::claude::settings::load(&self.claude.config_dirs, name) {
                Ok(Some(profile)) => Some((profile.upstream, profile.token)),
                Ok(None) => environment_key_with_shell(name, false)
                    .await
                    .ok()
                    .map(|token| (self.claude.base_url.clone(), token)),
                Err(_) => None,
            };
            if let Some((upstream, token)) = pair {
                if let Some(reference) = shared_api_reference("Claude", &upstream, &token) {
                    result.push(TrafficProviderReference {
                        service: "Claude".into(),
                        name: name.clone(),
                        upstream,
                        reference,
                    });
                }
            }
        }
        // Antigravity's named keys, as Claude's environment selectors.
        let antigravity = &self.antigravity;
        for name in antigravity.routing.api_key.keys() {
            let Some(upstream) = antigravity.api_key_upstream(name) else {
                continue;
            };
            let Ok(token) = environment_key_with_shell(name, false).await else {
                continue;
            };
            if let Some(reference) = shared_api_reference("Antigravity", upstream, &token) {
                result.push(TrafficProviderReference {
                    service: "Antigravity".into(),
                    name: name.clone(),
                    upstream: upstream.into(),
                    reference,
                });
            }
        }
        // Grok's named keys, as Antigravity's.
        let grok = &self.grok;
        for name in grok.routing.api_key.keys() {
            let Some(upstream) = grok.api_key_upstream(name) else {
                continue;
            };
            let Ok(token) = environment_key_with_shell(name, false).await else {
                continue;
            };
            if let Some(reference) = shared_api_reference("Grok", upstream, &token) {
                result.push(TrafficProviderReference {
                    service: "Grok".into(),
                    name: name.clone(),
                    upstream: upstream.trim_end_matches('/').into(),
                    reference,
                });
            }
        }
        result
    }
    /// Safe display names for recorded account IDs, using routing's selector precedence.
    pub async fn traffic_credential_labels(&self) -> BTreeMap<(String, String), String> {
        self.traffic_labels_with_shell(true).await
    }
    /// Display-only metadata must never run login-shell startup scripts.
    pub async fn local_traffic_credential_labels(&self) -> BTreeMap<(String, String), String> {
        self.traffic_labels_with_shell(false).await
    }
    async fn traffic_labels_with_shell(
        &self,
        allow_shell: bool,
    ) -> BTreeMap<(String, String), String> {
        let mut labels = BTreeMap::new();
        for (source, account) in self.codex.account_sources() {
            if let Ok(identity) = self
                .codex
                .account_identity_with_shell(&account, allow_shell)
                .await
            {
                if let Some(selector) = routing_account_label(
                    &self.codex.routing,
                    &identity.account_id,
                    &identity.usernames,
                    &source,
                ) {
                    labels.insert(("Codex".into(), identity.account_id), selector);
                }
            }
        }
        // Additional saved logins supply display metadata only. They never become
        // accepted credential sources or participate in proxy routing.
        for home in &self.codex.homes {
            let Ok(files) = std::fs::read_dir(expand(home)) else {
                continue;
            };
            let mut paths: Vec<_> = files
                .filter_map(std::result::Result::ok)
                .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
                .filter(|entry| {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    name.starts_with("auth-") && name.ends_with(".json")
                })
                .map(|entry| entry.path())
                .collect();
            paths.sort();
            for path in paths {
                let Ok(identity) = Identity::read(&path.to_string_lossy()) else {
                    continue;
                };
                // Only an explicit ID/email/name match is evidence of a label;
                // default and fallback routes cannot identify an archived login.
                if let Some(selector) = std::iter::once(&identity.account_id)
                    .chain(identity.usernames.iter())
                    .find(|name| self.codex.routing.account.contains_key(*name))
                {
                    labels
                        .entry(("Codex".into(), identity.account_id.clone()))
                        .or_insert_with(|| selector.clone());
                }
            }
        }
        for (source, account) in self.claude.account_sources() {
            if let Some(identity) = account.claude_identity() {
                if let Some(selector) = routing_account_label(
                    &self.claude.routing,
                    &identity.account_id,
                    &identity.usernames,
                    &source,
                ) {
                    labels.insert(("Claude".into(), identity.account_id), selector);
                }
            }
        }
        // Antigravity's records name their route in `account_label`.
        let antigravity = &self.antigravity.routing;
        // API key routes name theirs in `provider`, as Codex's and Claude's.
        for name in antigravity.account.keys().chain(antigravity.api_key.keys()) {
            labels.insert(("Antigravity".into(), name.clone()), name.clone());
        }
        for (name, choice) in [
            ("account_fallback", &antigravity.account_fallback),
            ("fallback", &antigravity.fallback),
        ] {
            if choice.is_some() {
                labels.insert(("Antigravity".into(), name.into()), name.into());
            }
        }
        if antigravity.fallback.is_some() {
            labels.insert(
                (
                    "Antigravity".into(),
                    crate::antigravity::API_KEY_FALLBACK.into(),
                ),
                "fallback".into(),
            );
        }
        // Grok's records name their route as Antigravity's do; its saved
        // logins also by the account `auth.json` names.
        let grok = &self.grok.routing;
        for name in grok.account.keys().chain(grok.api_key.keys()) {
            labels.insert(("Grok".into(), name.clone()), name.clone());
        }
        for (name, choice) in [
            ("account_fallback", &grok.account_fallback),
            ("fallback", &grok.fallback),
        ] {
            if choice.is_some() {
                labels.insert(("Grok".into(), name.into()), name.into());
            }
        }
        if grok.fallback.is_some() {
            labels.insert(
                ("Grok".into(), crate::grok::API_KEY_FALLBACK.into()),
                "fallback".into(),
            );
        }
        self.grok.login_labels(&mut labels);
        for (service, routing) in [
            ("Codex", &self.codex.routing),
            ("Claude", &self.claude.routing),
        ] {
            for name in routing.api_key.keys().chain(routing.account.keys()) {
                labels.insert((service.into(), name.clone()), name.clone());
            }
            if routing.fallback.is_some() {
                let provider = if service == "Codex" {
                    "openai-fallback"
                } else {
                    "claude-api-key-fallback"
                };
                labels.insert((service.into(), provider.into()), "fallback".into());
            }
            if routing.account_fallback.is_some() && service == "Claude" {
                labels.insert(
                    (service.into(), "claude-account-fallback".into()),
                    "account_fallback".into(),
                );
            }
        }
        labels
    }

    pub async fn check_credentials(&self) -> Result<()> {
        self.claude.check_credentials().await?;
        self.grok.check_credentials().await?;
        self.check_codex_credentials().await
    }
}

pub(crate) fn routing_account_label(
    routing: &crate::config::Routing,
    id: &str,
    usernames: &[String],
    source: &str,
) -> Option<String> {
    std::iter::once(id)
        .chain(usernames.iter().map(String::as_str))
        .chain(std::iter::once(source))
        .find(|name| routing.account.contains_key(*name))
        .map(str::to_owned)
        .or_else(|| {
            routing
                .account_fallback
                .as_ref()
                .map(|_| "account_fallback".into())
        })
        .or_else(|| routing.fallback.as_ref().map(|_| "fallback".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_secret_needs_a_held_secret_equal_in_every_byte() {
        assert!(same_secret(Some("token-a"), "token-a"));
        for (held, sent) in [
            (Some("token-a"), "token-b"),
            (Some("token-a"), "token-ab"),
            (Some("token"), ""),
            (None, "token-a"),
        ] {
            assert!(!same_secret(held, sent), "{held:?} {sent}");
        }
    }

    #[test]
    fn shared_api_proofs_bind_service_endpoint_and_secret_without_names() {
        let token = "sk-synthetic-0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ-abcdef";
        let reference = shared_api_reference("Codex", "https://private.example/v1", token).unwrap();
        assert_eq!(
            Some(reference.clone()),
            shared_api_reference("Codex", "https://private.example/v1/", token)
        );
        assert_ne!(
            Some(reference.clone()),
            shared_api_reference("Claude", "https://private.example/v1", token)
        );
        assert_ne!(
            Some(reference),
            shared_api_reference("Codex", "https://other.example/v1", token)
        );
        assert!(shared_api_reference("Codex", "https://private.example/v1", "password").is_none());
        assert!(
            shared_api_reference("Codex", "https://private.example/v1", &"a".repeat(64)).is_none()
        );
    }
    #[tokio::test]
    async fn shell_lookups_are_reused_including_failures_and_not_duplicated() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static CALLS: AtomicUsize = AtomicUsize::new(0);
        async fn lookup(name: &str) -> Result<String> {
            CALLS.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            if name.ends_with("_MISSING") {
                Err(Error::config("missing"))
            } else {
                Ok(format!("{name}-value"))
            }
        }
        let found = "COPORT_TEST_SHELL_FOUND";
        let (a, b) = tokio::join!(
            cached_shell_value(found, lookup),
            cached_shell_value(found, lookup)
        );
        assert_eq!(a.unwrap(), "COPORT_TEST_SHELL_FOUND-value");
        assert_eq!(b.unwrap(), "COPORT_TEST_SHELL_FOUND-value");
        assert_eq!(CALLS.load(Ordering::SeqCst), 1);
        let missing = "COPORT_TEST_SHELL_MISSING";
        assert!(cached_shell_value(missing, lookup).await.is_err());
        assert!(cached_shell_value(missing, lookup).await.is_err());
        assert_eq!(CALLS.load(Ordering::SeqCst), 2);
    }

    /// Route activation as the proxy reports it for `config`.
    async fn activation(config: &Config) -> [BTreeMap<String, &'static str>; 4] {
        let dir = tempfile::tempdir().unwrap();
        let logger = std::sync::Arc::new(crate::observability::logger::Logger::new(
            dir.path().join("proxy.log"),
        ));
        crate::server::Server::new(config.clone(), logger)
            .account_route_states()
            .await
    }

    #[tokio::test]
    async fn route_activation_tracks_saved_logins_and_file_only_mode() {
        let dir = tempfile::tempdir().unwrap();
        let auth = dir.path().join("auth.json");
        let saved = |id: &str| {
            serde_json::json!({"tokens": {"account_id": id, "access_token": "token"}}).to_string()
        };
        std::fs::write(&auth, saved("first")).unwrap();
        std::fs::write(dir.path().join("auth-second.json"), saved("second")).unwrap();
        let mut config = Config::parse(&format!(
            "listen_port: 8787\nrequest_timeout_seconds: 3\ncodex:\n  homes: [{}]\n  routing:\n    account: {{first: none, second: none}}\nclaude:\n  config_dirs: [{}]\n  routing:\n    account: {{'user@example.com': none}}\n",
            serde_json::to_string(dir.path()).unwrap(), serde_json::to_string(dir.path()).unwrap()
        )).unwrap();
        let states = activation(&config).await;
        assert_eq!(states[0]["first"], "active");
        assert_eq!(states[0]["second"], "inactive");
        assert_eq!(states[1]["user@example.com"], "inactive");
        std::fs::write(&auth, saved("second")).unwrap();
        let states = activation(&config).await;
        assert_eq!(states[0]["first"], "inactive");
        assert_eq!(states[0]["second"], "active");
        std::fs::write(
            dir.path().join(".credentials.json"),
            r#"{"claudeAiOauth":{"accessToken":"token"}}"#,
        )
        .unwrap();
        config.claude.routing.account_probe = Some(crate::config::Choice::One("none".into()));
        assert_eq!(activation(&config).await[1]["user@example.com"], "unknown");
        std::fs::write(
            dir.path().join(".claude.json"),
            r#"{"oauthAccount":{"accountUuid":"uuid","emailAddress":"user@example.com"}}"#,
        )
        .unwrap();
        assert_eq!(activation(&config).await[1]["user@example.com"], "active");
        // Local OAuth identity takes precedence over a configured source label.
        config
            .claude
            .routing
            .account
            .insert("default".into(), crate::config::Choice::One("none".into()));
        let states = activation(&config).await;
        assert_eq!(states[1]["user@example.com"], "active");
        assert_eq!(states[1]["default"], "inactive");
        std::fs::write(dir.path().join(".credentials.json"), "{}").unwrap();
        assert_eq!(activation(&config).await[1]["user@example.com"], "inactive");
        config.codex.account_auth_file_only = false;
        config.claude.account_auth_file_only = false;
        assert!(activation(&config).await.iter().all(BTreeMap::is_empty));
    }
    #[tokio::test]
    async fn route_activation_never_runs_a_shell_for_environment_sources() {
        // Unset here; only a login shell could supply it, which status never runs.
        let config = Config::parse(
            "listen_port: 8787\nrequest_timeout_seconds: 3\ncodex:\n  homes: []\n  auth_env: COPORT_TEST_UNSET_ACCOUNT_STATE\n  routing:\n    account: {default: none, first: none}\nclaude:\n  config_dirs: []\n  auth_env: COPORT_TEST_UNSET_ACCOUNT_STATE\n  routing:\n    account: {default: none}\n",
        )
        .unwrap();
        let states = activation(&config).await;
        assert!(
            states[0].values().all(|state| *state == "unknown"),
            "{states:?}"
        );
        assert_eq!(states[1]["default"], "unknown");
    }
}
