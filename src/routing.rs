use crate::{Error, Result, identity::valid_token};
use url::Url;

pub mod accounts;
pub mod api_keys;
pub mod by_url;
pub mod target;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex::{Identity, routes::token_refresh};
    use crate::config::{Choice, Config};

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
