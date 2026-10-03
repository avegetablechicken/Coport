//! Native Anthropic forwarding, isolated from Codex credential discovery.
use crate::{
    Error, Result,
    config::{
        AccountSource, Choice, Config, Routing, expand, normalize_auth, validate_env,
        validate_upstream,
    },
    identity::{environment_key, environment_key_with_shell, valid_token},
};
use hyper::HeaderMap;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use url::Url;

fn default_base() -> String {
    "https://api.anthropic.com".into()
}
fn file_only() -> bool {
    true
}
#[derive(Clone, Serialize)]
pub struct Claude {
    pub config_dirs: Vec<String>,
    pub account_auth_file_only: bool,
    pub base_url: String,
    pub accounts: BTreeMap<String, AccountSource>,
    pub routing: Routing,
}
impl Default for Claude {
    fn default() -> Self {
        Self {
            config_dirs: crate::config::default_claude_config_dirs(),
            account_auth_file_only: true,
            base_url: default_base(),
            accounts: BTreeMap::new(),
            routing: Routing::default(),
        }
    }
}
impl<'de> Deserialize<'de> for Claude {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        use serde::de::Error as _;
        let mut value = serde_yaml_ng::Value::deserialize(deserializer)?;
        let map = value
            .as_mapping_mut()
            .ok_or_else(|| D::Error::custom("Claude settings must be a mapping."))?;
        // Routing lives under `routing`; the retired inline layout put it on
        // the section itself and on each account.
        if ["api_key", "account_fallback", "api_key_fallback"]
            .iter()
            .any(|key| map.contains_key(*key))
        {
            return Err(D::Error::custom(
                "Claude routing settings belong under claude.routing.",
            ));
        }
        if map.get("base_url").is_some_and(|base| !base.is_string()) {
            return Err(D::Error::custom("Claude base_url must be a single URL."));
        }
        normalize_auth(&mut value).map_err(D::Error::custom)?;
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Normalized {
            #[serde(default = "crate::config::default_claude_config_dirs")]
            config_dirs: Vec<String>,
            #[serde(default = "file_only")]
            account_auth_file_only: bool,
            #[serde(default = "default_base")]
            base_url: String,
            #[serde(default)]
            accounts: BTreeMap<String, AccountSource>,
            #[serde(default)]
            routing: Routing,
        }
        let n: Normalized = serde_yaml_ng::from_value(value).map_err(D::Error::custom)?;
        Ok(Self {
            config_dirs: n.config_dirs,
            account_auth_file_only: n.account_auth_file_only,
            base_url: n.base_url,
            accounts: n.accounts,
            routing: n.routing,
        })
    }
}
impl AccountSource {
    fn claude_credential_path(&self) -> Result<std::path::PathBuf> {
        self.auth_file
            .as_ref()
            .map(|s| expand(s))
            .ok_or(Error::config(
                "Claude account requires an explicit credential source.",
            ))
    }
    pub(crate) fn claude_identity(
        &self,
        directory: Option<&std::path::Path>,
    ) -> Option<ClaudeIdentity> {
        // An environment token is not evidence that it belongs to the local
        // CLI metadata. Such configured sources retain label/fallback routing.
        if self.auth_env.is_some() {
            return None;
        }
        let path = self.claude_credential_path().ok()?;
        let directory = directory.or_else(|| path.parent())?;
        let metadata = if directory == expand("~/.claude") {
            expand("~/.claude.json")
        } else {
            directory.join(".claude.json")
        };
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(metadata).ok()?).ok()?;
        ClaudeIdentity::local(&value["oauthAccount"])
    }
    async fn claude_token(&self) -> Result<String> {
        if let Some(name) = &self.auth_env {
            return environment_key(name).await;
        }
        let path = self.claude_credential_path()?;
        let raw = std::fs::read(path)
            .map_err(|_| Error::config("Cannot read Claude credentials file."))?;
        let value: serde_json::Value = serde_json::from_slice(&raw)
            .map_err(|_| Error::config("Invalid Claude credentials file."))?;
        value["claudeAiOauth"]["accessToken"]
            .as_str()
            .filter(|s| valid_token(s))
            .map(String::from)
            .ok_or(Error::config(
                "Claude credentials require claudeAiOauth.accessToken.",
            ))
    }
}

#[derive(Clone)]
pub struct ClaudeIdentity {
    pub account_id: String,
    pub usernames: Vec<String>,
}
impl ClaudeIdentity {
    fn parse(value: &serde_json::Value, id: &str, fields: &[&str]) -> Option<Self> {
        Some(Self {
            account_id: value[id].as_str().filter(|s| valid_token(s))?.into(),
            usernames: fields
                .iter()
                .filter_map(|key| value[*key].as_str())
                .filter(|s| !s.trim().is_empty() && !s.chars().any(char::is_control))
                .map(String::from)
                .collect(),
        })
    }
    fn local(value: &serde_json::Value) -> Option<Self> {
        Self::parse(
            value,
            "accountUuid",
            &["emailAddress", "displayName", "fullName"],
        )
    }
    pub(crate) fn profile(value: &serde_json::Value) -> Result<Self> {
        Self::parse(
            &value["account"],
            "uuid",
            &["email", "display_name", "full_name"],
        )
        .ok_or(Error::config("Claude profile is missing account identity."))
    }
}

pub struct ClaudeRoute {
    pub token: String,
    pub bearer: bool,
    pub matched_account: bool,
    pub label: String,
    pub proxy: Choice,
    pub identity: Option<ClaudeIdentity>,
    pub needs_profile: bool,
    pub upstream: String,
    pub oauth: bool,
    pub custom_upstream: bool,
}

pub const TOKEN_REFRESH_UPSTREAM: &str = "https://platform.claude.com/v1/oauth/token";

pub fn token_refresh(target: &str) -> bool {
    let target = ["/anthropic", "/claude"]
        .into_iter()
        .find_map(|prefix| {
            target
                .strip_prefix(prefix)
                .filter(|rest| rest.starts_with('/'))
        })
        .unwrap_or(target);
    matches!(
        target,
        "/v1/oauth/token" | "/https://platform.claude.com/v1/oauth/token"
    )
}

/// The namespace covers every Claude Code endpoint. Unprefixed Messages and
/// OAuth endpoints also work; ambiguous endpoints such as /v1/models use /anthropic.
pub fn target(target: &str) -> Option<&str> {
    for prefix in ["/anthropic", "/claude"] {
        if target
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('/'))
        {
            return target.strip_prefix(prefix);
        }
    }
    let path = target.split('?').next().unwrap_or("");
    let path = if path.starts_with("/https://") {
        Url::parse(&path[1..]).ok()?.path().to_owned()
    } else {
        path.to_owned()
    };
    (path == "/v1/messages" || path.starts_with("/v1/messages/") || path.starts_with("/api/oauth/"))
        .then_some(target)
}

impl Claude {
    pub(crate) fn account_sources(&self) -> Vec<crate::config::DirectoryAccount> {
        crate::config::directory_accounts(
            &self.accounts,
            &self.config_dirs,
            &self.routing,
            ".credentials.json",
        )
    }

    pub(crate) fn validate(&self, config: &Config) -> Result<()> {
        crate::config::validate_directories(&self.config_dirs, &self.accounts)?;
        validate_upstream(&self.base_url)?;
        if self.routing.mcp_fallback.is_some() {
            return Err(Error::config(
                "mcp_fallback is only supported under codex.routing.",
            ));
        }
        for (label, account) in &self.accounts {
            if label.trim().is_empty() {
                return Err(Error::config("Empty Claude account label."));
            }
            account.validate()?;
        }
        if self.account_auth_file_only
            && self.accounts.is_empty()
            && self.config_dirs.is_empty()
            && !self.routing.account.is_empty()
        {
            return Err(Error::config(
                "Claude file-only account routing requires an auth_file or auth_env.",
            ));
        }
        for (label, choice) in &self.routing.account {
            if label.trim().is_empty() {
                return Err(Error::config("Empty Claude routing identifier."));
            }
            config.validate_choice(choice)?;
        }
        for (name, choice) in &self.routing.api_key {
            if crate::claude_settings::is_url(name) {
                crate::claude_api::validate_api_upstream(name)?;
            } else {
                crate::claude_settings::validate_name(name)?;
            }
            config.validate_choice(choice)?;
        }
        crate::url_routing::validate_routes(&self.url_routes())?;
        for choice in [
            &self.routing.account_fallback,
            &self.routing.account_probe,
            &self.routing.api_key_fallback,
        ]
        .into_iter()
        .flatten()
        {
            config.validate_choice(choice)?;
        }
        Ok(())
    }

    fn url_routes(&self) -> BTreeMap<String, Choice> {
        self.routing
            .api_key
            .iter()
            .filter(|(name, _)| crate::claude_settings::is_url(name))
            .map(|(name, choice)| (name.clone(), choice.clone()))
            .collect()
    }

    /// The same named settings lookup used for forwarding, without exposing credentials.
    pub fn api_key_upstream(&self, selector: &str) -> Result<Option<String>> {
        if crate::claude_settings::is_url(selector) {
            return Ok(None);
        }
        Ok(Some(
            crate::claude_settings::load(&self.config_dirs, selector)?
                .map(|profile| profile.upstream)
                .unwrap_or_else(|| self.base_url.clone()),
        ))
    }

    pub async fn check_credentials(&self) -> Result<()> {
        let mut keys = HashSet::new();
        if self.account_auth_file_only {
            for (label, account, directory) in &self.account_sources() {
                self.account_choice(
                    account.claude_identity(directory.as_deref()).as_ref(),
                    Some(label),
                )?;
                if !keys.insert(account.claude_token().await?) {
                    return Err(Error::config(
                        "Multiple Claude routes have the same credential.",
                    ));
                }
            }
        }
        for name in self
            .routing
            .api_key
            .keys()
            .filter(|s| !crate::claude_settings::is_url(s))
        {
            let token = match crate::claude_settings::load(&self.config_dirs, name)? {
                Some(profile) => profile.token,
                None => {
                    validate_env(name)?;
                    environment_key_with_shell(name, true).await?
                }
            };
            if !keys.insert(token) {
                return Err(Error::config(
                    "Multiple Claude routes have the same credential.",
                ));
            }
        }
        Ok(())
    }

    /// Refresh credentials select the same local account route as access tokens.
    /// No profile lookup is possible with a refresh token alone.
    pub async fn resolve_refresh(&self, token: &str) -> Result<(Choice, Option<String>)> {
        let mut matches = Vec::new();
        let mut unavailable = false;
        for (label, account, directory) in self.account_sources() {
            if account.auth_env.is_some() {
                continue;
            }
            let value = account.claude_credential_path().and_then(|path| {
                let raw = std::fs::read(path)
                    .map_err(|_| Error::config("Cannot read Claude credentials file."))?;
                serde_json::from_slice::<serde_json::Value>(&raw)
                    .map_err(|_| Error::config("Invalid Claude credentials file."))
            });
            match value {
                Ok(value) if value["claudeAiOauth"]["refreshToken"].as_str() == Some(token) => {
                    matches.push((label, account.claude_identity(directory.as_deref())));
                }
                Ok(_) => {}
                Err(_) => unavailable = true,
            }
        }
        if matches.len() > 1 {
            return Err(Error::new(
                409,
                "Claude refresh credential matches multiple routes.",
            ));
        }
        if let Some((label, identity)) = matches.pop() {
            let proxy = self.account_choice(identity.as_ref(), Some(&label))?;
            return Ok((proxy, identity.map(|i| i.account_id)));
        }
        if !self.account_auth_file_only {
            if let Some(proxy) = &self.routing.account_fallback {
                return Ok((proxy.clone(), None));
            }
        }
        if unavailable {
            Err(Error::config(
                "No matching Claude refresh route; credential source unavailable.",
            ))
        } else {
            Err(Error::new(
                403,
                "Claude refresh requires a saved refreshToken or an explicit account fallback with account_auth_file_only: false.",
            ))
        }
    }

    pub async fn resolve(&self, headers: &HeaderMap) -> Result<ClaudeRoute> {
        let auth = headers.get("authorization");
        let key = headers.get("x-api-key");
        if auth.is_some() && key.is_some() {
            return Err(Error::new(
                400,
                "Supply only one Claude authentication header.",
            ));
        }
        let bearer = auth.is_some();
        let token = if bearer {
            auth.and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
        } else {
            key.and_then(|v| v.to_str().ok())
        }
        .filter(|s| valid_token(s))
        .ok_or(Error::new(
            401,
            "Claude requires x-api-key or Bearer authentication.",
        ))?;
        let mut matches = Vec::new();
        let mut identity = None;
        let mut unavailable = false;
        let mut matched_account = false;
        let mut api_upstream = None;
        let mut local_needs_profile = false;
        let account_sources = self.account_sources();
        if bearer {
            let mut sources = Vec::new();
            for (label, account, directory) in &account_sources {
                match account.claude_token().await {
                    Ok(value) if value == token => sources.push((label, account, directory)),
                    Ok(_) => {}
                    Err(_) => unavailable = true,
                }
            }
            if sources.len() > 1 {
                return Err(Error::new(
                    409,
                    "Claude credential matches multiple routes.",
                ));
            }
            if let Some((label, account, directory)) = sources.pop() {
                matched_account = true;
                identity = account.claude_identity(directory.as_deref());
                match self.account_choice(identity.as_ref(), Some(label)) {
                    Ok(proxy) => matches.push((label, proxy)),
                    // A saved token proves the credential source, not the account's
                    // identity. Resolve missing metadata before selecting an email route.
                    Err(_) if identity.is_none() && self.routing.account_probe.is_some() => {
                        local_needs_profile = true;
                    }
                    Err(error) => return Err(error),
                }
            }
        }
        for (name, proxy) in self
            .routing
            .api_key
            .iter()
            .filter(|(s, _)| !crate::claude_settings::is_url(s))
        {
            let profile = crate::claude_settings::load(&self.config_dirs, name)?;
            let (credential, upstream) = match profile {
                Some(profile) => (Ok(profile.token), Some(profile.upstream)),
                None => (
                    environment_key_with_shell(name, !matched_account).await,
                    None,
                ),
            };
            match credential {
                Ok(value) if value == token => {
                    matches.push((name, proxy.clone()));
                    api_upstream = upstream;
                }
                Ok(_) => {}
                Err(_) => unavailable = true,
            }
        }
        if matches.len() > 1 || (local_needs_profile && !matches.is_empty()) {
            return Err(Error::new(
                409,
                "Claude credential matches multiple routes.",
            ));
        }
        let matched_api = !matched_account && !matches.is_empty();
        if bearer && !matched_account && !matched_api && self.account_auth_file_only {
            if unavailable {
                return Err(Error::config(
                    "No matching Claude route; credential source unavailable.",
                ));
            }
            return Err(Error::new(
                401,
                "Claude token does not match a saved account credential.",
            ));
        }
        let needs_profile = local_needs_profile || (bearer && !matched_account && !matched_api);
        let (label, proxy) = if let Some((label, proxy)) = matches.pop() {
            (label.clone(), proxy.clone())
        } else if needs_profile {
            let proxy = self.routing.account_probe.as_ref()
                .or(self.routing.account_fallback.as_ref())
                .ok_or(Error::config(
                    "Claude profile lookup requires routing.account_probe or routing.account_fallback.",
                ))?;
            ("claude-profile".into(), proxy.clone())
        } else if let Some(proxy) = if bearer {
            &self.routing.account_fallback
        } else {
            &self.routing.api_key_fallback
        } {
            (
                if bearer {
                    "claude-account-fallback"
                } else {
                    "claude-api-key-fallback"
                }
                .into(),
                proxy.clone(),
            )
        } else if unavailable {
            return Err(Error::config(
                "No matching Claude route; credential source unavailable.",
            ));
        } else {
            return Err(Error::new(
                401,
                "Claude credential does not match a configured route.",
            ));
        };
        Ok(ClaudeRoute {
            token: token.into(),
            bearer,
            matched_account,
            label,
            proxy,
            identity,
            needs_profile,
            custom_upstream: api_upstream.is_some(),
            upstream: api_upstream.unwrap_or_else(|| self.base_url.clone()),
            oauth: bearer && !matched_api,
        })
    }

    pub(crate) fn explicit_api_route(
        &self,
        headers: &HeaderMap,
        target: &str,
    ) -> Result<Option<ClaudeRoute>> {
        let Some(explicit) = crate::claude_api::explicit_target(target) else {
            return Ok(None);
        };
        let routes = self.url_routes();
        let Some((base, proxy)) = crate::url_routing::match_route(&routes, explicit)? else {
            return Ok(None);
        };
        let auth = headers.get("authorization");
        let key = headers.get("x-api-key");
        if auth.is_some() && key.is_some() {
            return Err(Error::new(
                400,
                "Supply only one Claude authentication header.",
            ));
        }
        let bearer = auth.is_some();
        let token = if bearer {
            auth.and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
        } else {
            key.and_then(|v| v.to_str().ok())
        }
        .filter(|s| valid_token(s))
        .ok_or(Error::new(
            401,
            "Claude API requires x-api-key or Bearer authentication.",
        ))?;
        Ok(Some(ClaudeRoute {
            token: token.into(),
            bearer,
            matched_account: false,
            label: base.into(),
            proxy: proxy.clone(),
            identity: None,
            needs_profile: false,
            upstream: crate::url_routing::validate_upstream(base)?.into(),
            oauth: false,
            custom_upstream: true,
        }))
    }

    fn account_choice(
        &self,
        identity: Option<&ClaudeIdentity>,
        label: Option<&str>,
    ) -> Result<Choice> {
        let by_identity = identity.and_then(|i| {
            self.routing
                .account
                .get(&i.account_id)
                .or_else(|| i.usernames.iter().find_map(|u| self.routing.account.get(u)))
        });
        by_identity
            .or_else(|| label.and_then(|s| self.routing.account.get(s)))
            .or(self.routing.account_fallback.as_ref())
            .cloned()
            .ok_or(Error::config("Claude account has no proxy route."))
    }
    pub(crate) fn apply_profile(
        &self,
        route: &mut ClaudeRoute,
        identity: ClaudeIdentity,
    ) -> Result<()> {
        route.proxy = self.account_choice(Some(&identity), None)?;
        route.label = "claude-account".into();
        route.identity = Some(identity);
        route.needs_profile = false;
        route.matched_account = true;
        Ok(())
    }
    pub fn url(&self, target: &str) -> Result<Url> {
        api_url(&self.base_url, target)
    }
}
fn api_url(base: &str, target: &str) -> Result<Url> {
    if !target.starts_with('/') || target.starts_with("//") {
        return Err(Error::config("Invalid Claude request target."));
    }
    // Reuse origin/path validation without OpenAI's /v1 stripping.
    if target.starts_with("/https://") || target.starts_with("/http://") {
        return crate::routing::upstream_url(base, target, false);
    }
    let explicit = format!("/{}{target}", base.trim_end_matches('/'));
    crate::routing::upstream_url(base, &explicit, false)
}

impl ClaudeRoute {
    pub fn url(&self, target: &str) -> Result<Url> {
        api_url(&self.upstream, target)
    }
    pub fn headers(&self, headers: &mut HeaderMap) -> Result<()> {
        let (name, value) = if self.bearer {
            ("authorization", format!("Bearer {}", self.token))
        } else {
            ("x-api-key", self.token.clone())
        };
        headers.insert(
            name,
            value
                .parse()
                .map_err(|_| Error::new(401, "Invalid Claude credential."))?,
        );
        if !headers.contains_key("anthropic-version") {
            headers.insert("anthropic-version", "2023-06-01".parse().unwrap());
        }
        if self.oauth {
            let existing = headers
                .get("anthropic-beta")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            if !existing.split(',').any(|v| v.trim() == "oauth-2025-04-20") {
                let beta = if existing.is_empty() {
                    "oauth-2025-04-20".into()
                } else {
                    format!("{existing},oauth-2025-04-20")
                };
                headers.insert(
                    "anthropic-beta",
                    beta.parse()
                        .map_err(|_| Error::new(400, "Invalid Claude beta header."))?,
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn refresh_routing_reloads_credentials_and_requires_explicit_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(".credentials.json");
        let mut claude = Claude {
            config_dirs: vec![dir.path().to_string_lossy().into_owned()],
            routing: Routing {
                account_fallback: Some(Choice::direct()),
                ..Default::default()
            },
            ..Default::default()
        };
        for token in ["old-refresh", "new-refresh"] {
            std::fs::write(
                &file,
                serde_json::json!({"claudeAiOauth": {
                    "accessToken": "access", "refreshToken": token
                }})
                .to_string(),
            )
            .unwrap();
            assert_eq!(
                claude.resolve_refresh(token).await.unwrap().0.label(),
                "none"
            );
        }
        assert_eq!(
            claude
                .resolve_refresh("old-refresh")
                .await
                .err()
                .unwrap()
                .status,
            403
        );
        claude.account_auth_file_only = false;
        assert!(claude.resolve_refresh("external-refresh").await.is_ok());
        claude.routing.account_fallback = None;
        claude.routing.account_probe = Some(Choice::direct());
        assert_eq!(
            claude
                .resolve_refresh("external-refresh")
                .await
                .err()
                .unwrap()
                .status,
            403
        );
        claude.routing.account_fallback = Some(Choice::direct());
        let other = tempfile::tempdir().unwrap();
        std::fs::copy(&file, other.path().join(".credentials.json")).unwrap();
        claude
            .config_dirs
            .push(other.path().to_string_lossy().into_owned());
        assert_eq!(
            claude
                .resolve_refresh("new-refresh")
                .await
                .err()
                .unwrap()
                .status,
            409
        );
    }

    #[test]
    fn refresh_endpoint_is_exact_and_separate_from_codex() {
        for path in [
            "/oauth/token",
            "/codex/https://platform.claude.com/v1/oauth/token",
            "/https://platform.claude.com.evil.invalid/v1/oauth/token",
            "/https://platform.claude.com:444/v1/oauth/token",
            "/anthropic/v1/oauth/token?redirect=evil",
            "/anthropic-evil/v1/oauth/token",
        ] {
            assert!(!token_refresh(path), "{path}");
        }
    }

    #[tokio::test]
    async fn configured_directories_resolve_relative_credentials_and_local_identity() {
        let dir = tempfile::tempdir().unwrap();
        for label in ["a", "b"] {
            let home = dir.path().join(label);
            std::fs::create_dir_all(home.join("nested")).unwrap();
            std::fs::write(
                home.join("nested/login.json"),
                serde_json::json!({
                    "claudeAiOauth":{"accessToken":format!("token-{label}")}
                })
                .to_string(),
            )
            .unwrap();
            std::fs::write(home.join(".claude.json"), serde_json::json!({
                "oauthAccount":{"accountUuid":label,"emailAddress":format!("{label}@example.com")}
            }).to_string()).unwrap();
        }
        let config = Config::parse(&format!(
            "listen_port: 8787\nrequest_timeout_seconds: 3\nclaude:\n  config_dirs: [{}, {}]\n  auth_file: nested/login.json\n  routing:\n    account: {{a: none, b: none}}\n",
            serde_json::to_string(&dir.path().join("a")).unwrap(),
            serde_json::to_string(&dir.path().join("b")).unwrap()
        )).unwrap();
        let mut c = config.claude;
        c.check_credentials().await.unwrap();
        for label in ["a", "b"] {
            let mut headers = HeaderMap::new();
            headers.insert(
                "authorization",
                format!("Bearer token-{label}").parse().unwrap(),
            );
            let route = c.resolve(&headers).await.unwrap();
            assert_eq!(route.label, "default");
            assert_eq!(route.identity.unwrap().account_id, label);
        }
        c.config_dirs.pop();
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer token-b".parse().unwrap());
        assert!(c.resolve(&headers).await.is_err());
    }

    fn config(extra: &str) -> Config {
        Config::parse(&format!(
            "listen_port: 7889\nrequest_timeout_seconds: 3\n{extra}"
        ))
        .unwrap()
    }

    #[tokio::test]
    async fn account_probe_is_separate_from_model_fallback() {
        let config = config(
            r#"
proxies:
  lookup: http://127.0.0.1:8101
  payload: http://127.0.0.1:8102
claude:
  account_auth_file_only: false
  routing:
    account_probe: [lookup, none]
    account:
      person@example.invalid: payload
    account_fallback: payload
"#,
        );
        let mut c = config.claude;
        // A readable local login that differs from the request token, so the
        // result does not depend on the machine's own ~/.claude.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".credentials.json"),
            r#"{"claudeAiOauth": {"accessToken": "local-secret"}}"#,
        )
        .unwrap();
        c.config_dirs = vec![dir.path().to_string_lossy().into_owned()];
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer remote-secret".parse().unwrap());
        let mut route = c.resolve(&headers).await.unwrap();
        assert!(route.needs_profile);
        assert_eq!(
            route.proxy.label(),
            c.routing.account_probe.as_ref().unwrap().label()
        );
        let known = || {
            ClaudeIdentity::profile(&serde_json::json!({
                "account": {"uuid": "id", "email": "person@example.invalid"}
            }))
            .unwrap()
        };
        let unknown = || {
            ClaudeIdentity::profile(&serde_json::json!({
                "account": {"uuid": "other", "email": "other@example.invalid"}
            }))
            .unwrap()
        };
        c.apply_profile(&mut route, unknown()).unwrap();
        assert_eq!(route.proxy.label(), "payload");
        c.routing.account_fallback = None;
        let mut route = c.resolve(&headers).await.unwrap();
        assert!(c.apply_profile(&mut route, unknown()).is_err());
        c.apply_profile(&mut route, known()).unwrap();
        assert_eq!(route.proxy.label(), "payload");
        assert!(!route.needs_profile);
        c.routing.account_probe = Some(Choice::One("none".into()));
        assert_eq!(c.resolve(&headers).await.unwrap().proxy.label(), "none");
        c.routing.account_probe = None;
        assert!(c.resolve(&headers).await.is_err());
        c.routing.account_fallback = Some(Choice::One("payload".into()));
        assert_eq!(c.resolve(&headers).await.unwrap().proxy.label(), "payload");
        c.account_auth_file_only = true;
        assert_eq!(c.resolve(&headers).await.err().unwrap().status, 401);
    }

    #[test]
    fn account_probe_validates_choices_and_service() {
        for yaml in [
            "claude:\n  routing:\n    account_probe: absent\n",
            "claude:\n  routing:\n    account_probe: []\n",
            "codex:\n  routing:\n    account_probe: none\n",
        ] {
            assert!(
                Config::parse(&format!(
                    "listen_port: 7889\nrequest_timeout_seconds: 3\n{yaml}"
                ))
                .is_err()
            );
        }
    }

    #[test]
    fn paths_keep_version_queries_and_origin_boundaries() {
        let c = Claude::default();
        for path in [
            "/v1/messages?beta=true",
            "/v1/messages/count_tokens",
            "/api/oauth/usage",
            "/v1/models",
        ] {
            for prefix in ["/anthropic", "/claude"] {
                let prefixed = format!("{prefix}{path}");
                assert_eq!(target(&prefixed), Some(path));
                assert_eq!(
                    c.url(target(&prefixed).unwrap()).unwrap().as_str(),
                    format!("https://api.anthropic.com{path}")
                );
            }
        }
        assert!(target("/v1/responses").is_none());
        assert!(target("/v1/models").is_none());
        assert!(target("/claude-evil/v1/messages").is_none());
        assert!(target("/anthropic-evil/v1/messages").is_none());
        assert_eq!(
            target("/v1/messages?beta=true"),
            Some("/v1/messages?beta=true")
        );
        for path in [
            "//evil.invalid/v1/messages",
            "/https://evil.invalid/v1/messages",
            "/https://api.anthropic.com.evil.invalid/v1/messages",
            "/%2e%2e/secret",
            "/x%5cy",
            "/v1/messages#fragment",
        ] {
            assert!(c.url(path).is_err(), "{path}");
        }
        assert_eq!(
            c.url("/https://api.anthropic.com/v1/messages?beta=true")
                .unwrap()
                .as_str(),
            "https://api.anthropic.com/v1/messages?beta=true"
        );
    }

    #[tokio::test]
    async fn accounts_reload_and_reject_duplicates_and_wrong_header_types() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("credentials.json");
        let write = |token: &str| {
            std::fs::write(&path, serde_json::json!({"claudeAiOauth":{"accessToken":token,"refreshToken":"never-forward"}}).to_string()).unwrap()
        };
        write("first-secret");
        let mut c = config("").claude;
        c.accounts.insert(
            "personal".into(),
            AccountSource {
                auth_file: Some(path.to_string_lossy().into()),
                auth_env: None,
            },
        );
        c.routing
            .account
            .insert("personal".into(), Choice::One("selected".into()));
        c.routing
            .account
            .insert("duplicate".into(), Choice::One("selected".into()));
        let mut h = HeaderMap::new();
        h.insert("authorization", "Bearer first-secret".parse().unwrap());
        let r = c.resolve(&h).await.unwrap();
        assert!(r.matched_account);
        assert_eq!(r.proxy.label(), "selected");
        assert_eq!(r.label, "personal");
        write("second-secret");
        assert_eq!(c.resolve(&h).await.err().unwrap().status, 401);
        h.insert("authorization", "Bearer second-secret".parse().unwrap());
        assert!(c.resolve(&h).await.is_ok());
        c.accounts
            .insert("duplicate".into(), c.accounts["personal"].clone());
        assert_eq!(c.resolve(&h).await.err().unwrap().status, 409);
        assert!(c.check_credentials().await.is_err());
        h.insert("x-api-key", "second-secret".parse().unwrap());
        assert_eq!(c.resolve(&h).await.err().unwrap().status, 400);
        h.remove("authorization");
        assert_eq!(c.resolve(&h).await.err().unwrap().status, 401);
    }

    #[tokio::test]
    async fn fallbacks_require_valid_auth_and_preserve_header_type_and_betas() {
        let c = config("claude:\n  account_auth_file_only: false\n  routing:\n    account_fallback: none\n    api_key_fallback: none\n").claude;
        for bearer in [false, true] {
            let mut h = HeaderMap::new();
            let name = if bearer { "authorization" } else { "x-api-key" };
            h.insert(
                name,
                if bearer {
                    "Bearer model-secret"
                } else {
                    "model-secret"
                }
                .parse()
                .unwrap(),
            );
            let r = c.resolve(&h).await.unwrap();
            assert!(!r.matched_account);
            let mut forwarded = crate::server::filtered_headers(&h);
            forwarded.insert("anthropic-beta", "custom-beta".parse().unwrap());
            r.headers(&mut forwarded).unwrap();
            assert_eq!(forwarded[name], h[name]);
            assert_eq!(forwarded["anthropic-version"], "2023-06-01");
            assert_eq!(
                forwarded["anthropic-beta"],
                if bearer {
                    "custom-beta,oauth-2025-04-20"
                } else {
                    "custom-beta"
                }
            );
            r.headers(&mut forwarded).unwrap();
            assert_eq!(
                forwarded["anthropic-beta"]
                    .to_str()
                    .unwrap()
                    .matches("oauth-2025-04-20")
                    .count(),
                usize::from(bearer)
            );
        }
        for value in ["", "Basic abc", "Bearer ", "Bearer two tokens"] {
            let mut h = HeaderMap::new();
            h.insert("authorization", value.parse().unwrap());
            assert_eq!(c.resolve(&h).await.err().unwrap().status, 401);
        }
        assert_eq!(
            c.resolve(&HeaderMap::new()).await.err().unwrap().status,
            401
        );
    }

    #[tokio::test]
    async fn named_settings_route_uses_file_credentials_and_upstream_and_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("api.json");
        for selector in ["api", "api.json"] {
            let c = config(&format!(
                "claude:\n  config_dirs: [{}]\n  routing:\n    api_key:\n      {selector}: none\n",
                serde_json::to_string(dir.path()).unwrap()
            ))
            .claude;
            for (field, key) in [
                ("ANTHROPIC_API_KEY", "first-key"),
                ("ANTHROPIC_AUTH_TOKEN", "rotated-key"),
            ] {
                std::fs::write(&path, format!(r#"{{"env":{{"ANTHROPIC_BASE_URL":"https://provider.invalid/api","{field}":"{key}"}}}}"#)).unwrap();
                c.check_credentials().await.unwrap();
                for header in ["x-api-key", "authorization"] {
                    let mut headers = HeaderMap::new();
                    headers.insert(
                        header,
                        (if header == "authorization" {
                            format!("Bearer {key}")
                        } else {
                            key.into()
                        })
                        .parse()
                        .unwrap(),
                    );
                    let route = c.resolve(&headers).await.unwrap();
                    assert_eq!(route.label, selector);
                    assert_eq!(route.proxy.label(), "none");
                    assert_eq!(route.upstream, "https://provider.invalid/api");
                    assert!(route.custom_upstream);
                    assert!(!route.oauth);
                    assert_eq!(
                        route.url("/v1/messages").unwrap().as_str(),
                        "https://provider.invalid/api/v1/messages"
                    );
                }
            }
            let mut headers = HeaderMap::new();
            headers.insert("x-api-key", "first-key".parse().unwrap());
            assert!(c.resolve(&headers).await.is_err());
            std::fs::write(&path, "broken").unwrap();
            assert!(c.check_credentials().await.is_err());
            assert!(c.resolve(&headers).await.is_err());
        }
    }

    #[test]
    fn configuration_validates_sources() {
        for extra in [
            "codex:\n  routing:\n      api_key:\n        BAD-NAME: none",
            "codex:\n  routing:\n      account_fallback: []",
            "codex:\n  routing:\n      api_key_fallback: absent",
            "codex:\n  base_url: http://api.anthropic.com",
            "auth_env: TOKEN\n  auth_file: file\n  routing:\n    account: {default: none}",
            "auth_file: ''\n  routing:\n    account: {default: none}",
        ] {
            assert!(
                Config::parse(&format!(
                    "listen_port: 7889\nrequest_timeout_seconds: 3\nclaude:\n  {extra}\n"
                ))
                .is_err(),
                "{extra}"
            );
        }
    }

    #[test]
    fn claude_has_one_base_and_rejects_split_bases() {
        let c = config("claude:\n  base_url: https://gateway.invalid/api\n  routing:\n    account_fallback: none\n    api_key_fallback: none\n").claude;
        assert_eq!(
            c.url("/v1/messages?beta=true").unwrap().as_str(),
            "https://gateway.invalid/api/v1/messages?beta=true"
        );
        assert!(Config::parse("listen_port: 8787\nrequest_timeout_seconds: 3\nclaude:\n  base_url:\n    account: https://account.invalid\n    api_key: https://keys.invalid\n").is_err());
    }

    #[test]
    fn explicit_api_upstreams_use_the_declared_route_and_keep_api_auth_separate_from_oauth() {
        let c = config("proxies:\n  selected: http://127.0.0.1:7893\nclaude:\n  routing:\n    api_key:\n      'https://182.92.106.196:6060': none\n      'https://gateway.invalid/api': selected\n      'https://gateway.invalid/api/specific': none\n").claude;
        for bearer in [false, true] {
            let mut headers = HeaderMap::new();
            headers.insert(
                if bearer { "authorization" } else { "x-api-key" },
                if bearer {
                    "Bearer api-secret"
                } else {
                    "api-secret"
                }
                .parse()
                .unwrap(),
            );
            for path in [
                "/https://182.92.106.196:6060/v1/models",
                "/anthropic/https://182.92.106.196:6060/v1/messages",
            ] {
                let route = c.explicit_api_route(&headers, path).unwrap().unwrap();
                assert_eq!(route.proxy.label(), "none");
                assert!(!route.oauth && !route.needs_profile && !route.matched_account);
                let mut forwarded = crate::server::filtered_headers(&headers);
                route.headers(&mut forwarded).unwrap();
                assert!(!forwarded.contains_key("anthropic-beta"));
                assert_eq!(
                    forwarded[if bearer { "authorization" } else { "x-api-key" }],
                    headers[if bearer { "authorization" } else { "x-api-key" }]
                );
            }
            assert_eq!(
                c.explicit_api_route(
                    &headers,
                    "/https://gateway.invalid/api/specific/v1/messages"
                )
                .unwrap()
                .unwrap()
                .proxy
                .label(),
                "none"
            );
            assert_eq!(
                c.explicit_api_route(&headers, "/https://gateway.invalid/api/v1/messages")
                    .unwrap()
                    .unwrap()
                    .proxy
                    .label(),
                "selected"
            );
            for path in [
                "/https://182.92.106.196:6061/v1/messages",
                "/https://evil.invalid/v1/messages",
                "/https://gateway.invalid/api-evil/v1/messages",
            ] {
                assert!(c.explicit_api_route(&headers, path).unwrap().is_none());
            }
            assert!(
                c.explicit_api_route(&headers, "/https://182.92.106.196:6060/%2e%2e/secret")
                    .is_err()
            );
        }
        assert_eq!(
            c.explicit_api_route(
                &HeaderMap::new(),
                "/https://182.92.106.196:6060/v1/messages"
            )
            .err()
            .unwrap()
            .status,
            401
        );
    }

    #[tokio::test]
    async fn file_only_identity_uses_local_metadata_and_reloads_without_leaking_to_other_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let credentials = dir.path().join(".credentials.json");
        let metadata = dir.path().join(".claude.json");
        std::fs::write(
            &credentials,
            r#"{"claudeAiOauth":{"accessToken":"saved-secret"}}"#,
        )
        .unwrap();
        std::fs::write(&metadata, r#"{"oauthAccount":{"accountUuid":"local-id","emailAddress":"local@example.invalid","displayName":"Local"}}"#).unwrap();
        let mut c = config(&format!("claude:\n  config_dirs: [{}]\n  auth_file: .credentials.json\n  routing:\n    account:\n      local-id: none\n      local@example.invalid: none\n    account_fallback: none\n", serde_json::to_string(&dir.path()).unwrap())).claude;
        c.routing
            .account
            .insert("local-id".into(), Choice::One("uuid-route".into()));
        c.routing.account.insert(
            "local@example.invalid".into(),
            Choice::One("email-route".into()),
        );
        c.routing.account_fallback = Some(Choice::One("bootstrap".into()));
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer saved-secret".parse().unwrap());
        let route = c.resolve(&headers).await.unwrap();
        assert_eq!(route.proxy.label(), "uuid-route");
        assert_eq!(route.identity.unwrap().account_id, "local-id");
        assert!(!route.needs_profile);
        c.routing.account.remove("local-id");
        assert_eq!(
            c.resolve(&headers).await.unwrap().proxy.label(),
            "email-route"
        );
        std::fs::write(
            &metadata,
            r#"{"oauthAccount":{"accountUuid":"new-id","emailAddress":"new@example.invalid"}}"#,
        )
        .unwrap();
        assert_eq!(
            c.resolve(&headers)
                .await
                .unwrap()
                .identity
                .unwrap()
                .account_id,
            "new-id"
        );
        headers.insert("authorization", "Bearer other-secret".parse().unwrap());
        assert_eq!(c.resolve(&headers).await.err().unwrap().status, 401);
        c.account_auth_file_only = false;
        let route = c.resolve(&headers).await.unwrap();
        assert!(route.identity.is_none());
        assert!(route.needs_profile);
        assert_eq!(route.proxy.label(), "bootstrap");
        c.routing.account_fallback = None;
        assert_eq!(c.resolve(&headers).await.err().unwrap().status, 502);
        std::fs::remove_file(&credentials).unwrap();
        c.check_credentials().await.unwrap();
        c.account_auth_file_only = true;
        assert!(c.check_credentials().await.is_err());
    }
}
