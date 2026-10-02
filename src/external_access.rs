//! Credentials and transport policy for the independent, read-only data API.
use crate::{
    Error, Result,
    config::{expand, validate_env},
};
use ipnet::Ipv4Net;
use ring::hmac;
use serde::{Deserialize, Serialize};
use std::{io::Read, net::IpAddr, path::Path, sync::Arc};
use tokio_rustls::{
    TlsAcceptor,
    rustls::{
        self,
        pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject},
    },
};

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DataConfig {
    pub port: u16,
    pub token_env: Option<String>,
    pub token_file: Option<String>,
    #[serde(default)]
    pub trusted_lan: Vec<Ipv4Net>,
    pub tls: Option<DataTls>,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DataTls {
    pub certificate: String,
    pub private_key: String,
}
impl DataConfig {
    pub fn validate(&self, proxy_port: u16) -> Result<()> {
        if self.port == 0 || self.port == proxy_port {
            return Err(Error::config("The data API needs its own nonzero port."));
        }
        match (&self.token_env, &self.token_file) {
            (Some(env), None) => validate_env(env)?,
            (None, Some(path)) => absolute(path)?,
            _ => {
                return Err(Error::config(
                    "Configure exactly one external data token_env or token_file.",
                ));
            }
        }
        if self.trusted_lan.len() > 32
            || self
                .trusted_lan
                .iter()
                .any(|net| !net.network().is_private() || !net.broadcast().is_private())
        {
            return Err(Error::config(
                "Trusted LAN CIDRs must be entirely within RFC1918 private IPv4 ranges.",
            ));
        }
        if self.trusted_lan.is_empty() && self.tls.is_none() {
            return Err(Error::config(
                "Configure trusted LAN CIDRs or HTTPS certificates for the data API.",
            ));
        }
        if let Some(tls) = &self.tls {
            absolute(&tls.certificate)?;
            absolute(&tls.private_key)?;
        }
        Ok(())
    }
    pub fn permits_http(&self, peer: IpAddr) -> bool {
        peer.is_loopback()
            || match peer {
                IpAddr::V4(ip) => {
                    ip.is_private() && self.trusted_lan.iter().any(|net| net.contains(&ip))
                }
                _ => false,
            }
    }
    pub fn load_token(&self) -> Result<String> {
        let token = match (&self.token_env, &self.token_file) {
            (Some(env), None) => std::env::var(env).map_err(|_| {
                Error::config("The data access key environment variable is unavailable.")
            })?,
            (None, Some(path)) => String::from_utf8(read_limited(&expand(path), true, 4096)?)
                .map_err(|_| Error::config("Invalid data access key file."))?,
            _ => return Err(Error::config("Invalid data access key source.")),
        };
        let token = token.trim_end_matches(['\r', '\n']).to_owned();
        DataKey::new(&token)?;
        Ok(token)
    }
    pub fn load_key(&self) -> Result<DataKey> {
        DataKey::new(&self.load_token()?)
    }
}
fn absolute(path: &str) -> Result<()> {
    if path.len() > 4096 || path.chars().any(char::is_control) || !expand(path).is_absolute() {
        return Err(Error::config(
            "External data credential and certificate paths must be absolute (or start with ~/).",
        ));
    }
    Ok(())
}
/// Open first, then inspect that same handle to avoid a check/read race.
pub fn read_limited(path: &Path, private: bool, limit: u64) -> Result<Vec<u8>> {
    let file = std::fs::File::open(path)
        .map_err(|_| Error::config("Cannot open external data credential or certificate file."))?;
    let meta = file
        .metadata()
        .map_err(|_| Error::config("Cannot inspect external data file."))?;
    if !meta.is_file() {
        return Err(Error::config("External data files must be regular files."));
    }
    #[cfg(unix)]
    if private {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: geteuid has no pointers or side effects.
        let uid = unsafe { libc::geteuid() };
        if meta.mode() & 0o077 != 0 || meta.uid() != uid {
            return Err(Error::config(
                "Data key and TLS private key files must be owned by this user and have mode 600 or 400.",
            ));
        }
    }
    #[cfg(not(unix))]
    if private {
        return Err(Error::config(
            "Private-file ownership checks are unavailable on this platform; use an environment data key.",
        ));
    }
    let mut data = Vec::new();
    let bound = limit
        .checked_add(1)
        .ok_or_else(|| Error::config("Invalid external data file size limit."))?;
    file.take(bound)
        .read_to_end(&mut data)
        .map_err(|_| Error::config("Cannot read external data file."))?;
    if data.len() as u64 > limit {
        return Err(Error::config("External data file exceeds its size limit."));
    }
    Ok(data)
}

/// The 256-bit secret is not serializable and never enters API DTOs or logs.
pub struct DataKey(hmac::Key);
impl DataKey {
    pub fn new(token: &str) -> Result<Self> {
        let key = decode(token).ok_or_else(|| Error::config("Data access keys must contain 64 hexadecimal characters; generate with openssl rand -hex 32."))?;
        Ok(Self(hmac::Key::new(hmac::HMAC_SHA256, &key)))
    }
    pub fn accepts(&self, token: &str) -> bool {
        let Ok(candidate) = Self::new(token) else {
            return false;
        };
        let tag = hmac::sign(&candidate.0, b"coport-data-auth-v1");
        hmac::verify(&self.0, b"coport-data-auth-v1", tag.as_ref()).is_ok()
    }
    pub fn reference(&self, kind: &str, canonical: &str) -> String {
        let mut context = hmac::Context::with_key(&self.0);
        context.update(b"coport-data-identity-v1\0");
        context.update(kind.as_bytes());
        context.update(b"\0");
        context.update(canonical.as_bytes());
        context
            .sign()
            .as_ref()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }
}
fn decode(token: &str) -> Option<[u8; 32]> {
    if token.len() != 64 || !token.is_ascii() {
        return None;
    }
    let mut key = [0; 32];
    for (i, value) in key.iter_mut().enumerate() {
        *value = u8::from_str_radix(&token[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(key)
}
pub fn tls_acceptor(tls: &DataTls) -> Result<TlsAcceptor> {
    let cert = read_limited(&expand(&tls.certificate), false, 1024 * 1024)?;
    let key = read_limited(&expand(&tls.private_key), true, 1024 * 1024)?;
    let certs = CertificateDer::pem_slice_iter(&cert)
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|_| Error::config("Invalid data HTTPS certificate chain."))?;
    let key = PrivateKeyDer::from_pem_slice(&key)
        .map_err(|_| Error::config("Invalid data HTTPS private key."))?;
    let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|_| Error::config("Cannot configure data HTTPS protocols."))?
    .with_no_client_auth()
    .with_single_cert(certs, key)
    .map_err(|_| Error::config("Data HTTPS certificate and key do not match."))?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(TlsAcceptor::from(Arc::new(config)))
}
#[cfg(test)]
mod tests {
    use super::*;
    const KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    #[test]
    fn auth_and_references_are_scoped_and_opaque() {
        let key = DataKey::new(KEY).unwrap();
        assert!(key.accepts(KEY));
        assert!(!key.accepts(&"f".repeat(64)));
        assert!(!key.accepts(""));
        assert_ne!(
            key.reference("proxy", "secret-host"),
            key.reference("upstream", "secret-host")
        );
        assert!(!key.reference("proxy", "secret-host").contains("secret"));
        assert_ne!(
            key.reference("proxy", "secret-host"),
            DataKey::new(&"f".repeat(64))
                .unwrap()
                .reference("proxy", "secret-host")
        );
    }
    #[test]
    fn http_policy_does_not_trust_forwarded_or_merely_private_addresses() {
        let config: DataConfig = serde_yaml_ng::from_str(
            "port: 8788\ntoken_env: TEST_KEY\ntrusted_lan: [10.42.0.0/24]\n",
        )
        .unwrap();
        config.validate(8787).unwrap();
        assert!(config.permits_http("10.42.0.196".parse().unwrap()));
        for ip in ["10.43.0.1", "192.168.1.1", "8.8.8.8", "169.254.1.1"] {
            assert!(!config.permits_http(ip.parse().unwrap()));
        }
        for cidr in ["0.0.0.0/0", "8.8.8.0/24", "172.0.0.0/8"] {
            let c: DataConfig = serde_yaml_ng::from_str(&format!(
                "port: 8788\ntoken_env: TEST_KEY\ntrusted_lan: [{cidr}]\n"
            ))
            .unwrap();
            assert!(c.validate(8787).is_err());
        }
    }
}

#[cfg(all(test, unix))]
mod file_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    #[test]
    fn private_credentials_are_bounded_and_never_read_from_world_readable_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("private.key");
        std::fs::write(&path, "f".repeat(64)).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_limited(&path, true, 4096).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(read_limited(&path, true, 4096).is_ok());
        std::fs::write(&path, vec![b'f'; 4097]).unwrap();
        assert!(read_limited(&path, true, 4096).is_err());
    }
}
