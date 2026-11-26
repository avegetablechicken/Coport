//! Saved logins: where each App keeps its OAuth login, how Coport reads,
//! renews and writes it back, and which account a renewal names. Each App
//! implements `SavedLogin`; renewal itself, its locks, pauses and rechecks,
//! are the server's and the same for every App. A saved login keeps its
//! storage backend on rotation.
use crate::{Error, Result, config::AccountSource, config::Config, routing::Service};
use serde_json::Value;
use std::{path::PathBuf, time::Duration};

mod antigravity;
mod claude;
mod codex;
mod grok;
pub(crate) use {antigravity::Antigravity, claude::Claude, codex::Codex, grok::Grok};

/// Runs `$body` with `$L` the `SavedLogin` of `$app`.
macro_rules! with_login {
    ($app:expr, $L:ident => $body:expr) => {
        match $app {
            $crate::routing::Service::Codex => {
                type $L = $crate::saved_login::Codex;
                $body
            }
            $crate::routing::Service::Claude => {
                type $L = $crate::saved_login::Claude;
                $body
            }
            $crate::routing::Service::Antigravity => {
                type $L = $crate::saved_login::Antigravity;
                $body
            }
            $crate::routing::Service::Grok => {
                type $L = $crate::saved_login::Grok;
                $body
            }
        }
    };
}
pub(crate) use with_login;

#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) enum Store {
    File(PathBuf),
    Keychain(String, String),
    /// An entry a Go program saved with go-keyring, holding a login document
    /// in Antigravity's form.
    GoKeyring(String, String),
}
impl Store {
    pub(crate) fn read_file(&self) -> Result<Value> {
        let Self::File(path) = self else {
            unreachable!()
        };
        std::fs::read(path)
            .ok()
            .and_then(|raw| serde_json::from_slice(&raw).ok())
            .ok_or(Error::config("Cannot read saved OAuth credentials."))
    }
    pub(crate) async fn replace(&self, expected: &Value, updated: &Value) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(updated)
            .map_err(|_| Error::config("Invalid OAuth credentials."))?;
        match self {
            Self::File(path) => {
                use std::io::Write;
                // Follow an existing symlink rather than replacing it. Never create
                // a new login location after an external logout or account switch.
                let path = path
                    .canonicalize()
                    .map_err(|_| Error::config("Saved login disappeared."))?;
                let write = || -> std::io::Result<()> {
                    let mut temp = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
                    temp.write_all(&bytes)?;
                    temp.as_file().sync_all()?;
                    if self.read_file().ok().as_ref() != Some(expected) {
                        return Err(std::io::Error::other("Saved login changed"));
                    }
                    temp.persist(&path).map_err(|e| e.error)?;
                    Ok(())
                };
                write().map_err(|_| {
                    Error::config("Cannot save refreshed login; login may have changed.")
                })
            }
            Self::Keychain(service, account) => {
                crate::keychain::replace(
                    service,
                    account,
                    expected.clone(),
                    String::from_utf8(bytes).unwrap(),
                )
                .await
            }
            Self::GoKeyring(service, account) => {
                let (expected, updated) = (expected.clone(), updated.clone());
                // Keep the entry's form: wrapped as it was, or plain JSON.
                crate::keychain::replace_go_keyring(service, account, move |raw| {
                    let (current, wrapped) = crate::antigravity::decode_item(raw)?;
                    (current == expected)
                        .then(|| crate::antigravity::encode_item(&updated, wrapped))
                })
                .await
            }
        }
    }
}

/// A Token refresh request for a saved login.
pub(crate) struct Refresh {
    pub url: &'static str,
    pub content_type: &'static str,
    pub body: String,
}

/// An App's Saved login: where it is kept, its tokens, and the Token refresh
/// that renews it. An App Coport does not renew lists no `sources`, so none
/// of its logins is ever renewed.
pub(crate) trait SavedLogin {
    const APP: Service;
    /// What a renewal prepares before it takes the login's lock.
    type Client: Send + Sync;
    /// Where its Saved logins are kept: each configured directory, or `None`
    /// for an App's one credential store item.
    fn sources(config: &Config) -> Vec<Option<AccountSource>>;
    /// The login kept at `source`. `recent` lets a credential store item
    /// answer from a read at most 30 seconds old, as its requests are
    /// matched with.
    async fn load(source: Option<&AccountSource>, recent: bool) -> Result<(Store, Value)>;
    fn access(login: &Value) -> Option<&str>;
    fn refresh(login: &Value) -> Option<&str>;
    async fn client() -> Result<Self::Client>;
    /// A lock other programs that renew the login at `source` also take,
    /// waited for at most `wait`: the renewal's time left, an error once it
    /// ran out. None by default.
    async fn lock(
        _source: Option<&AccountSource>,
        _wait: Result<Duration>,
    ) -> Result<Option<crate::claude_refresh_lock::RefreshLock>> {
        Ok(None)
    }
    /// The Token refresh spending `refresh`, the login's refresh token.
    fn request(login: &Value, refresh: &str, client: &Self::Client) -> Result<Refresh>;
    /// `login` with the tokens of a renewal's `payload`, whose `access`
    /// token and other tokens are already checked.
    fn merge(login: &Value, access: &str, payload: &Value) -> Result<Value>;
    /// Learn the account of the refresh token `updated` keeps, after a
    /// renewal that spent `spent`. `routed` is the account the renewed
    /// request was routed to, where Claude's routing knew it. Nothing by
    /// default.
    fn learn(
        _server: &crate::server::Server,
        _spent: &str,
        _updated: &Value,
        _payload: &Value,
        _routed: Option<&crate::claude::ClaudeIdentity>,
    ) {
    }
}

/// `L`'s access token in `login`, if it is a valid token.
pub(crate) fn access<L: SavedLogin>(login: &Value) -> Option<&str> {
    L::access(login).filter(|s| crate::identity::valid_token(s))
}
/// `L`'s refresh token in `login`, if it is a valid token.
pub(crate) fn refresh<L: SavedLogin>(login: &Value) -> Option<&str> {
    L::refresh(login).filter(|s| crate::identity::valid_token(s))
}

/// The Token refresh request renewing `login`.
pub(crate) fn request<L: SavedLogin>(
    login: &Value,
    client: &L::Client,
) -> Result<reqwest::Request> {
    let refresh = refresh::<L>(login).ok_or(Error::config("Login has no refresh token."))?;
    let Refresh {
        url,
        content_type,
        body,
    } = L::request(login, refresh, client)?;
    let mut request =
        reqwest::Request::new(reqwest::Method::POST, reqwest::Url::parse(url).unwrap());
    request
        .headers_mut()
        .insert("content-type", content_type.parse().unwrap());
    *request.body_mut() = Some(body.into());
    Ok(request)
}

/// `login` renewed with a Token refresh response's `payload`, if its tokens
/// are valid.
pub(crate) fn merge<L: SavedLogin>(login: &Value, payload: &Value) -> Result<Value> {
    let access = payload["access_token"]
        .as_str()
        .filter(|s| crate::identity::valid_token(s))
        .ok_or(Error::config(
            "OAuth refresh returned no valid access token.",
        ))?;
    for field in ["refresh_token", "id_token"] {
        if !payload[field].is_null()
            && !payload[field]
                .as_str()
                .is_some_and(crate::identity::valid_token)
        {
            return Err(Error::config("OAuth refresh returned an invalid token."));
        }
    }
    L::merge(login, access, payload)
}

/// The login kept in a configured directory, read by `read`; an
/// environment source cannot be renewed.
async fn in_directory<'a, F>(
    source: Option<&'a AccountSource>,
    read: impl FnOnce(&'a std::path::Path) -> F,
) -> Result<(Store, Value)>
where
    F: std::future::Future<Output = Result<(Store, Value)>>,
{
    match source {
        Some(AccountSource::Directory(dir)) => read(dir).await,
        Some(AccountSource::Env(_)) => Err(Error::config(
            "Environment credentials cannot be refreshed.",
        )),
        None => Err(Error::config("No saved login to refresh.")),
    }
}

/// Now as an RFC 3339 timestamp, as Codex records its renewals.
fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap()
}
