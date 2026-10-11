//! Loopback TLS for clients that refuse plain-HTTP base URLs (Codex 0.156+
//! requires an HTTPS `chatgpt_base_url`). A private CA is created once and
//! kept beside the configuration; clients trust `ca.pem`. The serving
//! certificate is reissued from that CA on every start.
//!
//! Proxy-port credential routing uses a second CA, constrained to the API
//! hosts it decrypts, so trusting it never covers loopback or other sites.
use crate::{Error, Result};
use rcgen::{
    BasicConstraints, CertificateParams, CidrSubnet, DistinguishedName, DnType,
    ExtendedKeyUsagePurpose, GeneralSubtree, IsCa, Issuer, KeyPair, KeyUsagePurpose,
    NameConstraints, PublicKeyData,
};
use std::{
    collections::{BTreeSet, HashMap},
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio_rustls::{
    TlsAcceptor,
    rustls::{
        self,
        pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, pem::PemObject},
        server::{ClientHello, ResolvesServerCert},
        sign::CertifiedKey,
    },
};

pub const CA_FILE: &str = "ca.pem";
const CA_KEY_FILE: &str = "ca-key.pem";
pub const PROXY_CA_FILE: &str = "proxy-ca.pem";
const PROXY_CA_KEY_FILE: &str = "proxy-ca-key.pem";
/// Held while a CA is created, so concurrent first starts write one pair.
const CA_LOCK_FILE: &str = "ca.lock";

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
    // BoringSSL (Bun, used by Claude Code) rejects a certificate carrying an IP
    // name under a CA with IP name constraints. Each certificate holds one
    // name and is chosen by SNI: localhost by name, 127.0.0.1 (no SNI) by IP.
    let certs = Arc::new(LoopbackCerts {
        localhost: leaf("localhost", "coport loopback", &issuer)?,
        ip: leaf("127.0.0.1", "coport loopback", &issuer)?,
    });
    server_acceptor(certs).map_err(failed)
}

/// Load (or create) the proxy-port CA in `dir`, valid for every name in
/// `hosts`, and build one acceptor per host. A stored CA whose constraints
/// omit a host is replaced, and must then be trusted again.
pub fn proxy_acceptors(
    dir: &Path,
    hosts: &BTreeSet<String>,
) -> Result<HashMap<String, TlsAcceptor>> {
    fn failed<E>(_: E) -> Error {
        Error::config("Cannot prepare the proxy port certificate.")
    }
    let ca_key = load_or_create(dir, &PROXY, &proxy_ca_params(hosts), &|cert| {
        hosts.iter().all(|host| permits(cert, host))
    })
    .map_err(failed)?;
    let issuer = Issuer::new(proxy_ca_params(hosts), &ca_key);
    hosts
        .iter()
        .map(|host| {
            let cert = leaf(host, "coport proxy", &issuer).map_err(failed)?;
            let acceptor = server_acceptor(Arc::new(SingleName {
                name: host.clone(),
                cert,
            }))
            .map_err(failed)?;
            Ok((host.clone(), acceptor))
        })
        .collect()
}

fn server_acceptor(certs: Arc<dyn ResolvesServerCert>) -> Result<TlsAcceptor> {
    let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(failed)?
    .with_no_client_auth()
    .with_cert_resolver(certs);
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// The certificate for the CONNECT destination; a client naming another
/// host in SNI is refused rather than shown a certificate it would reject.
#[derive(Debug)]
struct SingleName {
    name: String,
    cert: Arc<CertifiedKey>,
}
impl ResolvesServerCert for SingleName {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        hello
            .server_name()
            .is_none_or(|sni| sni.eq_ignore_ascii_case(&self.name))
            .then(|| self.cert.clone())
    }
}

#[derive(Debug)]
struct LoopbackCerts {
    localhost: Arc<CertifiedKey>,
    ip: Arc<CertifiedKey>,
}
impl ResolvesServerCert for LoopbackCerts {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(if hello.server_name() == Some("localhost") {
            self.localhost.clone()
        } else {
            self.ip.clone()
        })
    }
}

fn leaf(name: &str, common_name: &str, issuer: &Issuer<'_, &KeyPair>) -> Result<Arc<CertifiedKey>> {
    let mut params = CertificateParams::new(vec![name.into()]).map_err(failed)?;
    params
        .distinguished_name
        .push(DnType::CommonName, common_name);
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    params.use_authority_key_identifier_extension = true;
    // Apple rejects TLS server certificates valid for over 825 days, even under
    // a user-trusted CA. Reissued on every start, so a year is ample.
    let now = time::OffsetDateTime::now_utc();
    params.not_before = now - time::Duration::days(1);
    params.not_after = now + time::Duration::days(365);
    let key = KeyPair::generate().map_err(failed)?;
    let cert = params.signed_by(&key, issuer).map_err(failed)?;
    let signing = rustls::crypto::ring::sign::any_supported_type(&PrivateKeyDer::Pkcs8(
        PrivatePkcs8KeyDer::from(key.serialize_der()),
    ))
    .map_err(failed)?;
    Ok(Arc::new(CertifiedKey::new(
        vec![CertificateDer::from(cert.der().to_vec())],
        signing,
    )))
}

fn failed<E>(_: E) -> Error {
    Error::config("Cannot prepare the loopback TLS certificate.")
}

/// Certificate and key file names of one CA.
struct Authority {
    cert: &'static str,
    key: &'static str,
}
const LOOPBACK: Authority = Authority {
    cert: CA_FILE,
    key: CA_KEY_FILE,
};
const PROXY: Authority = Authority {
    cert: PROXY_CA_FILE,
    key: PROXY_CA_KEY_FILE,
};

fn ca_template(common_name: &str, permitted: Vec<GeneralSubtree>) -> CertificateParams {
    let mut params = CertificateParams::default();
    let mut name = DistinguishedName::new();
    name.push(DnType::CommonName, common_name);
    params.distinguished_name = name;
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    params.name_constraints = Some(NameConstraints {
        permitted_subtrees: permitted,
        excluded_subtrees: vec![],
    });
    params
}

fn ca_params() -> CertificateParams {
    // Safe to add to a system trust store: the CA can only vouch for loopback.
    ca_template(
        "coport local CA",
        vec![
            GeneralSubtree::DnsName("localhost".into()),
            GeneralSubtree::IpAddress(CidrSubnet::from_v4_prefix([127, 0, 0, 1], 32)),
        ],
    )
}

fn proxy_ca_params(hosts: &BTreeSet<String>) -> CertificateParams {
    ca_template(
        "coport proxy CA",
        hosts.iter().cloned().map(GeneralSubtree::DnsName).collect(),
    )
}

/// Whether the DER certificate `cert` lists `host` as a permitted dNSName.
fn permits(cert: &[u8], host: &str) -> bool {
    // A context-specific [2] IA5String: the tag, a DER length, then the name.
    let mut encoded = vec![0x82];
    match host.len() {
        n @ 0..=0x7f => encoded.push(n as u8),
        n @ 0x80..=0xff => encoded.extend([0x81, n as u8]),
        _ => return false,
    }
    encoded.extend_from_slice(host.as_bytes());
    cert.windows(encoded.len()).any(|w| w == encoded)
}

fn load_or_create_ca(dir: &Path) -> std::io::Result<KeyPair> {
    load_or_create(dir, &LOOPBACK, &ca_params(), &|_| true)
}

/// The stored key of `authority` if its certificate is acceptable, otherwise
/// a newly created pair from `params` replacing it.
fn load_or_create(
    dir: &Path,
    authority: &Authority,
    params: &CertificateParams,
    acceptable: &dyn Fn(&[u8]) -> bool,
) -> std::io::Result<KeyPair> {
    if let Some(key) = load(dir, authority, acceptable) {
        return Ok(key);
    }
    std::fs::create_dir_all(dir)?;
    let _lock = CreationLock::acquire(&dir.join(CA_LOCK_FILE), Duration::from_secs(30))?;
    // Another instance may have created the pair while this one waited.
    if let Some(key) = load(dir, authority, acceptable) {
        return Ok(key);
    }
    let key = KeyPair::generate().map_err(std::io::Error::other)?;
    let cert = params.self_signed(&key).map_err(std::io::Error::other)?;
    // NamedTempFile is created owner-only; the key keeps that mode.
    persist(
        dir,
        &dir.join(authority.key),
        key.serialize_pem().as_bytes(),
    )?;
    persist(dir, &dir.join(authority.cert), cert.pem().as_bytes())?;
    Ok(key)
}

#[cfg(test)]
fn load_ca(dir: &Path) -> Option<KeyPair> {
    load(dir, &LOOPBACK, &|_| true)
}

/// The stored key, only if its certificate certifies it. A certificate for
/// another key can never be verified, so such a pair is replaced like a missing one.
fn load(dir: &Path, authority: &Authority, acceptable: &dyn Fn(&[u8]) -> bool) -> Option<KeyPair> {
    let key = KeyPair::from_pem(&std::fs::read_to_string(dir.join(authority.key)).ok()?).ok()?;
    let cert = CertificateDer::from_pem_file(dir.join(authority.cert)).ok()?;
    let public = key.subject_public_key_info();
    (cert.windows(public.len()).any(|w| w == public.as_slice()) && acceptable(&cert)).then_some(key)
}

/// An operating system lock on a file beside the CA, held while the CA is
/// created. The system releases it when its holder exits, however it ends,
/// so the file is never removed and no holder can release another's lock.
struct CreationLock {
    _file: std::fs::File,
}

impl CreationLock {
    fn acquire(path: &Path, wait: Duration) -> std::io::Result<Self> {
        let mut options = std::fs::OpenOptions::new();
        options.create(true).truncate(false).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        // Without sharing, a second open fails while the first is held.
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.share_mode(0);
        }
        let deadline = Instant::now() + wait;
        loop {
            match options.open(path).and_then(lock_exclusively) {
                Ok(file) => return Ok(Self { _file: file }),
                Err(e) if is_held(&e) && Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(e) => return Err(e),
            }
        }
    }
}

#[cfg(unix)]
fn lock_exclusively(file: std::fs::File) -> std::io::Result<std::fs::File> {
    use std::os::unix::io::AsRawFd;
    // SAFETY: the descriptor belongs to `file`, which is open during the call.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        Ok(file)
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(unix))]
fn lock_exclusively(file: std::fs::File) -> std::io::Result<std::fs::File> {
    Ok(file)
}

/// Whether another holder has the lock: a refused flock, or on Windows a
/// sharing or lock violation.
fn is_held(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::WouldBlock
        || (cfg!(windows) && matches!(error.raw_os_error(), Some(32 | 33)))
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

    #[test]
    fn a_certificate_for_another_key_is_replaced_with_a_matching_pair() {
        let dir = tempfile::tempdir().unwrap();
        load_or_create_ca(dir.path()).unwrap();
        let other = tempfile::tempdir().unwrap();
        load_or_create_ca(other.path()).unwrap();
        let mismatched = std::fs::read(other.path().join(CA_FILE)).unwrap();
        std::fs::write(dir.path().join(CA_FILE), &mismatched).unwrap();
        assert!(load_ca(dir.path()).is_none());
        let key = load_or_create_ca(dir.path()).unwrap();
        assert_ne!(std::fs::read(dir.path().join(CA_FILE)).unwrap(), mismatched);
        assert_eq!(
            load_ca(dir.path()).unwrap().public_key_raw(),
            key.public_key_raw()
        );
    }

    #[test]
    fn concurrent_first_starts_write_one_consistent_pair() {
        for _ in 0..10 {
            let dir = tempfile::tempdir().unwrap();
            let barrier = std::sync::Barrier::new(8);
            let keys: Vec<Vec<u8>> = std::thread::scope(|scope| {
                let handles: Vec<_> = (0..8)
                    .map(|_| {
                        scope.spawn(|| {
                            barrier.wait();
                            load_or_create_ca(dir.path())
                                .unwrap()
                                .public_key_raw()
                                .to_vec()
                        })
                    })
                    .collect();
                handles.into_iter().map(|h| h.join().unwrap()).collect()
            });
            let stored = load_ca(dir.path()).unwrap();
            assert!(keys.iter().all(|k| k == stored.public_key_raw()));
        }
    }

    #[test]
    fn a_lock_file_left_behind_does_not_block_creation() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(CA_LOCK_FILE), b"").unwrap();
        load_or_create_ca(dir.path()).unwrap();
        assert!(load_ca(dir.path()).is_some());
    }

    #[test]
    fn a_creation_lock_is_released_only_by_its_own_holder() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CA_LOCK_FILE);
        let first = CreationLock::acquire(&path, Duration::ZERO).unwrap();
        assert!(CreationLock::acquire(&path, Duration::ZERO).is_err());
        drop(first);
        let second = CreationLock::acquire(&path, Duration::ZERO).unwrap();
        // However long the first holder took, it has nothing left to release.
        assert!(CreationLock::acquire(&path, Duration::ZERO).is_err());
        drop(second);
        CreationLock::acquire(&path, Duration::ZERO).unwrap();
    }

    fn hosts(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    #[test]
    fn proxy_ca_is_reused_while_it_covers_every_host() {
        let dir = tempfile::tempdir().unwrap();
        let both = hosts(&["api.anthropic.com", "chatgpt.com"]);
        let acceptors = proxy_acceptors(dir.path(), &both).unwrap();
        assert_eq!(acceptors.len(), 2);
        let first = std::fs::read(dir.path().join(PROXY_CA_FILE)).unwrap();
        // The loopback CA is separate and untouched.
        assert!(!dir.path().join(CA_FILE).exists());
        proxy_acceptors(dir.path(), &hosts(&["chatgpt.com"])).unwrap();
        assert_eq!(
            first,
            std::fs::read(dir.path().join(PROXY_CA_FILE)).unwrap()
        );
        proxy_acceptors(dir.path(), &hosts(&["chatgpt.com", "api.openai.com"])).unwrap();
        let replaced = std::fs::read(dir.path().join(PROXY_CA_FILE)).unwrap();
        assert_ne!(first, replaced);
        let cert = CertificateDer::from_pem_file(dir.path().join(PROXY_CA_FILE)).unwrap();
        assert!(permits(&cert, "api.openai.com"));
        assert!(!permits(&cert, "api.anthropic.com"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join(PROXY_CA_KEY_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0);
        }
    }

    #[test]
    fn proxy_ca_constraints_match_whole_names_only() {
        let key = KeyPair::generate().unwrap();
        let cert = proxy_ca_params(&hosts(&["xapi.anthropic.com", &"a".repeat(130)]))
            .self_signed(&key)
            .unwrap();
        let der = cert.der();
        assert!(permits(der, "xapi.anthropic.com"));
        assert!(permits(der, &"a".repeat(130)));
        assert!(!permits(der, "api.anthropic.com"));
        assert!(!permits(der, "anthropic.com"));
        assert!(!permits(der, &"a".repeat(129)));
    }

    #[tokio::test]
    async fn proxy_certificates_are_only_offered_for_their_destination() {
        let dir = tempfile::tempdir().unwrap();
        let acceptors = proxy_acceptors(dir.path(), &hosts(&["api.anthropic.com"])).unwrap();
        let mut roots = rustls::RootCertStore::empty();
        roots
            .add(CertificateDer::from_pem_file(dir.path().join(PROXY_CA_FILE)).unwrap())
            .unwrap();
        let client = tokio_rustls::TlsConnector::from(Arc::new(
            rustls::ClientConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth(),
        ));
        for (name, accepted) in [("api.anthropic.com", true), ("chatgpt.com", false)] {
            let (near, far) = tokio::io::duplex(65536);
            let acceptor = acceptors["api.anthropic.com"].clone();
            let server = tokio::spawn(async move { acceptor.accept(far).await.is_ok() });
            let server_name = rustls::pki_types::ServerName::try_from(name.to_owned()).unwrap();
            // Kept open until the server finishes writing its session tickets.
            let connected = client.connect(server_name, near).await;
            assert_eq!(server.await.unwrap(), accepted, "{name}");
            assert_eq!(connected.is_ok(), accepted, "{name}");
        }
    }
}
