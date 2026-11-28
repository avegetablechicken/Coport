//! Codex's Saved login: `auth.json` in each configured Codex home, renewed
//! at OpenAI's token endpoint. Its account is in the token's own claims, so
//! a renewal learns nothing.
use super::*;

pub(crate) struct Codex;

impl SavedLogin for Codex {
    const APP: Service = Service::Codex;
    type Client = ();

    fn sources(config: &Config) -> Vec<Option<AccountSource>> {
        let sources = config.codex.account_sources();
        sources
            .into_iter()
            .map(|(_, source)| Some(source))
            .collect()
    }
    async fn load(source: Option<&AccountSource>, _: bool) -> Result<(Store, Value)> {
        in_directory(source, crate::codex::saved_auth_store).await
    }
    fn access(login: &Value) -> Option<&str> {
        login["tokens"]["access_token"].as_str()
    }
    fn refresh(login: &Value) -> Option<&str> {
        login["tokens"]["refresh_token"].as_str()
    }
    async fn client() -> Result<()> {
        Ok(())
    }

    fn request(login: &Value, refresh: &str, (): &()) -> Result<Refresh> {
        let claims = crate::identity::claims(login["tokens"]["id_token"].as_str().unwrap_or(""));
        if claims["iss"]
            .as_str()
            .is_some_and(|s| s.trim_end_matches('/') != "https://auth.openai.com")
        {
            return Err(Error::config("Unsupported OAuth issuer."));
        }
        let client = claims["aud"]
            .as_str()
            .or_else(|| claims["aud"].as_array()?.iter().find_map(Value::as_str))
            .filter(|s| crate::identity::valid_token(s))
            .unwrap_or("app_EMoamEEZ73f0CkXaXp7hrann");
        Ok(Refresh {
            url: crate::codex::routes::TOKEN_REFRESH_UPSTREAM,
            content_type: "application/x-www-form-urlencoded",
            body: url::form_urlencoded::Serializer::new(String::new())
                .append_pair("grant_type", "refresh_token")
                .append_pair("refresh_token", refresh)
                .append_pair("client_id", client)
                .finish(),
        })
    }

    fn merge(login: &Value, access: &str, payload: &Value) -> Result<Value> {
        // Do not allow an unexpected response to switch the routed account.
        if crate::codex::Identity::from_token(access).is_some_and(|id| {
            Some(id.account_id.as_str()) != login["tokens"]["account_id"].as_str()
        }) {
            return Err(Error::config("OAuth refresh changed the account."));
        }
        let mut updated = login.clone();
        updated["tokens"]["access_token"] = access.into();
        for field in ["refresh_token", "id_token"] {
            if payload[field].is_string() {
                updated["tokens"][field] = payload[field].clone();
            }
        }
        updated["last_refresh"] = now_rfc3339().into();
        Ok(updated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use serde_json::json;

    fn jwt(claims: Value) -> String {
        format!("e30.{}.sig", URL_SAFE_NO_PAD.encode(claims.to_string()))
    }
    fn login(id_token: &str) -> Value {
        json!({"tokens": {"account_id": "acct", "access_token": "old", "refresh_token": "spent", "id_token": id_token}, "other": true})
    }

    #[test]
    fn a_renewal_spends_the_refresh_token_for_the_logins_own_client() {
        let saved = login(&jwt(
            json!({"iss": "https://auth.openai.com/", "aud": ["app_own"]}),
        ));
        assert_eq!(refresh::<Codex>(&saved), Some("spent"));
        let sent = request::<Codex>(&saved, &()).unwrap();
        assert_eq!(
            sent.url().as_str(),
            crate::codex::routes::TOKEN_REFRESH_UPSTREAM
        );
        assert_eq!(
            sent.headers()["content-type"],
            "application/x-www-form-urlencoded"
        );
        let body = std::str::from_utf8(sent.body().unwrap().as_bytes().unwrap()).unwrap();
        assert_eq!(
            body,
            "grant_type=refresh_token&refresh_token=spent&client_id=app_own"
        );
        // Codex's own client when the ID token names none; another issuer's
        // login is not renewed.
        let sent = request::<Codex>(&login(""), &()).unwrap();
        let body = std::str::from_utf8(sent.body().unwrap().as_bytes().unwrap()).unwrap();
        assert!(body.ends_with("client_id=app_EMoamEEZ73f0CkXaXp7hrann"));
        let other = login(&jwt(json!({"iss": "https://other.example"})));
        assert!(request::<Codex>(&other, &()).is_err());
    }

    #[test]
    fn a_renewal_keeps_the_rest_of_the_login_and_never_switches_its_account() {
        let saved = login("");
        let token = jwt(json!({"https://api.openai.com/auth": {"chatgpt_account_id": "acct"}}));
        let updated = merge::<Codex>(
            &saved,
            &json!({"access_token": token, "refresh_token": "next"}),
        )
        .unwrap();
        assert_eq!(access::<Codex>(&updated), Some(token.as_str()));
        assert_eq!(refresh::<Codex>(&updated), Some("next"));
        assert_eq!(updated["tokens"]["id_token"], "");
        assert_eq!(updated["other"], true);
        assert!(updated["last_refresh"].is_string());
        let other = jwt(json!({"https://api.openai.com/auth": {"chatgpt_account_id": "other"}}));
        assert!(merge::<Codex>(&saved, &json!({"access_token": other})).is_err());
    }
}
