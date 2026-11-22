//! Antigravity forwarding: the Cloud Code requests of the Antigravity IDE and
//! CLI (`agy`), routed by the Google login they carry. As Copool reads it,
//! Antigravity keeps that login in one item of the OS credential store,
//! service `gemini` and account `antigravity`, saved by go-keyring: an OAuth
//! token as JSON (`{"token": {"access_token", "refresh_token", "expiry"}}`),
//! on macOS wrapped as `go-keyring-base64:`.
use crate::{
    Error, Result,
    config::{Choice, Config, Routing, validate_upstream},
    identity::valid_token,
    refresh_routes::RefreshRoutes,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use hyper::HeaderMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

/// The Cloud Code endpoint Antigravity answers from first.
pub const DEFAULT_BASE: &str = "https://daily-cloudcode-pa.googleapis.com";
/// Every Cloud Code origin Antigravity sends to. An explicit target may name
/// one of these or `base_url.account`, nothing else.
pub const CLOUD_CODE_ORIGINS: [&str; 2] = [
    "https://daily-cloudcode-pa.googleapis.com",
    "https://cloudcode-pa.googleapis.com",
];
/// Names the Google account of an access token.
pub const USERINFO_URL: &str = "https://www.googleapis.com/oauth2/v2/userinfo";
/// AI Code's feature flags, which agy fetches with its login as it starts.
pub const AICODE_FETCH_CONFIG: &str = "https://aicode.googleapis.com/v1alpha:fetchConfig";
/// The Google endpoints besides Cloud Code that agy calls with its login and
/// waits on as it starts: the account lookup of its eligibility check and AI
/// Code's flags. Its `USER_INFO_OVERRIDE_URL` and `AICODE_ENDPOINT_URL` may
/// name them in the `/antigravity` scope, where they take the login's route
/// as its Cloud Code requests do; other paths on their hosts are refused.
pub const LOGIN_ENDPOINTS: [&str; 2] = [USERINFO_URL, AICODE_FETCH_CONFIG];
/// Where Antigravity renews its login, as Codex's and Claude's token endpoints.
pub const TOKEN_REFRESH_UPSTREAM: &str = "https://oauth2.googleapis.com/token";
/// SHA-256 of Antigravity's OAuth client ID; the token endpoint serves every
/// Google client, and only this one's refreshes are routed.
const CLIENT_ID_SHA256: &str = "bf00c418024ba6bf606ccdc37120976e41bc429dd1d46ecf16a729aa532626ea";
/// SHA-256 of Antigravity's OAuth client secret, which Google requires of its
/// refreshes.
const CLIENT_SECRET_SHA256: &str =
    "1d2f041093fd95aa8995a038c711d50a7960da09a505381c09a745d6ad0ecc60";
/// The client tests stand in for Antigravity's.
#[cfg(test)]
pub(crate) const TEST_CLIENT_ID: &str = "test-client.apps.googleusercontent.com";
#[cfg(test)]
pub(crate) const TEST_CLIENT_SECRET: &str = "test-client-secret";
/// The credential store item of Antigravity's login.
const SERVICE: &str = "gemini";
const ACCOUNT: &str = "antigravity";
const WRAPPED: &str = "go-keyring-base64:";

/// The Gemini API `agy` sends API key requests to without
/// `GOOGLE_GEMINI_BASE_URL`.
pub const DEFAULT_API_BASE: &str = "https://generativelanguage.googleapis.com";

fn default_base() -> String {
    DEFAULT_BASE.into()
}
fn default_api_base() -> String {
    DEFAULT_API_BASE.into()
}

/// Antigravity's upstreams, written as Codex's `base_url`.
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Bases {
    /// Cloud Code root that a Google login's origin-form requests go to.
    #[serde(default = "default_base")]
    pub account: String,
    /// Gemini API root that API key requests go to when they name no
    /// upstream, as Codex's `base_url.api_key`.
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

#[derive(Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Antigravity {
    pub base_url: Bases,
    /// Read Antigravity's login from the OS credential store to match tokens.
    pub saved_login: bool,
    /// Reject tokens other than the saved login's instead of naming their
    /// account with a Google lookup.
    pub account_auth_file_only: bool,
    pub routing: Routing,
}
impl Default for Antigravity {
    fn default() -> Self {
        Self {
            base_url: Bases::default(),
            saved_login: true,
            account_auth_file_only: true,
            routing: Routing::default(),
        }
    }
}

/// The Google account a token signs in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AntigravityIdentity {
    pub email: String,
}
impl AntigravityIdentity {
    fn new(email: &str) -> Option<Self> {
        let email = email.trim();
        (email.contains('@') && email.len() <= 320 && !email.chars().any(char::is_control)).then(
            || Self {
                email: email.to_owned(),
            },
        )
    }
    /// The account a token response's ID token names.
    pub(crate) fn token_response(value: &Value) -> Option<Self> {
        let id_token = value["id_token"].as_str()?;
        crate::identity::claims(id_token)["email"]
            .as_str()
            .and_then(Self::new)
    }
    /// Google's user information response.
    pub(crate) fn userinfo(value: &Value) -> Result<Self> {
        value["email"]
            .as_str()
            .and_then(Self::new)
            .ok_or(Error::config(
                "Google user information is missing an email.",
            ))
    }
}

/// The parts of a saved login Coport uses.
#[derive(Debug, PartialEq)]
pub(crate) struct Login {
    pub access_token: String,
    /// Stable while Antigravity renews the access token; learned identities
    /// are kept by its digest.
    pub refresh_token: Option<String>,
    pub identity: Option<AntigravityIdentity>,
}
/// A credential store item's login; none when it holds no access token.
pub(crate) fn parse_login(raw: &str) -> Option<Login> {
    let (document, _) = decode_item(raw)?;
    let access_token = access_token(&document)?.to_owned();
    let refresh_token = refresh_token(&document).map(str::to_owned);
    // Copool records the email of its own sign-ins; Antigravity's do not.
    let identity = [
        "/id_token_email",
        "/email",
        "/account/email",
        "/user/email",
        "/token/email",
    ]
    .iter()
    .find_map(|path| document.pointer(path).and_then(Value::as_str))
    .and_then(AntigravityIdentity::new);
    Some(Login {
        access_token,
        refresh_token,
        identity,
    })
}

/// A credential store item's login document, and whether the item wraps it
/// as go-keyring does on macOS.
pub(crate) fn decode_item(raw: &str) -> Option<(Value, bool)> {
    let text = raw.trim().trim_start_matches('\u{feff}').trim();
    let (text, wrapped) = match text.strip_prefix(WRAPPED) {
        Some(encoded) => (
            String::from_utf8(STANDARD.decode(encoded.trim()).ok()?).ok()?,
            true,
        ),
        None => (text.to_owned(), false),
    };
    let document = serde_json::from_str(text.trim_start_matches('\u{feff}')).ok()?;
    Some((document, wrapped))
}
/// The item's secret for a login document, wrapped as `decode_item` found it.
pub(crate) fn encode_item(document: &Value, wrapped: bool) -> String {
    let json = document.to_string();
    if wrapped {
        format!("{WRAPPED}{}", STANDARD.encode(json))
    } else {
        json
    }
}

const ACCESS_KEYS: [&str; 2] = ["access_token", "accessToken"];
const REFRESH_KEYS: [&str; 2] = ["refresh_token", "refreshToken"];
const EXPIRY_KEYS: [&str; 3] = ["expiry", "expires_at", "expiresAt"];

/// Where a login document keeps its tokens: its `token` object, as Go's
/// oauth2 token in agy's item, else itself.
fn token(document: &Value) -> Option<&serde_json::Map<String, Value>> {
    let object = document.as_object()?;
    Some(
        object
            .get("token")
            .and_then(Value::as_object)
            .unwrap_or(object),
    )
}
fn token_field<'a>(document: &'a Value, keys: &[&str]) -> Option<&'a str> {
    let token = token(document)?;
    keys.iter()
        .find_map(|key| token.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .filter(|token| valid_token(token))
}
pub(crate) fn access_token(document: &Value) -> Option<&str> {
    token_field(document, &ACCESS_KEYS)
}
pub(crate) fn refresh_token(document: &Value) -> Option<&str> {
    token_field(document, &REFRESH_KEYS)
}

/// Puts a renewal's tokens in a login document under the keys and forms it
/// already uses, as Copool does: a numeric expiry keeps its unit, any other
/// becomes RFC 3339 in UTC, as Go writes it.
pub(crate) fn apply_renewal(
    document: &mut Value,
    access: &str,
    expires_in: u64,
    refresh: Option<&str>,
) -> Result<()> {
    let nested = document.get("token").is_some_and(Value::is_object);
    let object = document
        .as_object_mut()
        .ok_or(Error::config("Invalid Antigravity login."))?;
    let token = if nested {
        object["token"].as_object_mut().unwrap()
    } else {
        object
    };
    let key = |keys: [&'static str; 2], token: &serde_json::Map<String, Value>| {
        keys.into_iter()
            .find(|key| token.contains_key(*key))
            .unwrap_or(keys[0])
    };
    let access_key = key(ACCESS_KEYS, token);
    token.insert(access_key.into(), access.into());
    if let Some(refresh) = refresh {
        let refresh_key = key(REFRESH_KEYS, token);
        token.insert(refresh_key.into(), refresh.into());
    }
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let expires_at = now.saturating_add(expires_in.min(i64::MAX as u64) as i64);
    let expiry_key = EXPIRY_KEYS
        .into_iter()
        .find(|key| token.contains_key(*key))
        .unwrap_or(EXPIRY_KEYS[0]);
    let expiry = match token.get(expiry_key).and_then(Value::as_i64) {
        Some(n) if n > 100_000_000_000 => expires_at.saturating_mul(1000).into(),
        Some(_) => expires_at.into(),
        None => time::OffsetDateTime::from_unix_timestamp(expires_at)
            .ok()
            .and_then(|at| {
                at.format(&time::format_description::well_known::Rfc3339)
                    .ok()
            })
            .ok_or(Error::config("Invalid Antigravity login expiry."))?
            .into(),
    };
    token.insert(expiry_key.into(), expiry);
    // Antigravity also keeps the token's lifetime.
    if token.contains_key("expires_in") {
        token.insert("expires_in".into(), expires_in.into());
    }
    Ok(())
}

/// Antigravity's OAuth client. It is an installed app's: `agy` ships it, so it
/// is public and confers no access by itself (Copool and OpenQuota send the
/// same). This source names it only by digest; the proxy learns it from a
/// refresh Antigravity sends through it, else from the `agy` executable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OAuthClient {
    pub id: String,
    pub secret: String,
}

static CLIENT: std::sync::Mutex<Option<OAuthClient>> = std::sync::Mutex::new(None);
static SHIPPED_CLIENT: tokio::sync::OnceCell<Option<OAuthClient>> =
    tokio::sync::OnceCell::const_new();

fn sha256_hex(value: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, value)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Whether `id` is Antigravity's OAuth client ID.
pub(crate) fn is_client_id(id: &str) -> bool {
    #[cfg(test)]
    if id == TEST_CLIENT_ID {
        return true;
    }
    sha256_hex(id.as_bytes()) == CLIENT_ID_SHA256
}

fn is_client_secret(secret: &str) -> bool {
    #[cfg(test)]
    if secret == TEST_CLIENT_SECRET {
        return true;
    }
    sha256_hex(secret.as_bytes()) == CLIENT_SECRET_SHA256
}

/// Antigravity's client as a token request sends it, if it does.
fn sent_client(body: &[u8]) -> Option<OAuthClient> {
    let id = form_field(body, "client_id").filter(|id| is_client_id(id))?;
    let secret = form_field(body, "client_secret").filter(|s| is_client_secret(s))?;
    Some(OAuthClient { id, secret })
}

/// Keeps Antigravity's client from a refresh it sent through the proxy.
pub(crate) fn learn_client(body: &[u8]) {
    if let Some(client) = sent_client(body) {
        *CLIENT.lock().unwrap() = Some(client);
    }
}

/// Antigravity's client for the proxy's own refreshes: as Antigravity last
/// sent it, else as the `agy` on `PATH` ships it, read once per process.
pub(crate) async fn oauth_client() -> Result<OAuthClient> {
    if let Some(client) = CLIENT.lock().unwrap().clone() {
        return Ok(client);
    }
    SHIPPED_CLIENT
        .get_or_init(|| async {
            tokio::task::spawn_blocking(shipped_client)
                .await
                .ok()
                .flatten()
        })
        .await
        .clone()
        .ok_or(Error::config(
            "Antigravity's OAuth client is unknown until agy refreshes its login through the proxy.",
        ))
}

/// The ID and secret with Antigravity's digests among those the installed
/// `agy` carries.
fn shipped_client() -> Option<OAuthClient> {
    // Tests never read an installed agy.
    if cfg!(test) {
        return None;
    }
    agy_files().into_iter().find_map(|file| {
        let file = std::fs::File::open(file).ok()?;
        scan_client(file, CLIENT_ID_SHA256, CLIENT_SECRET_SHA256)
    })
}

/// The files that may hold `agy`'s client: the `agy` found on `PATH` (or in a
/// usual install directory), else, for a launcher script such as Homebrew's,
/// the files it names.
fn agy_files() -> Vec<std::path::PathBuf> {
    use std::path::PathBuf;
    let names: &[&str] = if cfg!(windows) {
        &["agy.exe", "agy.cmd", "agy"]
    } else {
        &["agy"]
    };
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default();
    // A daemon may start with a minimal PATH.
    if !cfg!(windows) {
        dirs.extend(["/opt/homebrew/bin", "/usr/local/bin"].map(PathBuf::from));
        dirs.extend(dirs::home_dir().map(|home| home.join(".local/bin")));
    }
    let Some(found) = dirs
        .iter()
        .flat_map(|dir| names.iter().map(move |name| dir.join(name)))
        .find(|path| path.is_file())
    else {
        return Vec::new();
    };
    let found = std::fs::canonicalize(&found).unwrap_or(found);
    let script = std::fs::metadata(&found).is_ok_and(|m| m.len() < 64 * 1024);
    if !script {
        return vec![found];
    }
    let text = std::fs::read_to_string(&found).unwrap_or_default();
    let named: Vec<PathBuf> = text
        .split('"')
        .skip(1)
        .step_by(2)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute() && path.is_file())
        .collect();
    if named.is_empty() { vec![found] } else { named }
}

/// The longest candidate a scan considers, and the overlap between the
/// chunks it reads.
const CANDIDATE_LIMIT: usize = 128;
/// How Google's OAuth client IDs end and their secrets begin.
const ID_SUFFIX: &str = ".apps.googleusercontent.com";
const SECRET_PREFIX: &str = "GOCSPX-";

/// The OAuth client with digests `id_digest` and `secret_digest` among the
/// strings `reader` holds. Binaries pack strings without separators, so
/// every length around a client ID's suffix or a secret's prefix is tried.
fn scan_client(
    mut reader: impl std::io::Read,
    id_digest: &str,
    secret_digest: &str,
) -> Option<OAuthClient> {
    let (mut id, mut secret) = (None, None);
    let mut buffer = Vec::new();
    let mut chunk = vec![0; 1 << 20];
    loop {
        let read = match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => read,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return None,
        };
        buffer.extend_from_slice(&chunk[..read]);
        let matches = |candidate: &[u8], digest: &str| {
            (sha256_hex(candidate) == digest)
                .then(|| String::from_utf8(candidate.to_vec()).ok())
                .flatten()
        };
        for at in 0..buffer.len() {
            if id.is_none() && buffer[at..].starts_with(ID_SUFFIX.as_bytes()) {
                let end = at + ID_SUFFIX.len();
                let start = buffer[..at]
                    .iter()
                    .rev()
                    .take(CANDIDATE_LIMIT - ID_SUFFIX.len())
                    .take_while(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || **b == b'-')
                    .count();
                id = (at - start..at).find_map(|from| matches(&buffer[from..end], id_digest));
            }
            if secret.is_none() && buffer[at..].starts_with(SECRET_PREFIX.as_bytes()) {
                let tail = buffer[at + SECRET_PREFIX.len()..]
                    .iter()
                    .take(CANDIDATE_LIMIT - SECRET_PREFIX.len())
                    .take_while(|b| b.is_ascii_alphanumeric() || **b == b'-' || **b == b'_')
                    .count();
                let base = at + SECRET_PREFIX.len();
                secret = (1..=tail).find_map(|len| matches(&buffer[at..base + len], secret_digest));
            }
        }
        if let (Some(id), Some(secret)) = (&id, &secret) {
            return Some(OAuthClient {
                id: id.clone(),
                secret: secret.clone(),
            });
        }
        let keep = buffer.len().min(CANDIDATE_LIMIT);
        buffer.drain(..buffer.len() - keep);
    }
    None
}

/// The form a refresh of `refresh_token` sends with Antigravity's client.
pub(crate) fn refresh_form(refresh_token: &str, client: &OAuthClient) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .append_pair("client_id", &client.id)
        .append_pair("client_secret", &client.secret)
        .append_pair("grant_type", "refresh_token")
        .append_pair("refresh_token", refresh_token)
        .finish()
}

/// The saved login's document and the item it is kept in, to renew it in
/// place. `recent` accepts a read at most 30 seconds old, as requests are
/// matched with.
pub(crate) async fn saved_document(
    recent: bool,
) -> Result<(crate::oauth_credentials::Store, Value)> {
    let raw = if recent {
        crate::keychain::read_go_keyring_recent(SERVICE, ACCOUNT, RECENT).await?
    } else {
        crate::keychain::read_go_keyring(SERVICE, ACCOUNT).await?
    }
    .ok_or(Error::config(
        "No Antigravity login in the OS credential store.",
    ))?;
    let (document, _) = decode_item(&raw)
        .filter(|(document, _)| access_token(document).is_some())
        .ok_or(Error::config(
            "Invalid Antigravity login in the OS credential store.",
        ))?;
    let store = crate::oauth_credentials::Store::GoKeyring(SERVICE.into(), ACCOUNT.into());
    Ok((store, document))
}
/// Bounds how long a login Antigravity replaced may still match.
const RECENT: std::time::Duration = std::time::Duration::from_secs(30);

fn digest(token: &str) -> String {
    ring::digest::digest(&ring::digest::SHA256, token.as_bytes())
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Access tokens issued by refreshes the proxy relayed for a known login.
/// Apps that renew the saved login themselves, as Copool and OpenQuota do,
/// keep the new access token privately while the credential store still
/// holds an older one; Google keeps the refresh token, so the new token is
/// still that login. Kept in memory as digests until they expire.
#[derive(Default)]
pub struct RenewedTokens {
    tokens: std::sync::Mutex<std::collections::HashMap<String, Renewed>>,
}
#[derive(Clone)]
struct Renewed {
    /// Digest of the refresh token the access token was issued for.
    refresh: String,
    identity: Option<AntigravityIdentity>,
    expires: std::time::Instant,
}
impl RenewedTokens {
    const LIMIT: usize = 64;
    /// Google's default lifetime, when a response names none.
    const LIFETIME: u64 = 3600;

    /// Records the access token of a successful refresh `response` for
    /// `refresh_token`: the account its ID token names, else `identity`.
    pub(crate) fn learn_response(
        &self,
        response: &[u8],
        refresh_token: &str,
        identity: Option<AntigravityIdentity>,
    ) {
        let Ok(value) = serde_json::from_slice::<Value>(response) else {
            return;
        };
        let Some(access) = value["access_token"].as_str().filter(|t| valid_token(t)) else {
            return;
        };
        let lifetime = value["expires_in"]
            .as_u64()
            .unwrap_or(Self::LIFETIME)
            .min(24 * Self::LIFETIME);
        let renewed = Renewed {
            refresh: digest(refresh_token),
            identity: AntigravityIdentity::token_response(&value).or(identity),
            expires: std::time::Instant::now() + std::time::Duration::from_secs(lifetime),
        };
        let Ok(mut tokens) = self.tokens.lock() else {
            return;
        };
        let now = std::time::Instant::now();
        tokens.retain(|_, renewed| renewed.expires > now);
        while tokens.len() >= Self::LIMIT {
            let Some(soonest) = tokens
                .iter()
                .min_by_key(|(_, renewed)| renewed.expires)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            tokens.remove(&soonest);
        }
        tokens.insert(digest(access), renewed);
    }

    fn get(&self, access_token: &str) -> Option<Renewed> {
        let tokens = self.tokens.lock().ok()?;
        tokens
            .get(&digest(access_token))
            .filter(|renewed| renewed.expires > std::time::Instant::now())
            .cloned()
    }
}
impl Renewed {
    /// Whether this token was issued for `login`'s refresh token.
    fn renews(&self, login: &Login) -> bool {
        login
            .refresh_token
            .as_deref()
            .is_some_and(|refresh| digest(refresh) == self.refresh)
    }
}

pub struct AntigravityRoute {
    pub token: String,
    pub proxy: Choice,
    pub identity: Option<AntigravityIdentity>,
    /// The token is the saved login's.
    pub matched_login: bool,
    /// The saved login's refresh token, under which a looked-up identity is
    /// learned.
    pub refresh_token: Option<String>,
    /// A Google lookup names the account before the request is sent.
    pub needs_profile: bool,
    pub label: String,
    /// The credential is the saved login or an account the routing maps, not
    /// one a fallback carries.
    pub configured: bool,
}

/// How a Gemini API key request routes: by its configured upstream URL.
pub struct ApiRoute {
    /// The `routing.api_key` key, which records name.
    pub selector: String,
    pub proxy: Choice,
    pub upstream: String,
    pub key: String,
    pub credential: ApiCredential,
    /// The key is a named one, not one a URL route or `fallback` carries.
    pub configured: bool,
    /// A URL route's upstream, reached with the platform's TLS as Codex's.
    pub custom_upstream: bool,
}
impl ApiRoute {
    /// The upstream URL of an origin-form or explicit target.
    pub fn url(&self, target: &str) -> Result<Url> {
        if target.starts_with("/https://") || target.starts_with("/http://") {
            return crate::routing::upstream_url(&self.upstream, target, false);
        }
        if !target.starts_with('/') || target.starts_with("//") {
            return Err(Error::config("Invalid Antigravity request target."));
        }
        let explicit = format!("/{}{target}", self.upstream.trim_end_matches('/'));
        crate::routing::upstream_url(&self.upstream, &explicit, false)
    }
}
/// Where a request carried its Gemini API key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApiCredential {
    Header,
    Bearer,
    Query,
}
/// The route name records give an API key `fallback` carried, as Claude's
/// `claude-api-key-fallback`.
pub const API_KEY_FALLBACK: &str = "antigravity-api-key-fallback";

/// A request's Gemini API key, where the Gemini API accepts one:
/// `x-goog-api-key`, a Bearer header or the `key` query parameter. None
/// without any; more than one is refused.
pub(crate) fn api_credential(
    headers: &HeaderMap,
    target: &str,
) -> Result<Option<(String, ApiCredential)>> {
    let header = headers
        .get("x-goog-api-key")
        .map(|v| v.to_str().ok().map(str::to_owned));
    let bearer = headers.get("authorization").map(|v| {
        v.to_str()
            .ok()
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(str::to_owned)
    });
    let query = target.split_once('?').and_then(|(_, query)| {
        url::form_urlencoded::parse(query.as_bytes())
            .find(|(name, _)| name == "key")
            .map(|(_, value)| Some(value.into_owned()))
    });
    let (key, credential) = match (header, bearer, query) {
        (Some(key), None, None) => (key, ApiCredential::Header),
        (None, Some(key), None) => (key, ApiCredential::Bearer),
        (None, None, Some(key)) => (key, ApiCredential::Query),
        (None, None, None) => return Ok(None),
        _ => {
            return Err(Error::new(
                400,
                "Supply the Gemini API key once: x-goog-api-key, Bearer or key.",
            ));
        }
    };
    key.filter(|key| valid_token(key))
        .map(|key| Some((key, credential)))
        .ok_or(Error::new(401, "Invalid Gemini API key."))
}

/// How a refresh routes: as `AntigravityRoute`, by its refresh token.
pub struct RefreshRoute {
    pub proxy: Choice,
    /// The refresh token is the saved login's.
    pub saved: bool,
    pub identity: Option<AntigravityIdentity>,
    pub configured: bool,
    /// The saved login's access token, for an explicit `account_probe` to
    /// name it before the refresh is sent.
    pub probe_token: Option<String>,
}

/// Whether a target in the `/antigravity` scope is on Google's token host.
pub fn token_host(target: &str) -> bool {
    target
        .strip_prefix('/')
        .and_then(|url| Url::parse(url).ok())
        .is_some_and(|url| url.host_str() == Url::parse(TOKEN_REFRESH_UPSTREAM).unwrap().host_str())
}
/// Whether a target in the `/antigravity` scope is the token endpoint.
pub fn token_refresh(target: &str) -> bool {
    target.strip_prefix('/') == Some(TOKEN_REFRESH_UPSTREAM)
}
/// A token request's field `name`, from a form or JSON body.
pub(crate) fn form_field(body: &[u8], name: &str) -> Option<String> {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| v[name].as_str().map(String::from))
        .or_else(|| {
            url::form_urlencoded::parse(body)
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.into_owned())
        })
}

/// The Antigravity scope of a request target, without its prefix.
pub fn target(target: &str) -> Option<&str> {
    target
        .strip_prefix("/antigravity")
        .filter(|rest| rest.starts_with('/'))
}

/// An Antigravity target with the `//` of the upstream it names: agy joins
/// its `GOOGLE_GEMINI_BASE_URL` with each request's path collapsing `//`, so
/// `<Coport>/antigravity/https://relay.example.com/antigravity` arrives as
/// `/https:/relay.example.com/antigravity/…`.
pub fn named_upstream_target(rest: &str) -> std::borrow::Cow<'_, str> {
    for scheme in ["/https:/", "/http:/"] {
        if let Some(after) = rest.strip_prefix(scheme).filter(|a| !a.starts_with('/')) {
            return format!("{scheme}/{after}").into();
        }
    }
    rest.into()
}

impl Antigravity {
    /// Whether any request is routed: an account route or a fallback.
    pub fn enabled(&self) -> bool {
        !self.routing.account.is_empty() || self.routing.account_default().is_some()
    }

    pub(crate) fn validate(&self, config: &Config) -> Result<()> {
        validate_upstream(&self.base_url.account)?;
        if self.routing.mcp_fallback.is_some() {
            return Err(Error::config("Antigravity does not support mcp_fallback."));
        }
        self.validate_api_routes(config)?;
        if self.account_auth_file_only && !self.saved_login && self.enabled() {
            return Err(Error::config(
                "Antigravity file-only account routing requires saved_login.",
            ));
        }
        for (label, choice) in &self.routing.account {
            if label.trim().is_empty() {
                return Err(Error::config("Empty Antigravity routing identifier."));
            }
            config.validate_choice(choice)?;
        }
        for choice in [
            &self.routing.account_fallback,
            &self.routing.account_probe,
            &self.routing.fallback,
        ]
        .into_iter()
        .flatten()
        {
            config.validate_choice(choice)?;
        }
        Ok(())
    }

    /// As Claude's: an API key route names an environment variable whose
    /// value is the key, such as `GEMINI_API_KEY`, or an upstream URL. Google's
    /// own hosts stay routed by login, so a URL route cannot carry a login's
    /// token past its account route.
    fn validate_api_routes(&self, config: &Config) -> Result<()> {
        crate::config::validate_upstream(&self.base_url.api_key)?;
        for (selector, choice) in &self.routing.api_key {
            if crate::url_routing::is_url_selector(selector) {
                self.api_upstream(&crate::url_routing::validate_upstream(selector)?)?;
            } else {
                crate::config::validate_env(selector)?;
            }
            config.validate_choice(choice)?;
        }
        crate::url_routing::validate_routes(&self.routing.api_key)
    }

    /// Refuses a Gemini API upstream on a Cloud Code, token or login
    /// endpoint's host: those stay routed by login, so a key route cannot
    /// carry a login's token past its account route.
    fn api_upstream(&self, url: &Url) -> Result<()> {
        let google = self
            .origins()
            .into_iter()
            .chain([TOKEN_REFRESH_UPSTREAM])
            .chain(LOGIN_ENDPOINTS)
            .filter_map(|origin| Url::parse(origin).ok());
        for origin in google {
            if origin.host_str() == url.host_str()
                && origin.port_or_known_default() == url.port_or_known_default()
            {
                return Err(Error::config(
                    "Antigravity API key routes cannot name a Cloud Code, Google token or login host.",
                ));
            }
        }
        Ok(())
    }

    /// Environment variable selectors, as opposed to URL routes.
    fn key_selectors(&self) -> impl Iterator<Item = (&String, &Choice)> {
        self.routing
            .api_key
            .iter()
            .filter(|(selector, _)| !crate::url_routing::is_url_selector(selector))
    }

    /// Whether API keys without a URL route are served: by name or `fallback`.
    pub(crate) fn serves_api_keys(&self) -> bool {
        self.routing.fallback.is_some() || self.key_selectors().next().is_some()
    }

    /// Display classification, as Codex's and Claude's `api_key_kind`.
    pub fn api_key_kind(&self, selector: &str) -> &'static str {
        if crate::url_routing::is_url_selector(selector) {
            "gateway"
        } else {
            "api_key"
        }
    }

    /// The upstream a selector forwards to when its request names none, as
    /// Claude's `api_key_upstream`; none for URL routes, whose requests name it.
    pub fn api_key_upstream(&self, selector: &str) -> Option<&str> {
        (!crate::url_routing::is_url_selector(selector)).then_some(self.base_url.api_key.as_str())
    }

    /// Whether an API key route serves this unprefixed target.
    pub(crate) fn url_route_matches(&self, target: &str) -> Result<bool> {
        Ok(crate::url_routing::match_route(&self.routing.api_key, target)?.is_some())
    }

    /// The URL route of an explicit target, as for Codex and Claude: any key,
    /// forwarded as sent, for the upstream to validate.
    pub(crate) fn api_route(&self, headers: &HeaderMap, target: &str) -> Result<Option<ApiRoute>> {
        let Some((selector, proxy)) =
            crate::url_routing::match_route(&self.routing.api_key, target)?
        else {
            return Ok(None);
        };
        let (key, credential) = api_credential(headers, target)?.ok_or(Error::new(
            401,
            "Antigravity API routes require a Gemini API key.",
        ))?;
        Ok(Some(ApiRoute {
            selector: selector.into(),
            proxy: proxy.clone(),
            upstream: crate::url_routing::validate_upstream(selector)?.into(),
            key,
            credential,
            configured: false,
            custom_upstream: true,
        }))
    }

    /// The route of a Gemini API key sent in `x-goog-api-key` or `key` (a
    /// Bearer token is a Google login), as Claude's `x-api-key`: the
    /// environment variable selector whose value it is, else `fallback`.
    /// A named key goes to the upstream its `target` names, if any.
    pub(crate) async fn resolve_api_key(
        &self,
        key: String,
        credential: ApiCredential,
        target: &str,
    ) -> Result<ApiRoute> {
        let route = self
            .resolve_api_key_with(key, credential, |name| async move {
                crate::identity::environment_key_with_shell(&name, true).await
            })
            .await?;
        self.named_upstream(route, target)
    }

    /// A named key goes where its request names, as a Claude settings
    /// file's key goes to that file's upstream: agy's `GOOGLE_GEMINI_BASE_URL`
    /// may be `<Coport>/antigravity/https://relay.example.com/antigravity`,
    /// with no `base_url.api_key` set. A key `fallback` carries, which any
    /// client may send, reaches `base_url.api_key` alone. The upstream is
    /// checked as a URL route's is, and reached with the platform's TLS.
    fn named_upstream(&self, mut route: ApiRoute, target: &str) -> Result<ApiRoute> {
        if !route.configured || !(target.starts_with("/https://") || target.starts_with("/http://"))
        {
            return Ok(route);
        }
        let named =
            Url::parse(&target[1..]).map_err(|_| Error::config("Invalid explicit upstream."))?;
        let base = Url::parse(&self.base_url.api_key).ok();
        if base.is_some_and(|base| {
            base.host_str() == named.host_str()
                && base.port_or_known_default() == named.port_or_known_default()
        }) {
            return Ok(route);
        }
        let origin = named.origin().ascii_serialization();
        self.api_upstream(&crate::url_routing::validate_upstream(&origin)?)?;
        route.upstream = origin;
        route.custom_upstream = true;
        Ok(route)
    }

    async fn resolve_api_key_with<F, Fut>(
        &self,
        key: String,
        credential: ApiCredential,
        lookup: F,
    ) -> Result<ApiRoute>
    where
        F: Fn(String) -> Fut,
        Fut: std::future::Future<Output = Result<String>>,
    {
        let mut matches = Vec::new();
        let mut unavailable = false;
        for (name, proxy) in self.key_selectors() {
            match lookup(name.clone()).await {
                Ok(value) if value == key => matches.push((name, proxy)),
                Ok(_) => {}
                Err(_) => unavailable = true,
            }
        }
        if matches.len() > 1 {
            return Err(Error::new(
                409,
                "Antigravity credential matches multiple routes.",
            ));
        }
        let (selector, proxy, configured) = if let Some((name, proxy)) = matches.pop() {
            (name.clone(), proxy.clone(), true)
        } else if let Some(proxy) = &self.routing.fallback {
            (API_KEY_FALLBACK.into(), proxy.clone(), false)
        } else if unavailable {
            return Err(Error::config(
                "No matching Antigravity route; credential source unavailable.",
            ));
        } else {
            return Err(Error::new(
                401,
                "Antigravity API key does not match a configured route.",
            ));
        };
        Ok(ApiRoute {
            selector,
            proxy,
            upstream: self.base_url.api_key.clone(),
            key,
            credential,
            configured,
            custom_upstream: false,
        })
    }

    /// As Claude's: each named key is readable and names one route.
    async fn check_api_keys(&self) -> Result<()> {
        let mut keys = std::collections::HashSet::new();
        for (name, _) in self.key_selectors() {
            if !keys.insert(crate::identity::environment_key_with_shell(name, true).await?) {
                return Err(Error::config(
                    "Multiple Antigravity routes have the same credential.",
                ));
            }
        }
        Ok(())
    }

    /// Origins proxy-port clients send Antigravity's requests to.
    pub(crate) fn origins(&self) -> Vec<&str> {
        let mut origins = vec![self.base_url.account.as_str()];
        origins.extend(CLOUD_CODE_ORIGINS);
        origins
    }

    /// The saved login as the credential store holds it now: Antigravity
    /// renews its token itself and saves the new one in the same item.
    pub(crate) async fn saved(&self) -> Result<Option<Login>> {
        if !self.saved_login {
            return Ok(None);
        }
        Ok(crate::keychain::read_go_keyring(SERVICE, ACCOUNT)
            .await?
            .as_deref()
            .and_then(parse_login))
    }

    /// The saved login for a request: a recent read when `matches` accepts
    /// it, so requests with the current token start no `security` process,
    /// else the item as it is now.
    async fn saved_for(&self, matches: impl Fn(&Login) -> bool) -> (Option<Login>, bool) {
        if !self.saved_login {
            return (None, false);
        }
        let recent = crate::keychain::read_go_keyring_recent(SERVICE, ACCOUNT, RECENT)
            .await
            .ok()
            .flatten()
            .as_deref()
            .and_then(parse_login)
            .filter(|login| matches(login));
        if recent.is_some() {
            return (recent, false);
        }
        match self.saved().await {
            Ok(login) => (login, false),
            Err(_) => (None, true),
        }
    }

    fn mapped_choice(&self, identity: &AntigravityIdentity) -> Option<&Choice> {
        // Google addresses are case-insensitive.
        self.mapped_key(identity)
            .and_then(|key| self.routing.account.get(&key))
    }
    /// A saved login takes its email's route, then `default`; any account then
    /// takes `account_fallback`, then `fallback`.
    fn account_choice(
        &self,
        identity: Option<&AntigravityIdentity>,
        saved: bool,
    ) -> Option<&Choice> {
        identity
            .and_then(|i| self.mapped_choice(i))
            .or_else(|| saved.then(|| self.routing.account.get("default")).flatten())
            .or(self.routing.account_default())
    }

    /// The saved login's account: the email its document records, else the
    /// one learned for its refresh token.
    pub(crate) fn login_identity(
        login: &Login,
        learned: &RefreshRoutes<AntigravityIdentity>,
    ) -> Option<AntigravityIdentity> {
        login.identity.clone().or_else(|| {
            login
                .refresh_token
                .as_deref()
                .and_then(|refresh| learned.identity(refresh))
        })
    }

    /// The saved login's route label: its email's route, else `default`.
    pub(crate) fn login_label(&self, identity: Option<&AntigravityIdentity>) -> Option<String> {
        identity.and_then(|i| self.mapped_key(i)).or_else(|| {
            self.routing
                .account
                .contains_key("default")
                .then(|| "default".to_owned())
        })
    }
    /// The routing key naming this account, regardless of case.
    pub(crate) fn mapped_key(&self, identity: &AntigravityIdentity) -> Option<String> {
        self.routing
            .account
            .keys()
            .find(|key| key.eq_ignore_ascii_case(&identity.email))
            .cloned()
    }

    pub async fn resolve(
        &self,
        headers: &HeaderMap,
        learned: &RefreshRoutes<AntigravityIdentity>,
        renewed: &RenewedTokens,
    ) -> Result<AntigravityRoute> {
        let token = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .filter(|s| valid_token(s))
            .ok_or(Error::new(401, "Antigravity requires a Bearer token."))?;
        let (login, unavailable) = self.saved_for(|login| login.access_token == token).await;
        let route = |proxy: &Choice,
                     identity: Option<AntigravityIdentity>,
                     matched_login: bool,
                     needs_profile: bool,
                     label: &str| {
            Ok(AntigravityRoute {
                token: token.into(),
                proxy: proxy.clone(),
                configured: matched_login
                    || identity
                        .as_ref()
                        .is_some_and(|i| self.mapped_choice(i).is_some()),
                identity,
                matched_login,
                refresh_token: None,
                needs_profile,
                label: label.into(),
            })
        };
        let renewed = renewed.get(token);
        // The saved login's current token, or one a relayed refresh issued
        // for its refresh token; the credential store's is checked first.
        if let Some(login) = login.filter(|login| {
            login.access_token == token || renewed.as_ref().is_some_and(|r| r.renews(login))
        }) {
            let identity = Self::login_identity(&login, learned)
                .or_else(|| renewed.as_ref().and_then(|r| r.identity.clone()));
            // Antigravity's own login names no account; an explicit probe
            // route lets email routes apply to it, once per login.
            let probe = self
                .routing
                .account_probe
                .as_ref()
                .filter(|_| identity.is_none());
            let mut matched = match probe {
                Some(probe) => route(probe, None, true, true, "antigravity-profile")?,
                None => {
                    let proxy = self
                        .account_choice(identity.as_ref(), true)
                        .ok_or(Error::config("Antigravity account has no proxy route."))?;
                    // As for Claude's saved logins, the source label.
                    route(proxy, identity, true, false, "default")?
                }
            };
            matched.refresh_token = login.refresh_token;
            return Ok(matched);
        }
        if self.account_auth_file_only {
            if unavailable {
                return Err(Error::config(
                    "No matching Antigravity route; credential source unavailable.",
                ));
            }
            return Err(Error::new(
                401,
                "Antigravity token does not match the saved login.",
            ));
        }
        // Issued by a relayed refresh of another account's learned login.
        if let Some(identity) = renewed.and_then(|r| r.identity) {
            let proxy = self
                .account_choice(Some(&identity), false)
                .ok_or(Error::config("Antigravity account has no proxy route."))?;
            return route(proxy, Some(identity), false, false, "antigravity-account");
        }
        let probe = self
            .routing
            .account_probe
            .as_ref()
            .or(self.routing.account_default())
            .ok_or(Error::config(
                "Antigravity account lookup requires routing.account_probe or routing.account_fallback/fallback.",
            ))?;
        route(probe, None, false, true, "antigravity-profile")
    }

    /// The route of a refresh, as Codex's: the saved login's refresh token
    /// takes its account's route; with `account_auth_file_only: false`, a
    /// learned refresh token takes its account's, else `account_fallback`,
    /// then `fallback`. Anything else is refused before any connection.
    pub(crate) async fn resolve_refresh(
        &self,
        refresh_token: &str,
        learned: &RefreshRoutes<AntigravityIdentity>,
    ) -> Result<RefreshRoute> {
        let (login, unavailable) = self
            .saved_for(|login| login.refresh_token.as_deref() == Some(refresh_token))
            .await;
        if let Some(login) =
            login.filter(|login| login.refresh_token.as_deref() == Some(refresh_token))
        {
            let identity = Self::login_identity(&login, learned);
            if identity.is_none() {
                if let Some(probe) = &self.routing.account_probe {
                    return Ok(RefreshRoute {
                        saved: true,
                        proxy: probe.clone(),
                        identity: None,
                        configured: true,
                        probe_token: Some(login.access_token),
                    });
                }
            }
            let proxy = self
                .account_choice(identity.as_ref(), true)
                .ok_or(Error::config("Antigravity account has no proxy route."))?
                .clone();
            return Ok(RefreshRoute {
                saved: true,
                proxy,
                identity,
                configured: true,
                probe_token: None,
            });
        }
        if !self.account_auth_file_only {
            if let Some(identity) = learned.identity(refresh_token) {
                let proxy = self
                    .account_choice(Some(&identity), false)
                    .ok_or(Error::config("Antigravity account has no proxy route."))?
                    .clone();
                return Ok(RefreshRoute {
                    saved: false,
                    proxy,
                    configured: self.mapped_choice(&identity).is_some(),
                    identity: Some(identity),
                    probe_token: None,
                });
            }
            if let Some(proxy) = self.routing.account_default() {
                return Ok(RefreshRoute {
                    saved: false,
                    proxy: proxy.clone(),
                    identity: None,
                    configured: false,
                    probe_token: None,
                });
            }
        }
        if unavailable {
            Err(Error::config(
                "No matching refresh route; one or more credential sources are unavailable.",
            ))
        } else {
            Err(Error::new(
                403,
                "Token refresh requires the refresh token of Antigravity's saved login, or a learned route or account_fallback/fallback with account_auth_file_only: false.",
            ))
        }
    }

    /// Routes a refresh by the account a lookup named for the saved login.
    pub(crate) fn apply_refresh_profile(
        &self,
        route: &mut RefreshRoute,
        identity: AntigravityIdentity,
    ) -> Result<()> {
        route.proxy = self
            .account_choice(Some(&identity), true)
            .ok_or(Error::config("Antigravity account has no proxy route."))?
            .clone();
        route.identity = Some(identity);
        route.probe_token = None;
        Ok(())
    }

    /// Routes the saved login's refresh when its access token, which has
    /// usually expired by then, cannot name its account: as without a probe,
    /// `default`, then `account_fallback`/`fallback`; else the probe route
    /// already chosen. The response's ID token then names the account.
    pub(crate) fn apply_unnamed_refresh(&self, route: &mut RefreshRoute) {
        if let Some(choice) = self.account_choice(None, true) {
            route.proxy = choice.clone();
        }
        route.probe_token = None;
    }

    /// Routes a token by the account a Google lookup named.
    pub(crate) fn apply_profile(
        &self,
        route: &mut AntigravityRoute,
        identity: AntigravityIdentity,
    ) -> Result<()> {
        route.proxy = self
            .account_choice(Some(&identity), route.matched_login)
            .ok_or(Error::config("Antigravity account has no proxy route."))?
            .clone();
        route.configured |= self.mapped_choice(&identity).is_some();
        route.label = "antigravity-account".into();
        route.identity = Some(identity);
        route.needs_profile = false;
        Ok(())
    }

    /// The upstream URL of a target inside the `/antigravity` scope: a path
    /// under `base_url.account`, an explicit Cloud Code URL, or exactly one of
    /// `LOGIN_ENDPOINTS`.
    pub fn url(&self, target: &str) -> Result<Url> {
        if target.starts_with("/https://") || target.starts_with("/http://") {
            if let Some(url) = LOGIN_ENDPOINTS.into_iter().find_map(|endpoint| {
                crate::routing::upstream_url(endpoint, target, false)
                    .ok()
                    .filter(|url| url.path() == Url::parse(endpoint).unwrap().path())
            }) {
                return Ok(url);
            }
            let host = Url::parse(&target[1..])
                .ok()
                .and_then(|u| u.host_str().map(str::to_ascii_lowercase));
            let base = self
                .origins()
                .into_iter()
                .find(|origin| {
                    Url::parse(origin)
                        .ok()
                        .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
                        == host
                })
                .ok_or(Error::config(
                    "Explicit Antigravity upstream must be a Cloud Code or login endpoint.",
                ))?;
            return crate::routing::upstream_url(base, target, false);
        }
        if !target.starts_with('/') || target.starts_with("//") {
            return Err(Error::config("Invalid Antigravity request target."));
        }
        let explicit = format!("/{}{target}", self.base_url.account.trim_end_matches('/'));
        crate::routing::upstream_url(&self.base_url.account, &explicit, false)
    }

    /// Whether named API keys and the saved login can be read, for
    /// credential checks.
    pub async fn check_credentials(&self) -> Result<()> {
        self.check_api_keys().await?;
        if !self.saved_login || !self.enabled() {
            return Ok(());
        }
        match crate::keychain::read_go_keyring(SERVICE, ACCOUNT).await? {
            Some(raw) if parse_login(&raw).is_some() => Ok(()),
            Some(_) => Err(Error::config(
                "Invalid Antigravity login in the OS credential store.",
            )),
            None => Err(Error::config(
                "No Antigravity login in the OS credential store; sign in to Antigravity first.",
            )),
        }
    }
}

/// Tests that use the saved login hold this, so concurrent tests do not
/// replace it under each other.
#[cfg(test)]
pub(crate) static TEST_LOGIN: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
#[cfg(test)]
pub(crate) const TEST_SAVED_TOKEN: &str = "antigravity-saved-token";
#[cfg(test)]
pub(crate) fn set_test_login(access: &str, refresh: &str) {
    crate::keychain::set_test_entry(
        SERVICE,
        ACCOUNT,
        Some(
            &serde_json::json!({"token": {"access_token": access, "refresh_token": refresh}})
                .to_string(),
        ),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn config(extra: &str) -> Config {
        Config::parse(&format!(
            "listen_port: 8787\nrequest_timeout_seconds: 3\nproxies:\n  us: http://127.0.0.1:9\n  jp: http://127.0.0.1:10\n{extra}"
        ))
        .unwrap()
    }
    fn headers(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", format!("Bearer {token}").parse().unwrap());
        headers
    }

    #[test]
    fn renewals_keep_the_documents_keys_and_forms() {
        let mut nested = json!({"token": {"access_token": "a", "refresh_token": "r", "expiry": "2026-01-01T00:00:00.5+08:00", "expires_in": 1}, "other": 1});
        apply_renewal(&mut nested, "b", 3600, None).unwrap();
        assert_eq!(nested["token"]["access_token"], "b");
        assert_eq!(nested["token"]["refresh_token"], "r");
        assert_eq!(nested["token"]["expires_in"], 3600);
        assert_eq!(nested["other"], 1);
        let expiry = nested["token"]["expiry"].as_str().unwrap();
        assert!(expiry.ends_with('Z') && !expiry.contains('.'), "{expiry}");
        let mut flat =
            json!({"accessToken": "a", "refreshToken": "r", "expiresAt": 1_000_000_000_000_i64});
        apply_renewal(&mut flat, "b", 60, Some("s")).unwrap();
        assert_eq!(flat["accessToken"], "b");
        assert_eq!(flat["refreshToken"], "s");
        assert!(flat.get("access_token").is_none() && flat.get("expires_in").is_none());
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let at = flat["expiresAt"].as_i64().unwrap();
        assert!((now + 60) * 1000 <= at && at <= (now + 61) * 1000);
        let mut seconds = json!({"access_token": "a", "expires_at": 5});
        apply_renewal(&mut seconds, "b", 60, None).unwrap();
        assert!((now + 60..=now + 61).contains(&seconds["expires_at"].as_i64().unwrap()));
        assert!(apply_renewal(&mut json!([]), "b", 60, None).is_err());
    }

    #[test]
    fn items_keep_their_wrapping() {
        let document = json!({"token": {"access_token": "a", "refresh_token": "r"}});
        for wrapped in [true, false] {
            let raw = encode_item(&document, wrapped);
            assert_eq!(raw.starts_with(WRAPPED), wrapped);
            assert_eq!(decode_item(&raw), Some((document.clone(), wrapped)));
        }
        let client = OAuthClient {
            id: "i".into(),
            secret: "s&".into(),
        };
        let form: std::collections::HashMap<String, String> =
            url::form_urlencoded::parse(refresh_form("r&=", &client).as_bytes())
                .into_owned()
                .collect();
        assert_eq!(form["refresh_token"], "r&=");
        assert_eq!(form["client_id"], "i");
        assert_eq!(form["client_secret"], "s&");
        assert_eq!(form["grant_type"], "refresh_token");
    }

    #[test]
    fn only_a_refresh_with_antigravitys_id_and_secret_teaches_its_client() {
        let client = OAuthClient {
            id: TEST_CLIENT_ID.into(),
            secret: TEST_CLIENT_SECRET.into(),
        };
        let form = format!("client_id={TEST_CLIENT_ID}&client_secret={TEST_CLIENT_SECRET}");
        assert_eq!(sent_client(form.as_bytes()), Some(client.clone()));
        let json = json!({"client_id": TEST_CLIENT_ID, "client_secret": TEST_CLIENT_SECRET});
        assert_eq!(sent_client(json.to_string().as_bytes()), Some(client));
        for body in [
            format!("client_id={TEST_CLIENT_ID}&client_secret=other"),
            format!(
                "client_id=other.apps.googleusercontent.com&client_secret={TEST_CLIENT_SECRET}"
            ),
            format!("client_id={TEST_CLIENT_ID}"),
        ] {
            assert_eq!(sent_client(body.as_bytes()), None, "{body}");
        }
        // Only the digests name the real client.
        assert!(!is_client_id("other.apps.googleusercontent.com"));
        assert_eq!(sha256_hex(b"abc").len(), 64);
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn a_scan_finds_the_client_with_the_digests_among_packed_strings() {
        // Fakes in the shapes of Google's, built so no source line has one.
        let id = format!("123456-fake{}{ID_SUFFIX}", "x".repeat(28));
        let secret = format!("{SECRET_PREFIX}fake_{}-9", "Y".repeat(22));
        let (id_digest, secret_digest) = (sha256_hex(id.as_bytes()), sha256_hex(secret.as_bytes()));
        let expected = OAuthClient {
            id: id.clone(),
            secret: secret.clone(),
        };
        // Strings packed without separators, beside decoys of the same shapes.
        let packed = |filler: usize| {
            let mut data = vec![0u8; filler];
            data.extend_from_slice(format!("{SECRET_PREFIX}decoy{}", "D".repeat(23)).as_bytes());
            data.extend_from_slice(format!("{secret}abc").as_bytes());
            data.extend(std::iter::repeat_n(7u8, filler));
            data.extend_from_slice(format!("99-decoy{ID_SUFFIX}").as_bytes());
            data.extend_from_slice(format!("xyz{id}more").as_bytes());
            data
        };
        for filler in [0, 5, (1 << 20) - 40, (1 << 20) - 3] {
            let data = packed(filler);
            assert_eq!(
                scan_client(&data[..], &id_digest, &secret_digest),
                Some(expected.clone()),
                "{filler}"
            );
            // Both are needed.
            assert_eq!(scan_client(&data[..], &id_digest, &id_digest), None);
        }
        // Across the chunks a reader returns a few bytes at a time.
        struct Trickle<'a>(&'a [u8]);
        impl std::io::Read for Trickle<'_> {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let n = self.0.len().min(buf.len()).min(5);
                buf[..n].copy_from_slice(&self.0[..n]);
                self.0 = &self.0[n..];
                Ok(n)
            }
        }
        let data = packed(300);
        assert_eq!(
            scan_client(Trickle(&data), &id_digest, &secret_digest),
            Some(expected)
        );
    }

    #[test]
    fn logins_are_read_in_go_keyrings_forms() {
        let value = json!({"token": {"access_token": "ya29.a", "refresh_token": "r", "expiry": "2026-07-12T10:00:00Z"}});
        let wrapped = format!("{WRAPPED}{}", STANDARD.encode(value.to_string()));
        let expected = Login {
            access_token: "ya29.a".into(),
            refresh_token: Some("r".into()),
            identity: None,
        };
        assert_eq!(parse_login(&wrapped), Some(expected));
        let plain = format!("\u{feff}{value}\r\n");
        assert_eq!(parse_login(&plain).unwrap().access_token, "ya29.a");
        // Copool's sign-ins record the account's email.
        let named = json!({"token": {"access_token": "a"}, "id_token_email": "Alice@Gmail.com"});
        assert_eq!(
            parse_login(&named.to_string())
                .unwrap()
                .identity
                .unwrap()
                .email,
            "Alice@Gmail.com"
        );
        let flat = json!({"accessToken": "b", "refreshToken": "r", "expiresAt": 1});
        assert_eq!(parse_login(&flat.to_string()).unwrap().access_token, "b");
        for raw in [
            "raw-access-token",
            r#"{"token": {"refresh_token": "r"}}"#,
            "[]",
            "go-keyring-base64:not-base64!",
            r#"{"token": {"access_token": "has space"}}"#,
        ] {
            assert!(parse_login(raw).is_none(), "{raw}");
        }
    }

    #[test]
    fn configuration_validates_routes_and_sources() {
        let parsed = config("antigravity:\n  routing:\n    account:\n      default: us\n");
        assert_eq!(parsed.antigravity.base_url.account, DEFAULT_BASE);
        assert!(parsed.antigravity.saved_login && parsed.antigravity.account_auth_file_only);
        assert!(parsed.antigravity.enabled());
        assert!(!config("").antigravity.enabled());
        assert_eq!(parsed.antigravity.base_url.api_key, DEFAULT_API_BASE);
        // Either base may be given alone; the other keeps its default.
        let one = config(
            "antigravity:\n  base_url:\n    api_key: https://relay.example.com/antigravity\n",
        );
        assert_eq!(one.antigravity.base_url.account, DEFAULT_BASE);
        assert_eq!(
            one.antigravity.base_url.api_key,
            "https://relay.example.com/antigravity"
        );
        for invalid in [
            // Selectors are environment variable names or URLs.
            "antigravity:\n  routing:\n    api_key:\n      GEMINI API KEY: us\n",
            "antigravity:\n  routing:\n    api_key:\n      1GEMINI_API_KEY: us\n",
            "antigravity:\n  routing:\n    api_key:\n      GEMINI_API_KEY: missing\n",
            "antigravity:\n  base_url:\n    api_key: http://gemini.example\n",
            "antigravity:\n  routing:\n    api_key:\n      relay.example.com: missing\n",
            "antigravity:\n  routing:\n    api_key:\n      http://relay.example.com: us\n",
            "antigravity:\n  routing:\n    api_key:\n      relay.example.com: us\n      'https://relay.example.com/': jp\n",
            // Google's hosts stay routed by login.
            "antigravity:\n  routing:\n    api_key:\n      cloudcode-pa.googleapis.com/v1beta: us\n",
            "antigravity:\n  routing:\n    api_key:\n      'https://daily-cloudcode-pa.googleapis.com': us\n",
            "antigravity:\n  routing:\n    api_key:\n      oauth2.googleapis.com: us\n",
            "antigravity:\n  routing:\n    api_key:\n      www.googleapis.com: us\n",
            "antigravity:\n  routing:\n    api_key:\n      aicode.googleapis.com: us\n",
            "antigravity:\n  base_url:\n    account: https://cloudcode.example\n  routing:\n    api_key:\n      cloudcode.example/v1beta: us\n",
            "antigravity:\n  routing:\n    mcp_fallback: us\n",
            "antigravity:\n  routing:\n    account:\n      default: missing\n",
            "antigravity:\n  routing:\n    account_probe: missing\n",
            "antigravity:\n  saved_login: false\n  routing:\n    fallback: us\n",
            "antigravity:\n  base_url:\n    account: http://cloudcode.example\n",
            "antigravity:\n  unknown: true\n",
            // `base_url` is written as Codex's, with no flat form.
            "antigravity:\n  base_url: https://daily-cloudcode-pa.googleapis.com\n",
            "antigravity:\n  api_key_base_url: https://generativelanguage.googleapis.com\n",
            "antigravity:\n  base_url:\n    gemini: https://generativelanguage.googleapis.com\n",
        ] {
            assert!(
                Config::parse(&format!(
                    "listen_port: 8787\nrequest_timeout_seconds: 3\nproxies:\n  us: http://127.0.0.1:9\n{invalid}"
                ))
                .is_err(),
                "{invalid}"
            );
        }
        config(
            "antigravity:\n  saved_login: false\n  account_auth_file_only: false\n  routing:\n    fallback: us\n",
        );
        // API key routes need no login and do not enable login routing.
        let api = config(
            "antigravity:\n  saved_login: false\n  routing:\n    api_key:\n      relay.example.com/antigravity: us\n      '182.92.106.196:6060': none\n",
        );
        assert!(!api.antigravity.enabled());
        // URL routes alone serve no unrouted key; a named key or `fallback` do.
        assert!(!api.antigravity.serves_api_keys());
        let named = config(
            "antigravity:\n  saved_login: false\n  base_url:\n    api_key: https://relay.example.com/antigravity\n  routing:\n    api_key:\n      GEMINI_API_KEY: us\n      relay.example.com/other: jp\n",
        );
        let ag = &named.antigravity;
        assert!(!ag.enabled() && ag.serves_api_keys());
        assert_eq!(
            ag.api_key_upstream("GEMINI_API_KEY"),
            Some("https://relay.example.com/antigravity")
        );
        assert_eq!(ag.api_key_upstream("relay.example.com/other"), None);
        assert_eq!(ag.api_key_kind("GEMINI_API_KEY"), "api_key");
        assert_eq!(ag.api_key_kind("relay.example.com/other"), "gateway");
        let fallback = config(
            "antigravity:\n  saved_login: false\n  account_auth_file_only: false\n  routing:\n    fallback: us\n",
        );
        assert!(fallback.antigravity.serves_api_keys());
    }

    #[tokio::test]
    async fn named_api_keys_route_by_value_then_fallback_as_claudes() {
        let routes = "    api_key:\n      GEMINI_API_KEY: us\n      OTHER_GEMINI_KEY: jp\n      UNSET_GEMINI_KEY: none\n";
        let with = |fallback: &str| {
            config(&format!(
                "antigravity:\n  saved_login: false\n  account_auth_file_only: false\n  base_url:\n    api_key: https://relay.example.com/antigravity\n  routing:\n{routes}{fallback}"
            ))
        };
        // The values an environment would hold, never this process's.
        let lookup = |name: String| async move {
            match name.as_str() {
                "GEMINI_API_KEY" => Ok("first-key".to_owned()),
                "OTHER_GEMINI_KEY" => Ok("second-key".to_owned()),
                _ => Err(Error::config(
                    "API Key environment variable is unavailable.",
                )),
            }
        };
        let config = with("    fallback: none\n");
        let ag = &config.antigravity;
        let route = ag
            .resolve_api_key_with("second-key".into(), ApiCredential::Header, lookup)
            .await
            .unwrap();
        assert_eq!(route.selector, "OTHER_GEMINI_KEY");
        assert_eq!(route.proxy.names(), ["jp"]);
        assert_eq!(route.upstream, "https://relay.example.com/antigravity");
        assert!(route.configured && !route.custom_upstream);
        assert_eq!(
            route
                .url("/v1beta/models/g:streamGenerateContent?alt=sse")
                .unwrap()
                .as_str(),
            "https://relay.example.com/antigravity/v1beta/models/g:streamGenerateContent?alt=sse"
        );
        assert_eq!(
            route
                .url("/https://relay.example.com/antigravity/v1beta/models")
                .unwrap()
                .as_str(),
            "https://relay.example.com/antigravity/v1beta/models"
        );
        for target in [
            "/https://other.example.com/antigravity/v1beta/models",
            "/https://relay.example.com/elsewhere/v1beta/models",
            "//relay.example.com/x",
            "/a/../../x",
        ] {
            assert!(route.url(target).is_err(), "{target}");
        }
        // An unknown key takes `fallback`, which records do not count as
        // configured.
        let route = ag
            .resolve_api_key_with("unknown-key".into(), ApiCredential::Query, lookup)
            .await
            .unwrap();
        assert_eq!(route.selector, API_KEY_FALLBACK);
        assert_eq!(route.proxy.names(), ["none"]);
        assert!(!route.configured);
        // Without `fallback`: unknown with every source read, else an
        // unavailable source is reported.
        let config = with("");
        let unknown = config
            .antigravity
            .resolve_api_key_with(
                "unknown-key".into(),
                ApiCredential::Header,
                |name| async move { lookup(name).await.or(Ok("unrelated".into())) },
            )
            .await
            .err()
            .unwrap();
        assert_eq!(unknown.status, 401);
        let unavailable = config
            .antigravity
            .resolve_api_key_with("unknown-key".into(), ApiCredential::Header, lookup)
            .await
            .err()
            .unwrap();
        assert_ne!(unavailable.status, 401);
        // Two names holding one key are ambiguous, as for Claude.
        let shared = config
            .antigravity
            .resolve_api_key_with("first-key".into(), ApiCredential::Header, |_| async {
                Ok("first-key".to_owned())
            })
            .await
            .err()
            .unwrap();
        assert_eq!(shared.status, 409);
    }

    #[tokio::test]
    async fn a_named_key_goes_to_the_upstream_its_request_names_as_claudes_settings() {
        // No base_url.api_key: Google's stays the default, as Codex's and
        // Claude's base URLs name their official APIs.
        let config = config(
            "antigravity:\n  saved_login: false\n  account_auth_file_only: false\n  routing:\n    api_key:\n      RELAY_GEMINI_KEY: none\n    fallback: jp\n",
        );
        let ag = &config.antigravity;
        let lookup = |_: String| async { Ok("relay-key".to_owned()) };
        let named = |key: &str| {
            let key = key.to_owned();
            async move {
                ag.resolve_api_key_with(key, ApiCredential::Header, lookup)
                    .await
                    .unwrap()
            }
        };
        let relay = "/https://182.92.106.196:6060/antigravity/v1beta/models/g:streamGenerateContent?alt=sse";
        let route = ag.named_upstream(named("relay-key").await, relay).unwrap();
        assert_eq!(route.selector, "RELAY_GEMINI_KEY");
        assert_eq!(route.upstream, "https://182.92.106.196:6060");
        assert!(route.configured && route.custom_upstream);
        assert_eq!(
            route.url(relay).unwrap().as_str(),
            "https://182.92.106.196:6060/antigravity/v1beta/models/g:streamGenerateContent?alt=sse"
        );
        // An origin-form target, or one naming base_url.api_key, stays there.
        for target in [
            "/v1beta/models",
            "/https://generativelanguage.googleapis.com/v1beta/models",
        ] {
            let route = ag.named_upstream(named("relay-key").await, target).unwrap();
            assert_eq!(route.upstream, DEFAULT_API_BASE, "{target}");
            assert!(!route.custom_upstream, "{target}");
        }
        // As URL routes: public HTTPS only, and never a login's host.
        for target in [
            "/http://182.92.106.196:6060/antigravity/v1beta/models",
            "/https://127.0.0.1:6060/antigravity/v1beta/models",
            "/https://10.0.0.2/antigravity/v1beta/models",
            "/https://relay.local/antigravity/v1beta/models",
            "/https://cloudcode-pa.googleapis.com/v1internal:generateContent",
            "/https://oauth2.googleapis.com/token",
        ] {
            assert!(
                ag.named_upstream(named("relay-key").await, target).is_err(),
                "{target}"
            );
        }
        // A key `fallback` carries reaches only base_url.api_key.
        let fallback = ag
            .resolve_api_key_with("any-key".into(), ApiCredential::Header, lookup)
            .await
            .unwrap();
        assert!(!fallback.configured);
        let route = ag.named_upstream(fallback, relay).unwrap();
        assert_eq!(route.upstream, DEFAULT_API_BASE);
        assert!(route.url(relay).is_err());
    }

    #[test]
    fn only_x_goog_api_key_and_key_are_gemini_api_keys_unless_a_url_route_matches() {
        let mut header = HeaderMap::new();
        header.insert("x-goog-api-key", "header-key".parse().unwrap());
        assert_eq!(
            api_credential(&header, "/v1beta/models").unwrap(),
            Some(("header-key".into(), ApiCredential::Header))
        );
        assert_eq!(
            api_credential(&HeaderMap::new(), "/v1beta/models?alt=sse&key=q").unwrap(),
            Some(("q".into(), ApiCredential::Query))
        );
        assert_eq!(
            api_credential(&headers("login"), "/v1internal:x").unwrap(),
            Some(("login".into(), ApiCredential::Bearer))
        );
        assert_eq!(
            api_credential(&HeaderMap::new(), "/v1internal:x").unwrap(),
            None
        );
        // `api_key`, which Sub2api retired, is not a key.
        assert_eq!(
            api_credential(&HeaderMap::new(), "/v1beta/models?api_key=q").unwrap(),
            None
        );
    }

    #[test]
    fn api_key_routes_take_one_key_where_agy_or_another_client_sends_it() {
        let config = config(
            "antigravity:\n  routing:\n    api_key:\n      relay.example.com/antigravity: us\n      relay.example.com/antigravity/v1beta: [jp, us]\n",
        );
        let ag = &config.antigravity;
        let target =
            "/https://relay.example.com/antigravity/v1beta/models/g:streamGenerateContent?alt=sse";
        let mut header = HeaderMap::new();
        header.insert("x-goog-api-key", "header-key".parse().unwrap());
        let route = ag.api_route(&header, target).unwrap().unwrap();
        assert_eq!(
            (route.key.as_str(), route.credential),
            ("header-key", ApiCredential::Header)
        );
        // The longest matching path wins.
        assert_eq!(route.selector, "relay.example.com/antigravity/v1beta");
        assert_eq!(route.proxy.names(), ["jp", "us"]);
        assert_eq!(
            route.upstream,
            "https://relay.example.com/antigravity/v1beta"
        );
        let route = ag
            .api_route(&headers("bearer-key"), target)
            .unwrap()
            .unwrap();
        assert_eq!(
            (route.key.as_str(), route.credential),
            ("bearer-key", ApiCredential::Bearer)
        );
        let query = "/https://relay.example.com/antigravity/v1/models?alt=json&key=query%2Dkey";
        let route = ag.api_route(&HeaderMap::new(), query).unwrap().unwrap();
        assert_eq!(
            (
                route.key.as_str(),
                route.credential,
                route.selector.as_str()
            ),
            (
                "query-key",
                ApiCredential::Query,
                "relay.example.com/antigravity"
            )
        );
        for (headers, target, status) in [
            (HeaderMap::new(), target, 401),
            (headers("has space"), target, 401),
            (
                headers("bearer-key"),
                &format!("{target}&key=query-key")[..],
                400,
            ),
        ] {
            assert_eq!(
                ag.api_route(&headers, target).err().unwrap().status,
                status,
                "{target}"
            );
        }
        let mut both = headers("bearer-key");
        both.insert("x-goog-api-key", "header-key".parse().unwrap());
        assert_eq!(ag.api_route(&both, target).err().unwrap().status, 400);
        // Other hosts, ports, paths and Cloud Code targets are not API routes.
        for other in [
            "/https://relay.example.com/other/v1beta/models/g:generateContent",
            "/https://relay.example.com:8443/antigravity/v1beta/models/g:generateContent",
            "/https://cloudcode-pa.googleapis.com/v1internal:generateContent",
            "/v1internal:generateContent",
        ] {
            assert!(ag.api_route(&header, other).unwrap().is_none(), "{other}");
        }
        assert!(ag.url_route_matches(target).unwrap());
        assert!(
            !ag.url_route_matches("/v1beta/models/g:generateContent")
                .unwrap()
        );
    }

    #[test]
    fn a_named_upstream_gets_back_the_slash_agy_collapses() {
        for (sent, named) in [
            (
                "/https:/182.92.106.196:6060/antigravity/v1beta/models",
                "/https://182.92.106.196:6060/antigravity/v1beta/models",
            ),
            (
                "/http:/relay.example.com/v1beta/models",
                "/http://relay.example.com/v1beta/models",
            ),
            // Already whole, or naming no upstream: as sent.
            (
                "/https://relay.example.com/v1beta/models",
                "/https://relay.example.com/v1beta/models",
            ),
            (
                "/v1beta/models/g:streamGenerateContent",
                "/v1beta/models/g:streamGenerateContent",
            ),
            ("/https:relay", "/https:relay"),
        ] {
            assert_eq!(named_upstream_target(sent), named, "{sent}");
        }
    }

    #[test]
    fn targets_stay_on_cloud_code_and_login_endpoints() {
        let config = config("");
        let ag = &config.antigravity;
        assert_eq!(target("/antigravity/v1internal:x"), Some("/v1internal:x"));
        assert_eq!(target("/antigravityx/v1internal:x"), None);
        assert_eq!(
            ag.url("/v1internal:streamGenerateContent?alt=sse")
                .unwrap()
                .as_str(),
            "https://daily-cloudcode-pa.googleapis.com/v1internal:streamGenerateContent?alt=sse"
        );
        assert_eq!(
            ag.url("/https://cloudcode-pa.googleapis.com/v1internal:loadCodeAssist")
                .unwrap()
                .as_str(),
            "https://cloudcode-pa.googleapis.com/v1internal:loadCodeAssist"
        );
        // agy's `USER_INFO_OVERRIDE_URL` and `AICODE_ENDPOINT_URL`, exactly.
        for endpoint in LOGIN_ENDPOINTS {
            assert_eq!(ag.url(&format!("/{endpoint}")).unwrap().as_str(), endpoint);
        }
        assert_eq!(
            ag.url("/https://WWW.googleapis.com:443/oauth2/v2/userinfo?alt=json")
                .unwrap()
                .as_str(),
            "https://www.googleapis.com/oauth2/v2/userinfo?alt=json"
        );
        for invalid in [
            "/https://www.googleapis.com/oauth2/v2/userinfo/x",
            "/https://www.googleapis.com/oauth2/v2/userinfox",
            "/https://www.googleapis.com/oauth2/v2/",
            "/https://www.googleapis.com/drive/v3/files",
            "/https://www.googleapis.com/oauth2/v2/userinfo/../../drive/v3/files",
            "/http://www.googleapis.com/oauth2/v2/userinfo",
            "/https://user@www.googleapis.com/oauth2/v2/userinfo",
            "/https://www.googleapis.com:8443/oauth2/v2/userinfo",
            "/https://aicode.googleapis.com/v1alpha:other",
            "/https://aicode.googleapis.com/v1alpha:fetchConfig#x",
            "/https://example.com/v1internal:loadCodeAssist",
            "/http://cloudcode-pa.googleapis.com/v1internal:loadCodeAssist",
            "/https://user@cloudcode-pa.googleapis.com/v1internal:x",
            "//cloudcode-pa.googleapis.com/x",
            "/a/../../x",
        ] {
            assert!(ag.url(invalid).is_err(), "{invalid}");
        }
    }

    #[tokio::test]
    async fn the_saved_login_routes_by_email_then_default_and_others_need_a_lookup() {
        let _login = TEST_LOGIN.lock().await;
        set_test_login(TEST_SAVED_TOKEN, "saved-refresh");
        let learned = RefreshRoutes::default();
        let file_only = config(
            "antigravity:\n  routing:\n    account:\n      default: us\n      bob@example.com: jp\n",
        );
        let route = file_only
            .antigravity
            .resolve(
                &headers(TEST_SAVED_TOKEN),
                &learned,
                &RenewedTokens::default(),
            )
            .await
            .unwrap();
        assert!(route.matched_login && route.configured && !route.needs_profile);
        assert_eq!(route.proxy.label(), "us");
        let error = file_only
            .antigravity
            .resolve(&headers("other"), &learned, &RenewedTokens::default())
            .await
            .err()
            .unwrap();
        assert_eq!(error.status, 401);
        assert!(
            file_only
                .antigravity
                .resolve(&HeaderMap::new(), &learned, &RenewedTokens::default())
                .await
                .is_err()
        );

        // With a probe route, the unnamed saved login is looked up first.
        let probed = config(
            "antigravity:\n  account_auth_file_only: false\n  routing:\n    account:\n      default: us\n      bob@example.com: jp\n    account_probe: us\n",
        );
        let mut route = probed
            .antigravity
            .resolve(
                &headers(TEST_SAVED_TOKEN),
                &learned,
                &RenewedTokens::default(),
            )
            .await
            .unwrap();
        assert!(route.needs_profile && route.matched_login);
        assert_eq!(route.refresh_token.as_deref(), Some("saved-refresh"));
        let bob = AntigravityIdentity::userinfo(&json!({"email": "Bob@Example.com"})).unwrap();
        probed
            .antigravity
            .apply_profile(&mut route, bob.clone())
            .unwrap();
        assert_eq!(route.proxy.label(), "jp");
        // Once learned for the login's refresh token, no lookup is needed,
        // whichever access token it holds.
        learned.learn("saved-refresh", &bob).unwrap();
        set_test_login("renewed-token", "saved-refresh");
        let renewed = probed
            .antigravity
            .resolve(
                &headers("renewed-token"),
                &learned,
                &RenewedTokens::default(),
            )
            .await
            .unwrap();
        assert!(!renewed.needs_profile && renewed.matched_login);
        assert_eq!(renewed.proxy.label(), "jp");
        assert_eq!(
            probed.antigravity.login_label(renewed.identity.as_ref()),
            Some("bob@example.com".into())
        );
        // Another login is not taken for the learned account.
        set_test_login("switched-token", "other-refresh");
        let switched = probed
            .antigravity
            .resolve(
                &headers("switched-token"),
                &learned,
                &RenewedTokens::default(),
            )
            .await
            .unwrap();
        assert!(switched.needs_profile && switched.identity.is_none());
        set_test_login(TEST_SAVED_TOKEN, "saved-refresh");
        let mut other = probed
            .antigravity
            .resolve(&headers("other"), &learned, &RenewedTokens::default())
            .await
            .unwrap();
        assert!(other.needs_profile && !other.matched_login && !other.configured);
        let carol = AntigravityIdentity::userinfo(&json!({"email": "carol@example.com"})).unwrap();
        // Another token neither takes the saved login's default nor a fallback.
        assert!(probed.antigravity.apply_profile(&mut other, carol).is_err());
        assert!(AntigravityIdentity::userinfo(&json!({"name": "x"})).is_err());
    }

    #[test]
    fn relayed_refreshes_name_their_new_access_tokens_until_they_expire() {
        let renewed = RenewedTokens::default();
        // {"email":"remote@example.invalid","sub":"1"}
        let named = br#"{"access_token":"named","expires_in":3599,"id_token":"h.eyJlbWFpbCI6InJlbW90ZUBleGFtcGxlLmludmFsaWQiLCJzdWIiOiIxIn0.s"}"#;
        let carol = AntigravityIdentity::new("carol@example.invalid");
        renewed.learn_response(named, "refresh", carol.clone());
        let token = renewed.get("named").unwrap();
        // The ID token names the account ahead of the refresh route's.
        assert_eq!(token.identity.unwrap().email, "remote@example.invalid");
        let login = |refresh: &str| Login {
            access_token: "stored".into(),
            refresh_token: Some(refresh.into()),
            identity: None,
        };
        assert!(token_renews(&renewed, "named", &login("refresh")));
        assert!(!token_renews(&renewed, "named", &login("other")));
        // Without an ID token, the route's account; an expired, malformed or
        // tokenless response names nothing.
        renewed.learn_response(br#"{"access_token":"plain"}"#, "refresh", carol);
        assert_eq!(
            renewed.get("plain").unwrap().identity.unwrap().email,
            "carol@example.invalid"
        );
        renewed.learn_response(
            br#"{"access_token":"spent","expires_in":0}"#,
            "refresh",
            None,
        );
        assert!(renewed.get("spent").is_none());
        renewed.learn_response(b"not json", "refresh", None);
        renewed.learn_response(br#"{"expires_in":3599}"#, "refresh", None);
        assert!(renewed.get("unknown").is_none());
        // Bounded: the soonest to expire make room.
        for n in 0..RenewedTokens::LIMIT + 8 {
            let body = format!(r#"{{"access_token":"t{n}","expires_in":{}}}"#, 60 + n);
            renewed.learn_response(body.as_bytes(), "refresh", None);
        }
        let held = renewed.tokens.lock().unwrap().len();
        assert_eq!(held, RenewedTokens::LIMIT);
        assert!(renewed.get("t0").is_none());
        assert!(
            renewed
                .get(&format!("t{}", RenewedTokens::LIMIT + 7))
                .is_some()
        );
        // Only digests are kept.
        assert!(
            !renewed
                .tokens
                .lock()
                .unwrap()
                .keys()
                .any(|key| key.contains("t1"))
        );
    }

    fn token_renews(renewed: &RenewedTokens, token: &str, login: &Login) -> bool {
        renewed.get(token).is_some_and(|r| r.renews(login))
    }

    // Requests with the saved login's current token reuse a recent read, so
    // they start no `security` process; any other token reads the item again
    // at once, so Antigravity's renewal applies to its next request.
    #[tokio::test]
    async fn the_current_token_reuses_a_recent_read_and_others_read_the_item_again() {
        let _login = TEST_LOGIN.lock().await;
        set_test_login("current-access", "saved-refresh");
        let learned = RefreshRoutes::default();
        let renewed = RenewedTokens::default();
        let settings = config("antigravity:\n  routing:\n    account:\n      default: us\n");
        let resolve = |token: &'static str| {
            let (settings, learned, renewed) = (&settings, &learned, &renewed);
            async move {
                settings
                    .antigravity
                    .resolve(&headers(token), learned, renewed)
                    .await
            }
        };
        let loads = crate::keychain::test_loads(SERVICE);
        for _ in 0..3 {
            assert!(resolve("current-access").await.unwrap().matched_login);
        }
        assert_eq!(crate::keychain::test_loads(SERVICE), loads + 1);

        // Antigravity renews and saves its token, unknown to the recent read.
        let renewed_login =
            json!({"token": {"access_token": "next-access", "refresh_token": "saved-refresh"}});
        crate::keychain::write_test_entry_elsewhere(SERVICE, ACCOUNT, &renewed_login.to_string());
        assert!(resolve("next-access").await.unwrap().matched_login);
        assert_eq!(crate::keychain::test_loads(SERVICE), loads + 2);
        // The replaced token no longer matches once the item was read again.
        assert_eq!(resolve("current-access").await.err().unwrap().status, 401);
        assert!(resolve("next-access").await.unwrap().matched_login);
        assert_eq!(crate::keychain::test_loads(SERVICE), loads + 3);
        set_test_login(TEST_SAVED_TOKEN, "saved-refresh");
    }
}
