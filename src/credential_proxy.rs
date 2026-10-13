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
    /// Credential-routed origins the stored proxy CA does not permit.
    uncovered: BTreeSet<(String, u16)>,
    unrestricted: bool,
}

impl CredentialProxy {
    /// `None` unless `connect_credential_routing` is enabled. The proxy CA is
    /// kept in `tls_dir` beside the loopback CA. Origins the stored CA does
    /// not permit are left undecrypted (see `uncovered`).
    pub fn prepare(config: &Config, tls_dir: &Path) -> Result<Option<Self>> {
        if !config.connect_credential_routing {
            return Ok(None);
        }
        let scopes = scopes(config)?;
        let ca = crate::local_tls::proxy_acceptors(tls_dir, &hosts(&scopes)?)?;
        let mut destinations = HashMap::new();
        let mut uncovered = BTreeSet::new();
        for ((host, port), scope) in scopes {
            let Some(acceptor) = ca.acceptors.get(&host) else {
                uncovered.insert((host, port));
                continue;
            };
            let destination = Destination {
                host: host.clone(),
                port,
                acceptor: acceptor.clone(),
                scope: scope.into(),
            };
            destinations.insert((host, port), Arc::new(destination));
        }
        Ok(Some(Self {
            destinations,
            uncovered,
            unrestricted: ca.unrestricted,
        }))
    }

    /// Replace the proxy CA with one for every origin `config` routes by
    /// credential, once `trust` accepts its certificate file. Takes effect on
    /// the next start.
    pub fn replace_ca(
        config: &Config,
        tls_dir: &Path,
        trust: impl FnOnce(&Path) -> std::io::Result<()>,
    ) -> std::io::Result<()> {
        let hosts = scopes(config)
            .and_then(|scopes| hosts(&scopes))
            .map_err(|e| std::io::Error::other(e.message))?;
        crate::local_tls::replace_proxy_ca(tls_dir, &hosts, trust)
    }

    /// Whether `host:port` is credential-routed but not decrypted, since the
    /// stored proxy CA does not permit it.
    pub(crate) fn is_uncovered(&self, host: &str, port: u16) -> bool {
        self.uncovered.contains(&(host.to_owned(), port))
    }

    /// Credential-routed origins left undecrypted, as `host:port`.
    pub fn uncovered(&self) -> Vec<String> {
        self.uncovered
            .iter()
            .map(|(host, port)| format!("{host}:{port}"))
            .collect()
    }

    /// Whether the stored proxy CA leaves a name type unconstrained.
    pub fn ca_unrestricted(&self) -> bool {
        self.unrestricted
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

/// Hosts the proxy CA must permit for `scopes`.
fn hosts(scopes: &BTreeMap<(String, u16), String>) -> Result<BTreeSet<String>> {
    let hosts: BTreeSet<String> = scopes.keys().map(|(host, _)| host.clone()).collect();
    if hosts.is_empty() {
        return Err(Error::config(
            "connect_credential_routing requires codex or claude routes.",
        ));
    }
    Ok(hosts)
}

/// Request prefixes of each credential-routed API origin, keyed by host and
/// port. Hosts routed by both services stay unprefixed, so the usual explicit
/// URL rules decide (or report the conflict).
fn scopes(config: &Config) -> Result<BTreeMap<(String, u16), String>> {
    let mut origins: BTreeMap<(String, u16), &str> = BTreeMap::new();
    let mut add = |url: &Url, prefix: &'static str| {
        let (Some(host), Some(port)) = (url.host_str(), url.port_or_known_default()) else {
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
        || claude
            .api_key
            .keys()
            .any(|key| !crate::claude_settings::is_url(key))
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
        .filter(|k| crate::claude_settings::is_url(k))
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
    // Resolve configured provider definitions, not credentials or active profiles.
    // One unavailable home must not hide routes from the other configured homes.
    for provider in &config.codex.providers {
        for upstream in provider
            .upstreams(&config.codex.base_url.api_key, &config.codex)
            .into_iter()
            .flatten()
        {
            add(&url(&upstream)?, "/codex/");
        }
    }
    for key in claude
        .api_key
        .keys()
        .filter(|key| !crate::claude_settings::is_url(key))
    {
        if let Ok(Some(upstream)) =
            crate::claude_settings::upstream(&config.claude.config_dirs, key)
        {
            add(&url(&upstream)?, "/anthropic/");
        }
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
        let mut parsed = Config::parse(&format!(
            "listen_port: 8787\nrequest_timeout_seconds: 3\nconnect_credential_routing: true\n{extra}"
        ))
        .unwrap();
        parsed.codex.homes.clear();
        parsed.claude.config_dirs.clear();
        parsed
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
        // No Codex API Key selector; public IP URL routes are decrypted too.
        assert_eq!(get("api.openai.com", 443), None);
        assert_eq!(
            get("182.92.106.196", 6060),
            Some("/anthropic/https://182.92.106.196:6060")
        );
        assert_eq!(scopes.len(), 6);
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
    fn provider_and_named_settings_destinations_use_configured_sources() {
        let dirs = tempfile::tempdir().unwrap();
        let a = dirs.path().join("a");
        let b = dirs.path().join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        for (home, host) in [(&a, "gateway.invalid"), (&b, "182.92.106.196:6060")] {
            // No token is needed for destination discovery.
            std::fs::write(home.join("config.toml"), format!(
                "[model_providers.ShareCoder]\nbase_url = 'https://{host}/v1'\nenv_key = 'FAKE_PROVIDER_KEY'\n"
            )).unwrap();
        }
        std::fs::write(
            a.join("custom.settings.json"),
            r#"{"env":{"ANTHROPIC_BASE_URL":"https://claude-gateway.invalid/api"}}"#,
        )
        .unwrap();
        let mut c = config(
            "codex:\n  routing:\n    api_key: {ShareCoder: none}\nclaude:\n  routing:\n    api_key: {custom.settings.json: none}\n",
        );
        c.codex.homes = vec![a.to_string_lossy().into(), b.to_string_lossy().into()];
        c.claude.config_dirs = vec![a.to_string_lossy().into()];
        let routes = scopes(&c).unwrap();
        for (host, port, prefix) in [
            ("gateway.invalid", 443, "codex"),
            ("182.92.106.196", 6060, "codex"),
            ("claude-gateway.invalid", 443, "anthropic"),
        ] {
            assert!(routes[&(host.into(), port)].starts_with(&format!("/{prefix}/https://")));
        }
        assert!(!routes.contains_key(&("custom.settings.json".into(), 443)));
        // env_key aliases use the same definition, and other homes survive an unreadable definition.
        c.codex.providers[0].selector = "FAKE_PROVIDER_KEY".into();
        std::fs::write(b.join("config.toml"), "invalid [").unwrap();
        assert!(
            scopes(&c)
                .unwrap()
                .contains_key(&("gateway.invalid".into(), 443))
        );
    }

    #[test]
    fn hosts_outside_the_stored_ca_stay_undecrypted_until_it_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let accounts = "claude:\n  routing:\n    account: {default: none}\n";
        CredentialProxy::prepare(&config(accounts), dir.path()).unwrap();
        let grown = config(&format!(
            "{accounts}    api_key:\n      'https://182.92.106.196:6060': none\n"
        ));
        let proxy = CredentialProxy::prepare(&grown, dir.path())
            .unwrap()
            .unwrap();
        assert_eq!(proxy.uncovered(), ["182.92.106.196:6060"]);
        assert!(proxy.is_uncovered("182.92.106.196", 6060));
        assert!(proxy.destination("182.92.106.196", 6060).is_none());
        assert!(proxy.destination("api.anthropic.com", 443).is_some());
        assert!(!proxy.ca_unrestricted());
        let refused = CredentialProxy::replace_ca(&grown, dir.path(), |_| {
            Err(std::io::Error::other("cancelled"))
        });
        assert!(refused.is_err());
        let proxy = CredentialProxy::prepare(&grown, dir.path())
            .unwrap()
            .unwrap();
        assert_eq!(proxy.uncovered(), ["182.92.106.196:6060"]);
        CredentialProxy::replace_ca(&grown, dir.path(), |_| Ok(())).unwrap();
        let proxy = CredentialProxy::prepare(&grown, dir.path())
            .unwrap()
            .unwrap();
        assert!(proxy.uncovered().is_empty());
        assert!(proxy.destination("182.92.106.196", 6060).is_some());
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
