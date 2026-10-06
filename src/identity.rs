use crate::{
    Error, Result,
    config::{AccountSource, Codex, Config, Provider, expand, unwrap_upstream},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::Deserialize;
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashSet},
    path::Path,
};

#[derive(Clone)]
pub struct Identity {
    pub account_id: String,
    pub token: String,
    pub refresh_token: Option<String>,
    pub usernames: Vec<String>,
}
pub fn valid_token(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b > 32 && b < 127)
}
fn claims(token: &str) -> Value {
    let parts: Vec<_> = token.split('.').collect();
    if parts.len() != 3 {
        return Value::Null;
    }
    URL_SAFE_NO_PAD
        .decode(parts[1].trim_end_matches('='))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null)
}
fn usernames(access: &Value, id: &Value) -> Vec<String> {
    ["email", "preferred_username", "name"]
        .into_iter()
        .filter_map(|key| {
            access["https://api.openai.com/profile"][key]
                .as_str()
                .or(access[key].as_str())
                .or(id[key].as_str())
                .filter(|s| !s.trim().is_empty() && !s.chars().any(char::is_control))
                .map(String::from)
        })
        .collect()
}
impl Identity {
    /// A saved Codex login file, such as an archived `auth-*.json`.
    pub fn read(path: &str) -> Result<Self> {
        let raw = std::fs::read(expand(path))
            .map_err(|_| Error::config("Cannot read Codex login file."))?;
        Self::parse(
            &serde_json::from_slice(&raw)
                .map_err(|_| Error::config("Invalid Codex login file."))?,
        )
    }
    /// The ChatGPT account of a saved Codex login (`auth.json` layout).
    pub fn parse(value: &Value) -> Result<Self> {
        let tokens = &value["tokens"];
        let id = tokens["account_id"]
            .as_str()
            .filter(|s| valid_token(s))
            .ok_or(Error::config("Codex login requires tokens.account_id."))?;
        let token = tokens["access_token"]
            .as_str()
            .filter(|s| valid_token(s))
            .ok_or(Error::config("Codex login requires tokens.access_token."))?;
        Ok(Self {
            account_id: id.into(),
            token: token.into(),
            refresh_token: tokens["refresh_token"]
                .as_str()
                .filter(|s| valid_token(s))
                .map(String::from),
            usernames: usernames(
                &claims(token),
                &claims(tokens["id_token"].as_str().unwrap_or("")),
            ),
        })
    }
    pub fn from_token(token: &str) -> Option<Self> {
        let c = claims(token);
        let id = c["https://api.openai.com/auth"]["chatgpt_account_id"]
            .as_str()
            .filter(|s| valid_token(s))?;
        Some(Self {
            account_id: id.into(),
            token: token.into(),
            refresh_token: None,
            usernames: usernames(&c, &Value::Null),
        })
    }
}
/// Where the Codex CLI keeps its login for a home (`cli_auth_credentials_store`).
#[derive(Clone, Copy, Default, Deserialize, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
enum AuthStore {
    #[default]
    File,
    Keyring,
    Auto,
    Ephemeral,
}
#[derive(Default, Deserialize)]
struct AuthSettings {
    #[serde(default)]
    cli_auth_credentials_store: AuthStore,
    #[serde(default)]
    features: AuthFeatures,
}
#[derive(Deserialize)]
struct AuthFeatures {
    /// Encrypted local secrets file instead of a direct keyring entry; Codex
    /// enables it by default on Windows only.
    #[serde(default = "secret_auth_storage_default")]
    secret_auth_storage: bool,
}
impl Default for AuthFeatures {
    fn default() -> Self {
        Self {
            secret_auth_storage: secret_auth_storage_default(),
        }
    }
}
fn secret_auth_storage_default() -> bool {
    cfg!(windows)
}
fn auth_settings(home: &Path) -> Result<AuthSettings> {
    match std::fs::read_to_string(home.join("config.toml")) {
        Ok(text) => {
            toml::from_str(&text).map_err(|_| Error::config("Cannot parse Codex config.toml."))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(AuthSettings::default()),
        Err(_) => Err(Error::config("Cannot read Codex config.toml.")),
    }
}
/// The Codex CLI's keyring account for a home: a digest of its canonical path.
fn keyring_account(home: &Path) -> String {
    let home = home.canonicalize().unwrap_or_else(|_| home.to_path_buf());
    let digest = ring::digest::digest(&ring::digest::SHA256, home.to_string_lossy().as_bytes());
    let hex: String = digest.as_ref()[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("cli|{hex}")
}
const KEYRING_SERVICE: &str = "Codex Auth";
/// The login the Codex CLI saved for `home`, read from wherever that home's
/// `config.toml` tells Codex to keep it.
pub(crate) async fn saved_auth(home: &Path) -> Result<Value> {
    let settings = auth_settings(home)?;
    let file = || {
        let raw = std::fs::read(home.join("auth.json"))
            .map_err(|_| Error::config("Cannot read Codex auth.json."))?;
        serde_json::from_slice::<Value>(&raw).map_err(|_| Error::config("Invalid Codex auth.json."))
    };
    let keyring = || async {
        if settings.features.secret_auth_storage {
            return Err(Error::config(
                "Codex encrypted auth storage (secret_auth_storage) is not supported.",
            ));
        }
        crate::keychain::read(KEYRING_SERVICE, &keyring_account(home))
            .await?
            .map(|raw| {
                serde_json::from_str::<Value>(&raw)
                    .map_err(|_| Error::config("Invalid Codex login in the OS credential store."))
            })
            .transpose()
    };
    match settings.cli_auth_credentials_store {
        AuthStore::File => file(),
        AuthStore::Keyring => keyring()
            .await?
            .ok_or(Error::config("No Codex login in the OS credential store.")),
        // As in Codex, any keyring miss or failure falls back to the file.
        AuthStore::Auto => match keyring().await {
            Ok(Some(value)) => Ok(value),
            Ok(None) => file(),
            Err(error) => file().map_err(|_| error),
        },
        AuthStore::Ephemeral => Err(Error::config(
            "Codex keeps ephemeral logins in its own process memory.",
        )),
    }
}
#[derive(Deserialize)]
struct Definition {
    env_key: Option<String>,
    base_url: Option<String>,
    experimental_bearer_token: Option<String>,
    #[serde(default)]
    requires_openai_auth: bool,
}
fn definitions(home: Option<&Path>) -> Result<BTreeMap<String, Definition>> {
    #[derive(Deserialize)]
    struct File {
        #[serde(default)]
        model_providers: BTreeMap<String, Definition>,
    }
    let mut defs = match home.map(|home| std::fs::read_to_string(home.join("config.toml"))) {
        Some(Ok(text)) => {
            toml::from_str::<File>(&text)
                .map_err(|_| Error::config("Cannot parse Codex config.toml."))?
                .model_providers
        }
        None => BTreeMap::new(),
        Some(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
        Some(Err(_)) => return Err(Error::config("Cannot read Codex config.toml.")),
    };
    if defs.contains_key("openai") {
        return Err(Error::config("Reserved Codex provider ID openai."));
    }
    if defs
        .values()
        .any(|d| d.env_key.as_ref().is_some_and(|k| k.trim().is_empty()))
    {
        return Err(Error::config("Invalid Codex provider env_key."));
    }
    defs.insert(
        "openai".into(),
        Definition {
            env_key: Some("OPENAI_API_KEY".into()),
            base_url: None,
            requires_openai_auth: false,
            experimental_bearer_token: None,
        },
    );
    Ok(defs)
}
#[derive(PartialEq, Eq)]
pub struct ProviderCredential {
    pub token: String,
    pub upstream: String,
    pub account_id: Option<String>,
}

async fn saved_provider_auth(home: &Path) -> Result<(String, Option<String>)> {
    let value = saved_auth(home).await?;
    let mode = value["auth_mode"].as_str().unwrap_or_else(|| {
        if value["OPENAI_API_KEY"].is_string() {
            "apikey"
        } else {
            "chatgpt"
        }
    });
    match mode {
        "apikey" => Ok((
            key(value["OPENAI_API_KEY"].as_str().ok_or(Error::config(
                "Codex provider login requires OPENAI_API_KEY.",
            ))?)?,
            None,
        )),
        "chatgpt" => {
            let token = key(value["tokens"]["access_token"]
                .as_str()
                .ok_or(Error::config(
                    "Codex provider login requires tokens.access_token.",
                ))?)?;
            let account = value["tokens"]["account_id"]
                .as_str()
                .filter(|s| valid_token(s))
                .map(String::from);
            Ok((token, account))
        }
        _ => Err(Error::config("Unsupported Codex provider login auth_mode.")),
    }
}
impl Codex {
    /// Display classification uses the same provider definitions as credential routing.
    pub fn api_key_kind(&self, selector: &str) -> Result<&'static str> {
        if crate::url_routing::is_url_selector(selector) {
            return Ok("gateway");
        }
        let provider = Provider {
            selector: selector.into(),
            proxy: crate::config::Choice::direct(),
        };
        for home in &self.homes {
            let defs = definitions(Some(&expand(home)))?;
            // The built-in OpenAI definition is the ordinary API-key route.
            // Custom provider IDs and their env_key selectors share a type.
            if provider
                .definition(&defs)?
                .is_some_and(|(id, _)| id != "openai")
            {
                return Ok("provider");
            }
        }
        Ok("api_key")
    }
}

impl Provider {
    /// Upstream base URLs this route forwards to, one per configured home,
    /// resolved like requests are but without reading any credential.
    pub fn upstreams(&self, default: &str, codex: &Codex) -> Vec<Result<String>> {
        let homes: Vec<_> = if codex.homes.is_empty() {
            vec![None]
        } else {
            codex.homes.iter().map(|home| Some(expand(home))).collect()
        };
        homes
            .iter()
            .map(|home| {
                let defs = definitions(home.as_deref())?;
                provider_upstream(self.definition(&defs)?, default)
            })
            .collect()
    }

    /// The definition a selector names: a Codex provider ID, or else the one
    /// provider whose `env_key` it is.
    fn definition<'a>(
        &self,
        defs: &'a BTreeMap<String, Definition>,
    ) -> Result<Option<(&'a str, &'a Definition)>> {
        let selector = self.selector.as_str();
        if let Some((id, definition)) = defs.get_key_value(selector) {
            return Ok(Some((id, definition)));
        }
        let candidates: Vec<_> = defs
            .iter()
            .filter(|(_, d)| d.env_key.as_deref() == Some(selector))
            .collect();
        if candidates.len() > 1 {
            return Err(Error::config(
                "API Key environment variable matches multiple Codex providers.",
            ));
        }
        Ok(candidates.first().map(|(id, d)| (id.as_str(), *d)))
    }

    pub async fn credentials(
        &self,
        default: &str,
        shell: bool,
        codex: &Codex,
    ) -> Result<Vec<ProviderCredential>> {
        if codex.homes.is_empty() {
            return self
                .credential(default, shell, None, None)
                .await
                .map(|value| vec![value]);
        }
        let mut values = Vec::new();
        let mut error = Error::config("No credential found in configured Codex homes.");
        let inherited = crate::codex_env::inherited();
        for home in &codex.homes {
            let home = expand(home);
            let environment = crate::codex_env::load(&home.join(".env"), &inherited);
            match self
                .credential(default, shell, Some(&home), Some(&environment))
                .await
            {
                Ok(value) => {
                    // The same route may discover an identical credential in multiple homes.
                    // Different upstreams remain separate matches and are rejected as ambiguous.
                    if !values.contains(&value) {
                        values.push(value);
                    }
                }
                Err(e) => error = e,
            }
        }
        if values.is_empty() {
            Err(error)
        } else {
            Ok(values)
        }
    }

    async fn credential(
        &self,
        default: &str,
        shell: bool,
        home: Option<&Path>,
        environment: Option<&crate::codex_env::Environment>,
    ) -> Result<ProviderCredential> {
        let defs = definitions(home)?;
        // A selector is a Codex provider ID when one is defined, otherwise an
        // API Key environment variable name.
        let selector = self.selector.as_str();
        let selected_env = (!defs.contains_key(selector)).then_some(selector);
        let definition = self.definition(&defs)?;
        let var = selected_env.or(definition.and_then(|(_, d)| d.env_key.as_deref()));
        let (token, account_id) = if let Some(var) = var {
            (
                match environment.and_then(|environment| environment.get(var)) {
                    Some(value) => key(value)?,
                    None => environment_key_with_shell(var, shell).await?,
                },
                None,
            )
        } else if let Some(token) =
            definition.and_then(|(_, d)| d.experimental_bearer_token.as_deref())
        {
            (key(token)?, None)
        } else if definition.is_some_and(|(_, d)| d.requires_openai_auth) {
            saved_provider_auth(home.ok_or(Error::config(
                "Saved provider authentication requires a configured Codex home.",
            ))?)
            .await?
        } else {
            return Err(Error::config(
                "Codex provider has no configured Bearer credential.",
            ));
        };
        Ok(ProviderCredential {
            token,
            upstream: provider_upstream(definition, default)?,
            account_id,
        })
    }
}
/// The built-in `openai` provider and providers without `base_url` use the
/// configured API Key base.
fn provider_upstream(definition: Option<(&str, &Definition)>, default: &str) -> Result<String> {
    unwrap_upstream(
        definition
            .filter(|(id, _)| *id != "openai")
            .and_then(|(_, d)| d.base_url.as_deref())
            .unwrap_or(default),
    )
}
fn key(raw: &str) -> Result<String> {
    let k = raw.trim();
    if !valid_token(k) {
        return Err(Error::config(
            "API Key must be a nonempty ASCII token without whitespace or control characters.",
        ));
    }
    Ok(k.into())
}
pub(crate) async fn environment_key(name: &str) -> Result<String> {
    environment_key_with_shell(name, true).await
}
pub(crate) async fn environment_key_with_shell(name: &str, shell: bool) -> Result<String> {
    let raw = match std::env::var(name) {
        Ok(value) => value,
        Err(_) if shell => shell_value(name).await?,
        Err(_) => {
            return Err(Error::config(
                "API Key environment variable is unavailable.",
            ));
        }
    };
    key(&raw)
}
impl Codex {
    pub(crate) async fn account_identity(&self, source: &AccountSource) -> Result<Identity> {
        let name = match source {
            AccountSource::Directory(home) => return Identity::parse(&saved_auth(home).await?),
            AccountSource::Env(name) => name,
        };
        let mut found = None;
        let inherited = crate::codex_env::inherited();
        for home in &self.homes {
            let environment = crate::codex_env::load(&expand(home).join(".env"), &inherited);
            if let Some(value) = environment.get(name) {
                let token = key(value)?;
                if found.as_ref().is_some_and(|previous| previous != &token) {
                    return Err(Error::config(
                        "Account environment variable has different values across Codex homes; use their saved logins instead.",
                    ));
                }
                found = Some(token);
            }
        }
        let token = match found {
            Some(token) => token,
            None => environment_key(name).await?,
        };
        Identity::from_token(&token).ok_or(Error::config(
            "Codex account token requires ChatGPT account claims.",
        ))
    }
}
#[cfg(unix)]
async fn shell_value(name: &str) -> Result<String> {
    use std::{os::unix::process::CommandExt, process::Stdio};
    let bad =
        || Error::config("API Key environment variable is unavailable or shell lookup timed out.");
    if name.is_empty()
        || !name
            .bytes()
            .enumerate()
            .all(|(i, b)| b == b'_' || b.is_ascii_alphabetic() || i > 0 && b.is_ascii_digit())
    {
        return Err(bad());
    }
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let shell_path = std::path::Path::new(&shell);
    if !shell_path.is_absolute()
        || !matches!(
            shell_path.file_name().and_then(|s| s.to_str()),
            Some("zsh" | "bash" | "sh")
        )
    {
        return Err(bad());
    }
    let dir = tempfile::tempdir().map_err(|_| bad())?;
    let output = dir.path().join("value");
    let mut cmd = std::process::Command::new(shell);
    cmd.args([
        "-l",
        "-i",
        "-c",
        "umask 077; exec /usr/bin/printenv \"$1\" > \"$2\"",
        "coport",
        name,
    ])
    .arg(&output)
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .process_group(0);
    let mut cmd = tokio::process::Command::from(cmd);
    cmd.kill_on_drop(true);
    let mut child = cmd.spawn().map_err(|_| bad())?;
    // Kill the process group even on cancellation, including shell startup descendants.
    struct Group(u32);
    impl Drop for Group {
        fn drop(&mut self) {
            unsafe {
                libc::kill(-(self.0 as i32), libc::SIGKILL);
            }
        }
    }
    let group = Group(child.id().ok_or_else(bad)?);
    let status = tokio::time::timeout(std::time::Duration::from_secs(3), child.wait()).await;
    drop(group);
    if !matches!(status, Ok(Ok(s)) if s.success()) {
        let _ = child.wait().await;
        return Err(bad());
    }
    if std::fs::metadata(&output).map_err(|_| bad())?.len() > 65536 {
        return Err(bad());
    }
    std::fs::read_to_string(output).map_err(|_| bad())
}
#[cfg(not(unix))]
async fn shell_value(_: &str) -> Result<String> {
    Err(Error::config(
        "API Key must be exported into the service process environment on Windows.",
    ))
}

impl Config {
    /// Local route activation, not upstream token validity. Archived logins are
    /// deliberately excluded: only configured sources can admit requests.
    pub async fn account_route_states(&self) -> [BTreeMap<String, &'static str>; 2] {
        let initial = |required: bool, routing: &crate::config::Routing| {
            routing
                .account
                .keys()
                .filter(|_| required)
                .map(|name| (name.clone(), "inactive"))
                .collect::<BTreeMap<_, _>>()
        };
        let mut codex = initial(self.codex.account_auth_file_only, &self.codex.routing);
        if !codex.is_empty() {
            for (source, account) in self.codex.account_sources() {
                if let Ok(identity) = self.codex.account_identity(&account).await {
                    if let Some(label) = routing_account_label(
                        &self.codex.routing,
                        &identity.account_id,
                        &identity.usernames,
                        &source,
                    ) {
                        codex.insert(label, "active");
                    }
                }
            }
        }
        let mut claude = initial(self.claude.account_auth_file_only, &self.claude.routing);
        if !claude.is_empty() {
            for (source, account) in self.claude.account_sources() {
                if account.claude_token().await.is_err() {
                    continue;
                }
                let identity = account.claude_identity();
                let label = match &identity {
                    Some(identity) => routing_account_label(
                        &self.claude.routing,
                        &identity.account_id,
                        &identity.usernames,
                        &source,
                    ),
                    None => self
                        .claude
                        .routing
                        .account
                        .contains_key(&source)
                        .then_some(source),
                };
                if let Some(label) = label {
                    claude.insert(label, "active");
                } else if self.claude.routing.account_fallback.is_none()
                    && self.claude.routing.account_probe.is_some()
                {
                    for state in claude.values_mut() {
                        if *state == "inactive" {
                            *state = "unknown";
                        }
                    }
                }
            }
        }
        [codex, claude]
    }

    /// Safe display names for recorded account IDs, using routing's selector precedence.
    pub async fn traffic_credential_labels(&self) -> BTreeMap<(String, String), String> {
        let mut labels = BTreeMap::new();
        for (source, account) in self.codex.account_sources() {
            if let Ok(identity) = self.codex.account_identity(&account).await {
                if let Some(selector) = routing_account_label(
                    &self.codex.routing,
                    &identity.account_id,
                    &identity.usernames,
                    &source,
                ) {
                    labels.insert(("Codex".into(), identity.account_id), selector);
                }
            }
        }
        // Additional saved logins supply display metadata only. They never become
        // accepted credential sources or participate in proxy routing.
        for home in &self.codex.homes {
            let Ok(files) = std::fs::read_dir(expand(home)) else {
                continue;
            };
            let mut paths: Vec<_> = files
                .filter_map(std::result::Result::ok)
                .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
                .filter(|entry| {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    name.starts_with("auth-") && name.ends_with(".json")
                })
                .map(|entry| entry.path())
                .collect();
            paths.sort();
            for path in paths {
                let Ok(identity) = Identity::read(&path.to_string_lossy()) else {
                    continue;
                };
                // Only an explicit ID/email/name match is evidence of a label;
                // default and fallback routes cannot identify an archived login.
                if let Some(selector) = std::iter::once(&identity.account_id)
                    .chain(identity.usernames.iter())
                    .find(|name| self.codex.routing.account.contains_key(*name))
                {
                    labels
                        .entry(("Codex".into(), identity.account_id.clone()))
                        .or_insert_with(|| selector.clone());
                }
            }
        }
        for (source, account) in self.claude.account_sources() {
            if let Some(identity) = account.claude_identity() {
                if let Some(selector) = routing_account_label(
                    &self.claude.routing,
                    &identity.account_id,
                    &identity.usernames,
                    &source,
                ) {
                    labels.insert(("Claude".into(), identity.account_id), selector);
                }
            }
        }
        for (service, routing) in [
            ("Codex", &self.codex.routing),
            ("Claude", &self.claude.routing),
        ] {
            for name in routing.api_key.keys().chain(routing.account.keys()) {
                labels.insert((service.into(), name.clone()), name.clone());
            }
            if routing.api_key_fallback.is_some() {
                let provider = if service == "Codex" {
                    "openai-fallback"
                } else {
                    "claude-api-key-fallback"
                };
                labels.insert((service.into(), provider.into()), "api_key_fallback".into());
            }
            if routing.account_fallback.is_some() && service == "Claude" {
                labels.insert(
                    (service.into(), "claude-account-fallback".into()),
                    "account_fallback".into(),
                );
            }
        }
        labels
    }

    pub fn account_choice(
        &self,
        identity: &Identity,
        source: Option<&str>,
    ) -> Result<crate::config::Choice> {
        self.codex
            .routing
            .account
            .get(&identity.account_id)
            .or_else(|| {
                identity
                    .usernames
                    .iter()
                    .find_map(|u| self.codex.routing.account.get(u))
            })
            .or_else(|| source.and_then(|s| self.codex.routing.account.get(s)))
            .or(self.codex.routing.account_fallback.as_ref())
            .cloned()
            .ok_or(Error::config(
                "Current account has no proxy mapping; forwarding refused.",
            ))
    }
    pub async fn check_credentials(&self) -> Result<()> {
        self.claude.check_credentials().await?;
        let mut keys = HashSet::new();
        if self.codex.account_auth_file_only {
            for (label, source) in &self.codex.account_sources() {
                let i = self.codex.account_identity(source).await?;
                self.account_choice(&i, Some(label))?;
                if !keys.insert(i.token) {
                    return Err(Error::config("Multiple routes have the same credential."));
                }
            }
        }
        for p in &self.codex.providers {
            for credential in p
                .credentials(&self.codex.base_url.api_key, true, &self.codex)
                .await?
            {
                if !keys.insert(credential.token) {
                    return Err(Error::config("Multiple routes have the same credential."));
                }
            }
        }
        Ok(())
    }
}

pub(crate) fn routing_account_label(
    routing: &crate::config::Routing,
    id: &str,
    usernames: &[String],
    source: &str,
) -> Option<String> {
    std::iter::once(id)
        .chain(usernames.iter().map(String::as_str))
        .chain(std::iter::once(source))
        .find(|name| routing.account.contains_key(*name))
        .map(str::to_owned)
        .or_else(|| {
            routing
                .account_fallback
                .as_ref()
                .map(|_| "account_fallback".into())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn route_activation_tracks_saved_logins_and_file_only_mode() {
        let dir = tempfile::tempdir().unwrap();
        let auth = dir.path().join("auth.json");
        let saved = |id: &str| {
            serde_json::json!({"tokens": {"account_id": id, "access_token": "token"}}).to_string()
        };
        std::fs::write(&auth, saved("first")).unwrap();
        std::fs::write(dir.path().join("auth-second.json"), saved("second")).unwrap();
        let mut config = Config::parse(&format!(
            "listen_port: 8787\nrequest_timeout_seconds: 3\ncodex:\n  homes: [{}]\n  routing:\n    account: {{first: none, second: none}}\nclaude:\n  config_dirs: [{}]\n  routing:\n    account: {{'user@example.com': none}}\n",
            serde_json::to_string(dir.path()).unwrap(), serde_json::to_string(dir.path()).unwrap()
        )).unwrap();
        let states = config.account_route_states().await;
        assert_eq!(states[0]["first"], "active");
        assert_eq!(states[0]["second"], "inactive");
        assert_eq!(states[1]["user@example.com"], "inactive");
        std::fs::write(&auth, saved("second")).unwrap();
        let states = config.account_route_states().await;
        assert_eq!(states[0]["first"], "inactive");
        assert_eq!(states[0]["second"], "active");
        std::fs::write(
            dir.path().join(".credentials.json"),
            r#"{"claudeAiOauth":{"accessToken":"token"}}"#,
        )
        .unwrap();
        config.claude.routing.account_probe = Some(crate::config::Choice::One("none".into()));
        assert_eq!(
            config.account_route_states().await[1]["user@example.com"],
            "unknown"
        );
        std::fs::write(
            dir.path().join(".claude.json"),
            r#"{"oauthAccount":{"accountUuid":"uuid","emailAddress":"user@example.com"}}"#,
        )
        .unwrap();
        assert_eq!(
            config.account_route_states().await[1]["user@example.com"],
            "active"
        );
        // Local OAuth identity takes precedence over a configured source label.
        config
            .claude
            .routing
            .account
            .insert("default".into(), crate::config::Choice::One("none".into()));
        let states = config.account_route_states().await;
        assert_eq!(states[1]["user@example.com"], "active");
        assert_eq!(states[1]["default"], "inactive");
        std::fs::write(dir.path().join(".credentials.json"), "{}").unwrap();
        assert_eq!(
            config.account_route_states().await[1]["user@example.com"],
            "inactive"
        );
        config.codex.account_auth_file_only = false;
        config.claude.account_auth_file_only = false;
        assert!(
            config
                .account_route_states()
                .await
                .iter()
                .all(BTreeMap::is_empty)
        );
    }
    use serde_json::json;
    #[tokio::test]
    async fn default_api_base_keys_keep_rustls() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".env"), "PLAIN_TEST_KEY_5512=plain-key\n").unwrap();
        let c = Config::parse(&format!(
            "listen_port: 8787\nrequest_timeout_seconds: 3\ncodex:\n  homes: [{}]\n  routing:\n    api_key: {{PLAIN_TEST_KEY_5512: none}}\n",
            serde_json::to_string(dir.path()).unwrap()
        ))
        .unwrap();
        let route = c.resolve(Some("Bearer plain-key"), false).await.unwrap();
        assert_eq!(route.upstream, c.codex.base_url.api_key);
        assert!(!route.custom_upstream);
    }
    #[tokio::test]
    async fn configured_homes_keep_provider_config_and_credentials_together() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        for (home, token, host) in [
            (&a, "key-a", "a.example.com"),
            (&b, "key-b", "b.example.com"),
        ] {
            std::fs::create_dir(home).unwrap();
            std::fs::write(home.join("config.toml"), format!(
                "[model_providers.custom]\nenv_key = 'DIRECTORY_TEST_KEY_8371'\nbase_url = 'https://{host}/v1'\n"
            )).unwrap();
            std::fs::write(
                home.join(".env"),
                format!("DIRECTORY_TEST_KEY_8371={token}\n"),
            )
            .unwrap();
        }
        let mut c = Config::parse(&format!(
            "listen_port: 8787\nrequest_timeout_seconds: 3\ncodex:\n  homes: [{}, {}]\n  routing:\n    api_key: {{custom: none}}\n",
            serde_json::to_string(&a).unwrap(), serde_json::to_string(&b).unwrap()
        )).unwrap();
        c.check_credentials().await.unwrap();
        for (token, upstream) in [
            ("key-a", "https://a.example.com/v1"),
            ("key-b", "https://b.example.com/v1"),
        ] {
            let route = c
                .resolve(Some(&format!("Bearer {token}")), false)
                .await
                .unwrap();
            assert_eq!(route.upstream, upstream);
            // Own base_url: native TLS, as for explicit URL routes.
            assert!(route.custom_upstream);
        }
        // An unlisted home is not searched, even when its files remain present.
        c.codex.homes.pop();
        assert_eq!(
            c.resolve(Some("Bearer key-b"), false)
                .await
                .err()
                .unwrap()
                .status,
            401
        );
        c.codex.homes.push(b.to_string_lossy().into_owned());
        std::fs::write(b.join(".env"), "DIRECTORY_TEST_KEY_8371=key-a\n").unwrap();
        assert_eq!(
            c.resolve(Some("Bearer key-a"), false)
                .await
                .err()
                .unwrap()
                .status,
            409
        );
        assert!(c.check_credentials().await.is_err());
        // Saved provider authentication must also stay in its own home.
        for (home, token, host) in [
            (&a, "saved-a", "a.example.com"),
            (&b, "saved-b", "b.example.com"),
        ] {
            std::fs::write(home.join("config.toml"), format!(
                "[model_providers.custom]\nrequires_openai_auth = true\nbase_url = 'https://{host}/v1'\n"
            )).unwrap();
            std::fs::write(
                home.join("auth.json"),
                json!({"OPENAI_API_KEY":token}).to_string(),
            )
            .unwrap();
        }
        assert_eq!(
            c.resolve(Some("Bearer saved-b"), false)
                .await
                .unwrap()
                .upstream,
            "https://b.example.com/v1"
        );
    }

    #[tokio::test]
    async fn account_logins_come_from_each_home_and_its_credential_store() {
        let dir = tempfile::tempdir().unwrap();
        let login = |label: &str, token: &str| {
            json!({"tokens":{"account_id":label, "access_token":token}}).to_string()
        };
        for label in ["a", "b"] {
            let home = dir.path().join(label);
            std::fs::create_dir(&home).unwrap();
            std::fs::write(
                home.join("auth.json"),
                login(label, &format!("token-{label}")),
            )
            .unwrap();
        }
        // Home b uses the direct keyring on every platform; its auth.json is stale.
        let b = dir.path().join("b");
        std::fs::write(
            b.join("config.toml"),
            "cli_auth_credentials_store = \"keyring\"\n[features]\nsecret_auth_storage = false\n",
        )
        .unwrap();
        crate::keychain::set_test_entry(
            KEYRING_SERVICE,
            &keyring_account(&b),
            Some(&login("b", "keyring-b")),
        );
        let c = Config::parse(&format!(
            "listen_port: 8787\nrequest_timeout_seconds: 3\ncodex:\n  homes: [{}, {}]\n  auth_file: login.json\n  routing:\n    account: {{a: none, b: none}}\n",
            serde_json::to_string(&dir.path().join("a")).unwrap(),
            serde_json::to_string(&b).unwrap()
        )).unwrap();
        c.check_credentials().await.unwrap();
        for (token, account) in [("token-a", "a"), ("keyring-b", "b")] {
            assert_eq!(
                c.resolve(Some(&format!("Bearer {token}")), false)
                    .await
                    .unwrap()
                    .account_id
                    .as_deref(),
                Some(account)
            );
        }
        assert_eq!(
            c.resolve(Some("Bearer token-b"), false)
                .await
                .err()
                .unwrap()
                .status,
            401
        );
    }

    #[tokio::test]
    async fn saved_auth_follows_the_configured_credential_store() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let token = |value: Value| value["tokens"]["access_token"].as_str().map(String::from);
        let set_store = |store: &str| {
            std::fs::write(home.join("config.toml"), format!("model = \"x\"\ncli_auth_credentials_store = \"{store}\"\n[features]\nother = true\nsecret_auth_storage = false\n")).unwrap()
        };
        let keyring = |value: Option<&str>| {
            crate::keychain::set_test_entry(KEYRING_SERVICE, &keyring_account(home), value)
        };
        std::fs::write(
            home.join("auth.json"),
            r#"{"tokens":{"access_token":"file"}}"#,
        )
        .unwrap();
        // No config.toml, or the default store: only auth.json counts.
        assert_eq!(
            token(saved_auth(home).await.unwrap()).as_deref(),
            Some("file")
        );
        keyring(Some(r#"{"tokens":{"access_token":"keyring"}}"#));
        set_store("file");
        assert_eq!(
            token(saved_auth(home).await.unwrap()).as_deref(),
            Some("file")
        );
        set_store("keyring");
        assert_eq!(
            token(saved_auth(home).await.unwrap()).as_deref(),
            Some("keyring")
        );
        set_store("auto");
        assert_eq!(
            token(saved_auth(home).await.unwrap()).as_deref(),
            Some("keyring")
        );
        set_store("ephemeral");
        assert!(saved_auth(home).await.is_err());
        // The encrypted secrets backend is not a readable keyring entry.
        std::fs::write(
            home.join("config.toml"),
            "cli_auth_credentials_store = \"keyring\"\n[features]\nsecret_auth_storage = true\n",
        )
        .unwrap();
        assert!(saved_auth(home).await.is_err());
        std::fs::write(
            home.join("config.toml"),
            "cli_auth_credentials_store = \"auto\"\n[features]\nsecret_auth_storage = true\n",
        )
        .unwrap();
        assert_eq!(
            token(saved_auth(home).await.unwrap()).as_deref(),
            Some("file")
        );
        // Auto falls back to the file when the keyring has no login or fails.
        keyring(None);
        set_store("auto");
        assert_eq!(
            token(saved_auth(home).await.unwrap()).as_deref(),
            Some("file")
        );
        std::fs::remove_file(home.join("auth.json")).unwrap();
        assert!(saved_auth(home).await.is_err());
        set_store("unknown");
        assert!(saved_auth(home).await.is_err());
    }

    #[test]
    fn secret_auth_storage_defaults_and_overrides() {
        // Both an absent features table and an unrelated feature retain the
        // platform default; explicit settings override it on every platform.
        for config in ["", "[features]\nother = true\n"] {
            let settings: AuthSettings = toml::from_str(config).unwrap();
            assert_eq!(settings.features.secret_auth_storage, cfg!(windows));
        }
        assert_eq!(
            AuthSettings::default().features.secret_auth_storage,
            cfg!(windows)
        );
        for enabled in [false, true] {
            let settings: AuthSettings =
                toml::from_str(&format!("[features]\nsecret_auth_storage = {enabled}\n")).unwrap();
            assert_eq!(settings.features.secret_auth_storage, enabled);
        }
    }

    #[test]
    fn keyring_account_matches_codex() {
        let dir = tempfile::tempdir().unwrap();
        let canonical = dir.path().canonicalize().unwrap();
        let digest = ring::digest::digest(
            &ring::digest::SHA256,
            canonical.to_string_lossy().as_bytes(),
        );
        let hex: String = digest.as_ref().iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(keyring_account(dir.path()), format!("cli|{}", &hex[..16]));
    }

    #[tokio::test]
    async fn traffic_names_include_saved_logins_without_authorizing_them() {
        let dir = tempfile::tempdir().unwrap();
        for (file, id, email) in [
            ("auth-hmm.json", "123", "one@example.test"),
            ("auth-suzhi.json", "456", "two@example.test"),
            ("auth-unmatched.json", "789", "other@example.test"),
            ("unrelated.json", "999", "one@example.test"),
        ] {
            std::fs::write(
                dir.path().join(file),
                json!({"tokens": {
                    "account_id": id, "access_token": "archived-token",
                    "id_token": jwt(json!({"email": email}))
                }})
                .to_string(),
            )
            .unwrap();
        }
        std::fs::write(dir.path().join("auth-invalid.json"), "invalid").unwrap();
        let config = Config::parse(&format!(
            "listen_port: 7889\nrequest_timeout_seconds: 3\ncodex:\n  homes: [{}]\n  routing:\n    account: {{'one@example.test': none, 'two@example.test': none, default: none}}\n    account_fallback: none\n",
            serde_json::to_string(dir.path()).unwrap()
        )).unwrap();
        let labels = config.traffic_credential_labels().await;
        assert_eq!(labels[&("Codex".into(), "123".into())], "one@example.test");
        assert_eq!(labels[&("Codex".into(), "456".into())], "two@example.test");
        assert!(!labels.contains_key(&("Codex".into(), "789".into())));
        assert!(!labels.contains_key(&("Codex".into(), "999".into())));
        assert!(
            config
                .resolve(Some("Bearer archived-token"), true)
                .await
                .is_err()
        );
    }

    fn jwt(value: Value) -> String {
        format!(
            "e30.{}.signature",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&value).unwrap())
        )
    }
    #[tokio::test]
    async fn incoming_accounts_preserve_identity_precedence_and_fail_closed() {
        let token = jwt(
            json!({"https://api.openai.com/auth":{"chatgpt_account_id":"account-1"},"https://api.openai.com/profile":{"email":"profile@example.com"},"email":"top@example.com"}),
        );
        let i = Identity::from_token(&token).unwrap();
        assert_eq!(i.usernames, vec!["profile@example.com"]);
        let config=Config::parse("listen_port: 7889\nrequest_timeout_seconds: 3\nproxies:\n  other: http://localhost:8080\ncodex:\n  account_auth_file_only: false\n  routing:\n    account:\n      account-1: none\n      profile@example.com: other\n").unwrap();
        let route = config
            .resolve(Some(&format!("Bearer {token}")), true)
            .await
            .unwrap();
        assert_eq!(route.proxy.label(), "none");
        assert_eq!(route.account_label.as_deref(), Some("account-1"));
        let mut email_config = config.clone();
        email_config.codex.routing.account.remove("account-1");
        let route = email_config
            .resolve(Some(&format!("Bearer {token}")), true)
            .await
            .unwrap();
        assert_eq!(route.account_label.as_deref(), Some("profile@example.com"));
        assert_eq!(route.account_id.as_deref(), Some("account-1"));
        assert_eq!(route.proxy.label(), "other");
        email_config.codex.account_auth_file_only = true;
        assert!(
            email_config
                .resolve(Some(&format!("Bearer {token}")), true)
                .await
                .is_err()
        );
        assert!(
            config
                .resolve(Some("Bearer arbitrary"), true)
                .await
                .is_err()
        );
        let no_account = jwt(json!({"email":"profile@example.com"}));
        assert!(
            config
                .resolve(Some(&format!("Bearer {no_account}")), true)
                .await
                .is_err()
        );
        assert!(
            config
                .resolve(Some("Bearer token with spaces"), true)
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn saved_identity_rotates_and_duplicate_keys_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let auth = dir.path().join("auth.json");
        let env = dir.path().join(".env");
        let write = |id: &str, token: &str| {
            std::fs::write(&auth,json!({"tokens":{"account_id":id,"access_token":token,"id_token":jwt(json!({"email":"id@example.com"}))}}).to_string()).unwrap()
        };
        write("a", "token-a");
        std::fs::write(&env, "SAVED_IDENTITY_TEST_KEY_5170=other-key\n").unwrap();
        let c=Config::parse(&format!("listen_port: 7889\nrequest_timeout_seconds: 3\ncodex:\n  homes: [{}]\n  routing:\n    account: {{a: none, b: none}}\n    api_key: {{SAVED_IDENTITY_TEST_KEY_5170: none}}\n",serde_json::to_string(&dir.path()).unwrap())).unwrap();
        assert_eq!(
            Identity::read(auth.to_str().unwrap()).unwrap().usernames,
            vec!["id@example.com"]
        );
        assert!(c.resolve(Some("Bearer token-a"), true).await.is_ok());
        write("b", "token-b");
        assert!(c.resolve(Some("Bearer token-a"), true).await.is_err());
        assert_eq!(
            c.resolve(Some("Bearer token-b"), true)
                .await
                .unwrap()
                .account_id
                .as_deref(),
            Some("b")
        );
        std::fs::write(&env, "SAVED_IDENTITY_TEST_KEY_5170=token-b\n").unwrap();
        assert_eq!(
            c.resolve(Some("Bearer token-b"), true)
                .await
                .err()
                .unwrap()
                .status,
            409
        );
        assert!(c.check_credentials().await.is_err());
    }

    #[tokio::test]
    async fn codex_homes_route_by_identity_and_reject_shared_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let homes = [dir.path().join("a"), dir.path().join("b")];
        let write = |home: &Path, id: &str, token: &str| {
            std::fs::write(
                home.join("auth.json"),
                json!({"tokens":{"account_id":id,"access_token":token}}).to_string(),
            )
            .unwrap()
        };
        for (home, id, token) in [
            (&homes[0], "id-a", "secret-a"),
            (&homes[1], "id-b", "secret-b"),
        ] {
            std::fs::create_dir(home).unwrap();
            write(home, id, token);
        }
        let text = format!(
            "listen_port: 8787\nrequest_timeout_seconds: 3\nproxies:\n  selected: http://127.0.0.1:7893\ncodex:\n  homes: [{}, {}]\n  routing:\n    account:\n      id-a: selected\n      id-b: none\n",
            serde_json::to_string(&homes[0]).unwrap(),
            serde_json::to_string(&homes[1]).unwrap()
        );
        let mut c = Config::parse(&text).unwrap();
        c.check_credentials().await.unwrap();
        for (token, label) in [("secret-a", "selected"), ("secret-b", "none")] {
            assert_eq!(
                c.resolve(Some(&format!("Bearer {token}")), true)
                    .await
                    .unwrap()
                    .proxy
                    .label(),
                label
            );
        }
        c.codex
            .routing
            .account
            .insert("id-a".into(), crate::config::Choice::direct());
        assert_eq!(
            c.resolve(Some("Bearer secret-a"), true)
                .await
                .unwrap()
                .proxy
                .label(),
            "none"
        );
        write(&homes[1], "id-a", "secret-a");
        assert_eq!(
            c.resolve(Some("Bearer secret-a"), true)
                .await
                .err()
                .unwrap()
                .status,
            409
        );
        assert!(c.check_credentials().await.is_err());
    }
}
