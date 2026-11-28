//! Codex's model providers: the `model_providers` a home's `config.toml`
//! defines, and the Bearer credential and upstream each route resolves to.
use super::login::saved_auth;
use crate::{
    Error, Result,
    codex::{Codex, Provider},
    config::{expand, unwrap_upstream},
    identity::{environment_key_with_shell, key, valid_token},
};
use serde::Deserialize;
use std::{collections::BTreeMap, path::Path};

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
        if crate::routing::by_url::is_url_selector(selector) {
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
                let upstream = provider_upstream(self.definition(&defs)?, default)?;
                codex.base_url.validate_api_upstream(
                    &crate::routing::by_url::validate_upstream(&upstream)?,
                )?;
                Ok(upstream)
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
        let inherited = crate::codex::env::inherited();
        for home in &codex.homes {
            let home = expand(home);
            let environment = crate::codex::env::load(&home.join(".env"), &inherited);
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
        environment: Option<&crate::codex::env::Environment>,
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

#[cfg(test)]
mod tests {
    use crate::config::Config;
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
        let route = c
            .resolve(Some("Bearer plain-key"), false)
            .await
            .unwrap()
            .api();
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
                .unwrap()
                .api();
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
                .upstream(),
            "https://b.example.com/v1"
        );
    }
}
