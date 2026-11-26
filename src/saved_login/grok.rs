//! Grok's Saved login is renewed by Grok Build itself, which rewrites its
//! `auth.json`; Coport relays that Token refresh and learns its account.
//! Grok lists no `sources`, so Coport renews none of its logins; the rest
//! stands ready for when it does.
use super::*;

pub(crate) struct Grok;

const CLIENT_RENEWS: &str = "Grok renewal is managed by the client.";

impl SavedLogin for Grok {
    const APP: Service = Service::Grok;
    type Client = ();

    fn sources(_: &Config) -> Vec<Option<AccountSource>> {
        Vec::new()
    }
    async fn load(_: Option<&AccountSource>, _: bool) -> Result<(Store, Value)> {
        Err(Error::config(CLIENT_RENEWS))
    }
    fn access(_: &Value) -> Option<&str> {
        None
    }
    fn refresh(_: &Value) -> Option<&str> {
        None
    }
    async fn client() -> Result<()> {
        Ok(())
    }
    fn request(_: &Value, _: &str, (): &()) -> Result<Refresh> {
        Err(Error::config(CLIENT_RENEWS))
    }
    fn merge(_: &Value, _: &str, _: &Value) -> Result<Value> {
        Err(Error::config(CLIENT_RENEWS))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn coport_renews_no_grok_login() {
        let config = Config::parse("listen_port: 7889\nrequest_timeout_seconds: 3\n").unwrap();
        assert!(Grok::sources(&config).is_empty());
        let saved = json!({"key": "access", "refresh_token": "refresh"});
        assert_eq!(
            (access::<Grok>(&saved), refresh::<Grok>(&saved)),
            (None, None)
        );
        assert!(Grok::load(None, true).await.is_err());
        assert!(request::<Grok>(&saved, &()).is_err());
        assert!(merge::<Grok>(&saved, &json!({"access_token": "new"})).is_err());
    }
}
