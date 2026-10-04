use crate::{
    Error, Result,
    config::{Choice, Config},
    identity::{Identity, valid_token},
};
use url::Url;
#[derive(Clone)]
pub struct Route {
    pub token: String,
    pub account_id: Option<String>,
    pub account_label: Option<String>,
    pub provider: Option<String>,
    pub proxy: Choice,
    pub upstream: String,
    pub custom_upstream: bool,
}
pub fn codex_target(target: &str) -> &str {
    target
        .strip_prefix("/codex")
        .filter(|rest| rest.starts_with("/https://"))
        .unwrap_or(target)
}
impl Config {
    pub fn resolve_url(&self, authorization: Option<&str>, target: &str) -> Result<Option<Route>> {
        let Some((base, proxy)) =
            crate::url_routing::match_route(&self.codex.routing.api_key, codex_target(target))?
        else {
            return Ok(None);
        };
        let token = authorization
            .and_then(|s| s.strip_prefix("Bearer "))
            .filter(|s| valid_token(s))
            .ok_or(Error::new(401, "An API Bearer token is required."))?;
        Ok(Some(Route {
            token: token.into(),
            account_id: None,
            account_label: None,
            provider: Some(base.into()),
            proxy: proxy.clone(),
            upstream: crate::url_routing::validate_upstream(base)?.into(),
            custom_upstream: true,
        }))
    }

    pub async fn resolve(&self, authorization: Option<&str>, fallback: bool) -> Result<Route> {
        let token = authorization
            .and_then(|s| s.strip_prefix("Bearer "))
            .filter(|s| valid_token(s))
            .ok_or(Error::new(401, "A configured Bearer token is required."))?;
        let mut unavailable = false;
        let mut identities = Vec::new();
        let account_sources = self.codex.account_sources();
        for (label, source, _) in &account_sources {
            match self.codex.account_identity(source).await {
                Ok(i) if i.token == token => identities.push((i, Some(label.as_str()))),
                Ok(_) => {}
                Err(_) => unavailable = true,
            }
        }
        if identities.is_empty()
            && !self.codex.account_auth_file_only
            && (!self.codex.routing.account.is_empty()
                || self.codex.routing.account_fallback.is_some())
        {
            if let Some(i) = Identity::from_token(token) {
                identities.push((i, None));
            }
        }
        let mut matches = Vec::new();
        for p in &self.codex.providers {
            match p
                .credentials(
                    &self.codex.base_url.api_key,
                    identities.is_empty(),
                    &self.codex,
                )
                .await
            {
                Ok(credentials) => {
                    for credential in credentials.into_iter().filter(|c| c.token == token) {
                        // A provider with its own base_url is a third-party API, like
                        // an explicit URL route: use native TLS for compatibility with
                        // certificates rustls rejects (e.g. a self-signed CA as leaf).
                        let custom_upstream = credential.upstream != self.codex.base_url.api_key;
                        matches.push(Route {
                            token: credential.token,
                            account_id: credential.account_id,
                            account_label: None,
                            provider: Some(p.label().into()),
                            proxy: p.proxy.clone(),
                            upstream: credential.upstream,
                            custom_upstream,
                        });
                    }
                }
                Err(_) => unavailable = true,
            }
        }
        if matches.len() + identities.len() > 1 {
            return Err(Error::new(
                409,
                "Bearer token matches multiple routes; configure distinct credentials.",
            ));
        }
        if let Some((i, source)) = identities.pop() {
            let proxy = self.account_choice(&i, source)?;
            let account_label = crate::identity::routing_account_label(
                &self.codex.routing,
                &i.account_id,
                &i.usernames,
                source.unwrap_or(""),
            );
            return Ok(Route {
                token: i.token,
                account_id: Some(i.account_id),
                account_label,
                provider: None,
                proxy,
                upstream: self.codex.base_url.account.clone(),
                custom_upstream: false,
            });
        }
        if let Some(r) = matches.pop() {
            return Ok(r);
        }
        if fallback {
            if let Some(proxy) = &self.codex.routing.api_key_fallback {
                return Ok(Route {
                    token: token.into(),
                    account_id: None,
                    account_label: None,
                    provider: Some("openai-fallback".into()),
                    proxy: proxy.clone(),
                    upstream: self.codex.base_url.api_key.clone(),
                    custom_upstream: false,
                });
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
    /// Refresh requests carry no Bearer token; the refresh token selects the
    /// saved login whose proxy is used. Nothing is injected upstream.
    pub async fn resolve_refresh(&self, refresh_token: &str) -> Result<(Choice, Option<String>)> {
        let mut unavailable = false;
        for (label, source, _) in &self.codex.account_sources() {
            match self.codex.account_identity(source).await {
                Ok(i) if i.refresh_token.as_deref() == Some(refresh_token) => {
                    let proxy = self.account_choice(&i, Some(label))?;
                    return Ok((proxy, Some(i.account_id)));
                }
                Ok(_) => {}
                Err(_) => unavailable = true,
            }
        }
        if !self.codex.account_auth_file_only {
            if let Some(proxy) = &self.codex.routing.account_fallback {
                return Ok((proxy.clone(), None));
            }
        }
        if unavailable {
            Err(Error::config(
                "No matching refresh route; one or more credential sources are unavailable.",
            ))
        } else {
            Err(Error::new(
                403,
                "Token refresh requires a refresh token saved in a configured Codex auth.json.",
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
    let path = if target.starts_with("/https://") || target.starts_with("/http://") {
        Url::parse(&target[1..])
            .ok()
            .map(|u| u.path().to_string())
            .unwrap_or_default()
    } else {
        target.split('?').next().unwrap_or("").to_string()
    };
    match path.as_str() {
        "/backend-api/wham/usage"
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
    upstream_url(base, target, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn explicit_url_routes_need_auth_but_no_local_key_source_or_fallback() {
        let config = Config::parse("listen_port: 7889\nrequest_timeout_seconds: 3\ncodex:\n  routing:\n    api_key:\n      'api.invalid/v1': none\n").unwrap();
        config.check_credentials().await.unwrap();
        assert!(config.codex.providers.is_empty());
        for target in [
            "/https://api.invalid/v1/responses",
            "/codex/https://api.invalid/v1/responses",
        ] {
            let route = config
                .resolve_url(Some("Bearer supplied-key"), target)
                .unwrap()
                .unwrap();
            assert!(route.custom_upstream);
            assert_eq!(route.proxy.label(), "none");
            assert_eq!(route.upstream, "https://api.invalid/v1");
            assert!(route.account_id.is_none());
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
