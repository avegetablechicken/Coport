//! Grok forwarding: the requests of Grok Build (`grok`) to xAI, routed by the
//! login or API key they carry. As Copool and OpenQuota read it, Grok Build
//! keeps its sign-ins in `auth.json` in its home (`~/.grok`, or `GROK_HOME`):
//! an object keyed `<issuer>::<client id>`, each holding the access token
//! (`key`), `refresh_token`, `expires_at` and the account's `user_id` and
//! `email`. Grok Build renews the access token itself and rewrites the file,
//! so it is read for each request.
//!
//! A login's requests go to the session API Grok Build names
//! `cli_chat_proxy_base_url`; an API key's (`XAI_API_KEY`) to the xAI API it
//! names `xai_api_base_url`. Both may point at the one `/grok` prefix: the
//! credential decides the upstream.
use crate::{
    Error, Result,
    config::{Choice, Config, Routing, validate_upstream},
    identity::valid_token,
    refresh_routes::RefreshRoutes,
    routing::api_keys::{ApiKeys, Read, Texts},
};
mod account;
pub(crate) use account::Endpoint;
pub use account::GrokAccount;
pub const API: &str = "https://cli-chat-proxy.grok.com";
pub const ISSUER: &str = AUTH_ORIGIN;
pub(crate) const CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
use hyper::HeaderMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;
use url::Url;

/// Grok Build's session API, its `cli_chat_proxy_base_url` by default.
pub const DEFAULT_BASE: &str = "https://cli-chat-proxy.grok.com/v1";
/// The xAI API, Grok Build's `xai_api_base_url` by default.
pub const DEFAULT_API_BASE: &str = "https://api.x.ai/v1";
/// xAI OAuth issuer, reserved for Grok Build token renewal.
pub const AUTH_ORIGIN: &str = "https://auth.x.ai";
/// Where Grok Build's `[auth.oauth2] issuer` points to reach xAI's sign-in
/// through this listener: `<listener>/grok/https://auth.x.ai`.
pub const ISSUER_SCOPE: &str = "/grok/https://auth.x.ai";
/// xAI's token endpoint: renewals, and sign-ins with a device code.
pub const TOKEN_URL: &str = "https://auth.x.ai/oauth2/token";
/// xAI's device authorization endpoint, under the issuer as Grok Build asks.
pub const DEVICE_CODE_URL: &str = "https://auth.x.ai/oauth2/device/code";
/// xAI's OpenID discovery document, which names the token endpoint.
pub const DISCOVERY_URL: &str = "https://auth.x.ai/.well-known/openid-configuration";
/// The route name records give an API key `fallback` carried, as Claude's
/// `claude-api-key-fallback`.
pub const API_KEY_FALLBACK: &str = "grok-api-key-fallback";

fn default_base() -> String {
    DEFAULT_BASE.into()
}
fn default_api_base() -> String {
    DEFAULT_API_BASE.into()
}

/// Grok's upstreams, written as Codex's `base_url`.
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Bases {
    /// The session API a login's requests go to.
    #[serde(default = "default_base")]
    pub account: String,
    /// The xAI API an API key's requests go to when they name no upstream.
    #[serde(default = "default_api_base")]
    pub api_key: String,
}
impl Default for Bases {
    fn default() -> Self {
        Self {
            account: default_base(),
            api_key: default_api_base(),
        }
    }
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Grok {
    pub base_url: Bases,
    /// Grok Build homes whose `auth.json` logins are matched, as Codex's
    /// `homes`.
    pub homes: Option<Vec<String>>,
    pub auth_file: Option<String>,
    /// Reject logins other than the saved ones instead of routing them by
    /// the account their token's claims name.
    pub account_auth_file_only: Option<bool>,
    pub routing: Routing,
}
/// The xAI account a login signs in: its user ID and email, either of which
/// an account route may name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GrokIdentity {
    pub user_id: Option<String>,
    pub email: Option<String>,
    team_id: Option<String>,
}
impl GrokIdentity {
    fn new(user_id: Option<&str>, email: Option<&str>, team_id: Option<&str>) -> Self {
        let clean = |value: Option<&str>| {
            value
                .map(str::trim)
                .filter(|v| !v.is_empty() && v.len() <= 320 && !v.chars().any(char::is_control))
                .map(str::to_owned)
        };
        Self {
            user_id: clean(user_id),
            team_id: clean(team_id),
            email: clean(email).filter(|email| email.contains('@')),
        }
    }
    /// The account records name: the user ID, else the email.
    pub fn account(&self) -> Option<&str> {
        self.user_id.as_deref().or(self.email.as_deref())
    }
    fn is_known(&self) -> bool {
        self.user_id.is_some() || self.email.is_some()
    }
    /// The account an unverified access token's claims name.
    fn claims(token: &str) -> Option<Self> {
        let claims = crate::identity::claims(token);
        let identity = Self::new(
            claims["sub"].as_str(),
            claims["email"].as_str(),
            claims["team_id"].as_str(),
        );
        identity.is_known().then_some(identity)
    }
}

/// Whether an issuer Grok Build saved a login under is xAI's: its own, or
/// xAI's reached through this listener's issuer scope. The scope is part of
/// the key Grok Build files the login under, so both name the same account.
pub(crate) fn xai_issuer(issuer: &str) -> bool {
    issuer == ISSUER
        || (issuer.starts_with("http://") || issuer.starts_with("https://"))
            && issuer.ends_with(ISSUER_SCOPE)
}

/// Whether an `auth.json` key, `<issuer>::<client id>`, files Grok Build's
/// own xAI login.
pub(crate) fn xai_login_key(key: &str) -> bool {
    key.rsplit_once("::")
        .is_some_and(|(issuer, client)| client == CLIENT_ID && xai_issuer(issuer))
}

/// xAI's discovery document as Grok Build reads it through this listener:
/// the token and device endpoints, which carry its refresh token and
/// sign-in, point at `base`, the listener the request reached; the issuer
/// and every other endpoint stay xAI's.
pub(crate) fn discovery_through(document: &mut Value, base: &str) {
    let Some(fields) = document.as_object_mut() else {
        return;
    };
    for field in ["token_endpoint", "device_authorization_endpoint"] {
        if let Some(value) = fields.get_mut(field) {
            if let Some(url) = value
                .as_str()
                .filter(|url| [TOKEN_URL, DEVICE_CODE_URL].contains(url))
            {
                *value = format!("{base}/grok/{url}").into();
            }
        }
    }
}

/// The parts of a saved login Coport uses.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Login {
    pub access_token: String,
    pub identity: GrokIdentity,
}
/// A sign-in: an object with an access token.
fn login(value: &Value) -> Option<Login> {
    let access_token = value["key"]
        .as_str()
        .map(str::trim)
        .filter(|t| valid_token(t))?;
    Some(Login {
        access_token: access_token.into(),
        identity: GrokIdentity::new(
            value["user_id"].as_str(),
            value["email"].as_str(),
            value["team_id"].as_str(),
        ),
    })
}
/// The logins an `auth.json` holds, xAI's first: the object of sign-ins
/// Grok Build writes, or one sign-in by itself. A byte order mark is
/// skipped, as Copool does. None when the text is not such JSON.
pub(crate) fn parse_logins(text: &str) -> Option<Vec<Login>> {
    let value: Value = serde_json::from_str(text.trim_start_matches('\u{feff}').trim()).ok()?;
    if let Some(login) = login(&value) {
        return Some(vec![login]);
    }
    let object = value.as_object()?;
    let mut entries: Vec<_> = object.iter().collect();
    entries.sort_by_key(|(key, _)| !xai_login_key(key));
    Some(entries.into_iter().filter_map(|(_, v)| login(v)).collect())
}

/// How a login's request routes.
pub struct GrokRoute {
    pub token: String,
    pub proxy: Choice,
    pub identity: Option<GrokIdentity>,
    /// The token is a saved login's.
    pub matched_login: bool,
    /// The routing key that carried it, which records name.
    pub account_label: Option<String>,
    /// A saved login or an account the routing maps, not one a fallback
    /// carries.
    pub configured: bool,
    pub url: Url,
}

/// An API key's Route: by its key or upstream URL. Grok adds nothing.
pub type ApiRoute = crate::routing::api_keys::ApiRoute;

pub enum GrokTarget {
    Account(GrokRoute),
    /// An API Key Route and the URL its request goes to.
    Api(ApiRoute, Url),
}

/// Whether a target names an upstream URL rather than a path.
fn explicit(target: &str) -> bool {
    target.starts_with("/https://") || target.starts_with("/http://")
}

/// The origin of an upstream, for host and port comparisons.
fn same_origin(a: &Url, b: &Url) -> bool {
    a.host_str() == b.host_str() && a.port_or_known_default() == b.port_or_known_default()
}

impl Grok {
    /// The one file policy of every Grok endpoint, as Settings shows it:
    /// only saved logins unless `account_auth_file_only: false`.
    pub fn file_only(&self) -> bool {
        self.account_auth_file_only.unwrap_or(true)
    }

    fn login_files(&self) -> Vec<PathBuf> {
        if let Some(file) = &self.auth_file {
            return vec![crate::config::expand(file)];
        }
        if let Some(homes) = &self.homes {
            return homes
                .iter()
                .map(|home| crate::config::expand(home).join("auth.json"))
                .collect();
        }
        let home = std::env::var_os("GROK_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| crate::config::expand("~/.grok"));
        vec![home.join("auth.json")]
    }

    /// Whether logins are routed: an account route or a fallback.
    pub fn enabled(&self) -> bool {
        !self.routing.account.is_empty() || self.routing.account_default().is_some()
    }
    /// Whether any Grok request is routed.
    pub fn configured(&self) -> bool {
        self.enabled() || !self.routing.api_key.is_empty()
    }

    pub(crate) fn validate(&self, config: &Config) -> Result<()> {
        if self.routing.mcp_fallback.is_some() {
            return Err(Error::config("Grok does not support mcp_fallback."));
        }
        if self.routing.account_probe.is_some() {
            return Err(Error::config(
                "Grok logins name their account; account_probe is not supported.",
            ));
        }
        if self.auth_file.as_ref().is_some_and(|p| p.trim().is_empty())
            || self.auth_file.is_some() && self.homes.is_some()
        {
            return Err(Error::config(
                "Configure either Grok homes or one nonempty auth_file.",
            ));
        }
        if let Some(homes) = &self.homes {
            crate::config::validate_directories(homes)?;
        }
        let account = validate_upstream(&self.base_url.account)?;
        if [AUTH_ORIGIN, DEFAULT_API_BASE]
            .iter()
            .any(|base| same_origin(&account, &Url::parse(base).unwrap()))
        {
            return Err(Error::config(
                "Grok's subscription upstream cannot be the xAI API or OAuth issuer.",
            ));
        }
        if self.file_only() && self.homes.as_ref().is_some_and(Vec::is_empty) && self.enabled() {
            return Err(Error::config(
                "Grok file-only account routing requires homes.",
            ));
        }
        self.validate_api_routes(config)?;
        for (label, choice) in &self.routing.account {
            if label.trim().is_empty() {
                return Err(Error::config("Empty Grok routing identifier."));
            }
            config.validate_choice(choice)?;
        }
        for choice in [&self.routing.account_fallback, &self.routing.fallback]
            .into_iter()
            .flatten()
        {
            config.validate_choice(choice)?;
        }
        Ok(())
    }

    /// As Claude's: an API key route names an environment variable whose
    /// value is the key, such as `XAI_API_KEY`, or an upstream URL.
    fn validate_api_routes(&self, config: &Config) -> Result<()> {
        self.api_upstream(&validate_upstream(&self.base_url.api_key)?, true)?;
        self.api_keys().validate(
            config,
            |url| self.api_upstream(&crate::routing::by_url::validate_upstream(url)?, false),
            crate::config::validate_env,
        )
    }

    fn api_keys(&self) -> ApiKeys<'_> {
        ApiKeys::new(
            &self.routing.api_key,
            Texts {
                ambiguous: "Grok credential matches multiple routes.",
                duplicate: "Multiple Grok routes have the same credential.",
            },
        )
    }

    /// xAI's own hosts stay routed by credential: a URL route forwards any
    /// token, so it cannot name one and carry a login past its account
    /// route. The API key base may be the xAI API itself.
    fn api_upstream(&self, url: &Url, base: bool) -> Result<()> {
        let mut reserved: Vec<&str> = self.origins();
        reserved.push(AUTH_ORIGIN);
        if !base {
            reserved.push(DEFAULT_API_BASE);
        }
        for origin in reserved.into_iter().filter_map(|o| Url::parse(o).ok()) {
            if same_origin(&origin, url) {
                return Err(Error::config(if base {
                    "grok.base_url.api_key cannot be Grok's session API or sign-in host."
                } else {
                    "Grok API key URL routes cannot name an xAI host; route its keys by name."
                }));
            }
        }
        Ok(())
    }

    /// Whether API keys without a URL route are served: by name or `fallback`.
    pub(crate) fn serves_api_keys(&self) -> bool {
        self.routing.fallback.is_some() || self.api_keys().named().next().is_some()
    }

    /// Display classification, as Codex's and Claude's `api_key_kind`.
    pub fn api_key_kind(&self, selector: &str) -> &'static str {
        self.api_keys().kind(selector)
    }

    /// The upstream a selector forwards to when its request names none; none
    /// for URL routes, whose requests name it.
    pub fn api_key_upstream(&self, selector: &str) -> Option<&str> {
        (!self.api_keys().is_url(selector)).then_some(self.base_url.api_key.as_str())
    }

    /// Whether an API key URL route serves this unprefixed target.
    pub(crate) fn url_route_matches(&self, target: &str) -> Result<bool> {
        Ok(self.api_keys().url_route(target)?.is_some())
    }

    /// Origins of the session API: the configured one and Grok Build's own.
    pub(crate) fn origins(&self) -> Vec<&str> {
        let mut origins = vec![self.base_url.account.as_str()];
        if self.base_url.account.trim_end_matches('/') != DEFAULT_BASE {
            origins.push(DEFAULT_BASE);
        }
        origins
    }

    /// The logins saved in each home, as they are now, and whether a home's
    /// `auth.json` could not be read. A home without one has no login.
    pub(crate) fn logins(&self) -> (Vec<Login>, bool) {
        let mut logins = Vec::new();
        let mut unavailable = false;
        for path in self.login_files() {
            match std::fs::read_to_string(&path) {
                Ok(text) => match parse_logins(&text) {
                    Some(found) => logins.extend(found),
                    None => unavailable = true,
                },
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => unavailable = true,
            }
        }
        (logins, unavailable)
    }

    /// The routing key naming this account: its user ID, else its email
    /// regardless of case.
    pub(crate) fn mapped_key(&self, identity: &GrokIdentity) -> Option<String> {
        let membership = identity
            .user_id
            .as_ref()
            .zip(identity.team_id.as_ref())
            .map(|(user, team)| format!("{user}/{team}"));
        membership
            .iter()
            .chain(&identity.user_id)
            .chain(&identity.email)
            .find_map(|key| self.account_selector(key))
            .map(|(key, _)| key.clone())
    }

    fn account_selector(&self, key: &str) -> Option<(&String, &Choice)> {
        self.routing.account.get_key_value(key).or_else(|| {
            if !key.contains('@') {
                return None;
            }
            self.routing
                .account
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(key))
        })
    }

    /// The route key of an account: its own route, then `default` for a
    /// saved login, then `account_fallback`, then `fallback`.
    pub(crate) fn account_label(&self, identity: &GrokIdentity, saved: bool) -> Option<String> {
        self.mapped_key(identity)
            .or_else(|| {
                (saved && self.routing.account.contains_key("default")).then(|| "default".into())
            })
            .or_else(|| crate::identity::routing_account_label(&self.routing, "", &[], ""))
    }

    /// The route of xAI sign-in requests no saved credential names: the
    /// discovery document a renewal reads first, and a device sign-in. They
    /// take the first saved login's account route (an account renewing
    /// itself, or signing in again), else `default`, `account_fallback` or
    /// `fallback`.
    pub(crate) fn issuer_route(&self) -> Result<(&Choice, String)> {
        let (logins, _) = self.logins();
        logins
            .iter()
            .find_map(|login| self.account_label(&login.identity, true))
            .or_else(|| {
                self.routing
                    .account
                    .contains_key("default")
                    .then(|| "default".into())
            })
            .or_else(|| crate::identity::routing_account_label(&self.routing, "", &[], ""))
            .and_then(|label| Some((self.account_choice(&label)?, label)))
            .ok_or(Error::new(
                403,
                "Grok sign-in needs a saved login's account route, a default account route or a fallback.",
            ))
    }

    fn account_choice(&self, label: &str) -> Option<&Choice> {
        match label {
            "account_fallback" => self.routing.account_fallback.as_ref(),
            "fallback" => self.routing.fallback.as_ref(),
            _ => self.routing.account.get(label),
        }
    }

    /// The route of a request in the `/grok` scope, `rest` without the
    /// prefix: a URL route; a saved login's access token; a named API key;
    /// with `account_auth_file_only: false`, another login named by its
    /// token's claims; else an API key `fallback` carries.
    pub(crate) async fn resolve(
        &self,
        headers: &HeaderMap,
        rest: &str,
        learned: &RefreshRoutes<GrokAccount>,
    ) -> Result<GrokTarget> {
        self.resolve_with(headers, rest, Some(learned), |name| async move {
            crate::identity::environment_key_with_shell(&name, true).await
        })
        .await
    }

    async fn resolve_with<F, Fut>(
        &self,
        headers: &HeaderMap,
        rest: &str,
        learned: Option<&RefreshRoutes<GrokAccount>>,
        lookup: F,
    ) -> Result<GrokTarget>
    where
        F: Fn(String) -> Fut,
        Fut: std::future::Future<Output = Result<String>>,
    {
        if !rest.starts_with('/') || rest.starts_with("//") {
            return Err(Error::config("Invalid Grok request target."));
        }
        let token = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .filter(|s| valid_token(s))
            .ok_or(Error::new(401, "Grok requires a Bearer token."))?;
        let (logins, unavailable) = if self.enabled() {
            self.logins()
        } else {
            (Vec::new(), false)
        };
        if let Some(login) = logins.into_iter().find(|l| l.access_token == token) {
            let label = self
                .account_label(&login.identity, true)
                .ok_or(Error::config("Grok account has no proxy route."))?;
            return self.account_route(token, Some(login.identity), true, label, rest);
        }
        // A rotated opaque access token is still an account credential.
        // Resolve it before a gateway or key fallback can claim it.
        if let Some(identity) = learned.and_then(|routes| routes.access_identity(token)) {
            if !self.enabled() || self.file_only() {
                return Err(Error::new(401, "Grok token does not match a saved login."));
            }
            let identity = identity.model_identity();
            let label = self
                .account_label(&identity, false)
                .ok_or(Error::new(403, "No matching Grok account route."))?;
            return self.account_route(token, Some(identity), false, label, rest);
        }
        // A URL route forwards any key to its upstream, for it to validate.
        if let Some((selector, proxy, upstream)) = self.api_keys().url_route(rest)? {
            let url = crate::routing::upstream_url(upstream.as_str(), rest, false)?;
            return Ok(GrokTarget::Api(
                ApiRoute {
                    selector,
                    proxy,
                    upstream: upstream.as_str().trim_end_matches('/').into(),
                    key: token.into(),
                    configured: false,
                    custom_upstream: true,
                    ext: (),
                },
                url,
            ));
        }
        let lookup = &lookup;
        let found = self
            .api_keys()
            .find(|name| async move { Ok(Read::compare(lookup(name.into()).await, token)) })
            .await?;
        let keys_unavailable = found.unavailable;
        if let Some((name, proxy, ())) = found.named {
            return self.api_route(name, proxy, true, token, rest);
        }
        // xAI's access tokens are JWTs; its API keys are opaque.
        if let Some(identity) = GrokIdentity::claims(token) {
            if self.file_only() || !self.enabled() {
                return Err(if unavailable {
                    Error::config("No matching Grok route; credential source unavailable.")
                } else {
                    Error::new(401, "Grok token does not match a saved login.")
                });
            }
            let label = self
                .account_label(&identity, false)
                .ok_or(Error::config("Grok account has no proxy route."))?;
            return self.account_route(token, Some(identity), false, label, rest);
        }
        if let Some(proxy) = &self.routing.fallback {
            return self.api_route(API_KEY_FALLBACK, proxy, false, token, rest);
        }
        Err(if unavailable || keys_unavailable {
            Error::config("No matching Grok route; credential source unavailable.")
        } else {
            Error::new(401, "Grok credential does not match a configured route.")
        })
    }

    fn account_route(
        &self,
        token: &str,
        identity: Option<GrokIdentity>,
        matched_login: bool,
        account_label: String,
        rest: &str,
    ) -> Result<GrokTarget> {
        let proxy = self
            .account_choice(&account_label)
            .ok_or(Error::config("Grok account has no proxy route."))?
            .clone();
        let mapped = identity
            .as_ref()
            .is_some_and(|i| self.mapped_key(i).is_some());
        Ok(GrokTarget::Account(GrokRoute {
            token: token.into(),
            proxy,
            configured: matched_login || mapped,
            identity,
            matched_login,
            account_label: Some(account_label),
            url: self.account_url(rest)?,
        }))
    }

    /// An API key's route: `base_url.api_key` for a path or the xAI API host
    /// the request names; a named key may go to another upstream its
    /// request names, as a Claude settings file's key goes to that file's
    /// upstream. A key `fallback` carries, which any client may send,
    /// reaches xAI alone.
    fn api_route(
        &self,
        selector: &str,
        proxy: &Choice,
        configured: bool,
        key: &str,
        rest: &str,
    ) -> Result<GrokTarget> {
        let route = |upstream: &str, url: Url, custom_upstream: bool| {
            GrokTarget::Api(
                ApiRoute {
                    selector: selector.into(),
                    proxy: proxy.clone(),
                    upstream: upstream.trim_end_matches('/').into(),
                    key: key.into(),
                    configured,
                    custom_upstream,
                    ext: (),
                },
                url,
            )
        };
        let base = self.base_url.api_key.as_str();
        if !explicit(rest) {
            return Ok(route(
                base,
                crate::routing::upstream_url(base, rest, false)?,
                false,
            ));
        }
        let named =
            Url::parse(&rest[1..]).map_err(|_| Error::config("Invalid explicit upstream."))?;
        // The logins' session API is not a key's: a named key there is
        // refused below, as a URL route naming it is.
        let xai = [base, DEFAULT_API_BASE]
            .into_iter()
            .filter_map(|origin| Url::parse(origin).ok())
            .find(|origin| same_origin(origin, &named));
        if let Some(origin) = xai {
            let origin = origin.origin().ascii_serialization();
            let url = crate::routing::upstream_url(&origin, rest, false)?;
            return Ok(route(&origin, url, false));
        }
        if !configured {
            return Err(Error::config(
                "Explicit Grok upstream must be the xAI API for a fallback key.",
            ));
        }
        let origin = named.origin().ascii_serialization();
        let validated = crate::routing::by_url::validate_upstream(&origin)?;
        self.api_upstream(&validated, false)?;
        let url = crate::routing::upstream_url(validated.as_str(), rest, false)?;
        Ok(route(&origin, url, true))
    }

    /// A login's upstream URL: a path under `base_url.account`, or an
    /// explicit URL on its origin, Grok Build's own or the xAI API's.
    pub fn account_url(&self, rest: &str) -> Result<Url> {
        if explicit(rest) {
            return self
                .origins()
                .into_iter()
                .chain([DEFAULT_API_BASE])
                .filter_map(|base| Url::parse(base).ok())
                .find_map(|base| {
                    crate::routing::upstream_url(&base.origin().ascii_serialization(), rest, false)
                        .ok()
                })
                .ok_or(Error::config(
                    "Explicit Grok upstream for a login must be an xAI API host.",
                ));
        }
        crate::routing::upstream_url(&self.base_url.account, rest, false)
    }

    /// The saved logins for Route activation, as Codex's: each names its
    /// account's route; one that cannot be read leaves the others unknown.
    pub(crate) fn route_logins(&self) -> Vec<crate::route_activation::Login> {
        use crate::route_activation::Login as Activation;
        let (logins, unavailable) = self.logins();
        let mut found: Vec<_> = logins
            .iter()
            .filter_map(|login| self.account_label(&login.identity, true))
            .map(Activation::Active)
            .collect();
        if unavailable {
            found.push(Activation::Unreadable);
        }
        found
    }

    /// Display labels of the saved logins' accounts, keyed as records name
    /// them. Reads only `auth.json`.
    pub(crate) fn login_labels(&self, labels: &mut BTreeMap<(String, String), String>) {
        if !self.enabled() {
            return;
        }
        for login in self.logins().0 {
            let Some(account) = login.identity.account() else {
                continue;
            };
            if let Some(label) = self.account_label(&login.identity, true) {
                labels.insert(("Grok".into(), account.into()), label);
            }
        }
    }

    /// As Claude's: each named key is readable and names one route.
    async fn check_api_keys(&self) -> Result<()> {
        self.api_keys()
            .check_distinct(&mut Default::default(), |name| async move {
                Ok(vec![
                    crate::identity::environment_key_with_shell(name, true).await?,
                ])
            })
            .await
    }

    /// Whether named API keys and, for file-only routing, a saved login can
    /// be read, for credential checks.
    pub async fn check_credentials(&self) -> Result<()> {
        self.check_api_keys().await?;
        if !self.enabled() || !self.file_only() {
            return Ok(());
        }
        match self.logins() {
            (_, true) => Err(Error::config(
                "Cannot read Grok's auth.json in a configured home.",
            )),
            (logins, false) if logins.is_empty() => Err(Error::config(
                "No Grok login in the configured homes; run grok login first.",
            )),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};

    pub(crate) const USER_ID: &str = "8f6b2c1e-3d4a-4b5c-9d6e-7f8091a2b3c4";

    /// An access token with xAI's claims, as Grok's are.
    pub(crate) fn jwt(sub: &str, email: &str) -> String {
        let claims = serde_json::json!({"sub": sub, "email": email}).to_string();
        format!("h.{}.s", URL_SAFE_NO_PAD.encode(claims))
    }

    /// `auth.json` as Grok Build writes it, with another issuer's sign-in.
    pub(crate) fn auth_json(token: &str, email: &str) -> String {
        serde_json::json!({
            "https://idp.example.com::other": {"key": "other-issuer-token", "refresh_token": "r0", "email": "sso@example.com"},
            "https://auth.x.ai::b1a00492-073a-47ea-816f-4c329264a828": {
                "key": token, "auth_mode": "oidc", "user_id": USER_ID, "email": email,
                "refresh_token": "saved-refresh", "expires_at": "2026-11-23T10:00:00.000000Z",
                "oidc_issuer": "https://auth.x.ai",
            },
        })
        .to_string()
    }

    fn config(home: &std::path::Path, extra: &str) -> Config {
        Config::parse(&format!(
            "listen_port: 8787\nrequest_timeout_seconds: 3\ncodex:\n  homes: []\nclaude:\n  config_dirs: []\ngrok:\n  homes: [{:?}]\n{extra}",
            home.to_string_lossy()
        ))
        .unwrap()
    }

    fn headers(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", format!("Bearer {token}").parse().unwrap());
        headers
    }

    async fn resolve(grok: &Grok, token: &str, rest: &str) -> Result<GrokTarget> {
        grok.resolve_with(&headers(token), rest, None, |name| async move {
            match name.as_str() {
                "XAI_API_KEY" => Ok("xai-named-key".into()),
                "OTHER_KEY" => Ok("xai-other-key".into()),
                _ => Err(Error::config("unset")),
            }
        })
        .await
    }

    fn account(target: Result<GrokTarget>) -> GrokRoute {
        match target {
            Ok(GrokTarget::Account(route)) => route,
            Ok(GrokTarget::Api(route, _)) => panic!("API route {}", route.selector),
            Err(error) => panic!("{}", error.message),
        }
    }
    /// An API Key Route, with the URL its request goes to.
    struct Api {
        route: ApiRoute,
        url: Url,
    }
    impl std::ops::Deref for Api {
        type Target = ApiRoute;
        fn deref(&self) -> &ApiRoute {
            &self.route
        }
    }
    fn api(target: Result<GrokTarget>) -> Api {
        match target {
            Ok(GrokTarget::Api(route, url)) => Api { route, url },
            Ok(GrokTarget::Account(_)) => panic!("account route"),
            Err(error) => panic!("{}", error.message),
        }
    }
    fn status(target: Result<GrokTarget>) -> u16 {
        match target {
            Err(error) => error.status,
            Ok(_) => panic!("routed"),
        }
    }

    #[test]
    fn logins_are_read_from_grok_builds_auth_json() {
        let logins =
            parse_logins(&format!("\u{feff}{}", auth_json("saved", "A@Example.com"))).unwrap();
        // xAI's sign-in first, then the others.
        assert_eq!(logins.len(), 2);
        assert_eq!(logins[0].access_token, "saved");
        assert_eq!(logins[0].identity.user_id.as_deref(), Some(USER_ID));
        assert_eq!(logins[0].identity.email.as_deref(), Some("A@Example.com"));
        assert_eq!(logins[1].access_token, "other-issuer-token");
        // One sign-in by itself, as Copool imports it.
        let single = parse_logins(r#"{"key":"one","email":"not-an-email"}"#).unwrap();
        assert_eq!(single[0].identity, GrokIdentity::default());
        assert_eq!(
            parse_logins(r#"{"a":{"refresh_token":"r"}}"#),
            Some(Vec::new())
        );
        assert!(parse_logins("not json").is_none());
        // CRLF files read the same.
        let crlf = auth_json("saved", "a@example.com").replace(',', ",\r\n");
        assert_eq!(parse_logins(&crlf).unwrap()[0].access_token, "saved");
    }

    #[test]
    fn the_issuer_scope_files_xais_login_and_moves_only_its_token_endpoints() {
        for (issuer, xai) in [
            ("https://auth.x.ai", true),
            ("http://127.0.0.1:8787/grok/https://auth.x.ai", true),
            ("https://localhost:8787/grok/https://auth.x.ai", true),
            ("https://auth.x.ai/", false),
            ("https://idp.example.com", false),
            ("/grok/https://auth.x.ai", false),
            ("http://127.0.0.1:8787/grok/https://auth.x.ai/", false),
        ] {
            assert_eq!(xai_issuer(issuer), xai, "{issuer}");
        }
        assert!(xai_login_key(&format!(
            "http://127.0.0.1:8787{ISSUER_SCOPE}::{CLIENT_ID}"
        )));
        assert!(!xai_login_key(&format!(
            "http://127.0.0.1:8787{ISSUER_SCOPE}::other"
        )));
        // A login filed under the issuer scope reads first, as xAI's own.
        let text = serde_json::json!({
            "https://idp.example.com::other": {"key": "other-issuer-token"},
            format!("http://127.0.0.1:8787{ISSUER_SCOPE}::{CLIENT_ID}"): {"key": "scoped", "user_id": USER_ID},
        })
        .to_string();
        assert_eq!(parse_logins(&text).unwrap()[0].access_token, "scoped");

        let mut document = serde_json::json!({
            "issuer": "https://auth.x.ai",
            "authorization_endpoint": "https://accounts.x.ai/oauth2/auth",
            "token_endpoint": TOKEN_URL,
            "device_authorization_endpoint": DEVICE_CODE_URL,
            "jwks_uri": "https://auth.x.ai/.well-known/jwks.json",
        });
        discovery_through(&mut document, "http://127.0.0.1:8787");
        assert_eq!(
            document["token_endpoint"],
            "http://127.0.0.1:8787/grok/https://auth.x.ai/oauth2/token"
        );
        assert_eq!(
            document["device_authorization_endpoint"],
            "http://127.0.0.1:8787/grok/https://auth.x.ai/oauth2/device/code"
        );
        for field in ["issuer", "authorization_endpoint", "jwks_uri"] {
            assert!(
                !document[field].as_str().unwrap().contains("127.0.0.1"),
                "{field}"
            );
        }
        // Endpoints Coport does not serve, and documents that are not
        // objects, are left as they are.
        let mut other = serde_json::json!({"token_endpoint": "https://auth.x.ai/oauth2/token2"});
        discovery_through(&mut other, "http://127.0.0.1:8787");
        assert_eq!(other["token_endpoint"], "https://auth.x.ai/oauth2/token2");
        let mut list = serde_json::json!([TOKEN_URL]);
        discovery_through(&mut list, "http://127.0.0.1:8787");
        assert_eq!(list, serde_json::json!([TOKEN_URL]));
    }

    #[test]
    fn sign_in_takes_the_saved_logins_route_then_default_then_a_fallback() {
        let home = tempfile::tempdir().unwrap();
        let label = |extra: &str| {
            config(home.path(), extra)
                .grok
                .issuer_route()
                .map(|(_, label)| label)
                .map_err(|e| e.status)
        };
        let routes = "  routing:\n    account:\n      a@example.com: none\n      default: none\n    account_fallback: none\n";
        // Without a saved login: `default`, then the fallbacks.
        assert_eq!(label(routes), Ok("default".into()));
        assert_eq!(
            label("  routing:\n    account: {a@example.com: none}\n    fallback: none\n"),
            Ok("fallback".into())
        );
        assert_eq!(
            label("  routing:\n    account: {a@example.com: none}\n"),
            Err(403)
        );
        // A saved login takes its own account's route.
        std::fs::write(
            home.path().join("auth.json"),
            auth_json("saved", "A@Example.com"),
        )
        .unwrap();
        assert_eq!(label(routes), Ok("a@example.com".into()));
        assert_eq!(
            label("  routing:\n    account: {other@example.com: none}\n"),
            Err(403)
        );
    }

    #[test]
    fn configuration_validates_routes_and_homes() {
        let home = tempfile::tempdir().unwrap();
        let ok = config(
            home.path(),
            "  routing:\n    account:\n      default: none\n    api_key:\n      XAI_API_KEY: none\n      relay.example.com/v1: none\n",
        );
        assert!(ok.grok.enabled() && ok.grok.serves_api_keys());
        assert_eq!(ok.grok.api_key_kind("relay.example.com/v1"), "gateway");
        assert_eq!(
            ok.grok.api_key_upstream("XAI_API_KEY"),
            Some(DEFAULT_API_BASE)
        );
        let parse = |grok: &str| {
            Config::parse(&format!(
                "listen_port: 8787\nrequest_timeout_seconds: 3\ngrok:\n{grok}"
            ))
        };
        for invalid in [
            "  routing:\n    mcp_fallback: none\n",
            "  routing:\n    account_probe: none\n",
            "  homes: []\n  routing:\n    account: {default: none}\n",
            "  homes: [relative]\n",
            "  routing:\n    account: {'': none}\n",
            "  routing:\n    account: {default: missing}\n",
            "  routing:\n    api_key: {lower-case: none}\n",
            // xAI's own hosts stay routed by credential.
            "  routing:\n    api_key: {api.x.ai/v1: none}\n",
            "  routing:\n    api_key: {cli-chat-proxy.grok.com: none}\n",
            "  routing:\n    api_key: {auth.x.ai: none}\n",
            "  base_url:\n    api_key: https://auth.x.ai/v1\n",
            "  base_url:\n    account: http://cli-chat-proxy.grok.com/v1\n",
        ] {
            assert!(parse(invalid).is_err(), "{invalid}");
        }
        // API keys alone need no homes.
        assert!(parse("  homes: []\n  routing:\n    api_key: {XAI_API_KEY: none}\n").is_ok());
        assert!(
            parse("  homes: []\n  account_auth_file_only: false\n  routing:\n    fallback: none\n")
                .is_ok()
        );
    }

    #[tokio::test]
    async fn saved_logins_route_by_user_id_then_email_then_default() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join("auth.json"),
            auth_json("saved-token", "Alice@Example.com"),
        )
        .unwrap();
        let grok = |routes: &str| config(home.path(), &format!("  routing:\n{routes}")).grok;
        for (routes, label) in [
            (
                format!(
                    "    account:\n      {USER_ID}: none\n      alice@example.com: none\n      default: none\n"
                ),
                USER_ID,
            ),
            (
                "    account:\n      alice@example.com: none\n      default: none\n".into(),
                "alice@example.com",
            ),
            ("    account:\n      default: none\n".into(), "default"),
            ("    account_fallback: none\n".into(), "account_fallback"),
            ("    fallback: none\n".into(), "fallback"),
        ] {
            let route = account(resolve(&grok(&routes), "saved-token", "/v1/responses").await);
            assert_eq!(route.account_label.as_deref(), Some(label), "{routes}");
            assert!(route.matched_login && route.configured);
            assert_eq!(route.identity.unwrap().user_id.as_deref(), Some(USER_ID));
            assert_eq!(
                route.url.as_str(),
                "https://cli-chat-proxy.grok.com/v1/responses"
            );
        }
        let grok = grok("    account:\n      default: none\n");
        // Another issuer's sign-in in the same file is a saved login too.
        let other = account(resolve(&grok, "other-issuer-token", "/v1/models").await);
        assert_eq!(other.account_label.as_deref(), Some("default"));
        // A base URL without `/v1` reaches the same paths.
        let route = account(resolve(&grok, "saved-token", "/chat/completions").await);
        assert_eq!(
            route.url.as_str(),
            "https://cli-chat-proxy.grok.com/v1/chat/completions"
        );
        // Explicit targets stay on xAI's hosts.
        let route = account(
            resolve(
                &grok,
                "saved-token",
                "/https://cli-chat-proxy.grok.com/v1/settings",
            )
            .await,
        );
        assert_eq!(
            route.url.as_str(),
            "https://cli-chat-proxy.grok.com/v1/settings"
        );
        let route = account(resolve(&grok, "saved-token", "/https://api.x.ai/v1/models").await);
        assert_eq!(route.url.as_str(), "https://api.x.ai/v1/models");
        for foreign in [
            "/https://relay.example.com/v1/responses",
            "/https://cli-chat-proxy.grok.com:8443/v1/responses",
            "/http://cli-chat-proxy.grok.com/v1/responses",
        ] {
            assert_eq!(
                status(resolve(&grok, "saved-token", foreign).await),
                502,
                "{foreign}"
            );
        }
        // Other logins are refused with file-only routing, named by their
        // claims without it.
        let token = jwt("other-user", "bob@example.com");
        assert_eq!(status(resolve(&grok, &token, "/v1/responses").await), 401);
        let open = config(
            home.path(),
            "  account_auth_file_only: false\n  routing:\n    account:\n      default: none\n      Bob@example.com: none\n",
        )
        .grok;
        let route = account(resolve(&open, &token, "/v1/responses").await);
        assert_eq!(route.account_label.as_deref(), Some("Bob@example.com"));
        assert!(!route.matched_login && route.configured);
        let stranger = jwt("third", "carol@example.com");
        assert_eq!(
            status(resolve(&open, &stranger, "/v1/responses").await),
            502
        );
        // `default` stays the saved logins'.
        let open = config(
            home.path(),
            "  account_auth_file_only: false\n  routing:\n    account:\n      default: none\n    account_fallback: none\n",
        )
        .grok;
        let route = account(resolve(&open, &stranger, "/v1/responses").await);
        assert_eq!(route.account_label.as_deref(), Some("account_fallback"));
        assert!(!route.configured);
        // No Bearer token, no route.
        assert_eq!(
            status(
                grok.resolve_with(&HeaderMap::new(), "/v1/responses", None, |_| async {
                    Err(Error::config(""))
                })
                .await
            ),
            401
        );
    }

    #[tokio::test]
    async fn api_keys_route_by_name_url_or_fallback_to_the_xai_api() {
        let home = tempfile::tempdir().unwrap();
        let grok = config(
            home.path(),
            "  routing:\n    api_key:\n      XAI_API_KEY: none\n      relay.example.com/v1: none\n",
        )
        .grok;
        let named = api(resolve(&grok, "xai-named-key", "/v1/responses").await);
        assert_eq!(named.selector, "XAI_API_KEY");
        assert!(named.configured && !named.custom_upstream);
        assert_eq!(named.url.as_str(), "https://api.x.ai/v1/responses");
        // A named key may go where its request names, as a Claude settings
        // file's key goes to that file's upstream.
        let gateway = api(resolve(
            &grok,
            "xai-named-key",
            "/https://gateway.example.com/v1/responses",
        )
        .await);
        assert!(gateway.custom_upstream);
        assert_eq!(gateway.upstream, "https://gateway.example.com");
        assert_eq!(
            gateway.url.as_str(),
            "https://gateway.example.com/v1/responses"
        );
        // A URL route forwards any key.
        let url = api(resolve(&grok, "anything", "/https://relay.example.com/v1/responses").await);
        assert_eq!(url.selector, "relay.example.com/v1");
        assert_eq!(url.upstream, "https://relay.example.com/v1");
        assert!(!url.configured && url.custom_upstream);
        // Unknown keys need `fallback`, which reaches xAI alone.
        assert_eq!(
            status(resolve(&grok, "xai-unknown", "/v1/responses").await),
            401
        );
        let fallback = config(
            home.path(),
            "  routing:\n    api_key:\n      XAI_API_KEY: none\n    fallback: none\n",
        )
        .grok;
        let route = api(resolve(&fallback, "xai-unknown", "/v1/models").await);
        assert_eq!(route.selector, API_KEY_FALLBACK);
        assert_eq!(route.url.as_str(), "https://api.x.ai/v1/models");
        assert_eq!(
            status(
                resolve(
                    &fallback,
                    "xai-unknown",
                    "/https://gateway.example.com/v1/models"
                )
                .await
            ),
            502
        );
        // Fallback keys may still reach xAI's API by URL.
        let explicit = api(resolve(&fallback, "xai-unknown", "/https://api.x.ai/v1/models").await);
        assert_eq!(explicit.url.as_str(), "https://api.x.ai/v1/models");
        // No key, named or not, reaches the logins' session API, so an
        // unknown login token cannot take a key route there either.
        let named = config(
            home.path(),
            "  routing:\n    api_key:\n      XAI_API_KEY: none\n    fallback: none\n",
        )
        .grok;
        for token in ["xai-named-key", "xai-unknown"] {
            for target in [
                "/https://cli-chat-proxy.grok.com/v1/responses",
                "/https://cli-chat-proxy.grok.com/v1/settings",
            ] {
                assert_eq!(
                    status(resolve(&named, token, target).await),
                    502,
                    "{token} {target}"
                );
            }
        }
        // A key two names hold is refused.
        let two = config(
            home.path(),
            "  routing:\n    api_key:\n      XAI_API_KEY: none\n      OTHER_KEY: none\n",
        )
        .grok;
        let both = two
            .resolve_with(&headers("xai-shared"), "/v1/models", None, |_| async {
                Ok("xai-shared".to_owned())
            })
            .await;
        assert_eq!(status(both), 409);
    }

    fn route_states(grok: &Grok) -> BTreeMap<String, &'static str> {
        crate::route_activation::route_activation(grok.routing.account.keys(), grok.route_logins())
    }

    #[tokio::test]
    async fn route_states_mark_saved_logins_active() {
        let home = tempfile::tempdir().unwrap();
        let grok = config(
            home.path(),
            "  routing:\n    account:\n      alice@example.com: none\n      bob@example.com: none\n",
        )
        .grok;
        assert_eq!(
            route_states(&grok).into_iter().collect::<Vec<_>>(),
            [
                ("alice@example.com".into(), "inactive"),
                ("bob@example.com".into(), "inactive")
            ]
        );
        std::fs::write(
            home.path().join("auth.json"),
            auth_json("t", "Alice@example.com"),
        )
        .unwrap();
        assert_eq!(route_states(&grok)["alice@example.com"], "active");
        assert_eq!(route_states(&grok)["bob@example.com"], "inactive");
        let mut labels = BTreeMap::new();
        grok.login_labels(&mut labels);
        assert_eq!(
            labels[&("Grok".into(), USER_ID.into())],
            "alice@example.com"
        );
        assert!(grok.check_credentials().await.is_ok());
        std::fs::write(home.path().join("auth.json"), "{").unwrap();
        assert_eq!(route_states(&grok)["bob@example.com"], "unknown");
        assert!(grok.check_credentials().await.is_err());
        std::fs::remove_file(home.path().join("auth.json")).unwrap();
        assert!(grok.check_credentials().await.is_err());
    }
}
