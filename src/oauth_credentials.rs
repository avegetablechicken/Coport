//! Saved OAuth credentials retain their original storage backend on rotation.
use crate::{Error, Result, config::AccountSource, routing::Service};
use serde_json::Value;
use std::path::PathBuf;

#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) enum Store {
    File(PathBuf),
    Keychain(String, String),
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
        }
    }
}

pub(crate) async fn load(source: &AccountSource, service: Service) -> Result<(Store, Value)> {
    let AccountSource::Directory(dir) = source else {
        return Err(Error::config(
            "Environment credentials cannot be refreshed.",
        ));
    };
    match service {
        Service::Codex => crate::identity::saved_auth_store(dir).await,
        Service::Claude => crate::claude::saved_credentials_store(dir).await,
    }
}
pub(crate) fn access(value: &Value, service: Service) -> Option<&str> {
    match service {
        Service::Codex => value["tokens"]["access_token"].as_str(),
        Service::Claude => value["claudeAiOauth"]["accessToken"].as_str(),
    }
    .filter(|s| crate::identity::valid_token(s))
}
pub(crate) fn refresh(value: &Value, service: Service) -> Option<&str> {
    match service {
        Service::Codex => value["tokens"]["refresh_token"].as_str(),
        Service::Claude => value["claudeAiOauth"]["refreshToken"].as_str(),
    }
    .filter(|s| crate::identity::valid_token(s))
}
