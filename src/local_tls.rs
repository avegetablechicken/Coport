//! Loopback TLS for clients that refuse plain-HTTP base URLs (Codex 0.160+
//! requires an HTTPS `chatgpt_base_url`). A private CA is created once and
//! kept beside the configuration; clients trust `ca.pem`. The serving
//! certificate is reissued from that CA on every start.
use crate::{Error, Result};
use rcgen::{
    BasicConstraints, CertificateParams, CidrSubnet, DistinguishedName, DnType,
    ExtendedKeyUsagePurpose, GeneralSubtree, IsCa, Issuer, KeyPair, KeyUsagePurpose,
    NameConstraints,
};
use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio_rustls::{
    TlsAcceptor,
    rustls::{
        self,
        pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer},
    },
};

pub const CA_FILE: &str = "ca.pem";
const CA_KEY_FILE: &str = "ca-key.pem";

/// Directory holding the CA for the configuration at `config`.
pub fn dir_for(config: &Path) -> PathBuf {
    config.parent().unwrap_or(Path::new(".")).join("tls")
}

/// Load (or create) the CA in `dir` and build an acceptor for 127.0.0.1 and localhost.
pub fn acceptor(dir: &Path) -> Result<TlsAcceptor> {
    let ca_key = load_or_create_ca(dir).map_err(failed)?;
    // Only the name, key identifier method and key usages are taken from the
    // issuer, so rebuilding the parameters matches the stored certificate.
    let issuer = Issuer::new(ca_params(), &ca_key);
    let mut params =
        CertificateParams::new(vec!["localhost".into(), "127.0.0.1".into()]).map_err(failed)?;
    params
        .distinguished_name
        .push(DnType::CommonName, "coport loopback");
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    params.use_authority_key_identifier_extension = true;
    // Apple rejects TLS server certificates valid for over 825 days, even under
    // a user-trusted CA. Reissued on every start, so a year is ample.
    let now = time::OffsetDateTime::now_utc();
    params.not_before = now - time::Duration::days(1);
    params.not_after = now + time::Duration::days(365);
    let key = KeyPair::generate().map_err(failed)?;
    let cert = params.signed_by(&key, &issuer).map_err(failed)?;
    let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .and_then(|b| {
        b.with_no_client_auth().with_single_cert(
            vec![CertificateDer::from(cert.der().to_vec())],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
        )
    })
    .map_err(failed)?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(TlsAcceptor::from(Arc::new(config)))
}

fn failed<E>(_: E) -> Error {
    Error::config("Cannot prepare the loopback TLS certificate.")
}

fn ca_params() -> CertificateParams {
    let mut params = CertificateParams::default();
    let mut name = DistinguishedName::new();
    name.push(DnType::CommonName, "coport local CA");
    params.distinguished_name = name;
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    // Safe to add to a system trust store: the CA can only vouch for loopback.
    params.name_constraints = Some(NameConstraints {
        permitted_subtrees: vec![
            GeneralSubtree::DnsName("localhost".into()),
            GeneralSubtree::IpAddress(CidrSubnet::from_v4_prefix([127, 0, 0, 1], 32)),
        ],
        excluded_subtrees: vec![],
    });
    params
}

fn load_or_create_ca(dir: &Path) -> std::io::Result<KeyPair> {
    let (cert_path, key_path) = (dir.join(CA_FILE), dir.join(CA_KEY_FILE));
    if cert_path.is_file()
        && let Ok(text) = std::fs::read_to_string(&key_path)
        && let Ok(key) = KeyPair::from_pem(&text)
    {
        return Ok(key);
    }
    std::fs::create_dir_all(dir)?;
    let key = KeyPair::generate().map_err(std::io::Error::other)?;
    let cert = ca_params()
        .self_signed(&key)
        .map_err(std::io::Error::other)?;
    // NamedTempFile is created owner-only; the key keeps that mode.
    persist(dir, &key_path, key.serialize_pem().as_bytes())?;
    persist(dir, &cert_path, cert.pem().as_bytes())?;
    Ok(key)
}

fn persist(dir: &Path, path: &Path, data: &[u8]) -> std::io::Result<()> {
    let mut temp = tempfile::NamedTempFile::new_in(dir)?;
    temp.write_all(data)?;
    temp.as_file().sync_all()?;
    temp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ca_is_created_once_and_reused() {
        let dir = tempfile::tempdir().unwrap();
        acceptor(dir.path()).unwrap();
        let first = std::fs::read(dir.path().join(CA_FILE)).unwrap();
        acceptor(dir.path()).unwrap();
        assert_eq!(first, std::fs::read(dir.path().join(CA_FILE)).unwrap());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join(CA_KEY_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0);
        }
    }
}
