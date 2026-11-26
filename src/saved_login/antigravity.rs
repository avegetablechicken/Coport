//! Antigravity's Saved login: its one credential store item, renewed with
//! the OAuth client Antigravity's own executable names. A renewal's ID token
//! names its account.
use super::*;
use crate::antigravity::{AntigravityIdentity, OAuthClient};

pub(crate) struct Antigravity;

impl SavedLogin for Antigravity {
    const APP: Service = Service::Antigravity;
    type Client = OAuthClient;

    fn sources(config: &Config) -> Vec<Option<AccountSource>> {
        if config.antigravity.saved_login {
            vec![None]
        } else {
            Vec::new()
        }
    }
    async fn load(source: Option<&AccountSource>, recent: bool) -> Result<(Store, Value)> {
        match source {
            None => crate::antigravity::saved_document(recent).await,
            Some(_) => Err(Error::config("No saved login to refresh.")),
        }
    }
    fn access(login: &Value) -> Option<&str> {
        crate::antigravity::access_token(login)
    }
    fn refresh(login: &Value) -> Option<&str> {
        crate::antigravity::refresh_token(login)
    }
    /// Found before the login's lock is taken: it may read agy's executable.
    async fn client() -> Result<OAuthClient> {
        crate::antigravity::oauth_client().await
    }

    fn request(_: &Value, refresh: &str, client: &OAuthClient) -> Result<Refresh> {
        Ok(Refresh {
            url: crate::antigravity::TOKEN_REFRESH_UPSTREAM,
            content_type: "application/x-www-form-urlencoded",
            body: crate::antigravity::refresh_form(refresh, client),
        })
    }

    fn merge(login: &Value, access: &str, payload: &Value) -> Result<Value> {
        let mut updated = login.clone();
        let lifetime = payload["expires_in"].as_u64().unwrap_or(3600);
        crate::antigravity::apply_renewal(
            &mut updated,
            access,
            lifetime,
            payload["refresh_token"].as_str(),
        )?;
        Ok(updated)
    }

    /// The ID token names the account. Google keeps the refresh token; should
    /// it issue another, the account learned for the old one carries over.
    fn learn(
        server: &crate::server::Server,
        spent: &str,
        updated: &Value,
        payload: &Value,
        _: Option<&crate::claude::ClaudeIdentity>,
    ) {
        let token = refresh::<Self>(updated).unwrap_or(spent);
        let identity = AntigravityIdentity::token_response(payload).or_else(|| {
            (token != spent)
                .then(|| server.antigravity_refresh_routes().identity(spent))
                .flatten()
        });
        if let Some(identity) = identity {
            server.learn_antigravity_login(token, &identity);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_renewal_uses_antigravitys_client_and_keeps_the_documents_form() {
        let saved = json!({"token": {"access_token": "old", "refresh_token": "spent"}, "other": 1});
        let client = OAuthClient {
            id: "id".into(),
            secret: "secret".into(),
        };
        let request = request::<Antigravity>(&saved, &client).unwrap();
        assert_eq!(
            request.url().as_str(),
            crate::antigravity::TOKEN_REFRESH_UPSTREAM
        );
        let body = std::str::from_utf8(request.body().unwrap().as_bytes().unwrap()).unwrap();
        assert_eq!(
            body,
            "client_id=id&client_secret=secret&grant_type=refresh_token&refresh_token=spent"
        );
        let updated = merge::<Antigravity>(&saved, &json!({"access_token": "new"})).unwrap();
        assert_eq!(access::<Antigravity>(&updated), Some("new"));
        // Google keeps the refresh token unless it issues another.
        assert_eq!(refresh::<Antigravity>(&updated), Some("spent"));
        assert_eq!(updated["other"], 1);
    }
}
