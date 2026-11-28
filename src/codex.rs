//! Codex, the first App Coport routed: its saved logins, the model
//! providers its homes define, and the `.env` semantics it reads them with.
use crate::{
    Error, Result,
    config::{AccountSource, Choice, Routing, account_sources},
};
use serde::{Deserialize, Serialize};
use url::Url;

pub(crate) mod env;
mod login;
mod providers;

pub use login::Identity;
pub(crate) use login::saved_auth_store;
pub use providers::ProviderCredential;

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Bases {
    #[serde(default = "account_base")]
    pub account: String,
    #[serde(default = "api_base")]
    pub api_key: String,
}
fn account_base() -> String {
    CODEX_ACCOUNT_BASE.into()
}
const CODEX_ACCOUNT_BASE: &str = "https://chatgpt.com/backend-api";
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
impl Bases {
    pub(crate) fn validate_api_upstream(&self, url: &Url) -> Result<()> {
        for reserved in [
            self.account.as_str(),
            CODEX_ACCOUNT_BASE,
            crate::routing::TOKEN_REFRESH_UPSTREAM,
        ] {
            let reserved = Url::parse(reserved)
                .map_err(|_| Error::config("Invalid Codex account upstream."))?;
            if crate::routing::by_url::overlaps(url, &reserved) {
                return Err(Error::config(
                    "Codex API key upstream overlaps an account API or token refresh endpoint.",
                ));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Codex {
    pub homes: Vec<String>,
    pub base_url: Bases,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_env: Option<String>,
    pub routing: Routing,
    pub account_auth_file_only: bool,
}
impl Default for Codex {
    fn default() -> Self {
        Self {
            homes: default_codex_homes(),
            base_url: Bases::default(),
            auth_env: None,
            routing: Routing::default(),
            account_auth_file_only: true,
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

pub(crate) fn default_codex_homes() -> Vec<String> {
    vec!["~/.codex".into()]
}

impl Codex {
    pub(crate) fn account_sources(&self) -> Vec<(String, AccountSource)> {
        account_sources(self.auth_env.as_deref(), &self.homes, &self.routing)
    }
}
