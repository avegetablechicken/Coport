//! Credential routing for proxy-port clients. A CONNECT to an API host that
//! the codex or claude section routes by credential is decrypted with the
//! proxy CA; each request inside then takes the explicit upstream URL form,
//! so it is matched and forwarded exactly like a base URL request.
use crate::{Error, Result, config::Config};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::Path,
    sync::Arc,
};
use tokio_rustls::TlsAcceptor;
use url::Url;

pub(crate) struct Destination {
    pub host: String,
    pub port: u16,
    pub acceptor: TlsAcceptor,
    /// Prepended to each request target, e.g. `/anthropic/https://api.anthropic.com`.
    pub scope: Arc<str>,
}

pub struct CredentialProxy {
    destinations: HashMap<(String, u16), Arc<Destination>>,
}

impl CredentialProxy {
    /// `None` unless `connect_credential_routing` is enabled. The proxy CA is
    /// kept in `tls_dir` beside the loopback CA.
    pub fn prepare(config: &Config, tls_dir: &Path) -> Result<Option<Self>> {
        if !config.connect_credential_routing {
            return Ok(None);
        }
        let scopes = scopes(config)?;
        let hosts: BTreeSet<String> = scopes.keys().map(|(host, _)| host.clone()).collect();
        if hosts.is_empty() {
            return Err(Error::config(
                "connect_credential_routing requires codex or claude routes.",
            ));
        }
        let acceptors = crate::local_tls::proxy_acceptors(tls_dir, &hosts)?;
        let destinations = scopes
            .into_iter()
            .map(|((host, port), scope)| {
                let destination = Destination {
                    host: host.clone(),
                    port,
                    acceptor: acceptors[&host].clone(),
                    scope: scope.into(),
                };
                ((host, port), Arc::new(destination))
            })
            .collect();
        Ok(Some(Self { destinations }))
    }

    pub(crate) fn destination(&self, host: &str, port: u16) -> Option<Arc<Destination>> {
        self.destinations.get(&(host.to_owned(), port)).cloned()
    }

    /// Decrypted destinations as `host:port`, for logs and diagnostics.
    pub fn authorities(&self) -> Vec<String> {
        let mut names: Vec<_> = self
            .destinations
            .keys()
            .map(|(host, port)| format!("{host}:{port}"))
            .collect();
        names.sort();
        names
    }
}

/// Request prefixes of each credential-routed API origin, keyed by host and
/// port. Hosts routed by both services stay unprefixed, so the usual explicit
/// URL rules decide (or report the conflict). IP upstreams keep base URLs.
fn scopes(config: &Config) -> Result<BTreeMap<(String, u16), String>> {
    let mut origins: BTreeMap<(String, u16), &str> = BTreeMap::new();
    let mut add = |url: &Url, prefix: &'static str| {
        let (Some(url::Host::Domain(host)), Some(port)) = (url.host(), url.port_or_known_default())
        else {
            return;
        };
        origins
            .entry((host.to_ascii_lowercase(), port))
            .and_modify(|p| {
                if *p != prefix {
                    *p = "/";
                }
            })
            .or_insert(prefix);
    };
    let claude = &config.claude.routing;
    let codex = &config.codex.routing;
    let url = |value: &str| crate::url_routing::validate_upstream(value);
    let credential_selectors = |routes: &BTreeMap<String, crate::config::Choice>| {
        routes
            .keys()
            .any(|key| !crate::url_routing::is_url_selector(key))
    };
    if !claude.account.is_empty()
        || claude.account_fallback.is_some()
        || claude.api_key_fallback.is_some()
        || credential_selectors(&claude.api_key)
    {
        add(&url(&config.claude.base_url)?, "/anthropic/");
        add(&url(crate::claude::TOKEN_REFRESH_UPSTREAM)?, "/anthropic/");
    }
    if !codex.account.is_empty() || codex.account_fallback.is_some() {
        add(&url(&config.codex.base_url.account)?, "/codex/");
        add(&url(crate::routing::TOKEN_REFRESH_UPSTREAM)?, "/codex/");
    }
    if codex.api_key_fallback.is_some() || credential_selectors(&codex.api_key) {
        add(&url(&config.codex.base_url.api_key)?, "/codex/");
    }
    for key in claude
        .api_key
        .keys()
        .filter(|k| crate::url_routing::is_url_selector(k))
    {
        add(&url(key)?, "/anthropic/");
    }
    for key in codex
        .api_key
        .keys()
        .filter(|k| crate::url_routing::is_url_selector(k))
    {
        add(&url(key)?, "/codex/");
    }
    Ok(origins
        .into_iter()
        .map(|((host, port), prefix)| {
            let authority = if port == 443 {
                host.clone()
            } else {
                format!("{host}:{port}")
            };
            ((host, port), format!("{prefix}https://{authority}"))
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(extra: &str) -> Config {
        Config::parse(&format!(
            "listen_port: 8787\nrequest_timeout_seconds: 3\nconnect_credential_routing: true\n{extra}"
        ))
        .unwrap()
    }

    #[test]
    fn credential_routed_hosts_select_their_service() {
        let config = config(
            "codex:\n  routing:\n    account: {default: none}\n    api_key:\n      'gateway.example.com:8443/v1': none\nclaude:\n  routing:\n    account: {default: none}\n    api_key:\n      'https://182.92.106.196:6060': none\n",
        );
        let scopes = scopes(&config).unwrap();
        let get = |host: &str, port| scopes.get(&(host.to_owned(), port)).map(String::as_str);
        assert_eq!(
            get("api.anthropic.com", 443),
            Some("/anthropic/https://api.anthropic.com")
        );
        assert_eq!(
            get("platform.claude.com", 443),
            Some("/anthropic/https://platform.claude.com")
        );
        assert_eq!(get("chatgpt.com", 443), Some("/codex/https://chatgpt.com"));
        assert_eq!(
            get("auth.openai.com", 443),
            Some("/codex/https://auth.openai.com")
        );
        assert_eq!(
            get("gateway.example.com", 8443),
            Some("/codex/https://gateway.example.com:8443")
        );
        // No Codex API Key selector, and IP routes are not decrypted.
        assert_eq!(get("api.openai.com", 443), None);
        assert_eq!(scopes.len(), 5);
    }

    #[test]
    fn a_host_routed_by_both_services_stays_unprefixed() {
        let config = config(
            "codex:\n  routing:\n    api_key:\n      'shared.example.com/openai': none\nclaude:\n  routing:\n    api_key:\n      'shared.example.com/anthropic': none\n",
        );
        assert_eq!(
            scopes(&config).unwrap()[&("shared.example.com".to_owned(), 443)],
            "/https://shared.example.com"
        );
    }

    #[test]
    fn disabled_or_unrouted_configurations_decrypt_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let off = Config::parse(
            "listen_port: 8787\nrequest_timeout_seconds: 3\nclaude:\n  routing:\n    account: {default: none}\n",
        )
        .unwrap();
        assert!(
            CredentialProxy::prepare(&off, dir.path())
                .unwrap()
                .is_none()
        );
        assert!(CredentialProxy::prepare(&config(""), dir.path()).is_err());
        assert!(!dir.path().join(crate::local_tls::PROXY_CA_FILE).exists());
    }
}
