//! How Codex's requests route: by the ChatGPT login a Bearer token is, a
//! provider's key or a URL route; its Token refresh by the refresh token;
//! and the ChatGPT account endpoints and docs MCP only a login reaches.
use super::Identity;
use crate::{
    Error, Result,
    config::{Choice, Config},
    identity::valid_token,
    routing::{
        accounts,
        api_keys::{self, Read},
        by_url, upstream_url,
    },
};
use url::Url;

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

impl super::Codex {
    /// The Codex provider IDs and API Key variables `routing.api_key` names.
    pub fn providers(&self) -> impl Iterator<Item = super::Provider> + '_ {
        self.api_keys()
            .named()
            .map(|(selector, proxy)| super::Provider {
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
                let provider = super::Provider {
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
    async fn explicit_url_routes_need_auth_but_no_local_key_source_or_fallback() {
        let config = Config::parse("listen_port: 7889\nrequest_timeout_seconds: 3\ncodex:\n  routing:\n    api_key:\n      'api.invalid/v1': none\n").unwrap();
        config.check_credentials().await.unwrap();
        assert!(config.codex.providers().next().is_none());
        for target in [
            "/https://api.invalid/v1/responses",
            "/codex/https://api.invalid/v1/responses",
        ] {
            let classified = crate::routing::target::Target::new("POST", target);
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
}
