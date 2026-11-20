//! Antigravity forwarding: the Cloud Code requests of the Antigravity IDE and
//! CLI (`agy`), routed by the Google login they carry. As Copool reads it,
//! Antigravity keeps that login in one item of the OS credential store,
//! service `gemini` and account `antigravity`, saved by go-keyring: an OAuth
//! token as JSON (`{"token": {"access_token", "refresh_token", "expiry"}}`),
//! on macOS wrapped as `go-keyring-base64:`.
use crate::{
    Error, Result,
    config::{Choice, Config, Routing, validate_upstream},
    identity::valid_token,
    refresh_routes::RefreshRoutes,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use hyper::HeaderMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

/// The Cloud Code endpoint Antigravity answers from first.
pub const DEFAULT_BASE: &str = "https://daily-cloudcode-pa.googleapis.com";
/// Every Cloud Code origin Antigravity sends to. An explicit target may name
/// one of these or `base_url`, nothing else.
pub const CLOUD_CODE_ORIGINS: [&str; 2] = [
    "https://daily-cloudcode-pa.googleapis.com",
    "https://cloudcode-pa.googleapis.com",
];
/// Names the Google account of an access token.
pub const USERINFO_URL: &str = "https://www.googleapis.com/oauth2/v2/userinfo";
/// Where Antigravity renews its login, as Codex's and Claude's token endpoints.
pub const TOKEN_REFRESH_UPSTREAM: &str = "https://oauth2.googleapis.com/token";
/// Antigravity's OAuth client; the token endpoint serves every Google client,
/// and only this one's refreshes are routed.
pub const CLIENT_ID: &str = "antigravity-client.apps.googleusercontent.com";
/// The credential store item of Antigravity's login.
const SERVICE: &str = "gemini";
const ACCOUNT: &str = "antigravity";
const WRAPPED: &str = "go-keyring-base64:";

fn default_base() -> String {
    DEFAULT_BASE.into()
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Antigravity {
    /// Cloud Code root that origin-form requests go to.
    pub base_url: String,
    /// Read Antigravity's login from the OS credential store to match tokens.
    pub saved_login: bool,
    /// Reject tokens other than the saved login's instead of naming their
    /// account with a Google lookup.
    pub account_auth_file_only: bool,
    pub routing: Routing,
}
impl Default for Antigravity {
    fn default() -> Self {
        Self {
            base_url: default_base(),
            saved_login: true,
            account_auth_file_only: true,
            routing: Routing::default(),
        }
    }
}

/// The Google account a token signs in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AntigravityIdentity {
    pub email: String,
}
impl AntigravityIdentity {
    fn new(email: &str) -> Option<Self> {
        let email = email.trim();
        (email.contains('@') && email.len() <= 320 && !email.chars().any(char::is_control)).then(
            || Self {
                email: email.to_owned(),
            },
        )
    }
    /// The account a token response's ID token names.
    pub(crate) fn token_response(value: &Value) -> Option<Self> {
        let id_token = value["id_token"].as_str()?;
        crate::identity::claims(id_token)["email"]
            .as_str()
            .and_then(Self::new)
    }
    /// Google's user information response.
    pub(crate) fn userinfo(value: &Value) -> Result<Self> {
        value["email"]
            .as_str()
            .and_then(Self::new)
            .ok_or(Error::config(
                "Google user information is missing an email.",
            ))
    }
}

/// The parts of a saved login Coport uses.
#[derive(Debug, PartialEq)]
pub(crate) struct Login {
    pub access_token: String,
    /// Stable while Antigravity renews the access token; learned identities
    /// are kept by its digest.
    pub refresh_token: Option<String>,
    pub identity: Option<AntigravityIdentity>,
}
/// A credential store item's login; none when it holds no access token.
pub(crate) fn parse_login(raw: &str) -> Option<Login> {
    let text = raw.trim().trim_start_matches('\u{feff}').trim();
    let text = match text.strip_prefix(WRAPPED) {
        Some(encoded) => String::from_utf8(STANDARD.decode(encoded.trim()).ok()?).ok()?,
        None => text.to_owned(),
    };
    let document: Value = serde_json::from_str(text.trim_start_matches('\u{feff}')).ok()?;
    let object = document.as_object()?;
    let token = object
        .get("token")
        .and_then(Value::as_object)
        .unwrap_or(object);
    let access_token = ["access_token", "accessToken"]
        .iter()
        .find_map(|key| token.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .filter(|token| valid_token(token))?
        .to_owned();
    let refresh_token = ["refresh_token", "refreshToken"]
        .iter()
        .find_map(|key| token.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .filter(|token| valid_token(token))
        .map(str::to_owned);
    // Copool records the email of its own sign-ins; Antigravity's do not.
    let identity = [
        "/id_token_email",
        "/email",
        "/account/email",
        "/user/email",
        "/token/email",
    ]
    .iter()
    .find_map(|path| document.pointer(path).and_then(Value::as_str))
    .and_then(AntigravityIdentity::new);
    Some(Login {
        access_token,
        refresh_token,
        identity,
    })
}

pub struct AntigravityRoute {
    pub token: String,
    pub proxy: Choice,
    pub identity: Option<AntigravityIdentity>,
    /// The token is the saved login's.
    pub matched_login: bool,
    /// The saved login's refresh token, under which a looked-up identity is
    /// learned.
    pub refresh_token: Option<String>,
    /// A Google lookup names the account before the request is sent.
    pub needs_profile: bool,
    pub label: String,
    /// The credential is the saved login or an account the routing maps, not
    /// one a fallback carries.
    pub configured: bool,
}

/// How a refresh routes: as `AntigravityRoute`, by its refresh token.
pub struct RefreshRoute {
    pub proxy: Choice,
    /// The refresh token is the saved login's.
    pub saved: bool,
    pub identity: Option<AntigravityIdentity>,
    pub configured: bool,
    /// The saved login's access token, for an explicit `account_probe` to
    /// name it before the refresh is sent.
    pub probe_token: Option<String>,
}

/// Whether a target in the `/antigravity` scope is on Google's token host.
pub fn token_host(target: &str) -> bool {
    target
        .strip_prefix('/')
        .and_then(|url| Url::parse(url).ok())
        .is_some_and(|url| url.host_str() == Url::parse(TOKEN_REFRESH_UPSTREAM).unwrap().host_str())
}
/// Whether a target in the `/antigravity` scope is the token endpoint.
pub fn token_refresh(target: &str) -> bool {
    target.strip_prefix('/') == Some(TOKEN_REFRESH_UPSTREAM)
}
/// A token request's `client_id`, from a form or JSON body.
pub(crate) fn client_id(body: &[u8]) -> Option<String> {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| v["client_id"].as_str().map(String::from))
        .or_else(|| {
            url::form_urlencoded::parse(body)
                .find(|(k, _)| k == "client_id")
                .map(|(_, v)| v.into_owned())
        })
}

/// The Antigravity scope of a request target, without its prefix.
pub fn target(target: &str) -> Option<&str> {
    target
        .strip_prefix("/antigravity")
        .filter(|rest| rest.starts_with('/'))
}

impl Antigravity {
    /// Whether any request is routed: an account route or a fallback.
    pub fn enabled(&self) -> bool {
        !self.routing.account.is_empty() || self.routing.account_default().is_some()
    }

    pub(crate) fn validate(&self, config: &Config) -> Result<()> {
        validate_upstream(&self.base_url)?;
        if !self.routing.api_key.is_empty() || self.routing.mcp_fallback.is_some() {
            return Err(Error::config(
                "Antigravity routes accounts only; api_key and mcp_fallback are not supported.",
            ));
        }
        if self.account_auth_file_only && !self.saved_login && self.enabled() {
            return Err(Error::config(
                "Antigravity file-only account routing requires saved_login.",
            ));
        }
        for (label, choice) in &self.routing.account {
            if label.trim().is_empty() {
                return Err(Error::config("Empty Antigravity routing identifier."));
            }
            config.validate_choice(choice)?;
        }
        for choice in [
            &self.routing.account_fallback,
            &self.routing.account_probe,
            &self.routing.fallback,
        ]
        .into_iter()
        .flatten()
        {
            config.validate_choice(choice)?;
        }
        Ok(())
    }

    /// Origins proxy-port clients send Antigravity's requests to.
    pub(crate) fn origins(&self) -> Vec<&str> {
        let mut origins = vec![self.base_url.as_str()];
        origins.extend(CLOUD_CODE_ORIGINS);
        origins
    }

    /// The saved login, read for each request: Antigravity renews its token
    /// itself and saves the new one in the same item.
    pub(crate) async fn saved(&self) -> Result<Option<Login>> {
        if !self.saved_login {
            return Ok(None);
        }
        Ok(crate::keychain::read_go_keyring(SERVICE, ACCOUNT)
            .await?
            .as_deref()
            .and_then(parse_login))
    }

    fn mapped_choice(&self, identity: &AntigravityIdentity) -> Option<&Choice> {
        // Google addresses are case-insensitive.
        self.mapped_key(identity)
            .and_then(|key| self.routing.account.get(&key))
    }
    /// A saved login takes its email's route, then `default`; any account then
    /// takes `account_fallback`, then `fallback`.
    fn account_choice(
        &self,
        identity: Option<&AntigravityIdentity>,
        saved: bool,
    ) -> Option<&Choice> {
        identity
            .and_then(|i| self.mapped_choice(i))
            .or_else(|| saved.then(|| self.routing.account.get("default")).flatten())
            .or(self.routing.account_default())
    }

    /// The saved login's account: the email its document records, else the
    /// one learned for its refresh token.
    pub(crate) fn login_identity(
        login: &Login,
        learned: &RefreshRoutes<AntigravityIdentity>,
    ) -> Option<AntigravityIdentity> {
        login.identity.clone().or_else(|| {
            login
                .refresh_token
                .as_deref()
                .and_then(|refresh| learned.identity(refresh))
        })
    }

    /// The saved login's route label: its email's route, else `default`.
    pub(crate) fn login_label(&self, identity: Option<&AntigravityIdentity>) -> Option<String> {
        identity.and_then(|i| self.mapped_key(i)).or_else(|| {
            self.routing
                .account
                .contains_key("default")
                .then(|| "default".to_owned())
        })
    }
    /// The routing key naming this account, regardless of case.
    pub(crate) fn mapped_key(&self, identity: &AntigravityIdentity) -> Option<String> {
        self.routing
            .account
            .keys()
            .find(|key| key.eq_ignore_ascii_case(&identity.email))
            .cloned()
    }

    pub async fn resolve(
        &self,
        headers: &HeaderMap,
        learned: &RefreshRoutes<AntigravityIdentity>,
    ) -> Result<AntigravityRoute> {
        let token = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .filter(|s| valid_token(s))
            .ok_or(Error::new(401, "Antigravity requires a Bearer token."))?;
        let (login, unavailable) = match self.saved().await {
            Ok(login) => (login, false),
            Err(_) => (None, true),
        };
        let route = |proxy: &Choice,
                     identity: Option<AntigravityIdentity>,
                     matched_login: bool,
                     needs_profile: bool,
                     label: &str| {
            Ok(AntigravityRoute {
                token: token.into(),
                proxy: proxy.clone(),
                configured: matched_login
                    || identity
                        .as_ref()
                        .is_some_and(|i| self.mapped_choice(i).is_some()),
                identity,
                matched_login,
                refresh_token: None,
                needs_profile,
                label: label.into(),
            })
        };
        if let Some(login) = login.filter(|login| login.access_token == token) {
            let identity = Self::login_identity(&login, learned);
            // Antigravity's own login names no account; an explicit probe
            // route lets email routes apply to it, once per login.
            let probe = self
                .routing
                .account_probe
                .as_ref()
                .filter(|_| identity.is_none());
            let mut matched = match probe {
                Some(probe) => route(probe, None, true, true, "antigravity-profile")?,
                None => {
                    let proxy = self
                        .account_choice(identity.as_ref(), true)
                        .ok_or(Error::config("Antigravity account has no proxy route."))?;
                    // As for Claude's saved logins, the source label.
                    route(proxy, identity, true, false, "default")?
                }
            };
            matched.refresh_token = login.refresh_token;
            return Ok(matched);
        }
        if self.account_auth_file_only {
            if unavailable {
                return Err(Error::config(
                    "No matching Antigravity route; credential source unavailable.",
                ));
            }
            return Err(Error::new(
                401,
                "Antigravity token does not match the saved login.",
            ));
        }
        let probe = self
            .routing
            .account_probe
            .as_ref()
            .or(self.routing.account_default())
            .ok_or(Error::config(
                "Antigravity account lookup requires routing.account_probe or routing.account_fallback/fallback.",
            ))?;
        route(probe, None, false, true, "antigravity-profile")
    }

    /// The route of a refresh, as Codex's: the saved login's refresh token
    /// takes its account's route; with `account_auth_file_only: false`, a
    /// learned refresh token takes its account's, else `account_fallback`,
    /// then `fallback`. Anything else is refused before any connection.
    pub(crate) async fn resolve_refresh(
        &self,
        refresh_token: &str,
        learned: &RefreshRoutes<AntigravityIdentity>,
    ) -> Result<RefreshRoute> {
        let (login, unavailable) = match self.saved().await {
            Ok(login) => (login, false),
            Err(_) => (None, true),
        };
        if let Some(login) =
            login.filter(|login| login.refresh_token.as_deref() == Some(refresh_token))
        {
            let identity = Self::login_identity(&login, learned);
            if identity.is_none() {
                if let Some(probe) = &self.routing.account_probe {
                    return Ok(RefreshRoute {
                        saved: true,
                        proxy: probe.clone(),
                        identity: None,
                        configured: true,
                        probe_token: Some(login.access_token),
                    });
                }
            }
            let proxy = self
                .account_choice(identity.as_ref(), true)
                .ok_or(Error::config("Antigravity account has no proxy route."))?
                .clone();
            return Ok(RefreshRoute {
                saved: true,
                proxy,
                identity,
                configured: true,
                probe_token: None,
            });
        }
        if !self.account_auth_file_only {
            if let Some(identity) = learned.identity(refresh_token) {
                let proxy = self
                    .account_choice(Some(&identity), false)
                    .ok_or(Error::config("Antigravity account has no proxy route."))?
                    .clone();
                return Ok(RefreshRoute {
                    saved: false,
                    proxy,
                    configured: self.mapped_choice(&identity).is_some(),
                    identity: Some(identity),
                    probe_token: None,
                });
            }
            if let Some(proxy) = self.routing.account_default() {
                return Ok(RefreshRoute {
                    saved: false,
                    proxy: proxy.clone(),
                    identity: None,
                    configured: false,
                    probe_token: None,
                });
            }
        }
        if unavailable {
            Err(Error::config(
                "No matching refresh route; one or more credential sources are unavailable.",
            ))
        } else {
            Err(Error::new(
                403,
                "Token refresh requires the refresh token of Antigravity's saved login, or a learned route or account_fallback/fallback with account_auth_file_only: false.",
            ))
        }
    }

    /// Routes a refresh by the account a lookup named for the saved login.
    pub(crate) fn apply_refresh_profile(
        &self,
        route: &mut RefreshRoute,
        identity: AntigravityIdentity,
    ) -> Result<()> {
        route.proxy = self
            .account_choice(Some(&identity), true)
            .ok_or(Error::config("Antigravity account has no proxy route."))?
            .clone();
        route.identity = Some(identity);
        route.probe_token = None;
        Ok(())
    }

    /// Routes a token by the account a Google lookup named.
    pub(crate) fn apply_profile(
        &self,
        route: &mut AntigravityRoute,
        identity: AntigravityIdentity,
    ) -> Result<()> {
        route.proxy = self
            .account_choice(Some(&identity), route.matched_login)
            .ok_or(Error::config("Antigravity account has no proxy route."))?
            .clone();
        route.configured |= self.mapped_choice(&identity).is_some();
        route.label = "antigravity-account".into();
        route.identity = Some(identity);
        route.needs_profile = false;
        Ok(())
    }

    /// The upstream URL of a target inside the `/antigravity` scope: a path
    /// under `base_url`, or an explicit Cloud Code URL.
    pub fn url(&self, target: &str) -> Result<Url> {
        if target.starts_with("/https://") || target.starts_with("/http://") {
            let host = Url::parse(&target[1..])
                .ok()
                .and_then(|u| u.host_str().map(str::to_ascii_lowercase));
            let base = self
                .origins()
                .into_iter()
                .find(|origin| {
                    Url::parse(origin)
                        .ok()
                        .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
                        == host
                })
                .ok_or(Error::config(
                    "Explicit Antigravity upstream must be a Cloud Code endpoint.",
                ))?;
            return crate::routing::upstream_url(base, target, false);
        }
        if !target.starts_with('/') || target.starts_with("//") {
            return Err(Error::config("Invalid Antigravity request target."));
        }
        let explicit = format!("/{}{target}", self.base_url.trim_end_matches('/'));
        crate::routing::upstream_url(&self.base_url, &explicit, false)
    }

    /// Whether this saved login can be read, for credential checks.
    pub async fn check_credentials(&self) -> Result<()> {
        if !self.saved_login || !self.enabled() {
            return Ok(());
        }
        match crate::keychain::read_go_keyring(SERVICE, ACCOUNT).await? {
            Some(raw) if parse_login(&raw).is_some() => Ok(()),
            Some(_) => Err(Error::config(
                "Invalid Antigravity login in the OS credential store.",
            )),
            None => Err(Error::config(
                "No Antigravity login in the OS credential store; sign in to Antigravity first.",
            )),
        }
    }
}

/// Tests that use the saved login hold this, so concurrent tests do not
/// replace it under each other.
#[cfg(test)]
pub(crate) static TEST_LOGIN: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
#[cfg(test)]
pub(crate) const TEST_SAVED_TOKEN: &str = "antigravity-saved-token";
#[cfg(test)]
pub(crate) fn set_test_login(access: &str, refresh: &str) {
    crate::keychain::set_test_entry(
        SERVICE,
        ACCOUNT,
        Some(
            &serde_json::json!({"token": {"access_token": access, "refresh_token": refresh}})
                .to_string(),
        ),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn config(extra: &str) -> Config {
        Config::parse(&format!(
            "listen_port: 8787\nrequest_timeout_seconds: 3\nproxies:\n  us: http://127.0.0.1:9\n  jp: http://127.0.0.1:10\n{extra}"
        ))
        .unwrap()
    }
    fn headers(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", format!("Bearer {token}").parse().unwrap());
        headers
    }

    #[test]
    fn logins_are_read_in_go_keyrings_forms() {
        let value = json!({"token": {"access_token": "ya29.a", "refresh_token": "r", "expiry": "2026-07-12T10:00:00Z"}});
        let wrapped = format!("{WRAPPED}{}", STANDARD.encode(value.to_string()));
        let expected = Login {
            access_token: "ya29.a".into(),
            refresh_token: Some("r".into()),
            identity: None,
        };
        assert_eq!(parse_login(&wrapped), Some(expected));
        let plain = format!("\u{feff}{value}\r\n");
        assert_eq!(parse_login(&plain).unwrap().access_token, "ya29.a");
        // Copool's sign-ins record the account's email.
        let named = json!({"token": {"access_token": "a"}, "id_token_email": "Alice@Gmail.com"});
        assert_eq!(
            parse_login(&named.to_string())
                .unwrap()
                .identity
                .unwrap()
                .email,
            "Alice@Gmail.com"
        );
        let flat = json!({"accessToken": "b", "refreshToken": "r", "expiresAt": 1});
        assert_eq!(parse_login(&flat.to_string()).unwrap().access_token, "b");
        for raw in [
            "raw-access-token",
            r#"{"token": {"refresh_token": "r"}}"#,
            "[]",
            "go-keyring-base64:not-base64!",
            r#"{"token": {"access_token": "has space"}}"#,
        ] {
            assert!(parse_login(raw).is_none(), "{raw}");
        }
    }

    #[test]
    fn configuration_validates_routes_and_sources() {
        let parsed = config("antigravity:\n  routing:\n    account:\n      default: us\n");
        assert_eq!(parsed.antigravity.base_url, DEFAULT_BASE);
        assert!(parsed.antigravity.saved_login && parsed.antigravity.account_auth_file_only);
        assert!(parsed.antigravity.enabled());
        assert!(!config("").antigravity.enabled());
        for invalid in [
            "antigravity:\n  routing:\n    api_key:\n      KEY: us\n",
            "antigravity:\n  routing:\n    mcp_fallback: us\n",
            "antigravity:\n  routing:\n    account:\n      default: missing\n",
            "antigravity:\n  routing:\n    account_probe: missing\n",
            "antigravity:\n  saved_login: false\n  routing:\n    fallback: us\n",
            "antigravity:\n  base_url: http://cloudcode.example\n",
            "antigravity:\n  unknown: true\n",
        ] {
            assert!(
                Config::parse(&format!(
                    "listen_port: 8787\nrequest_timeout_seconds: 3\nproxies:\n  us: http://127.0.0.1:9\n{invalid}"
                ))
                .is_err(),
                "{invalid}"
            );
        }
        config(
            "antigravity:\n  saved_login: false\n  account_auth_file_only: false\n  routing:\n    fallback: us\n",
        );
    }

    #[test]
    fn targets_stay_on_cloud_code_endpoints() {
        let config = config("");
        let ag = &config.antigravity;
        assert_eq!(target("/antigravity/v1internal:x"), Some("/v1internal:x"));
        assert_eq!(target("/antigravityx/v1internal:x"), None);
        assert_eq!(
            ag.url("/v1internal:streamGenerateContent?alt=sse")
                .unwrap()
                .as_str(),
            "https://daily-cloudcode-pa.googleapis.com/v1internal:streamGenerateContent?alt=sse"
        );
        assert_eq!(
            ag.url("/https://cloudcode-pa.googleapis.com/v1internal:loadCodeAssist")
                .unwrap()
                .as_str(),
            "https://cloudcode-pa.googleapis.com/v1internal:loadCodeAssist"
        );
        for invalid in [
            "/https://example.com/v1internal:loadCodeAssist",
            "/http://cloudcode-pa.googleapis.com/v1internal:loadCodeAssist",
            "/https://user@cloudcode-pa.googleapis.com/v1internal:x",
            "//cloudcode-pa.googleapis.com/x",
            "/a/../../x",
        ] {
            assert!(ag.url(invalid).is_err(), "{invalid}");
        }
    }

    #[tokio::test]
    async fn the_saved_login_routes_by_email_then_default_and_others_need_a_lookup() {
        let _login = TEST_LOGIN.lock().await;
        set_test_login(TEST_SAVED_TOKEN, "saved-refresh");
        let learned = RefreshRoutes::default();
        let file_only = config(
            "antigravity:\n  routing:\n    account:\n      default: us\n      bob@example.com: jp\n",
        );
        let route = file_only
            .antigravity
            .resolve(&headers(TEST_SAVED_TOKEN), &learned)
            .await
            .unwrap();
        assert!(route.matched_login && route.configured && !route.needs_profile);
        assert_eq!(route.proxy.label(), "us");
        let error = file_only
            .antigravity
            .resolve(&headers("other"), &learned)
            .await
            .err()
            .unwrap();
        assert_eq!(error.status, 401);
        assert!(
            file_only
                .antigravity
                .resolve(&HeaderMap::new(), &learned)
                .await
                .is_err()
        );

        // With a probe route, the unnamed saved login is looked up first.
        let probed = config(
            "antigravity:\n  account_auth_file_only: false\n  routing:\n    account:\n      default: us\n      bob@example.com: jp\n    account_probe: us\n",
        );
        let mut route = probed
            .antigravity
            .resolve(&headers(TEST_SAVED_TOKEN), &learned)
            .await
            .unwrap();
        assert!(route.needs_profile && route.matched_login);
        assert_eq!(route.refresh_token.as_deref(), Some("saved-refresh"));
        let bob = AntigravityIdentity::userinfo(&json!({"email": "Bob@Example.com"})).unwrap();
        probed
            .antigravity
            .apply_profile(&mut route, bob.clone())
            .unwrap();
        assert_eq!(route.proxy.label(), "jp");
        // Once learned for the login's refresh token, no lookup is needed,
        // whichever access token it holds.
        learned.learn("saved-refresh", &bob).unwrap();
        set_test_login("renewed-token", "saved-refresh");
        let renewed = probed
            .antigravity
            .resolve(&headers("renewed-token"), &learned)
            .await
            .unwrap();
        assert!(!renewed.needs_profile && renewed.matched_login);
        assert_eq!(renewed.proxy.label(), "jp");
        assert_eq!(
            probed.antigravity.login_label(renewed.identity.as_ref()),
            Some("bob@example.com".into())
        );
        // Another login is not taken for the learned account.
        set_test_login("switched-token", "other-refresh");
        let switched = probed
            .antigravity
            .resolve(&headers("switched-token"), &learned)
            .await
            .unwrap();
        assert!(switched.needs_profile && switched.identity.is_none());
        set_test_login(TEST_SAVED_TOKEN, "saved-refresh");
        let mut other = probed
            .antigravity
            .resolve(&headers("other"), &learned)
            .await
            .unwrap();
        assert!(other.needs_profile && !other.matched_login && !other.configured);
        let carol = AntigravityIdentity::userinfo(&json!({"email": "carol@example.com"})).unwrap();
        // Another token neither takes the saved login's default nor a fallback.
        assert!(probed.antigravity.apply_profile(&mut other, carol).is_err());
        assert!(AntigravityIdentity::userinfo(&json!({"name": "x"})).is_err());
    }
}
