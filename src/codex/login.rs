//! The Codex CLI's saved logins: the ChatGPT account a login names and
//! where Codex keeps it for a home (`auth.json` or the OS credential store).
use crate::{
    Error, Result,
    codex::Codex,
    config::{AccountSource, Config, expand},
    identity::{claims, environment_key_with_shell, key, routing_account_label, valid_token},
};
use serde::Deserialize;
use serde_json::Value;
use std::{collections::HashSet, path::Path};

#[derive(Clone)]
pub struct Identity {
    pub account_id: String,
    pub token: String,
    pub refresh_token: Option<String>,
    pub usernames: Vec<String>,
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
    /// The account a successful token response names, by the ChatGPT
    /// account of its access or ID token.
    pub(crate) fn token_response(value: &Value) -> Option<Self> {
        let token = value["access_token"].as_str().unwrap_or("");
        let (access, id) = (
            claims(token),
            claims(value["id_token"].as_str().unwrap_or("")),
        );
        let account_id = [&access, &id].into_iter().find_map(|c| {
            c["https://api.openai.com/auth"]["chatgpt_account_id"]
                .as_str()
                .filter(|s| valid_token(s))
        })?;
        Some(Self {
            account_id: account_id.into(),
            token: token.into(),
            refresh_token: None,
            usernames: usernames(&access, &id),
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
    Ok(saved_auth_store(home).await?.1)
}
pub(crate) async fn saved_auth_store(home: &Path) -> Result<(crate::saved_login::Store, Value)> {
    use crate::saved_login::Store;
    let settings = auth_settings(home)?;
    let file = || {
        let store = Store::File(home.join("auth.json"));
        Ok((store.clone(), store.read_file()?))
    };
    let keyring = || async {
        if settings.features.secret_auth_storage {
            return Err(Error::config(
                "Codex encrypted auth storage (secret_auth_storage) is not supported.",
            ));
        }
        let account = keyring_account(home);
        crate::keychain::read(KEYRING_SERVICE, &account)
            .await?
            .map(|raw| {
                serde_json::from_str::<Value>(&raw)
                    .map(|value| (Store::Keychain(KEYRING_SERVICE.into(), account), value))
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
impl Codex {
    pub(crate) async fn account_identity(&self, source: &AccountSource) -> Result<Identity> {
        self.account_identity_with_shell(source, true).await
    }
    pub(crate) async fn account_identity_with_shell(
        &self,
        source: &AccountSource,
        allow_shell: bool,
    ) -> Result<Identity> {
        let name = match source {
            AccountSource::Directory(home) => return Identity::parse(&saved_auth(home).await?),
            AccountSource::Env(name) => name,
        };
        let mut found = None;
        let inherited = crate::codex::env::inherited();
        for home in &self.homes {
            let environment = crate::codex::env::load(&expand(home).join(".env"), &inherited);
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
            None => environment_key_with_shell(name, allow_shell).await?,
        };
        Identity::from_token(&token).ok_or(Error::config(
            "Codex account token requires ChatGPT account claims.",
        ))
    }
}
impl Config {
    /// Codex's logins for Route activation, from local data only. Archived
    /// logins are deliberately excluded: only configured sources can admit
    /// requests. This never runs a login shell, so an environment source this
    /// process cannot read is `Unreadable`.
    pub(crate) async fn codex_logins(&self) -> Vec<crate::route_activation::Login> {
        use crate::route_activation::Login;
        let mut logins = Vec::new();
        for (source, account) in self.codex.account_sources() {
            match self
                .codex
                .account_identity_with_shell(&account, false)
                .await
            {
                Ok(identity) => logins.extend(
                    routing_account_label(
                        &self.codex.routing,
                        &identity.account_id,
                        &identity.usernames,
                        &source,
                    )
                    .map(Login::Active),
                ),
                Err(_) if matches!(account, AccountSource::Env(_)) => {
                    logins.push(Login::Unreadable)
                }
                Err(_) => {}
            }
        }
        logins
    }

    /// Whether `routing.account` names this identity itself.
    pub(crate) fn account_mapped(&self, identity: &Identity) -> bool {
        let account = &self.codex.routing.account;
        account.contains_key(&identity.account_id)
            || identity.usernames.iter().any(|u| account.contains_key(u))
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
            .or(self.codex.routing.account_default())
            .cloned()
            .ok_or(Error::config(
                "Current account has no proxy mapping; forwarding refused.",
            ))
    }
    pub(crate) async fn check_codex_credentials(&self) -> Result<()> {
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
        let codex = &self.codex;
        codex
            .api_keys()
            .check_distinct(&mut keys, |selector| async move {
                let provider = crate::codex::Provider {
                    selector: selector.into(),
                    proxy: crate::config::Choice::direct(),
                };
                let credentials = provider
                    .credentials(&codex.base_url.api_key, true, codex)
                    .await?;
                Ok(credentials.into_iter().map(|c| c.token).collect())
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use serde_json::json;

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
                    .account_id(),
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

    #[tokio::test]
    async fn rotated_oauth_credentials_write_back_to_the_selected_keyring() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.toml"),
            r#"cli_auth_credentials_store = "keyring"
[features]
secret_auth_storage = false
"#,
        )
        .unwrap();
        let original = serde_json::json!({"tokens":{"account_id":"test", "access_token":"old", "refresh_token":"refresh"}, "other":42});
        crate::keychain::set_test_entry(
            KEYRING_SERVICE,
            &keyring_account(dir.path()),
            Some(&original.to_string()),
        );
        let (store, read) = saved_auth_store(dir.path()).await.unwrap();
        assert_eq!(read, original);
        let mut updated = original.clone();
        updated["tokens"]["access_token"] = "new".into();
        store.replace(&read, &updated).await.unwrap();
        assert_eq!(saved_auth(dir.path()).await.unwrap(), updated);
        assert!(!dir.path().join("auth.json").exists());
        assert!(store.replace(&read, &original).await.is_err());
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
            .unwrap()
            .account();
        assert_eq!(route.proxy.label(), "none");
        assert_eq!(route.account_label.as_deref(), Some("account-1"));
        let mut email_config = config.clone();
        email_config.codex.routing.account.remove("account-1");
        let route = email_config
            .resolve(Some(&format!("Bearer {token}")), true)
            .await
            .unwrap()
            .account();
        assert_eq!(route.account_label.as_deref(), Some("profile@example.com"));
        assert_eq!(route.account_id(), Some("account-1"));
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
                .account_id(),
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
                    .proxy()
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
                .proxy()
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
