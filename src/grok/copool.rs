//! Copool subscription endpoints and identity learned from successful OAuth renewals.
use super::{CLIENT_ID, DEFAULT_BASE, Grok, ISSUER};
use crate::{
    Error, Result,
    config::Choice,
    identity::{claims, valid_token},
    refresh_routes::{Account, RefreshRoutes},
};
use serde_json::Value;
use std::path::PathBuf;
use url::Url;
#[derive(Clone)]
pub struct GrokAccount {
    id: String,
    aliases: Vec<String>,
}

impl GrokAccount {
    pub(super) fn model_identity(&self) -> super::GrokIdentity {
        let (user, team) = self
            .id
            .split_once('/')
            .map_or((self.id.as_str(), None), |(u, t)| (u, Some(t)));
        let email = self.aliases.iter().find(|alias| alias.contains('@'));
        super::GrokIdentity::new(Some(user), email.map(String::as_str), team)
    }

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

impl GrokAccount {
    /// The account records name, as for a saved login's requests: the user
    /// ID, without the team a route may also name.
    pub fn user_id(&self) -> &str {
        self.id.split('/').next().unwrap_or(&self.id)
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
    async fn saved_in_file(
        path: &std::path::Path,
        token: &str,
        refresh: bool,
    ) -> Result<Option<GrokAccount>> {
        let raw = match tokio::fs::read(path).await {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(Error::config("Cannot read Grok auth_file.")),
        };
        let file: Value = serde_json::from_slice(raw.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&raw))
            .map_err(|_| Error::config("Invalid Grok auth_file."))?;
        let login = if file["key"].is_string()
            && file["oidc_issuer"]
                .as_str()
                .is_none_or(|issuer| issuer == ISSUER)
            && file["oidc_client_id"]
                .as_str()
                .is_none_or(|id| id == CLIENT_ID)
        {
            &file
        } else {
            &file[format!("{ISSUER}::{CLIENT_ID}")]
        };
        if login[if refresh { "refresh_token" } else { "key" }].as_str() != Some(token) {
            return Ok(None);
        }
        Ok(GrokAccount::from_fields(
            login["user_id"].as_str().unwrap_or(""),
            login["team_id"].as_str(),
            login["email"].as_str(),
        ))
    }

    /// The saved login holding `token`, as model requests find it, in every
    /// file: one that cannot be read does not hide a later file's login. The
    /// flag tells whether a file could not be read.
    async fn saved(files: Vec<PathBuf>, token: &str, refresh: bool) -> (Option<GrokAccount>, bool) {
        let mut unreadable = false;
        for file in files {
            match Self::saved_in_file(&file, token, refresh).await {
                Ok(Some(identity)) => return (Some(identity), false),
                Ok(None) => {}
                Err(_) => unreadable = true,
            }
        }
        (None, unreadable)
    }

    pub(crate) async fn route<'a>(
        &'a self,
        token: &str,
        refresh: bool,
        learned: &RefreshRoutes<GrokAccount>,
    ) -> Result<(&'a Choice, Option<GrokAccount>, Option<&'a str>)> {
        self.route_in(self.login_files(), token, refresh, learned)
            .await
    }

    async fn route_in<'a>(
        &'a self,
        files: Vec<PathBuf>,
        token: &str,
        refresh: bool,
        learned: &RefreshRoutes<GrokAccount>,
    ) -> Result<(&'a Choice, Option<GrokAccount>, Option<&'a str>)> {
        let known = if self.file_only() {
            None
        } else if refresh {
            learned.identity(token)
        } else {
            learned.access_identity(token)
        };
        let saved = known.is_none();
        let (identity, unreadable) = match known {
            Some(account) => (Some(account), false),
            None => Self::saved(files, token, refresh).await,
        };
        let unavailable = || Error::config("Cannot read Grok auth_file.");
        if identity.is_none() && self.file_only() {
            return Err(if unreadable {
                unavailable()
            } else {
                Error::new(
                    403,
                    "Grok requires the current saved login when account_auth_file_only is true.",
                )
            });
        }
        let mapped = identity.as_ref().and_then(|account| {
            std::iter::once(&account.id)
                .chain(&account.aliases)
                .find_map(|key| self.account_selector(key))
                .or_else(|| {
                    self.routing
                        .account
                        .get_key_value("default")
                        .filter(|_| saved)
                })
        });
        if let Some((key, choice)) = mapped {
            return Ok((choice, identity, Some(key)));
        }
        let choice = self.routing.account_default().ok_or_else(|| {
            if unreadable {
                unavailable()
            } else {
                Error::new(403, "No matching Grok account route.")
            }
        })?;
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
        // A scoped base URL may omit `/v1`, as model requests may: those
        // reach the same endpoints and keep their limits.
        let suffix = [Some(base), Some(DEFAULT_BASE), scoped.map(|_| "v1")]
            .into_iter()
            .flatten()
            .filter_map(|prefix| rest.strip_prefix(prefix).map(str::to_owned))
            .chain(scoped.map(|path| format!("/{path}")))
            .find(|path| {
                matches!(
                    path.as_str(),
                    "/billing" | "/billing?format=credits" | "/settings"
                )
            })?;
        Some((Url::parse(&format!("{base}{suffix}")).ok()?, false))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
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
            "/grok/billing?format=credits",
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
            "/grok/billing?format=other",
            "/grok/v1/settings/other",
        ] {
            assert!(config.grok.endpoint(target).is_none(), "{target}");
        }
        assert!(config.grok.endpoint("/grok/settings").is_some());
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
        // Settings shows the default as on, and every endpoint keeps it.
        config.grok.account_auth_file_only = None;
        for (token, refresh) in [("known", true), ("unknown", false)] {
            assert!(matches!(
                config.grok.route(token, refresh, &routes).await,
                Err(Error { status: 403, .. })
            ));
        }
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

    /// Without homes, auth_file or a file policy, Grok Build's own login in
    /// its default home still names its route for billing, settings and
    /// renewal, as for its model requests; an unreadable home does not hide
    /// a later one's login, and only stops a request no route can take.
    #[tokio::test]
    async fn subscription_requests_find_the_saved_login_in_every_home() {
        let dir = tempfile::tempdir().unwrap();
        let (broken, saved) = (dir.path().join("broken.json"), dir.path().join("auth.json"));
        std::fs::write(&broken, "{").unwrap();
        let login = json!({"user_id":"user","email":"user@example.test","key":"access","refresh_token":"refresh"});
        std::fs::write(
            &saved,
            json!({format!("{ISSUER}::{CLIENT_ID}"): login}).to_string(),
        )
        .unwrap();
        let parse = |grok: &str| {
            Config::parse(&format!(
                "listen_port: 8787\nrequest_timeout_seconds: 3\ngrok:\n{grok}"
            ))
            .unwrap()
            .grok
        };
        let routes = RefreshRoutes::default();
        let defaults = parse("  routing:\n    account: {'user@example.test': none}\n");
        for (token, refresh) in [("access", false), ("refresh", true)] {
            let (_, identity, label) = defaults
                .route_in(vec![broken.clone(), saved.clone()], token, refresh, &routes)
                .await
                .unwrap();
            assert_eq!(label, Some("user@example.test"));
            assert_eq!(identity.unwrap().account_id(), "user");
        }
        // An unknown token: no route is 403, an unreadable home 502, and a
        // fallback still carries it.
        for (files, status) in [(vec![saved.clone()], 403), (vec![broken.clone()], 502)] {
            assert!(matches!(
                defaults.route_in(files, "unknown", false, &routes).await,
                Err(Error { status: s, .. }) if s == status
            ));
        }
        let fallback =
            parse("  account_auth_file_only: false\n  routing:\n    account_fallback: none\n");
        let (_, identity, label) = fallback
            .route_in(vec![broken.clone()], "unknown", false, &routes)
            .await
            .unwrap();
        assert!(identity.is_none() && label.is_none());
        let strict =
            parse("  account_auth_file_only: true\n  routing:\n    account_fallback: none\n");
        assert!(matches!(
            strict
                .route_in(vec![broken], "unknown", false, &routes)
                .await,
            Err(Error { status: 502, .. })
        ));
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
