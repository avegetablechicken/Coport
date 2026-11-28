use crate::{
    Error, Result,
    codex::Identity,
    config::{Choice, Config},
    identity::valid_token,
};
use api_keys::Read;
use url::Url;

pub mod accounts;
pub mod api_keys;
pub mod by_url;
pub mod target;

/// A Codex Route: by the ChatGPT login a token is, or by API key.
#[derive(Clone)]
pub enum Route {
    Account(AccountRoute),
    Api(ProviderRoute),
}

/// A Codex API Key Route: a provider's, a URL route's or `fallback`'s. A
/// provider whose saved login is a ChatGPT one names its account (`ext`).
pub type ProviderRoute = api_keys::ApiRoute<Option<String>>;

/// A Route by a ChatGPT login, and the `base_url.account` it goes to.
pub type AccountRoute = accounts::AccountRoute<Identity, String>;

impl Route {
    pub fn proxy(&self) -> &Choice {
        match self {
            Self::Account(route) => &route.proxy,
            Self::Api(route) => &route.proxy,
        }
    }
    pub fn upstream(&self) -> &str {
        match self {
            Self::Account(route) => &route.ext,
            Self::Api(route) => &route.upstream,
        }
    }
    /// The ChatGPT account the Route's credential names.
    pub fn account_id(&self) -> Option<&str> {
        match self {
            Self::Account(route) => route.account_id(),
            Self::Api(route) => route.ext.as_deref(),
        }
    }
}

#[cfg(test)]
impl Route {
    /// The account Route a test expects.
    pub(crate) fn account(self) -> AccountRoute {
        match self {
            Self::Account(route) => route,
            Self::Api(route) => panic!("API Key Route {}", route.selector),
        }
    }
    /// The API Key Route a test expects.
    pub(crate) fn api(self) -> ProviderRoute {
        match self {
            Self::Api(route) => route,
            Self::Account(route) => panic!("account Route {:?}", route.account_id()),
        }
    }
}

impl crate::codex::Codex {
    /// The Codex provider IDs and API Key variables `routing.api_key` names.
    pub fn providers(&self) -> impl Iterator<Item = crate::codex::Provider> + '_ {
        self.api_keys()
            .named()
            .map(|(selector, proxy)| crate::codex::Provider {
                selector: selector.clone(),
                proxy: proxy.clone(),
            })
    }

    /// Codex's API Key Routes: a URL, or a provider ID or API Key
    /// environment variable its homes define.
    pub(crate) fn api_keys(&self) -> api_keys::ApiKeys<'_> {
        api_keys::ApiKeys::new(
            &self.routing.api_key,
            api_keys::Texts {
                ambiguous: "Bearer token matches multiple routes; configure distinct credentials.",
                duplicate: "Multiple routes have the same credential.",
            },
        )
    }
}

/// The app a proxied request belongs to. The proxy decides it once from the
/// request target, routes by it and logs it as `service` on every record.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Deserialize, serde::Serialize,
)]
pub enum Service {
    Codex,
    Claude,
    Antigravity,
    Grok,
}
impl Service {
    /// The logged `service` value.
    pub fn name(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Antigravity => "antigravity",
            Self::Grok => "grok",
        }
    }
    /// The name statistics show.
    pub fn label(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Claude => "Claude",
            Self::Antigravity => "Antigravity",
            Self::Grok => "Grok",
        }
    }
}

pub use target::logged_service;

/// Two of Codex's routes take one token.
const AMBIGUOUS: &str = "Bearer token matches multiple routes; configure distinct credentials.";

impl Config {
    pub fn resolve_url(&self, authorization: Option<&str>, target: &str) -> Result<Option<Route>> {
        let Some((selector, proxy, upstream)) = self.codex.api_keys().url_route(target)? else {
            return Ok(None);
        };
        let token = authorization
            .and_then(|s| s.strip_prefix("Bearer "))
            .filter(|s| valid_token(s))
            .ok_or(Error::new(401, "An API Bearer token is required."))?;
        Ok(Some(Route::Api(ProviderRoute {
            selector,
            proxy,
            upstream: upstream.into(),
            key: token.into(),
            configured: false,
            custom_upstream: true,
            ext: None,
        })))
    }

    pub async fn resolve(&self, authorization: Option<&str>, fallback: bool) -> Result<Route> {
        let token = authorization
            .and_then(|s| s.strip_prefix("Bearer "))
            .filter(|s| valid_token(s))
            .ok_or(Error::new(401, "A configured Bearer token is required."))?;
        let mut unavailable = false;
        let mut identities = Vec::new();
        let account_sources = self.codex.account_sources();
        for (label, source) in &account_sources {
            match self.codex.account_identity(source).await {
                Ok(i) if i.token == token => identities.push((i, Some(label.as_str()))),
                Ok(_) => {}
                Err(_) => unavailable = true,
            }
        }
        if identities.is_empty()
            && !self.codex.account_auth_file_only
            && (!self.codex.routing.account.is_empty()
                || self.codex.routing.account_default().is_some())
        {
            if let Some(i) = Identity::from_token(token) {
                identities.push((i, None));
            }
        }
        // A provider's credentials come from each home; one matching key
        // with two upstreams is ambiguous.
        let (codex, shell) = (&self.codex, identities.is_empty());
        let found = codex
            .api_keys()
            .find(|selector| async move {
                let provider = crate::codex::Provider {
                    selector: selector.into(),
                    proxy: Choice::direct(),
                };
                let Ok(credentials) = provider
                    .credentials(&codex.base_url.api_key, shell, codex)
                    .await
                else {
                    return Ok(Read::Unavailable);
                };
                let mut matching = credentials.into_iter().filter(|c| c.token == token);
                Ok(match (matching.next(), matching.next()) {
                    (Some(credential), None) => Read::Match(credential),
                    (None, _) => Read::Other,
                    (Some(_), Some(_)) => return Err(Error::new(409, AMBIGUOUS)),
                })
            })
            .await?;
        unavailable |= found.unavailable;
        if identities.len() + usize::from(found.named.is_some()) > 1 {
            return Err(Error::new(409, AMBIGUOUS));
        }
        if let Some((i, source)) = identities.pop() {
            let proxy = self.account_choice(&i, source)?;
            // A saved login, or an identity the routing maps; otherwise
            // account_fallback carries it.
            let configured = source.is_some() || self.account_mapped(&i);
            let account_label = crate::identity::routing_account_label(
                &self.codex.routing,
                &i.account_id,
                &i.usernames,
                source.unwrap_or(""),
            );
            return Ok(Route::Account(AccountRoute {
                token: i.token.clone(),
                proxy,
                identity: Some(i),
                matched_login: source.is_some(),
                provider: None,
                account_label,
                configured,
                needs_profile: false,
                ext: self.codex.base_url.account.clone(),
            }));
        }
        if let Some((selector, proxy, credential)) = found.named {
            // A provider with its own base_url is a third-party API, like
            // an explicit URL route: use native TLS for compatibility with
            // certificates rustls rejects (e.g. a self-signed CA as leaf).
            let custom_upstream = credential.upstream != self.codex.base_url.api_key;
            if custom_upstream {
                self.codex
                    .base_url
                    .validate_api_upstream(&by_url::validate_upstream(&credential.upstream)?)?;
            }
            return Ok(Route::Api(ProviderRoute {
                selector: selector.clone(),
                proxy: proxy.clone(),
                upstream: credential.upstream,
                key: credential.token,
                configured: true,
                custom_upstream,
                ext: credential.account_id,
            }));
        }
        if self.codex.account_auth_file_only && Identity::from_token(token).is_some() {
            return Err(Error::new(
                401,
                "Codex token does not match a saved account credential.",
            ));
        }
        if fallback {
            if let Some(proxy) = &self.codex.routing.fallback {
                return Ok(Route::Api(ProviderRoute {
                    selector: "openai-fallback".into(),
                    proxy: proxy.clone(),
                    upstream: self.codex.base_url.api_key.clone(),
                    key: token.into(),
                    configured: false,
                    custom_upstream: false,
                    ext: None,
                }));
            }
        }
        if unavailable {
            Err(Error::config(
                "No matching route; one or more credential sources are unavailable.",
            ))
        } else {
            Err(Error::new(
                401,
                "Bearer token does not match a configured credential.",
            ))
        }
    }
}
impl Config {
    /// Refresh requests carry no Bearer token. The refresh token selects the
    /// saved login whose proxy is used; otherwise the account an earlier token
    /// response named (`learned`, see `crate::refresh_routes`) routes as an
    /// unmatched access token would. Nothing is injected upstream. Returns
    /// the route, its account, and whether the configuration holds or maps it.
    pub async fn resolve_refresh(
        &self,
        refresh_token: &str,
        learned: Option<&Identity>,
    ) -> Result<(Choice, Option<Identity>, bool)> {
        let mut unavailable = false;
        for (label, source) in &self.codex.account_sources() {
            match self.codex.account_identity(source).await {
                Ok(i) if i.refresh_token.as_deref() == Some(refresh_token) => {
                    let proxy = self.account_choice(&i, Some(label))?;
                    return Ok((proxy, Some(i), true));
                }
                Ok(_) => {}
                Err(_) => unavailable = true,
            }
        }
        if !self.codex.account_auth_file_only {
            if let Some(identity) = learned {
                let proxy = self.account_choice(identity, None)?;
                return Ok((proxy, Some(identity.clone()), self.account_mapped(identity)));
            }
            if let Some(proxy) = self.codex.routing.account_default() {
                return Ok((proxy.clone(), None, false));
            }
        }
        if unavailable {
            Err(Error::config(
                "No matching refresh route; one or more credential sources are unavailable.",
            ))
        } else {
            Err(Error::new(
                403,
                "Token refresh requires a refresh token saved in a configured Codex auth.json, or a learned route or account_fallback/fallback with account_auth_file_only: false.",
            ))
        }
    }
}
pub const MCP_PATH: &str = "/mcp/openaiDeveloperDocs";
pub const MCP_UPSTREAM: &str = "https://developers.openai.com/mcp";
fn valid_target(target: &str) -> Result<()> {
    let decoded = percent_encoding::percent_decode_str(target)
        .decode_utf8()
        .map_err(|_| Error::config("Invalid request target."))?;
    if !target.starts_with('/')
        || target.starts_with("//")
        || target.contains('#')
        || decoded.contains('\\')
        || decoded.split('/').any(|s| s == "..")
    {
        return Err(Error::config("Invalid request target."));
    }
    Ok(())
}
/// Returns the only method allowed for a ChatGPT account endpoint.
pub fn account_query(target: &str) -> Option<&'static str> {
    account_query_for_route(target, true)
}

const USAGE_PATH: &str = "/backend-api/wham/usage";
const USAGE_ALIASES: [&str; 2] = ["/wham/usage", "/api/codex/usage"];

pub(crate) fn account_query_for_route(target: &str, account: bool) -> Option<&'static str> {
    let path = if target.starts_with("/https://") || target.starts_with("/http://") {
        Url::parse(&target[1..])
            .ok()
            .map(|u| u.path().to_string())
            .unwrap_or_default()
    } else {
        target.split('?').next().unwrap_or("").to_string()
    };
    match path.as_str() {
        path if account && USAGE_ALIASES.contains(&path) => Some("GET"),
        USAGE_PATH
        | "/backend-api/wham/profiles/me"
        | "/backend-api/wham/rate-limit-reset-credits" => Some("GET"),
        "/backend-api/wham/rate-limit-reset-credits/consume" => Some("POST"),
        _ => None,
    }
}
pub const TOKEN_REFRESH_UPSTREAM: &str = "https://auth.openai.com/oauth/token";
/// Codex sends refreshes here via CODEX_REFRESH_TOKEN_URL_OVERRIDE.
pub fn token_refresh(target: &str) -> bool {
    matches!(
        target,
        "/oauth/token" | "/https://auth.openai.com/oauth/token"
    )
}
pub fn refresh_token(body: &[u8]) -> Option<String> {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v["refresh_token"].as_str().map(String::from))
        .or_else(|| {
            url::form_urlencoded::parse(body)
                .find(|(k, _)| k == "refresh_token")
                .map(|(_, v)| v.into_owned())
        })
        .filter(|s| valid_token(s))
}
pub fn upstream_url(base: &str, target: &str, account: bool) -> Result<Url> {
    valid_target(target)?;
    let mut b = Url::parse(base).map_err(|_| Error::config("Invalid upstream URL."))?;
    let backend = account && b.path().trim_matches('/') == "backend-api";
    if target.starts_with("/https://") || target.starts_with("/http://") {
        let dest =
            Url::parse(&target[1..]).map_err(|_| Error::config("Invalid explicit upstream."))?;
        let root = b.path().trim_end_matches('/');
        if dest.scheme() != "https"
            || !dest.username().is_empty()
            || dest.password().is_some()
            || dest.fragment().is_some()
            || dest.host_str() != b.host_str()
            || dest.port_or_known_default() != b.port_or_known_default()
            || !(dest.path() == root || dest.path().starts_with(&format!("{root}/")))
        {
            return Err(Error::config(
                "Explicit upstream must match the credential's configured HTTPS upstream and API base.",
            ));
        }
        return Ok(dest);
    }
    let dest = if backend {
        if target.starts_with("/backend-api/") {
            b.set_path("");
            format!("{}{target}", b.as_str().trim_end_matches('/'))
        } else {
            let suffix = target
                .strip_prefix("/v1/")
                .map(|v| format!("/{v}"))
                .unwrap_or_else(|| target.into());
            format!(
                "{}{}{suffix}",
                b.as_str().trim_end_matches('/'),
                if suffix.starts_with("/codex/") {
                    ""
                } else {
                    "/codex"
                }
            )
        }
    } else {
        let suffix = ["/backend-api/codex", "/v1"]
            .into_iter()
            .find_map(|prefix| {
                target
                    .strip_prefix(&format!("{prefix}/"))
                    .map(|s| format!("/{s}"))
            })
            .unwrap_or_else(|| target.into());
        format!("{}{suffix}", base.trim_end_matches('/'))
    };
    Url::parse(&dest).map_err(|_| Error::config("Invalid upstream URL."))
}
pub fn query_url(base: &str, target: &str) -> Result<Url> {
    let b = Url::parse(base).map_err(|_| Error::config("Invalid account query base."))?;
    if account_query(target).is_none() || b.path().trim_end_matches('/') != "/backend-api" {
        return Err(Error::config(
            "Account queries require a ChatGPT /backend-api upstream.",
        ));
    }
    // Validate explicit targets against the configured origin first. Copool's
    // compatible usage paths live outside /backend-api on that same origin.
    let mut url = upstream_url(&b.origin().ascii_serialization(), target, false)?;
    if USAGE_ALIASES.contains(&url.path()) {
        url.set_path(USAGE_PATH);
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unified_fallback_preserves_account_and_probe_precedence() {
        let mut config = Config::parse("listen_port: 8787\nrequest_timeout_seconds: 3\ncodex:\n  homes: []\n  account_auth_file_only: false\n  routing:\n    fallback: none\nclaude:\n  config_dirs: []\n  account_auth_file_only: false\n  routing:\n    fallback: none\n").unwrap();
        let identity = Identity {
            account_id: "acct".into(),
            token: "access".into(),
            refresh_token: None,
            usernames: vec![],
        };
        let claude_identity = crate::claude::ClaudeIdentity {
            account_id: "acct".into(),
            usernames: vec![],
        };
        let choice = |name: &str| Choice::One(name.into());
        assert_eq!(
            config.resolve_refresh("new", None).await.unwrap().0.label(),
            "none"
        );
        assert_eq!(
            config
                .claude
                .resolve_refresh("new", None)
                .await
                .unwrap()
                .0
                .label(),
            "none"
        );
        assert_eq!(
            config.account_choice(&identity, None).unwrap().label(),
            "none"
        );
        let mut headers = hyper::HeaderMap::new();
        headers.insert("authorization", "Bearer access".parse().unwrap());
        let route = config.claude.resolve(&headers).await.unwrap().account();
        assert!(route.needs_profile);
        assert_eq!(route.proxy.label(), "none");
        for routing in [&mut config.codex.routing, &mut config.claude.routing] {
            routing.account_fallback = Some(choice("account"));
            routing.account.insert("acct".into(), choice("mapped"));
        }
        config.claude.routing.account_probe = Some(choice("probe"));
        assert_eq!(
            config.resolve_refresh("new", None).await.unwrap().0.label(),
            "account"
        );
        assert_eq!(
            config
                .claude
                .resolve_refresh("new", None)
                .await
                .unwrap()
                .0
                .label(),
            "probe"
        );
        assert_eq!(
            config
                .resolve_refresh("known", Some(&identity))
                .await
                .unwrap()
                .0
                .label(),
            "mapped"
        );
        assert_eq!(
            config
                .claude
                .resolve_refresh("known", Some(&claude_identity))
                .await
                .unwrap()
                .0
                .label(),
            "mapped"
        );
        assert_eq!(
            config
                .claude
                .resolve(&headers)
                .await
                .unwrap()
                .proxy()
                .label(),
            "probe"
        );
        let mut route = config.claude.resolve(&headers).await.unwrap().account();
        config
            .claude
            .apply_profile(&mut route, claude_identity.clone())
            .unwrap();
        assert_eq!(route.proxy.label(), "mapped");
        config.codex.routing.account.clear();
        config.claude.routing.account.clear();
        assert_eq!(
            config.account_choice(&identity, None).unwrap().label(),
            "account"
        );
        config
            .claude
            .apply_profile(&mut route, claude_identity.clone())
            .unwrap();
        assert_eq!(route.proxy.label(), "account");
        config.claude.routing.account_fallback = None;
        config
            .claude
            .apply_profile(&mut route, claude_identity.clone())
            .unwrap();
        assert_eq!(route.proxy.label(), "none");
        // An account override does not redirect API key traffic.
        assert_eq!(
            config
                .resolve(Some("Bearer api-key"), true)
                .await
                .unwrap()
                .proxy()
                .label(),
            "none"
        );
        headers.remove("authorization");
        headers.insert("x-api-key", "api-key".parse().unwrap());
        assert_eq!(
            config
                .claude
                .resolve(&headers)
                .await
                .unwrap()
                .proxy()
                .label(),
            "none"
        );
        let account_token = "h.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjdC0xIn19.s";
        assert_eq!(
            config
                .resolve(Some(&format!("Bearer {account_token}")), true)
                .await
                .unwrap()
                .proxy()
                .label(),
            "account"
        );
        config.codex.account_auth_file_only = true;
        assert!(
            config
                .resolve(Some(&format!("Bearer {account_token}")), true)
                .await
                .is_err()
        );
        config.claude.account_auth_file_only = true;
        assert!(
            config
                .resolve_refresh("known", Some(&identity))
                .await
                .is_err()
        );
        assert!(
            config
                .claude
                .resolve_refresh("known", Some(&claude_identity))
                .await
                .is_err()
        );
        headers.remove("x-api-key");
        headers.insert("authorization", "Bearer access".parse().unwrap());
        assert!(config.claude.resolve(&headers).await.is_err());
    }
    #[tokio::test]
    async fn explicit_url_routes_need_auth_but_no_local_key_source_or_fallback() {
        let config = Config::parse("listen_port: 7889\nrequest_timeout_seconds: 3\ncodex:\n  routing:\n    api_key:\n      'api.invalid/v1': none\n").unwrap();
        config.check_credentials().await.unwrap();
        assert!(config.codex.providers().next().is_none());
        for target in [
            "/https://api.invalid/v1/responses",
            "/codex/https://api.invalid/v1/responses",
        ] {
            let classified = target::Target::new("POST", target);
            let target = classified.rest();
            let route = config
                .resolve_url(Some("Bearer supplied-key"), target)
                .unwrap()
                .unwrap()
                .api();
            assert!(route.custom_upstream);
            assert_eq!(route.proxy.label(), "none");
            assert_eq!(route.upstream, "https://api.invalid/v1");
            assert!(route.ext.is_none());
            assert_eq!(config.resolve_url(None, target).err().unwrap().status, 401);
        }
        assert!(
            config
                .resolve_url(Some("Bearer key"), "/https://other.invalid/v1/responses")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn paths_and_origin_boundaries() {
        let b = "https://chatgpt.com/backend-api";
        for t in [
            "/responses",
            "/v1/responses",
            "/backend-api/codex/responses",
        ] {
            assert_eq!(
                upstream_url(b, t, true).unwrap().as_str(),
                "https://chatgpt.com/backend-api/codex/responses"
            );
        }
        assert_eq!(
            upstream_url(b, "/backend-api/ps/plugins/installed", true)
                .unwrap()
                .path(),
            "/backend-api/ps/plugins/installed"
        );
        assert_eq!(
            query_url(b, "/backend-api/wham/usage?x=1")
                .unwrap()
                .as_str(),
            "https://chatgpt.com/backend-api/wham/usage?x=1"
        );
        for (t, method) in [
            ("/backend-api/wham/usage", "GET"),
            ("/backend-api/wham/profiles/me", "GET"),
            ("/backend-api/wham/rate-limit-reset-credits", "GET"),
            ("/backend-api/wham/rate-limit-reset-credits/consume", "POST"),
        ] {
            assert_eq!(account_query(t), Some(method), "{t}");
            assert_eq!(
                query_url(b, t).unwrap().as_str(),
                format!("https://chatgpt.com{t}")
            );
        }
        assert_eq!(
            account_query("/https://chatgpt.com/backend-api/wham/profiles/me"),
            Some("GET")
        );
        assert!(account_query("/backend-api/wham/profiles/other").is_none());
        for alias in USAGE_ALIASES {
            assert_eq!(account_query(alias), Some("GET"));
            assert_eq!(account_query_for_route(alias, false), None);
            for target in [
                format!("{alias}?x=a%2Fb"),
                format!("/https://chatgpt.com{alias}?x=a%2Fb"),
            ] {
                assert_eq!(
                    query_url(b, &target).unwrap().as_str(),
                    "https://chatgpt.com/backend-api/wham/usage?x=a%2Fb"
                );
            }
            for origin in [
                "https://other.invalid",
                "http://chatgpt.com",
                "https://chatgpt.com:8443",
            ] {
                assert!(query_url(b, &format!("/{origin}{alias}")).is_err());
            }
            assert!(account_query(&format!("{alias}/other")).is_none());
        }
        assert!(query_url("https://api.openai.com/v1", "/backend-api/wham/usage").is_err());
        for t in [
            "//evil.com",
            "/https://evil.com/backend-api/a",
            "/https://chatgpt.com/backend-api-evil/a",
            "/%2e%2e/secret",
            "/x%5cy",
            "/responses#secret",
        ] {
            assert!(upstream_url(b, t, true).is_err(), "{t}");
        }
    }

    #[test]
    fn token_refresh_targets_and_bodies() {
        for t in ["/oauth/token", "/https://auth.openai.com/oauth/token"] {
            assert!(token_refresh(t), "{t}");
        }
        for t in [
            "/oauth/token?x=1",
            "/https://evil.com/oauth/token",
            "/https://auth.openai.com/oauth/authorize",
        ] {
            assert!(!token_refresh(t), "{t}");
        }
        assert_eq!(
            refresh_token(
                br#"{"client_id":"c","grant_type":"refresh_token","refresh_token":"rt"}"#
            )
            .as_deref(),
            Some("rt")
        );
        assert_eq!(
            refresh_token(b"grant_type=refresh_token&refresh_token=rt%2B1").as_deref(),
            Some("rt+1")
        );
        for body in [&b"{}"[..], b"{\"refresh_token\":\"a b\"}", b""] {
            assert!(refresh_token(body).is_none());
        }
    }
}
