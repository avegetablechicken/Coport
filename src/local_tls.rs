//! Loopback TLS for clients that refuse plain-HTTP base URLs (Codex 0.156+
//! requires an HTTPS `chatgpt_base_url`). A private CA is created once and
//! kept beside the configuration; clients trust `ca.pem`. The serving
//! certificate is reissued from that CA on every start.
use crate::{Error, Result};
use rcgen::{
    BasicConstraints, CertificateParams, CidrSubnet, DistinguishedName, DnType,
    ExtendedKeyUsagePurpose, GeneralSubtree, IsCa, Issuer, KeyPair, KeyUsagePurpose,
    NameConstraints, PublicKeyData,
};
use std::{
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
        localhost: leaf("localhost", &issuer)?,
        ip: leaf("127.0.0.1", &issuer)?,
    });
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

fn leaf(name: &str, issuer: &Issuer<'_, &KeyPair>) -> Result<Arc<CertifiedKey>> {
    let mut params = CertificateParams::new(vec![name.into()]).map_err(failed)?;
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
    if let Some(key) = load_ca(dir) {
        return Ok(key);
    }
    std::fs::create_dir_all(dir)?;
    let _lock = CreationLock::acquire(&dir.join(CA_LOCK_FILE), Duration::from_secs(30))?;
    // Another instance may have created the pair while this one waited.
    if let Some(key) = load_ca(dir) {
        return Ok(key);
    }
    let key = KeyPair::generate().map_err(std::io::Error::other)?;
    let cert = ca_params()
        .self_signed(&key)
        .map_err(std::io::Error::other)?;
    // NamedTempFile is created owner-only; the key keeps that mode.
    persist(dir, &dir.join(CA_KEY_FILE), key.serialize_pem().as_bytes())?;
    persist(dir, &dir.join(CA_FILE), cert.pem().as_bytes())?;
    Ok(key)
}

/// The stored key, only if `ca.pem` certifies it. A certificate for another
/// key can never be verified, so such a pair is replaced like a missing one.
fn load_ca(dir: &Path) -> Option<KeyPair> {
    let key = KeyPair::from_pem(&std::fs::read_to_string(dir.join(CA_KEY_FILE)).ok()?).ok()?;
    let cert = CertificateDer::from_pem_file(dir.join(CA_FILE)).ok()?;
    let public = key.subject_public_key_info();
    cert.windows(public.len())
        .any(|w| w == public.as_slice())
        .then_some(key)
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
}
