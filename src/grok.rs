//! Copool's Grok Build subscription requests. Only the published token,
//! billing and settings endpoints participate in credential routing.
use crate::{
    Error, Result,
    config::{Choice, Config, Routing, expand},
    identity::{claims, valid_token},
    refresh_routes::{Account, RefreshRoutes},
};
use serde::Deserialize;
use serde_json::Value;
use std::path::PathBuf;
use url::Url;

pub const API: &str = "https://cli-chat-proxy.grok.com";
const ACCOUNT_BASE: &str = "https://cli-chat-proxy.grok.com/v1";
const API_KEY_BASE: &str = "https://api.x.ai/v1";
pub const ISSUER: &str = "https://auth.x.ai";
pub(crate) const CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Bases {
    pub account: String,
    pub api_key: String,
}

impl Default for Bases {
    fn default() -> Self {
        Self {
            account: ACCOUNT_BASE.into(),
            api_key: API_KEY_BASE.into(),
        }
    }
}

#[derive(Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Grok {
    pub base_url: Bases,
    /// An explicit file policy uses Grok Build's standard login location
    /// when auth_file is omitted. Older configurations opt in via auth_file.
    pub account_auth_file_only: Option<bool>,
    /// Optional Grok Build auth.json for routing an account's first refresh.
    pub auth_file: Option<String>,
    pub routing: Routing,
}

#[derive(Clone)]
pub struct GrokAccount {
    id: String,
    aliases: Vec<String>,
}

impl GrokAccount {
    fn from_fields(user: &str, team: Option<&str>, email: Option<&str>) -> Option<Self> {
        if !valid_token(user) || user.contains('/') {
            return None;
        }
        let mut aliases = Vec::new();
        let id = match team.filter(|s| valid_token(s) && !s.contains('/')) {
            Some(team) => {
                aliases.push(user.to_owned());
                format!("{user}/{team}")
            }
            None => user.to_owned(),
        };
        aliases.extend(
            email
                .filter(|s| valid_token(s) && s.contains('@'))
                .map(str::to_owned),
        );
        Some(Self { id, aliases })
    }
}

impl Account for GrokAccount {
    const TRACK_ACCESS_TOKEN: bool = true;

    fn named_by(response: &Value, previous: Option<&Self>) -> Option<Self> {
        // These claims come from a successful TLS-authenticated xAI response.
        let id = claims(response["id_token"].as_str().unwrap_or(""));
        let access = claims(response["access_token"].as_str().unwrap_or(""));
        let field = |name: &str| id[name].as_str().or_else(|| access[name].as_str());
        let user = field("sub")?;
        // A refresh need not repeat the email or team of the same subject.
        let previous = previous.filter(|p| p.id.split('/').next() == Some(user));
        let team = field("team_id").or_else(|| Some(previous?.id.split_once('/')?.1));
        let email = field("email").or_else(|| {
            previous?
                .aliases
                .iter()
                .find(|s| s.contains('@'))
                .map(String::as_str)
        });
        Self::from_fields(user, team, email)
    }
    fn learned(id: String, aliases: Vec<String>) -> Self {
        Self { id, aliases }
    }
    fn account_id(&self) -> &str {
        &self.id
    }
    fn usernames(&self) -> &[String] {
        &self.aliases
    }
}

impl Grok {
    pub fn enabled(&self) -> bool {
        !self.routing.account.is_empty() || self.routing.account_default().is_some()
    }

    pub(crate) fn validate(&self, config: &Config) -> Result<()> {
        let account = crate::config::validate_upstream(&self.base_url.account)?;
        let key_base = crate::config::validate_upstream(&self.base_url.api_key)?;
        if account.origin().ascii_serialization() == ISSUER
            || crate::url_routing::overlaps(&account, &key_base)
            || crate::url_routing::overlaps(&account, &Url::parse(API_KEY_BASE).unwrap())
        {
            return Err(Error::config(
                "Grok subscription upstream overlaps the OAuth issuer or API key upstream.",
            ));
        }
        if !self.routing.api_key.is_empty()
            || self.routing.account_probe.is_some()
            || self.routing.mcp_fallback.is_some()
        {
            return Err(Error::config(
                "Grok supports subscription account routes only.",
            ));
        }
        if self.auth_file.as_ref().is_some_and(|p| p.trim().is_empty())
            || self.routing.account.keys().any(|key| !valid_token(key))
        {
            return Err(Error::config("Invalid Grok auth_file or account selector."));
        }
        for choice in self
            .routing
            .account
            .values()
            .chain(self.routing.account_fallback.iter())
            .chain(self.routing.fallback.iter())
        {
            config.validate_choice(choice)?;
        }
        Ok(())
    }

    async fn saved(&self, token: &str, refresh: bool) -> Result<Option<GrokAccount>> {
        let path = if let Some(path) = &self.auth_file {
            expand(path)
        } else if self.account_auth_file_only.is_some() {
            std::env::var_os("GROK_HOME")
                .filter(|dir| !dir.is_empty())
                .map(PathBuf::from)
                .or_else(|| dirs::home_dir().map(|home| home.join(".grok")))
                .ok_or(Error::config(
                    "Cannot locate Grok Build's login; configure grok.auth_file.",
                ))?
                .join("auth.json")
        } else {
            return Ok(None);
        };
        let raw = match tokio::fs::read(path).await {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(Error::config("Cannot read Grok auth_file.")),
        };
        let file: Value = serde_json::from_slice(raw.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&raw))
            .map_err(|_| Error::config("Invalid Grok auth_file."))?;
        let login = &file[format!("{ISSUER}::{CLIENT_ID}")];
        if login[if refresh { "refresh_token" } else { "key" }].as_str() != Some(token) {
            return Ok(None);
        }
        Ok(GrokAccount::from_fields(
            login["user_id"].as_str().unwrap_or(""),
            login["team_id"].as_str(),
            login["email"].as_str(),
        ))
    }

    pub(crate) async fn route<'a>(
        &'a self,
        token: &str,
        refresh: bool,
        learned: &RefreshRoutes<GrokAccount>,
    ) -> Result<(&'a Choice, Option<GrokAccount>, Option<&'a str>)> {
        let identity = if self.account_auth_file_only == Some(true) {
            Some(self.saved(token, refresh).await?.ok_or(Error::new(
                403,
                "Grok requires the current saved login when account_auth_file_only is true.",
            ))?)
        } else {
            let learned = if refresh {
                learned.identity(token)
            } else {
                learned.access_identity(token)
            };
            match learned {
                Some(account) => Some(account),
                None => self.saved(token, refresh).await?,
            }
        };
        let mapped = identity.as_ref().and_then(|account| {
            std::iter::once(&account.id)
                .chain(&account.aliases)
                .find_map(|key| self.routing.account.get_key_value(key))
        });
        if let Some((key, choice)) = mapped {
            return Ok((choice, identity, Some(key)));
        }
        let choice = self
            .routing
            .account_default()
            .ok_or(Error::new(403, "No matching Grok account route."))?;
        Ok((choice, identity, None))
    }

    /// Copool always asks for the official subscription URLs. Translate those
    /// and scoped base-URL requests to the configured base, preserving only
    /// the supported paths. OAuth renewal always stays at the xAI issuer.
    pub(crate) fn endpoint(&self, target: &str) -> Option<(Url, bool)> {
        let scoped = target.strip_prefix("/grok/");
        let rest = scoped.or_else(|| target.strip_prefix('/'))?;
        const TOKEN: &str = "https://auth.x.ai/oauth2/token";
        if rest == TOKEN || scoped == Some("oauth2/token") {
            return Some((Url::parse(TOKEN).unwrap(), true));
        }
        let url = Url::parse(&self.base_url.account).ok()?;
        let base = url.as_str().trim_end_matches('/');
        let suffix = [Some(base), Some(ACCOUNT_BASE), scoped.map(|_| "v1")]
            .into_iter()
            .flatten()
            .filter_map(|prefix| rest.strip_prefix(prefix))
            .find(|path| matches!(*path, "/billing" | "/billing?format=credits" | "/settings"))?;
        Some((Url::parse(&format!("{base}{suffix}")).ok()?, false))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_partial_renewal_preserves_selectors_only_for_the_same_subject() {
        let previous =
            GrokAccount::from_fields("alice", Some("team"), Some("a@example.test")).unwrap();
        for (sub, id, email) in [("alice", "alice/team", true), ("bob", "bob", false)] {
            use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
            let claims = URL_SAFE_NO_PAD.encode(json!({"sub":sub}).to_string());
            let current = GrokAccount::named_by(
                &json!({"access_token":format!("h.{claims}.s")}),
                Some(&previous),
            )
            .unwrap();
            assert_eq!(current.id, id);
            assert_eq!(current.aliases.iter().any(|s| s == "a@example.test"), email);
        }
    }

    #[test]
    fn custom_subscription_base_keeps_the_issuer_and_key_api_separate() {
        let config = Config::parse("listen_port: 8787\nrequest_timeout_seconds: 3\ngrok:\n  base_url: {account: 'https://gateway.example.com:8443/custom/'}\n  routing: {account_fallback: none}").unwrap();
        for target in [
            "/grok/v1/billing?format=credits",
            "/grok/https://cli-chat-proxy.grok.com/v1/billing?format=credits",
            "/https://gateway.example.com:8443/custom/billing?format=credits",
        ] {
            let (url, refresh) = config.grok.endpoint(target).unwrap();
            assert!(!refresh);
            assert_eq!(
                url.as_str(),
                "https://gateway.example.com:8443/custom/billing?format=credits"
            );
        }
        let (url, refresh) = config
            .grok
            .endpoint("/grok/https://auth.x.ai/oauth2/token")
            .unwrap();
        assert!(refresh);
        assert_eq!(url.as_str(), "https://auth.x.ai/oauth2/token");
        for target in [
            "/grok/https://api.x.ai/v1/settings",
            "/grok/https://gateway.example.com:8443/custom/oauth2/token",
            "/grok/https://gateway.example.com:8443/customized/settings",
        ] {
            assert!(config.grok.endpoint(target).is_none());
        }
    }

    #[tokio::test]
    async fn file_only_rejects_learned_tokens_even_with_a_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("auth.json");
        let mut config = Config::parse(&format!("listen_port: 8787\nrequest_timeout_seconds: 3\ngrok:\n  auth_file: {}\n  account_auth_file_only: true\n  routing: {{account_fallback: none}}",serde_json::to_string(&file).unwrap())).unwrap();
        let routes = RefreshRoutes::default();
        let identity = GrokAccount::from_fields("alice", None, None).unwrap();
        routes.learn("known", &identity).unwrap();
        assert!(matches!(
            config.grok.route("known", true, &routes).await,
            Err(Error { status: 403, .. })
        ));
        assert!(matches!(
            config.grok.route("unknown", false, &routes).await,
            Err(Error { status: 403, .. })
        ));
        config.grok.account_auth_file_only = Some(false);
        assert!(
            config
                .grok
                .route("known", true, &routes)
                .await
                .unwrap()
                .1
                .is_some()
        );
        assert!(
            config
                .grok
                .route("unknown", false, &routes)
                .await
                .unwrap()
                .1
                .is_none()
        );
    }

    #[tokio::test]
    async fn first_request_matches_only_the_configured_grok_build_login() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("auth.json");
        let login = json!({"user_id":"user","team_id":"team","email":"user@example.test","key":"access","refresh_token":"refresh"});
        let doc = json!({format!("{ISSUER}::{CLIENT_ID}"): login, "https://other.example::app": {"key":"other","user_id":"user"}});
        std::fs::write(&file, format!("\u{feff}{doc}")).unwrap();
        let config = Config::parse(&format!("listen_port: 8787\nrequest_timeout_seconds: 3\ngrok:\n  auth_file: {}\n  routing:\n    account: {{user: none, user/team: none, 'user@example.test': none}}\n",serde_json::to_string(&file).unwrap())).unwrap();
        let routes = RefreshRoutes::default();
        for (token, refresh) in [("access", false), ("refresh", true)] {
            let (_, _, label) = config.grok.route(token, refresh, &routes).await.unwrap();
            assert_eq!(label, Some("user/team"));
        }
        for (token, refresh) in [
            ("other", false),
            ("access", true),
            ("refresh", false),
            ("unknown", true),
        ] {
            assert!(matches!(
                config.grok.route(token, refresh, &routes).await,
                Err(Error { status: 403, .. })
            ));
        }
    }
}
