//! Claude's Saved login: `.credentials.json` (or the keychain item) of each
//! configured Claude directory, renewed under Claude Code's own refresh lock
//! so one refresh token is spent only once.
use super::*;
use crate::claude::ClaudeIdentity;
use crate::claude_refresh_lock::{RefreshLock, Wait};

pub(crate) struct Claude;

/// How long a model request waits for another program's Claude renewal.
const LOCK_WAIT: Duration = if cfg!(test) {
    Duration::from_millis(600)
} else {
    Duration::from_secs(10)
};

impl SavedLogin for Claude {
    const APP: Service = Service::Claude;
    type Client = ();

    fn sources(config: &Config) -> Vec<Option<AccountSource>> {
        let sources = config.claude.account_sources();
        sources
            .into_iter()
            .map(|(_, source)| Some(source))
            .collect()
    }
    async fn load(source: Option<&AccountSource>, _: bool) -> Result<(Store, Value)> {
        in_directory(source, crate::claude::saved_credentials_store).await
    }
    fn access(login: &Value) -> Option<&str> {
        login["claudeAiOauth"]["accessToken"].as_str()
    }
    fn refresh(login: &Value) -> Option<&str> {
        login["claudeAiOauth"]["refreshToken"].as_str()
    }
    async fn client() -> Result<()> {
        Ok(())
    }

    /// Claude Code's refresh lock for a login saved in a configuration
    /// directory. Taken only for renewals this server starts: a refresh it
    /// relays may come from Claude Code holding this very lock.
    async fn lock(
        source: Option<&AccountSource>,
        wait: Result<Duration>,
    ) -> Result<Option<RefreshLock>> {
        let Some(AccountSource::Directory(dir)) = source else {
            return Ok(None);
        };
        let dir = dir.clone();
        let wait = Wait::up_to(LOCK_WAIT.min(wait?));
        match tokio::task::spawn_blocking(move || RefreshLock::acquire(&dir, wait)).await {
            Ok(Ok(Some(lock))) => Ok(Some(lock)),
            Ok(Ok(None)) => Err(Error::config("Another program is renewing this login.")),
            _ => Err(Error::config("Cannot take the login refresh lock.")),
        }
    }

    fn request(login: &Value, refresh: &str, (): &()) -> Result<Refresh> {
        let scopes: Vec<_> = login["claudeAiOauth"]["scopes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .collect();
        let scope = if scopes.is_empty() {
            "user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload".into()
        } else {
            scopes.join(" ")
        };
        Ok(Refresh {
            url: crate::claude::TOKEN_REFRESH_UPSTREAM,
            content_type: "application/json",
            body: serde_json::json!({"grant_type":"refresh_token", "refresh_token":refresh, "client_id":"9d1c250a-e61b-44d9-88ed-5944d1962f5e", "scope":scope}).to_string(),
        })
    }

    fn merge(login: &Value, access: &str, payload: &Value) -> Result<Value> {
        let mut updated = login.clone();
        let oauth = &mut updated["claudeAiOauth"];
        oauth["accessToken"] = access.into();
        if payload["refresh_token"].is_string() {
            oauth["refreshToken"] = payload["refresh_token"].clone();
        }
        for (field, saved) in [
            ("expires_in", "expiresAt"),
            ("refresh_token_expires_in", "refreshTokenExpiresAt"),
        ] {
            if let Some(seconds) = payload[field].as_u64() {
                let now = time::OffsetDateTime::now_utc().unix_timestamp().max(0) as u64;
                oauth[saved] = now.saturating_add(seconds).saturating_mul(1000).into();
            }
        }
        if let Some(scope) = payload["scope"].as_str() {
            oauth["scopes"] = scope.split_whitespace().collect::<Vec<_>>().into();
        }
        Ok(updated)
    }

    /// The token response names the account, else the request's route did,
    /// else the refresh token it spent was learned for one.
    fn learn(
        server: &crate::server::Server,
        spent: &str,
        updated: &Value,
        payload: &Value,
        routed: Option<&ClaudeIdentity>,
    ) {
        let Some(token) = refresh::<Self>(updated) else {
            return;
        };
        let routes = server.claude_refresh_routes();
        let identity = ClaudeIdentity::token_response(payload)
            .or_else(|| routed.cloned())
            .or_else(|| routes.identity(spent));
        if let Some(identity) = identity {
            if let Err(error) = routes.learn(token, &identity) {
                server.logger.write(
                    "refresh_route_unsaved",
                    [("reason".to_owned(), error.to_string().into())]
                        .into_iter()
                        .collect(),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_renewal_asks_for_the_scopes_the_login_holds() {
        let body_of = |login: Value| {
            let sent = request::<Claude>(&login, &()).unwrap();
            assert_eq!(sent.url().as_str(), crate::claude::TOKEN_REFRESH_UPSTREAM);
            assert_eq!(sent.headers()["content-type"], "application/json");
            serde_json::from_slice::<Value>(sent.body().unwrap().as_bytes().unwrap()).unwrap()
        };
        let body = body_of(
            json!({"claudeAiOauth": {"refreshToken": "spent", "scopes": ["user:profile", " "]}}),
        );
        assert_eq!(body["refresh_token"], "spent");
        assert_eq!(body["scope"], "user:profile");
        let body = body_of(json!({"claudeAiOauth": {"refreshToken": "spent"}}));
        assert!(body["scope"].as_str().unwrap().contains("user:inference"));
        assert!(request::<Claude>(&json!({"claudeAiOauth": {}}), &()).is_err());
    }

    #[test]
    fn renewal_preserves_optional_tokens_and_updates_both_claude_expiries() {
        let original =
            json!({"claudeAiOauth":{"accessToken":"old", "refreshToken":"keep", "other":true}});
        let updated = merge::<Claude>(&original, &json!({"access_token":"new", "expires_in":60, "refresh_token_expires_in":120, "scope":"user:profile user:inference"})).unwrap();
        let oauth = &updated["claudeAiOauth"];
        assert_eq!(oauth["refreshToken"], "keep");
        assert_eq!(oauth["other"], true);
        assert_eq!(
            oauth["refreshTokenExpiresAt"].as_u64().unwrap() - oauth["expiresAt"].as_u64().unwrap(),
            60_000
        );
        assert_eq!(oauth["scopes"], json!(["user:profile", "user:inference"]));
        for payload in [
            json!({}),
            json!({"access_token":"bad token"}),
            json!({"access_token":"new", "refresh_token":42}),
        ] {
            assert!(merge::<Claude>(&original, &payload).is_err());
        }
    }
}
