use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};
use url::Url;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum Choice {
    One(String),
    List(Vec<String>),
}
impl Choice {
    pub fn names(&self) -> &[String] {
        match self {
            Self::One(x) => std::slice::from_ref(x),
            Self::List(x) => x,
        }
    }
    pub fn label(&self) -> String {
        self.names().join(", ")
    }
    pub fn direct() -> Self {
        Self::One("none".into())
    }
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Bases {
    #[serde(default = "account_base")]
    pub account: String,
    #[serde(default = "api_base")]
    pub api_key: String,
}
fn account_base() -> String {
    "https://chatgpt.com/backend-api".into()
}
fn api_base() -> String {
    "https://api.openai.com/v1".into()
}
impl Default for Bases {
    fn default() -> Self {
        Self {
            account: account_base(),
            api_key: api_base(),
        }
    }
}
#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Routing {
    #[serde(default)]
    pub account: BTreeMap<String, Choice>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub api_key: BTreeMap<String, Choice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_fallback: Option<Choice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_probe: Option<Choice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key_fallback: Option<Choice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp_fallback: Option<Choice>,
}
#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AccountSource {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_env: Option<String>,
}
impl AccountSource {
    pub(crate) fn validate(&self) -> Result<()> {
        if self.auth_file.is_some() == self.auth_env.is_some()
            || self.auth_file.as_ref().is_some_and(|s| s.trim().is_empty())
        {
            return Err(Error::config(
                "An account must select one credential source.",
            ));
        }
        if let Some(name) = &self.auth_env {
            validate_env(name)?;
        }
        Ok(())
    }
}
pub(crate) fn validate_env(name: &str) -> Result<()> {
    if name.is_empty()
        || !name
            .bytes()
            .enumerate()
            .all(|(i, b)| b == b'_' || b.is_ascii_alphabetic() || i > 0 && b.is_ascii_digit())
    {
        return Err(Error::config(
            "Credential selectors must be environment variable names.",
        ));
    }
    Ok(())
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Codex {
    pub homes: Vec<String>,
    pub base_url: Bases,
    pub accounts: BTreeMap<String, AccountSource>,
    pub routing: Routing,
    pub account_auth_file_only: bool,
    #[serde(skip)]
    pub providers: Vec<Provider>,
}
impl Default for Codex {
    fn default() -> Self {
        Self {
            homes: default_codex_homes(),
            base_url: Bases::default(),
            accounts: BTreeMap::new(),
            routing: Routing::default(),
            account_auth_file_only: true,
            providers: Vec::new(),
        }
    }
}
/// A non-URL `codex.routing.api_key` selector: a Codex provider ID or an API Key
/// environment variable name, with its proxy choice.
#[derive(Clone)]
pub struct Provider {
    pub selector: String,
    pub proxy: Choice,
}
impl Provider {
    pub fn label(&self) -> &str {
        &self.selector
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    #[serde(default)]
    codex: Codex,
    #[serde(default)]
    claude: crate::claude::Claude,
    listen_port: u16,
    request_timeout_seconds: f64,
    #[serde(default)]
    websocket: WebSocketTimeouts,
    #[serde(default)]
    proxies: BTreeMap<String, String>,
    #[serde(default)]
    connect: BTreeMap<String, Choice>,
}

/// Responses WebSockets use protocol phases, independently of HTTP/CONNECT idle time.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebSocketTimeouts {
    pub first_message_seconds: f64,
    pub first_output_seconds: f64,
    pub read_seconds: f64,
    pub write_seconds: f64,
    pub inter_turn_idle_seconds: f64,
}
impl Default for WebSocketTimeouts {
    fn default() -> Self {
        Self {
            first_message_seconds: 30.0,
            first_output_seconds: 900.0,
            read_seconds: 900.0,
            write_seconds: 120.0,
            inter_turn_idle_seconds: 300.0,
        }
    }
}
impl WebSocketTimeouts {
    fn validate(&self) -> Result<()> {
        if [
            self.first_message_seconds,
            self.first_output_seconds,
            self.read_seconds,
            self.write_seconds,
        ]
        .iter()
        .any(|n| !n.is_finite() || !(1.0..=3600.0).contains(n))
            || !self.inter_turn_idle_seconds.is_finite()
            || !(0.0..=3600.0).contains(&self.inter_turn_idle_seconds)
        {
            return Err(Error::config(
                "WebSocket timeouts must be 1–3600 seconds; inter-turn idle may also be 0 to disable.",
            ));
        }
        Ok(())
    }
}
/// Top-level keys of the retired configuration layout, now rejected with a
/// pointer to the `codex` section instead of a generic unknown-field error.
const RETIRED_TOP_LEVEL: [&str; 11] = [
    "auth_file",
    "account_auth_file_only",
    "base_url",
    "routing",
    "account_upstream_base_url",
    "upstream_base_url",
    "api_key_upstream_base_url",
    "accounts",
    "api_key_providers",
    "openai_fallback_proxy",
    "mcp_fallback_proxy",
];
#[derive(Clone)]
pub struct Config {
    pub codex: Codex,
    pub claude: crate::claude::Claude,
    pub listen_port: u16,
    pub request_timeout_seconds: f64,
    pub websocket: WebSocketTimeouts,
    pub proxies: BTreeMap<String, String>,
    pub connect: BTreeMap<String, Choice>,
}
impl Config {
    pub fn parse(text: &str) -> Result<Self> {
        let mut root: serde_yaml_ng::Value = serde_yaml_ng::from_str(text)
            .map_err(|_| Error::config("Invalid YAML configuration."))?;
        if RETIRED_TOP_LEVEL.iter().any(|key| root.get(*key).is_some()) {
            return Err(Error::config(
                "Top-level Codex settings are not supported; place them under codex.",
            ));
        }
        if let Some(codex) = root.get_mut("codex") {
            normalize_auth(codex)?;
        }
        let raw: Raw = serde_yaml_ng::from_value(root).map_err(|_| {
            Error::config(
                "Invalid YAML configuration; check required fields against config.example.yaml.",
            )
        })?;
        let mut codex = raw.codex;
        codex.providers = codex
            .routing
            .api_key
            .iter()
            .filter(|(key, _)| !crate::url_routing::is_url_selector(key))
            .map(|(selector, proxy)| Provider {
                selector: selector.clone(),
                proxy: proxy.clone(),
            })
            .collect();
        let c = Self {
            codex,
            claude: raw.claude,
            listen_port: raw.listen_port,
            request_timeout_seconds: raw.request_timeout_seconds,
            websocket: raw.websocket,
            proxies: raw.proxies,
            connect: raw.connect,
        };
        c.validate()?;
        Ok(c)
    }
    pub fn read(path: &std::path::Path) -> Result<Self> {
        Self::read_with_overrides(path, &[])
    }
    /// Apply dotted YAML overrides in order before configuration validation.
    /// The source file is never modified.
    pub fn read_with_overrides(path: &std::path::Path, overrides: &[String]) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|_| Error::config("Cannot read configuration file."))?;
        if overrides.is_empty() {
            return Self::parse(&text);
        }
        let mut root: serde_yaml_ng::Value = serde_yaml_ng::from_str(&text)
            .map_err(|_| Error::config("Invalid YAML configuration."))?;
        for entry in overrides {
            apply_override(&mut root, entry)?;
        }
        Self::parse(
            &serde_yaml_ng::to_string(&root)
                .map_err(|_| Error::config("Cannot apply configuration overrides."))?,
        )
    }
    fn validate(&self) -> Result<()> {
        self.claude.validate(self)?;
        let mut authorities = std::collections::HashSet::new();
        for (authority, choice) in &self.connect {
            let (host, port) = crate::tunnel::authority(authority)?;
            if !authorities.insert((host, port)) {
                return Err(Error::config("Duplicate CONNECT destination."));
            }
            self.validate_choice(choice)?;
        }
        validate_directories(&self.codex.homes, &self.codex.accounts)?;
        if self.codex.routing.account_probe.is_some() {
            return Err(Error::config(
                "account_probe is only supported under claude.routing.",
            ));
        }
        if self.listen_port == 0
            || !self.request_timeout_seconds.is_finite()
            || !(1.0..=3600.0).contains(&self.request_timeout_seconds)
        {
            return Err(Error::config("Invalid port or timeout (1–3600 seconds)."));
        }
        self.websocket.validate()?;
        crate::url_routing::validate_routes(&self.codex.routing.api_key)?;
        let account = validate_upstream(&self.codex.base_url.account)?;
        if account.path().trim_matches('/') == "backend-api/codex" {
            return Err(Error::config(
                "codex.base_url.account is the ChatGPT /backend-api root, without /codex.",
            ));
        }
        validate_upstream(&self.codex.base_url.api_key)?;
        for (label, source) in &self.codex.accounts {
            if label.trim().is_empty() {
                return Err(Error::config("Empty Codex account label."));
            }
            source.validate()?;
        }
        let accounts =
            !self.codex.routing.account.is_empty() || self.codex.routing.account_fallback.is_some();
        if (!self.codex.accounts.is_empty() && !accounts)
            || (self.codex.account_auth_file_only
                && accounts
                && self.codex.accounts.is_empty()
                && self.codex.homes.is_empty())
        {
            return Err(Error::config(
                "Codex routing requires account sources and account mappings or account_fallback.",
            ));
        }
        for (name, endpoint) in &self.proxies {
            if name.is_empty() || name == "none" {
                return Err(Error::config("Invalid proxy name; none is reserved."));
            }
            if endpoint != "none" {
                validate_proxy(endpoint)?;
            }
        }
        for (key, choice) in self
            .codex
            .routing
            .account
            .iter()
            .chain(self.codex.routing.api_key.iter())
        {
            if key.is_empty() {
                return Err(Error::config("Empty routing identifier."));
            }
            self.validate_choice(choice)?;
        }
        for c in [
            &self.codex.routing.account_fallback,
            &self.codex.routing.api_key_fallback,
            &self.codex.routing.mcp_fallback,
        ]
        .into_iter()
        .flatten()
        {
            self.validate_choice(c)?;
        }
        Ok(())
    }
    pub(crate) fn validate_choice(&self, c: &Choice) -> Result<()> {
        if c.names().is_empty()
            || c.names()
                .iter()
                .any(|n| n != "none" && !self.proxies.contains_key(n))
        {
            return Err(Error::config(
                "Every route must select an existing proxy or none; lists cannot be empty.",
            ));
        }
        Ok(())
    }
    pub fn endpoint(&self, name: &str) -> &str {
        if name == "none" {
            "none"
        } else {
            &self.proxies[name]
        }
    }
}

pub(crate) fn default_codex_homes() -> Vec<String> {
    vec!["~/.codex".into()]
}

pub(crate) fn default_claude_config_dirs() -> Vec<String> {
    vec!["~/.claude".into()]
}

pub(crate) fn validate_directories(
    directories: &[String],
    accounts: &BTreeMap<String, AccountSource>,
) -> Result<()> {
    let mut paths = std::collections::HashSet::new();
    for directory in directories {
        let path = expand(directory);
        if directory.trim().is_empty() || !path.is_absolute() || !paths.insert(path) {
            return Err(Error::config(
                "Credential directories must be unique absolute paths.",
            ));
        }
    }
    for source in accounts.values() {
        if let Some(file) = &source.auth_file {
            let path = std::path::Path::new(file);
            if path.is_absolute() || file.starts_with('~') {
                return Err(Error::config(
                    "auth_file must be a file name relative to the credential directories.",
                ));
            }
            if directories.is_empty() {
                return Err(Error::config(
                    "auth_file requires a configured credential directory.",
                ));
            }
            if path.components().any(|part| {
                !matches!(
                    part,
                    std::path::Component::Normal(_) | std::path::Component::CurDir
                )
            }) {
                return Err(Error::config(
                    "auth_file must stay inside its credential directory.",
                ));
            }
        }
    }
    Ok(())
}

// Keep the source label separate from its directory: every listed directory can
// supply the same relative auth_file, while account ID/email still selects routing.
pub(crate) type DirectoryAccount = (String, AccountSource, Option<PathBuf>);

pub(crate) fn directory_accounts(
    accounts: &BTreeMap<String, AccountSource>,
    directories: &[String],
    routing: &Routing,
    filename: &str,
) -> Vec<DirectoryAccount> {
    let mut accounts = accounts.clone();
    if accounts.is_empty() && (!routing.account.is_empty() || routing.account_fallback.is_some()) {
        accounts.insert(
            "default".into(),
            AccountSource {
                auth_file: Some(filename.into()),
                auth_env: None,
            },
        );
    }
    let mut sources = Vec::new();
    for (label, source) in accounts {
        if let Some(file) = &source.auth_file {
            for directory in directories {
                let directory = expand(directory);
                let mut resolved = source.clone();
                resolved.auth_file = Some(directory.join(file).to_string_lossy().into_owned());
                sources.push((label.clone(), resolved, Some(directory)));
            }
        } else {
            sources.push((label, source, None));
        }
    }
    sources
}

impl Codex {
    pub(crate) fn account_sources(&self) -> Vec<DirectoryAccount> {
        directory_accounts(&self.accounts, &self.homes, &self.routing, "auth.json")
    }
}

fn apply_override(root: &mut serde_yaml_ng::Value, entry: &str) -> Result<()> {
    use serde_yaml_ng::{Mapping, Value};

    let (path, text) = entry
        .split_once('=')
        .ok_or(Error::config("Expected -c PATH=VALUE."))?;
    let mut segments = Vec::new();
    let mut segment = String::new();
    let mut chars = path.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => match chars.next() {
                Some(escaped @ ('.' | '\\')) => segment.push(escaped),
                _ => {
                    return Err(Error::config(
                        "Only dots and backslashes can be escaped in -c paths.",
                    ));
                }
            },
            '.' => segments.push(std::mem::take(&mut segment)),
            _ => segment.push(ch),
        }
    }
    segments.push(segment);
    if segments.iter().any(|s| s.trim().is_empty()) {
        return Err(Error::config("A -c path cannot contain empty components."));
    }
    // An empty assignment is an empty string; use explicit null for YAML null.
    let value = if text.is_empty() {
        Value::String(String::new())
    } else {
        serde_yaml_ng::from_str(text).map_err(|_| Error::config("Invalid YAML value in -c."))?
    };
    let mut current = root;
    for (index, key) in segments.iter().enumerate() {
        let map = current.as_mapping_mut().ok_or(Error::config(
            "A -c path must traverse mappings; replace scalar or list values as a whole.",
        ))?;
        if index + 1 == segments.len() {
            map.insert(Value::String(key.clone()), value);
            return Ok(());
        }
        current = map
            .entry(Value::String(key.clone()))
            .or_insert_with(|| Value::Mapping(Mapping::new()));
    }
    Ok(())
}

/// Turn a flat `auth_file`/`auth_env` into the single `default` account
/// source used internally. Nested `accounts` maps are not accepted.
pub(crate) fn normalize_auth(value: &mut serde_yaml_ng::Value) -> Result<()> {
    let Some(map) = value.as_mapping_mut() else {
        return Ok(());
    };
    if map.contains_key("accounts") {
        return Err(Error::config(
            "Nested accounts are not supported; use auth_file or auth_env.",
        ));
    }
    if !map.contains_key("auth_file") && !map.contains_key("auth_env") {
        return Ok(());
    }
    let mut source = serde_yaml_ng::Mapping::new();
    for key in ["auth_file", "auth_env"] {
        if let Some(value) = map.remove(key) {
            source.insert(key.into(), value);
        }
    }
    map.insert(
        "accounts".into(),
        serde_yaml_ng::Value::Mapping(serde_yaml_ng::Mapping::from_iter([(
            "default".into(),
            serde_yaml_ng::Value::Mapping(source),
        )])),
    );
    Ok(())
}

pub fn expand(path: &str) -> PathBuf {
    if path == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from("~"));
    }
    if let Some(rest) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        return dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("~"))
            .join(rest);
    }
    PathBuf::from(path)
}
pub fn validate_upstream(value: &str) -> Result<Url> {
    let bad = || {
        Error::config(
            "Upstream must be HTTPS with a public hostname and no credentials, query or fragment.",
        )
    };
    let u = Url::parse(value).map_err(|_| bad())?;
    let host = u.host_str().ok_or_else(bad)?;
    if u.scheme() != "https"
        || !u.username().is_empty()
        || u.password().is_some()
        || u.query().is_some()
        || u.fragment().is_some()
        || u.port() == Some(0)
        || !host.contains('.')
        || host.contains(':')
        || host.chars().all(|c| c.is_ascii_digit() || c == '.')
        || host.ends_with('.')
        || ["localhost", "local", "internal", "lan"]
            .iter()
            .any(|s| host == *s || host.ends_with(&format!(".{s}")))
    {
        return Err(bad());
    }
    Ok(u)
}
// Codex custom providers may point at a public IPv4 HTTPS endpoint, like
// explicit API routes; private, loopback and reserved addresses stay rejected.
// Values without a scheme keep the strict hostname check.
fn validate_provider_upstream(value: &str) -> Result<()> {
    if value.contains("://") {
        crate::url_routing::validate_upstream(value).map(|_| ())
    } else {
        validate_upstream(value).map(|_| ())
    }
}
pub fn unwrap_upstream(value: &str) -> Result<String> {
    if let Ok(u) = Url::parse(value) {
        // The listener also serves TLS, so the wrapper may use either scheme.
        if matches!(u.scheme(), "http" | "https")
            && u.host_str() == Some("127.0.0.1")
            && u.port().is_some()
            && u.username().is_empty()
            && u.password().is_none()
            && u.query().is_none()
            && u.fragment().is_none()
            && u.path().starts_with("/https://")
        {
            let inner = &u.path()[1..];
            validate_provider_upstream(inner)?;
            return Ok(inner.into());
        }
    }
    validate_provider_upstream(value)?;
    Ok(value.into())
}
pub fn validate_proxy(value: &str) -> Result<Url> {
    let bad = || {
        Error::config(
            "Invalid proxy URL or credentials; use http/https/socks5://[username:password@]host:port.",
        )
    };
    let u = Url::parse(value).map_err(|_| bad())?;
    // Url normalizes default ports away; inspect the authority to require an explicit port.
    let authority = value
        .split_once("://")
        .map(|(_, v)| v.split('/').next().unwrap_or(""))
        .unwrap_or("");
    let explicit_port = authority
        .rsplit('@')
        .next()
        .unwrap_or("")
        .rsplit_once(':')
        .and_then(|(_, p)| p.parse::<u16>().ok());
    if !matches!(u.scheme(), "http" | "https" | "socks5")
        || u.host_str().is_none()
        || explicit_port.is_none_or(|p| p == 0)
        || u.query().is_some()
        || u.fragment().is_some()
        || !matches!(u.path(), "" | "/")
    {
        return Err(bad());
    }
    if authority.contains('@') {
        let user = percent_encoding::percent_decode_str(u.username())
            .decode_utf8()
            .map_err(|_| bad())?;
        // Url normalizes an explicitly empty password to None. Preserve the raw
        // separator so HTTP user: remains valid while user@ remains invalid.
        let raw_password = authority
            .rsplit_once('@')
            .and_then(|(userinfo, _)| userinfo.split_once(':').map(|(_, password)| password))
            .ok_or_else(bad)?;
        let password = percent_encoding::percent_decode_str(raw_password)
            .decode_utf8()
            .map_err(|_| bad())?;
        if user.is_empty()
            || user.chars().chain(password.chars()).any(|c| c.is_control())
            || (u.scheme() != "socks5" && user.contains(':'))
            || (u.scheme() == "socks5"
                && (!(1..=255).contains(&user.len()) || !(1..=255).contains(&password.len())))
        {
            return Err(bad());
        }
    }
    Ok(u)
}
pub fn redacted_endpoint(value: &str) -> String {
    if value == "none" {
        return value.into();
    }
    // Preserve an explicitly specified default port and omit userinfo entirely.
    match value.split_once("://") {
        Some((scheme, rest)) => format!(
            "{scheme}://{}",
            rest.rsplit('@').next().unwrap_or("").trim_end_matches('/')
        ),
        None => "<invalid-proxy>".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn websocket_phase_timeouts_are_independent_and_validated() {
        let config = Config::parse("listen_port: 8787\nrequest_timeout_seconds: 3\n").unwrap();
        assert_eq!(config.websocket.first_output_seconds, 900.0);
        let config = Config::parse("listen_port: 8787\nrequest_timeout_seconds: 3\nwebsocket:\n  read_seconds: 1200\n  inter_turn_idle_seconds: 0\n").unwrap();
        assert_eq!(config.websocket.read_seconds, 1200.0);
        assert_eq!(config.websocket.inter_turn_idle_seconds, 0.0);
        for line in [
            "write_seconds: 0",
            "first_output_seconds: .nan",
            "read_seconds: 3601",
            "inter_turn_idle_seconds: -1",
            "unknown: 2",
        ] {
            assert!(
                Config::parse(&format!(
                    "listen_port: 8787\nrequest_timeout_seconds: 3\nwebsocket:\n  {line}\n"
                ))
                .is_err()
            );
        }
    }
    const BASE: &str = "listen_port: 8787\nrequest_timeout_seconds: 3\n";

    #[test]
    fn directory_lists_default_only_when_omitted_and_preserve_empty_lists() {
        for extra in [
            "",
            "codex: {}\nclaude: {}\n",
            "codex:\n  auth_file: auth.json\n  routing:\n    account: {default: none}\nclaude:\n  auth_file: .credentials.json\n  routing:\n    account: {default: none}\n",
        ] {
            let c = Config::parse(&format!("{BASE}{extra}")).unwrap();
            assert_eq!(c.codex.homes, ["~/.codex"]);
            assert_eq!(c.claude.config_dirs, ["~/.claude"]);
        }
        let c = Config::parse(&format!(
            "{BASE}codex:\n  homes: []\nclaude:\n  config_dirs: []\n"
        ))
        .unwrap();
        assert!(c.codex.homes.is_empty());
        assert!(c.claude.config_dirs.is_empty());
        assert!(c.codex.account_sources().is_empty());
        assert!(c.claude.account_sources().is_empty());
    }

    #[test]
    fn auth_files_are_relative_to_each_credential_directory() {
        for (service, field) in [("codex", "homes"), ("claude", "config_dirs")] {
            let base = format!("{BASE}{service}:\n");
            let valid = format!(
                "{base}  {field}: [~/work, ~/other]\n  auth_file: nested/login.json\n  routing:\n    account: {{default: none}}\n"
            );
            let c = Config::parse(&valid).unwrap();
            let sources = if service == "codex" {
                c.codex.account_sources()
            } else {
                c.claude.account_sources()
            };
            let files: Vec<_> = sources
                .iter()
                .map(|(label, source, _)| {
                    (
                        label.as_str(),
                        PathBuf::from(source.auth_file.as_ref().unwrap()),
                    )
                })
                .collect();
            // Compare path components: Windows accepts both slash styles.
            assert_eq!(
                files,
                [
                    ("default", expand("~/work/nested/login.json")),
                    ("default", expand("~/other/nested/login.json")),
                ]
            );
            for invalid in [
                format!("{base}  {field}: [relative/path]\n"),
                format!("{base}  {field}: [~/same, ~/same]\n"),
                format!(
                    "{base}  {field}: []\n  auth_file: login.json\n  routing:\n    account: {{default: none}}\n"
                ),
                valid.replace("nested/login.json", "../login.json"),
                valid.replace("nested/login.json", "/absolute/login.json"),
                valid.replace("nested/login.json", "~/login.json"),
                format!("{base}  {field}: {{work: ~/work}}\n"),
            ] {
                assert!(Config::parse(&invalid).is_err(), "{invalid}");
            }
        }
    }

    #[test]
    fn rejects_retired_layouts() {
        for key in RETIRED_TOP_LEVEL {
            let text = format!("{BASE}{key}: {{}}\n");
            assert!(
                Config::parse(&text).is_err_and(|e| e.message.contains("under codex")),
                "{key}"
            );
        }
        for invalid in [
            "codex:\n  accounts:\n    a: {auth_file: auth.json}\n  routing:\n    account: {a: none}\n",
            "claude:\n  accounts:\n    a: {auth_file: .credentials.json}\n  routing:\n    account: {a: none}\n",
            "claude:\n  accounts:\n    a: {auth_file: .credentials.json, proxy: none}\n",
            "claude:\n  api_key: {ANTHROPIC_API_KEY: none}\n",
            "claude:\n  account_fallback: none\n",
            "claude:\n  api_key_fallback: none\n",
            "claude:\n  base_url: {account: https://api.anthropic.com, api_key: https://api.anthropic.com}\n",
            "codex:\n  base_url: {account: https://chatgpt.com/backend-api/codex}\n",
        ] {
            assert!(
                Config::parse(&format!("{BASE}{invalid}")).is_err(),
                "{invalid}"
            );
        }
    }

    #[test]
    fn rejects_invalid_layouts_and_proxy_choices() {
        for invalid in [
            "unknown: value\n",
            "codex:\n  routing:\n    api_key_fallback: []\n",
            "codex:\n  routing:\n    api_key_fallback: missing\n",
            "codex:\n  routing:\n    unknown: none\n",
            "codex:\n  typo: true\n",
            "codex:\n  homes: []\n  routing:\n    account: {test: none}\n",
            "codex:\n  routing:\n    api_key:\n      TEST:\n        proxy: none\n",
            "codex:\n  auth_file: auth.json\n  auth_env: TOKEN\n  routing:\n    account: {default: none}\n",
            "claude:\n  config_dirs: []\n  routing:\n    account: {absent: none}\n",
            "claude:\n  routing:\n    mcp_fallback: none\n",
        ] {
            assert!(
                Config::parse(&format!("{BASE}{invalid}")).is_err(),
                "{invalid}"
            );
        }
        assert!(
            Config::parse(&format!(
                "{BASE}codex:\n  account_auth_file_only: false\n  routing:\n    account_fallback: none\n"
            ))
            .is_ok()
        );
        assert!(Config::parse("listen_port: 0\nrequest_timeout_seconds: .nan\n").is_err());
    }

    #[test]
    fn proxy_credentials_ports_and_upstream_validation() {
        for p in [
            "http://u:p@localhost:80",
            "https://localhost:443",
            "socks5://u:p@127.0.0.1:1080",
            "http://u:@localhost:8080",
        ] {
            assert!(validate_proxy(p).is_ok(), "{p}");
        }
        for p in [
            "http://localhost",
            "http://localhost:0",
            "http://u@localhost:80",
            "http://u%3Ax:p@localhost:80",
            "socks5://u:@localhost:1080",
            "http://u:p%0A@localhost:80",
            "https://localhost:443/extra",
            "https://localhost:443?q=1",
        ] {
            assert!(validate_proxy(p).is_err(), "{p}");
        }
        for u in [
            "http://api.openai.com/v1",
            "https://127.0.0.1/v1",
            "https://2130706433/v1",
            "https://private.local/v1",
            "https://host.internal/v1",
            "https://[::1]/v1",
            "https://u:p@api.openai.com/v1",
            "https://api.openai.com/v1?q=1",
        ] {
            assert!(validate_upstream(u).is_err(), "{u}");
        }
        for wrapper in [
            "http://127.0.0.1:7889/https://provider.invalid/v1",
            "https://127.0.0.1:7889/https://provider.invalid/v1",
        ] {
            assert_eq!(
                unwrap_upstream(wrapper).unwrap(),
                "https://provider.invalid/v1"
            );
        }
        // Provider upstreams accept public IPv4 HTTPS addresses, directly or
        // through the local wrapper form, while base URLs stay hostname-only.
        assert_eq!(
            unwrap_upstream("https://182.92.106.196:6060").unwrap(),
            "https://182.92.106.196:6060"
        );
        assert_eq!(
            unwrap_upstream("https://127.0.0.1:7889/https://182.92.106.196:6060/v1").unwrap(),
            "https://182.92.106.196:6060/v1"
        );
        assert!(validate_upstream("https://182.92.106.196:6060").is_err());
        for u in [
            "https://10.0.0.1:6060",
            "https://192.168.1.2/v1",
            "https://127.0.0.1:6060",
            "https://169.254.1.1",
            "http://182.92.106.196:6060",
            "https://u:p@182.92.106.196:6060",
            "182.92.106.196:6060",
            "http://127.0.0.1:7889/https://10.0.0.1/v1",
        ] {
            assert!(unwrap_upstream(u).is_err(), "{u}");
        }
        assert_eq!(
            redacted_endpoint("https://u:p@proxy.invalid:443"),
            "https://proxy.invalid:443"
        );
    }
    #[test]
    fn sections_parse_flat_credentials_and_api_key_selectors() {
        let text = r#"
listen_port: 8787
request_timeout_seconds: 30
proxies:
  selected: http://127.0.0.1:7893
codex:
  auth_file: auth.json
  routing:
    account: {default: selected}
    api_key: {OPENAI_API_KEY: selected, 'provider.invalid/v1': none}
claude:
  auth_env: CLAUDE_TOKEN
  routing:
    account: {default: selected}
    api_key: {ANTHROPIC_API_KEY: selected}
"#;
        let c = Config::parse(text).unwrap();
        // URL selectors route explicit upstreams; only the others are providers.
        assert_eq!(c.codex.providers.len(), 1);
        assert_eq!(c.codex.providers[0].label(), "OPENAI_API_KEY");
        assert_eq!(
            c.codex.accounts["default"].auth_file.as_deref(),
            Some("auth.json")
        );
        assert_eq!(
            c.claude.accounts["default"].auth_env.as_deref(),
            Some("CLAUDE_TOKEN")
        );
        assert_eq!(c.claude.base_url, "https://api.anthropic.com");
    }

    #[test]
    fn proxy_candidate_lists_keep_their_order() {
        let text = r#"
listen_port: 8787
request_timeout_seconds: 30
proxies:
  jp_lab: http://127.0.0.1:7893
  jp: http://127.0.0.1:7892
codex:
  routing:
    api_key: {TOKEN: [jp_lab, jp]}
    api_key_fallback: [jp, none]
"#;
        let c = Config::parse(text).unwrap();
        assert_eq!(c.codex.routing.api_key["TOKEN"].names(), ["jp_lab", "jp"]);
        assert_eq!(
            c.codex.routing.api_key_fallback.unwrap().names(),
            ["jp", "none"]
        );
    }

    #[tokio::test]
    async fn namespaced_codex_url_routes_are_not_credential_sources() {
        let c = Config::parse(&format!(
            "{BASE}codex:\n  routing:\n    api_key:\n      'provider.invalid/v1': none\n"
        ))
        .unwrap();
        assert!(c.codex.providers.is_empty());
        c.check_credentials().await.unwrap();
        assert!(
            c.resolve_url(
                Some("Bearer key"),
                "/codex/https://provider.invalid/v1/responses"
            )
            .unwrap()
            .is_some()
        );
    }
}
