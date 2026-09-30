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
    pub fn read(path: &str) -> Result<Self> {
        let raw =
            std::fs::read(expand(path)).map_err(|_| Error::config("Cannot read auth_file."))?;
        let value: Value =
            serde_json::from_slice(&raw).map_err(|_| Error::config("Invalid auth_file."))?;
        let tokens = &value["tokens"];
        let id = tokens["account_id"]
            .as_str()
            .filter(|s| valid_token(s))
            .ok_or(Error::config("auth_file requires tokens.account_id."))?;
        let token = tokens["access_token"]
            .as_str()
            .filter(|s| valid_token(s))
            .ok_or(Error::config("auth_file requires tokens.access_token."))?;
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

fn saved_provider_auth(path: &std::path::Path) -> Result<(String, Option<String>)> {
    let raw =
        std::fs::read(path).map_err(|_| Error::config("Cannot read Codex provider auth.json."))?;
    let value: Value = serde_json::from_slice(&raw)
        .map_err(|_| Error::config("Invalid Codex provider auth.json."))?;
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
                "Codex provider auth.json requires OPENAI_API_KEY.",
            ))?)?,
            None,
        )),
        "chatgpt" => {
            let token = key(value["tokens"]["access_token"]
                .as_str()
                .ok_or(Error::config(
                    "Codex provider auth.json requires tokens.access_token.",
                ))?)?;
            let account = value["tokens"]["account_id"]
                .as_str()
                .filter(|s| valid_token(s))
                .map(String::from);
            Ok((token, account))
        }
        _ => Err(Error::config(
            "Unsupported Codex provider auth.json auth_mode.",
        )),
    }
}
impl Provider {
    pub async fn credentials(
        &self,
        default: &str,
        shell: bool,
        codex: &Codex,
    ) -> Result<Vec<ProviderCredential>> {
        if codex.homes.is_empty() || self.api_key_file.is_some() {
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
        if let Some(path) = &self.api_key_file {
            let raw = std::fs::read_to_string(expand(path))
                .map_err(|_| Error::config("Cannot read API Key file."))?;
            let upstream = unwrap_upstream(self.upstream_base_url.as_deref().unwrap_or(default))?;
            return Ok(ProviderCredential {
                token: key(&raw)?,
                upstream,
                account_id: None,
            });
        }
        let defs = definitions(home)?;
        let selected_id = self
            .name
            .as_deref()
            .or(self.selector.as_deref().filter(|s| defs.contains_key(*s)));
        let selected_env = self.api_key_env.as_deref().or(if selected_id.is_none() {
            self.selector.as_deref()
        } else {
            None
        });
        let named = if let Some(id) = selected_id {
            Some((
                id,
                defs.get(id).ok_or(Error::config(
                    "Codex Provider ID has no API Key configuration.",
                ))?,
            ))
        } else {
            None
        };
        let reversed = if named.is_none() {
            if let Some(var) = selected_env {
                let candidates: Vec<_> = defs
                    .iter()
                    .filter(|(_, d)| d.env_key.as_deref() == Some(var))
                    .collect();
                if candidates.len() > 1 {
                    return Err(Error::config(
                        "API Key environment variable matches multiple Codex providers.",
                    ));
                }
                candidates.first().map(|(id, d)| (id.as_str(), *d))
            } else {
                None
            }
        } else {
            None
        };
        let definition = named.or(reversed);
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
            saved_provider_auth(
                &home
                    .ok_or(Error::config(
                        "Saved provider authentication requires a configured Codex home.",
                    ))?
                    .join("auth.json"),
            )?
        } else {
            return Err(Error::config(
                "Codex provider has no configured Bearer credential.",
            ));
        };
        let upstream = self
            .upstream_base_url
            .as_deref()
            .or(definition
                .filter(|(id, _)| *id != "openai")
                .and_then(|(_, d)| d.base_url.as_deref()))
            .unwrap_or(default);
        Ok(ProviderCredential {
            token,
            upstream: unwrap_upstream(upstream)?,
            account_id,
        })
    }
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
        if let Some(name) = &source.auth_env {
            let mut found = None;
            let inherited = crate::codex_env::inherited();
            for home in &self.homes {
                let environment = crate::codex_env::load(&expand(home).join(".env"), &inherited);
                if let Some(value) = environment.get(name) {
                    let token = key(value)?;
                    if found.as_ref().is_some_and(|previous| previous != &token) {
                        return Err(Error::config(
                            "Account environment variable has different values across Codex homes; use auth_file sources.",
                        ));
                    }
                    found = Some(token);
                }
            }
            if let Some(token) = found {
                return Identity::from_token(&token).ok_or(Error::config(
                    "Codex account token requires ChatGPT account claims.",
                ));
            }
        }
        source.codex_identity().await
    }
}

impl AccountSource {
    pub(crate) async fn codex_identity(&self) -> Result<Identity> {
        if let Some(name) = &self.auth_env {
            let token = environment_key(name).await?;
            return Identity::from_token(&token).ok_or(Error::config(
                "Codex account token requires ChatGPT account claims.",
            ));
        }
        Identity::read(self.auth_file.as_deref().ok_or(Error::config(
            "Codex account requires an explicit credential source.",
        ))?)
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
        "coding-agent-proxy",
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
            for (label, source, _) in &self.codex.account_sources() {
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
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
            assert_eq!(
                c.resolve(Some(&format!("Bearer {token}")), false)
                    .await
                    .unwrap()
                    .upstream,
                upstream
            );
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
    async fn relative_account_files_use_each_configured_home() {
        let dir = tempfile::tempdir().unwrap();
        for label in ["a", "b"] {
            let home = dir.path().join(label);
            std::fs::create_dir(&home).unwrap();
            std::fs::write(
                home.join("login.json"),
                json!({"tokens":{
                    "account_id":label, "access_token":format!("token-{label}")
                }})
                .to_string(),
            )
            .unwrap();
        }
        let c = Config::parse(&format!(
            "listen_port: 8787\nrequest_timeout_seconds: 3\ncodex:\n  homes: [{}, {}]\n  auth_file: login.json\n  routing:\n    account: {{a: none, b: none}}\n",
            serde_json::to_string(&dir.path().join("a")).unwrap(),
            serde_json::to_string(&dir.path().join("b")).unwrap()
        )).unwrap();
        let c = Config::parse(&c.canonical_yaml().unwrap()).unwrap();
        c.check_credentials().await.unwrap();
        for label in ["a", "b"] {
            assert_eq!(
                c.resolve(Some(&format!("Bearer token-{label}")), false)
                    .await
                    .unwrap()
                    .account_id
                    .as_deref(),
                Some(label)
            );
        }
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
        let config=Config::parse("listen_port: 7889\nrequest_timeout_seconds: 3\naccount_auth_file_only: false\nrouting:\n  account:\n    account-1: none\n    profile@example.com: other\nproxies:\n  other: http://localhost:8080\n").unwrap();
        let route = config
            .resolve(Some(&format!("Bearer {token}")), true)
            .await
            .unwrap();
        assert_eq!(route.proxy.label(), "none");
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
        let keyfile = dir.path().join("api.key");
        let write = |id: &str, token: &str| {
            std::fs::write(&auth,json!({"tokens":{"account_id":id,"access_token":token,"id_token":jwt(json!({"email":"id@example.com"}))}}).to_string()).unwrap()
        };
        write("a", "token-a");
        std::fs::write(&keyfile, "other-key").unwrap();
        let quoted = |p: &std::path::Path| serde_json::to_string(&p.to_string_lossy()).unwrap();
        let c=Config::parse(&format!("listen_port: 7889\nrequest_timeout_seconds: 3\nauth_file: {}\naccounts:\n  a: none\n  b: none\napi_key_providers:\n  - name: test\n    api_key_file: {}\n    proxy: none\n",quoted(&auth),quoted(&keyfile))).unwrap();
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
        std::fs::write(&keyfile, "token-b").unwrap();
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
    async fn symmetric_codex_sources_route_by_label_and_preserve_identity_priority() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.json");
        let b = dir.path().join("b.json");
        for (path, id, token) in [(&a, "id-a", "secret-a"), (&b, "id-b", "secret-b")] {
            std::fs::write(
                path,
                json!({"tokens":{"account_id":id,"access_token":token}}).to_string(),
            )
            .unwrap();
        }
        let text = format!(
            "listen_port: 8787\nrequest_timeout_seconds: 3\nproxies:\n  selected: http://127.0.0.1:7893\ncodex:\n  accounts:\n    personal:\n      auth_file: {}\n    work:\n      auth_file: {}\n  routing:\n    account:\n      personal: selected\n      work: none\n",
            serde_json::to_string(&a.to_string_lossy()).unwrap(),
            serde_json::to_string(&b.to_string_lossy()).unwrap()
        );
        let mut c = Config::parse(&text).unwrap();
        c.check_credentials().await.unwrap();
        assert_eq!(
            c.resolve(Some("Bearer secret-a"), true)
                .await
                .unwrap()
                .proxy
                .label(),
            "selected"
        );
        assert_eq!(
            c.resolve(Some("Bearer secret-b"), true)
                .await
                .unwrap()
                .proxy
                .label(),
            "none"
        );
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
        c.codex
            .accounts
            .insert("duplicate".into(), c.codex.accounts["personal"].clone());
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

    #[tokio::test]
    async fn legacy_source_label_cannot_create_an_account_route() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("auth.json");
        std::fs::write(
            &file,
            json!({"tokens":{"account_id":"unmapped","access_token":"secret"}}).to_string(),
        )
        .unwrap();
        let old = format!(
            "listen_port: 8787\nrequest_timeout_seconds: 3\nauth_file: {}\nrouting:\n  account:\n    default: none\n",
            serde_json::to_string(&file.to_string_lossy()).unwrap()
        );
        let c = Config::parse(&old).unwrap();
        assert!(c.resolve(Some("Bearer secret"), true).await.is_err());
        let migrated = Config::parse(&c.canonical_yaml().unwrap()).unwrap();
        assert!(migrated.resolve(Some("Bearer secret"), true).await.is_err());
    }
}
