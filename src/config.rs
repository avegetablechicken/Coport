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
        if self.auth_file.is_some() && self.auth_env.is_some()
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
            base_url: Bases::default(),
            accounts: BTreeMap::new(),
            routing: Routing::default(),
            account_auth_file_only: true,
            providers: Vec::new(),
        }
    }
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provider {
    pub name: Option<String>,
    pub api_key_env: Option<String>,
    pub api_key_file: Option<String>,
    pub upstream_base_url: Option<String>,
    pub proxy: Choice,
    #[serde(skip)]
    pub selector: Option<String>,
}
impl Provider {
    pub fn label(&self) -> &str {
        self.selector
            .as_deref()
            .or(self.name.as_deref())
            .or(self.api_key_env.as_deref())
            .unwrap_or("")
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    codex: Option<Codex>,
    #[serde(default)]
    claude: crate::claude::Claude,
    listen_port: u16,
    request_timeout_seconds: f64,
    auth_file: Option<String>,
    account_auth_file_only: Option<bool>,
    #[serde(default)]
    proxies: BTreeMap<String, String>,
    base_url: Option<Bases>,
    routing: Option<Routing>,
    account_upstream_base_url: Option<String>,
    upstream_base_url: Option<String>,
    api_key_upstream_base_url: Option<String>,
    accounts: Option<BTreeMap<String, Choice>>,
    api_key_providers: Option<Vec<Provider>>,
    openai_fallback_proxy: Option<Choice>,
    mcp_fallback_proxy: Option<Choice>,
}
#[derive(Clone)]
pub struct Config {
    pub codex: Codex,
    pub claude: crate::claude::Claude,
    pub listen_port: u16,
    pub request_timeout_seconds: f64,
    pub proxies: BTreeMap<String, String>,
}
impl Config {
    pub fn parse(text: &str) -> Result<Self> {
        let mut root: serde_yaml_ng::Value = serde_yaml_ng::from_str(text)
            .map_err(|_| Error::config("Invalid YAML configuration."))?;
        if let Some(codex) = root.get_mut("codex") {
            normalize_auth(codex)?;
        }
        let raw: Raw = serde_yaml_ng::from_value(root.clone()).map_err(|_| {
            Error::config(
                "Invalid YAML configuration; check required fields against config.example.yaml.",
            )
        })?;
        // Even explicitly null legacy keys conflict with the new namespace.
        if root.get("codex").is_some()
            && [
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
            ]
            .iter()
            .any(|key| root.get(*key).is_some())
        {
            return Err(Error::config(
                "Do not mix codex with legacy top-level Codex settings.",
            ));
        }
        if raw.base_url.is_some()
            && (raw.account_upstream_base_url.is_some()
                || raw.upstream_base_url.is_some()
                || raw.api_key_upstream_base_url.is_some())
            || raw.routing.is_some()
                && (raw.accounts.is_some()
                    || raw.api_key_providers.is_some()
                    || raw.openai_fallback_proxy.is_some()
                    || raw.mcp_fallback_proxy.is_some())
            || raw.account_upstream_base_url.is_some() && raw.upstream_base_url.is_some()
        {
            return Err(Error::config(
                "Do not mix new and legacy configuration fields.",
            ));
        }
        let base_url = raw.base_url.unwrap_or(Bases {
            account: raw
                .account_upstream_base_url
                .or(raw.upstream_base_url)
                .unwrap_or_else(account_base),
            api_key: raw.api_key_upstream_base_url.unwrap_or_else(api_base),
        });
        let routing = raw.routing.unwrap_or(Routing {
            account: raw.accounts.unwrap_or_default(),
            api_key_fallback: raw.openai_fallback_proxy,
            mcp_fallback: raw.mcp_fallback_proxy,
            ..Default::default()
        });
        let providers = raw.api_key_providers.unwrap_or_else(|| {
            routing
                .api_key
                .iter()
                .filter(|(key, _)| !crate::url_routing::is_url_selector(key))
                .map(|(k, v)| Provider {
                    selector: Some(k.clone()),
                    proxy: v.clone(),
                    name: None,
                    api_key_env: None,
                    api_key_file: None,
                    upstream_base_url: None,
                })
                .collect()
        });
        // A migrated source label must not accidentally introduce a match that
        // did not exist in the original ID/email-only routing map.
        let mut legacy_label = "default".to_string();
        while routing.account.contains_key(&legacy_label) {
            legacy_label.push('_');
        }
        let mut codex = raw.codex.unwrap_or_else(|| Codex {
            accounts: raw
                .auth_file
                .filter(|s| !s.is_empty())
                .map(|auth_file| {
                    BTreeMap::from([(
                        legacy_label,
                        AccountSource {
                            auth_file: Some(auth_file),
                            auth_env: None,
                        },
                    )])
                })
                .unwrap_or_default(),
            account_auth_file_only: raw.account_auth_file_only.unwrap_or(true),
            base_url,
            routing,
            providers,
        });
        if codex.providers.is_empty() {
            codex.providers = codex
                .routing
                .api_key
                .iter()
                .filter(|(key, _)| !crate::url_routing::is_url_selector(key))
                .map(|(k, v)| Provider {
                    selector: Some(k.clone()),
                    proxy: v.clone(),
                    name: None,
                    api_key_env: None,
                    api_key_file: None,
                    upstream_base_url: None,
                })
                .collect();
        }
        let c = Self {
            codex,
            claude: raw.claude,
            listen_port: raw.listen_port,
            request_timeout_seconds: raw.request_timeout_seconds,
            proxies: raw.proxies,
        };
        c.validate()?;
        Ok(c)
    }
    pub fn read(path: &std::path::Path, migrate: bool) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|_| Error::config("Cannot read configuration file."))?;
        if !migrate {
            return Self::parse(&text);
        }
        let mut root: serde_yaml_ng::Value = serde_yaml_ng::from_str(&text)
            .map_err(|_| Error::config("Invalid YAML configuration."))?;
        if let Some(entries) = root
            .get("routing")
            .and_then(|v| v.get("api_key"))
            .and_then(|v| v.as_sequence())
            .cloned()
        {
            let mut map = serde_yaml_ng::Mapping::new();
            for entry in entries {
                let m = entry
                    .as_mapping()
                    .ok_or(Error::config("Invalid legacy API Key mapping."))?;
                if m.keys()
                    .any(|k| !matches!(k.as_str(), Some("name" | "api_key_env" | "proxy")))
                    || (entry.get("api_key_env").is_some()
                        && entry
                            .get("name")
                            .is_some_and(|v| v.as_str() != Some("openai")))
                {
                    return Err(Error::config(
                        "Cannot migrate API Key routes without losing configuration.",
                    ));
                }
                let mut key = entry
                    .get("api_key_env")
                    .or(entry.get("name"))
                    .and_then(|v| v.as_str())
                    .ok_or(Error::config("Missing API Key identifier."))?;
                if key == "openai" && entry.get("api_key_env").is_none() {
                    key = "OPENAI_API_KEY";
                }
                let value = entry
                    .get("proxy")
                    .ok_or(Error::config("Missing proxy."))?
                    .clone();
                if map.insert(key.into(), value).is_some() {
                    return Err(Error::config("Duplicate API Key mapping after migration."));
                }
            }
            root["routing"]["api_key"] = serde_yaml_ng::Value::Mapping(map);
        }
        Self::parse(
            &serde_yaml_ng::to_string(&root)
                .map_err(|_| Error::config("Cannot migrate configuration."))?,
        )
    }
    fn validate(&self) -> Result<()> {
        self.claude.validate(self)?;
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
        crate::url_routing::validate_routes(&self.codex.routing.api_key)?;
        validate_upstream(&self.codex.base_url.account)?;
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
            || (self.codex.account_auth_file_only && accounts && self.codex.accounts.is_empty())
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
        for p in &self.codex.providers {
            if p.label().is_empty()
                || p.api_key_env.is_some() && p.api_key_file.is_some()
                || [&p.name, &p.api_key_env, &p.api_key_file]
                    .into_iter()
                    .flatten()
                    .any(|s| s.is_empty())
            {
                return Err(Error::config(
                    "Invalid API Key provider credential sources.",
                ));
            }
            self.validate_choice(&p.proxy)?;
            if let Some(u) = &p.upstream_base_url {
                validate_upstream(u)?;
            }
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
    pub fn canonical_yaml(&self) -> Result<String> {
        #[derive(Serialize)]
        struct Output<'a> {
            listen_port: u16,
            request_timeout_seconds: f64,
            proxies: &'a BTreeMap<String, String>,
            codex: Codex,
            claude: &'a crate::claude::Claude,
        }
        let mut routing = self.codex.routing.clone();
        routing
            .api_key
            .retain(|key, _| crate::url_routing::is_url_selector(key));
        for p in &self.codex.providers {
            if p.api_key_file.is_some()
                || p.upstream_base_url.is_some()
                || (p.name.is_some()
                    && p.api_key_env.is_some()
                    && p.name.as_deref() != Some("openai"))
            {
                return Err(Error::config(
                    "Cannot encode legacy API Key route without losing configuration.",
                ));
            }
            let key = p
                .selector
                .as_deref()
                .or(p.api_key_env.as_deref())
                .or(p.name.as_deref())
                .unwrap();
            if routing
                .api_key
                .insert(key.into(), p.proxy.clone())
                .is_some()
            {
                return Err(Error::config("Duplicate API Key mapping after migration."));
            }
        }
        let mut bases = self.codex.base_url.clone();
        if bases.account.ends_with("/backend-api/codex") {
            bases.account.truncate(bases.account.len() - 6);
        }
        let mut codex = self.codex.clone();
        codex.base_url = bases;
        codex.routing = routing;
        let mut output = serde_yaml_ng::to_value(&Output {
            listen_port: self.listen_port,
            request_timeout_seconds: self.request_timeout_seconds,
            proxies: &self.proxies,
            codex,
            claude: &self.claude,
        })
        .map_err(|_| Error::config("Cannot serialize configuration."))?;
        compact_auth(&mut output["codex"], false);
        compact_auth(&mut output["claude"], true);
        if self.codex.providers.is_empty()
            && self.codex.routing.api_key_fallback.is_none()
            && self.codex.base_url.api_key == api_base()
        {
            output["codex"]["base_url"]
                .as_mapping_mut()
                .unwrap()
                .remove("api_key");
        }
        for name in ["codex", "claude"] {
            let map = output[name].as_mapping_mut().unwrap();
            let mut ordered = serde_yaml_ng::Mapping::new();
            for key in ["base_url", "auth_file", "account_auth_file_only", "routing"] {
                if let Some(value) = map.remove(key) {
                    ordered.insert(key.into(), value);
                }
            }
            ordered.extend(std::mem::take(map));
            *map = ordered;
        }
        let text = serde_yaml_ng::to_string(&output)
            .map_err(|_| Error::config("Cannot serialize configuration."))?;
        inline_proxy_lists(&text)
    }
}

// Canonical config sequences are ordered proxy choices. Render them on the
// preceding mapping line, retaining YAML quoting for unusual proxy names.
fn inline_proxy_lists(text: &str) -> Result<String> {
    let lines: Vec<_> = text.lines().collect();
    let mut output = Vec::<String>::new();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        let indent = line.len() - line.trim_start().len();
        if line.trim_start().starts_with("- ") && output.last().is_some_and(|s| s.ends_with(':')) {
            let start = index;
            index += 1;
            while index < lines.len() {
                let next = lines[index];
                let next_indent = next.len() - next.trim_start().len();
                if next_indent < indent
                    || next_indent == indent && !next.trim_start().starts_with("- ")
                {
                    break;
                }
                index += 1;
            }
            let names: Vec<String> = serde_yaml_ng::from_str(&lines[start..index].join("\n"))
                .map_err(|_| Error::config("Cannot format proxy candidates."))?;
            let names: Vec<String> = names
                .into_iter()
                .map(|name| {
                    if name
                        .bytes()
                        .next()
                        .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
                        && name
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
                        && !matches!(
                            name.to_ascii_lowercase().as_str(),
                            "null" | "true" | "false" | "yes" | "no" | "on" | "off"
                        )
                    {
                        name
                    } else {
                        serde_json::to_string(&name).unwrap()
                    }
                })
                .collect();
            output
                .last_mut()
                .unwrap()
                .push_str(&format!(" [{}]", names.join(", ")));
        } else {
            output.push(line.into());
            index += 1;
        }
    }
    Ok(output.join("\n") + "\n")
}

/// Flat credentials are the normal single-login format. Keep named accounts
/// readable for compatibility with existing multiple-source configurations.
pub(crate) fn normalize_auth(value: &mut serde_yaml_ng::Value) -> Result<()> {
    let Some(map) = value.as_mapping_mut() else {
        return Ok(());
    };
    if !map.contains_key("auth_file") && !map.contains_key("auth_env") {
        return Ok(());
    }
    if map.contains_key("accounts") {
        return Err(Error::config(
            "Do not mix auth_file/auth_env with nested accounts.",
        ));
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

fn compact_auth(service: &mut serde_yaml_ng::Value, claude: bool) {
    let accounts = service["accounts"].as_mapping().unwrap();
    if accounts.is_empty() {
        service.as_mapping_mut().unwrap().remove("accounts");
        return;
    }
    if accounts.len() != 1 {
        return;
    }
    let (label, source) = accounts.iter().next().unwrap();
    let label = label.clone();
    let source = source.as_mapping().unwrap().clone();
    let routes = service["routing"]["account"].as_mapping().unwrap();
    // Codex keys also identify real account IDs: preserve ambiguous legacy
    // source labels rather than silently changing ID/profile matching.
    if !claude
        && label.as_str() != Some("default")
        && (routes.contains_key(&label) || routes.contains_key("default"))
    {
        return;
    }
    if source.is_empty() {
        return;
    }
    if claude {
        let routes = service["routing"]["account"].as_mapping_mut().unwrap();
        if let Some(choice) = routes.remove(&label) {
            routes.insert("default".into(), choice);
        }
    }
    let map = service.as_mapping_mut().unwrap();
    map.remove("accounts");
    map.extend(source);
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
pub fn unwrap_upstream(value: &str) -> Result<String> {
    if let Ok(u) = Url::parse(value) {
        if u.scheme() == "http"
            && u.host_str() == Some("127.0.0.1")
            && u.port().is_some()
            && u.username().is_empty()
            && u.password().is_none()
            && u.query().is_none()
            && u.fragment().is_none()
            && u.path().starts_with("/https://")
        {
            let inner = &u.path()[1..];
            validate_upstream(inner)?;
            return Ok(inner.into());
        }
    }
    validate_upstream(value)?;
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
    fn rejects_invalid_layouts_and_proxy_choices() {
        let base = "listen_port: 7889\nrequest_timeout_seconds: 3\n";
        for invalid in [
            "unknown: value\n",
            "routing:\n  api_key_fallback: []\n",
            "routing:\n  api_key_fallback: missing\n",
            "routing:\n  unknown: none\n",
            "base_url: {}\nupstream_base_url: https://api.openai.com/v1\n",
            "routing: {}\naccounts: {}\n",
            "routing:\n  account:\n    test: none\n",
            "routing:\n  api_key:\n    TEST:\n      proxy: none\n",
        ] {
            assert!(
                Config::parse(&format!("{base}{invalid}")).is_err(),
                "{invalid}"
            );
        }
        assert!(
            Config::parse(&format!(
                "{base}account_auth_file_only: false\nrouting:\n  account_fallback: none\n"
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
        assert_eq!(
            unwrap_upstream("http://127.0.0.1:7889/https://provider.invalid/v1").unwrap(),
            "https://provider.invalid/v1"
        );
        assert_eq!(
            redacted_endpoint("https://u:p@proxy.invalid:443"),
            "https://proxy.invalid:443"
        );
    }
    #[test]
    fn canonical_migration_roundtrip_and_loss_refusal() {
        let old = "listen_port: 7889\nrequest_timeout_seconds: 3\nauth_file: ~/auth.json\nupstream_base_url: https://chatgpt.com/backend-api/codex\naccounts:\n  a: none\napi_key_providers:\n  - api_key_env: TEST_KEY\n    proxy: none\n";
        let c = Config::parse(old).unwrap();
        let text = c.canonical_yaml().unwrap();
        let migrated = Config::parse(&text).unwrap();
        assert_eq!(
            migrated.codex.base_url.account,
            "https://chatgpt.com/backend-api"
        );
        assert_eq!(migrated.codex.providers[0].label(), "TEST_KEY");
        let lossy = old.replace(
            "api_key_env: TEST_KEY",
            "name: custom\n    api_key_file: ~/key",
        );
        assert!(Config::parse(&lossy).unwrap().canonical_yaml().is_err());
    }

    #[test]
    fn symmetric_sections_roundtrip_and_reject_conflicting_layouts() {
        let text = r#"
listen_port: 8787
request_timeout_seconds: 30
proxies:
  selected: http://127.0.0.1:7893
codex:
  accounts:
    personal: {auth_file: ~/codex-auth.json}
  routing:
    account: {personal: selected}
    api_key: {OPENAI_API_KEY: selected}
claude:
  accounts:
    personal: {auth_file: ~/claude-auth.json}
  routing:
    account: {personal: selected}
    api_key: {ANTHROPIC_API_KEY: selected}
"#;
        let c = Config::parse(text).unwrap();
        assert_eq!(c.codex.providers[0].label(), "OPENAI_API_KEY");
        assert_eq!(c.claude.base_url, "https://api.anthropic.com");
        let serialized = c.canonical_yaml().unwrap();
        let root: serde_yaml_ng::Value = serde_yaml_ng::from_str(&serialized).unwrap();
        assert_eq!(root.as_mapping().unwrap().len(), 5);
        assert_eq!(
            root["codex"]["routing"]["account"]["personal"].as_str(),
            Some("selected")
        );
        assert_eq!(
            root["claude"]["routing"]["account"]["default"].as_str(),
            Some("selected")
        );
        assert_eq!(
            root["claude"]["auth_file"].as_str(),
            Some("~/claude-auth.json")
        );
        assert!(root["claude"]["base_url"].is_string());
        assert_eq!(
            Config::parse(&serialized)
                .unwrap()
                .canonical_yaml()
                .unwrap(),
            serialized
        );
        let prefix = "listen_port: 8787\nrequest_timeout_seconds: 30\n";
        for invalid in [
            "codex: {}\nauth_file: null\n",
            "codex: {}\nrouting: {}\n",
            "codex:\n  typo: true\n",
            "codex:\n  accounts:\n    a: {auth_file: file, auth_env: TOKEN}\n  routing:\n    account: {a: none}\n",
            "claude:\n  routing: {}\n  api_key: {}\n",
            "claude:\n  accounts:\n    a: {auth_file: file, proxy: none}\n  routing:\n    account: {a: none}\n",
            "claude:\n  routing:\n    account: {absent: none}\n",
            "claude:\n  routing:\n    mcp_fallback: none\n",
        ] {
            assert!(
                Config::parse(&format!("{prefix}{invalid}")).is_err(),
                "{invalid}"
            );
        }
    }

    #[test]
    fn flat_credentials_and_omitted_api_keys_roundtrip() {
        let text = r#"
listen_port: 8787
request_timeout_seconds: 30
codex:
  auth_file: ~/.codex/auth.json
  routing:
    account: {default: none}
claude:
  auth_file: ~/.claude/.credentials.json
  base_url: https://api.anthropic.com
  routing:
    account: {default: none}
"#;
        let config = Config::parse(text).unwrap();
        let output = config.canonical_yaml().unwrap();
        let root: serde_yaml_ng::Value = serde_yaml_ng::from_str(&output).unwrap();
        for name in ["codex", "claude"] {
            assert!(root[name]["auth_file"].is_string());
            assert!(root[name].get("accounts").is_none());
            assert!(root[name]["routing"].get("api_key").is_none());
        }
        assert!(root["codex"]["base_url"].get("api_key").is_none());
        assert_eq!(
            root["claude"]["base_url"].as_str(),
            Some("https://api.anthropic.com")
        );
        assert_eq!(
            Config::parse(&output).unwrap().canonical_yaml().unwrap(),
            output
        );
        for name in ["codex", "claude"] {
            let invalid = format!(
                "listen_port: 8787\nrequest_timeout_seconds: 3\n{name}:\n  auth_file: file\n  accounts: {{}}\n"
            );
            assert!(Config::parse(&invalid).is_err());
        }
        let with_keys = text.replace(
            "account: {default: none}",
            "account: {default: none}\n    api_key: {TEST_KEY: none}",
        );
        let output = Config::parse(&with_keys).unwrap().canonical_yaml().unwrap();
        let root: serde_yaml_ng::Value = serde_yaml_ng::from_str(&output).unwrap();
        assert!(root["codex"]["routing"].get("api_key").is_some());
        assert!(root["claude"]["routing"].get("api_key").is_some());
    }

    #[tokio::test]
    async fn namespaced_codex_url_routes_are_not_credential_sources() {
        let c = Config::parse("listen_port: 8787\nrequest_timeout_seconds: 3\ncodex:\n  routing:\n    api_key:\n      'provider.invalid/v1': none\n").unwrap();
        assert!(c.codex.providers.is_empty());
        c.check_credentials().await.unwrap();
        let output = c.canonical_yaml().unwrap();
        let c = Config::parse(&output).unwrap();
        assert!(
            c.resolve_url(
                Some("Bearer key"),
                "/codex/https://provider.invalid/v1/responses"
            )
            .unwrap()
            .is_some()
        );
    }

    #[test]
    fn proxy_candidates_are_inline_and_preserve_order_and_quoted_names() {
        let text = r#"
listen_port: 8787
request_timeout_seconds: 30
proxies:
  jp_lab: http://127.0.0.1:7893
  jp: http://127.0.0.1:7892
  'false': none
  'comma,name': none
codex:
  routing:
    api_key: {TOKEN: [jp_lab, jp]}
    api_key_fallback: [jp, none]
    mcp_fallback: ['false', 'comma,name']
claude:
  auth_file: ~/.claude/.credentials.json
  routing:
    account: {default: [jp_lab, jp]}
    account_fallback: [jp_lab]
"#;
        let c = Config::parse(text).unwrap();
        let output = c.canonical_yaml().unwrap();
        assert!(output.contains("TOKEN: [jp_lab, jp]"));
        assert!(output.contains("default: [jp_lab, jp]"));
        assert!(output.contains("api_key_fallback: [jp, none]"));
        assert!(output.contains("account_fallback: [jp_lab]"));
        assert!(output.contains("mcp_fallback: [\"false\", \"comma,name\"]"));
        assert!(
            !output
                .lines()
                .any(|line| line.trim_start().starts_with("- "))
        );
        assert_eq!(
            Config::parse(&output).unwrap().canonical_yaml().unwrap(),
            output
        );
    }
}
