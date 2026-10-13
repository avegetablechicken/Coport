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
/// Replaced pairs are moved here, since clients may still trust them.
pub const BACKUP_DIR: &str = "backup";

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
        covers(cert, hosts)
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

/// Permitted only for a CA without DNS hosts; reserved never to resolve (RFC 2606).
const UNUSED_DNS_NAME: &str = "invalid";
/// The excluded all-IPv4 and all-IPv6 subtrees: [7] OCTET STRING of a zero
/// address and a zero mask.
const ANY_IPV4: [u8; 10] = [0x87, 8, 0, 0, 0, 0, 0, 0, 0, 0];
const ANY_IPV6: [u8; 34] = {
    let mut encoded = [0; 34];
    encoded[0] = 0x87;
    encoded[1] = 32;
    encoded
};

fn proxy_ca_params(hosts: &BTreeSet<String>) -> CertificateParams {
    let mut permitted: Vec<_> = hosts
        .iter()
        .map(|host| match host.parse::<std::net::IpAddr>() {
            Ok(std::net::IpAddr::V4(ip)) => {
                GeneralSubtree::IpAddress(CidrSubnet::from_v4_prefix(ip.octets(), 32))
            }
            Ok(std::net::IpAddr::V6(ip)) => {
                GeneralSubtree::IpAddress(CidrSubnet::from_v6_prefix(ip.octets(), 128))
            }
            Err(_) => GeneralSubtree::DnsName(host.clone()),
        })
        .collect();
    // Constraints bind only the name types they list, so a type without a
    // decrypted host is closed rather than left unconstrained.
    if !permitted
        .iter()
        .any(|s| matches!(s, GeneralSubtree::DnsName(_)))
    {
        permitted.push(GeneralSubtree::DnsName(UNUSED_DNS_NAME.into()));
    }
    let no_ip_host = !permitted
        .iter()
        .any(|s| matches!(s, GeneralSubtree::IpAddress(_)));
    let mut params = ca_template("coport proxy CA", permitted);
    if no_ip_host {
        // BoringSSL rejects only IP certificates under IP constraints; this CA issues none.
        params.name_constraints.as_mut().unwrap().excluded_subtrees = vec![
            GeneralSubtree::IpAddress(CidrSubnet::from_v4_prefix([0; 4], 0)),
            GeneralSubtree::IpAddress(CidrSubnet::from_v6_prefix([0; 16], 0)),
        ];
    }
    params
}

/// Whether a stored proxy CA can issue for every host and closes the name
/// types `hosts` lacks, as `proxy_ca_params` would.
fn covers(cert: &[u8], hosts: &BTreeSet<String>) -> bool {
    let contains = |encoded: &[u8]| cert.windows(encoded.len()).any(|w| w == encoded);
    let ip = |host: &String| host.parse::<std::net::IpAddr>().is_ok();
    hosts.iter().all(|host| permits(cert, host))
        && (hosts.iter().any(|h| !ip(h)) || permits(cert, UNUSED_DNS_NAME))
        && (hosts.iter().any(ip) || (contains(&ANY_IPV4) && contains(&ANY_IPV6)))
}

/// Whether the DER certificate lists the DNS name or exact IP constraint.
fn permits(cert: &[u8], host: &str) -> bool {
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        let bytes = match ip {
            std::net::IpAddr::V4(ip) => ip.octets().to_vec(),
            std::net::IpAddr::V6(ip) => ip.octets().to_vec(),
        };
        let mut encoded = vec![0x87, (bytes.len() * 2) as u8];
        encoded.extend(&bytes);
        encoded.extend(vec![255; bytes.len()]);
        return cert.windows(encoded.len()).any(|w| w == encoded);
    }
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
    back_up(dir, authority)?;
    // NamedTempFile is created owner-only; the key keeps that mode.
    persist(
        dir,
        &dir.join(authority.key),
        key.serialize_pem().as_bytes(),
    )?;
    persist(dir, &dir.join(authority.cert), cert.pem().as_bytes())?;
    Ok(key)
}

/// Move the stored files of `authority` into `BACKUP_DIR` under one
/// replacement stamp. A pair that cannot be kept is not overwritten.
fn back_up(dir: &Path, authority: &Authority) -> std::io::Result<()> {
    let stored: Vec<_> = [authority.cert, authority.key]
        .into_iter()
        .filter(|name| dir.join(name).exists())
        .collect();
    if stored.is_empty() {
        return Ok(());
    }
    let backup = dir.join(BACKUP_DIR);
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(&backup)?;
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let stamp = format!(
        "{millis}-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    );
    for name in stored {
        let stem = name.strip_suffix(".pem").unwrap_or(name);
        // A rename keeps the key's owner-only mode.
        std::fs::rename(dir.join(name), backup.join(format!("{stem}.{stamp}.pem")))?;
    }
    Ok(())
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
        assert!(!dir.path().join(BACKUP_DIR).exists());
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
        let kept = backups(dir.path());
        assert_eq!(kept.len(), 2);
        assert!(
            kept.iter()
                .any(|(name, data)| name.starts_with("ca.") && *data == mismatched)
        );
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

    /// Names and contents of the files in the backup directory, by name.
    fn backups(dir: &Path) -> Vec<(String, Vec<u8>)> {
        let mut files: Vec<_> = std::fs::read_dir(dir.join(BACKUP_DIR))
            .unwrap()
            .map(|entry| {
                let path = entry.unwrap().path();
                let name = path.file_name().unwrap().to_string_lossy().into_owned();
                (name, std::fs::read(path).unwrap())
            })
            .collect();
        files.sort();
        files
    }

    #[test]
    fn a_replaced_proxy_ca_is_kept_with_its_key() {
        let dir = tempfile::tempdir().unwrap();
        acceptor(dir.path()).unwrap();
        let loopback = std::fs::read(dir.path().join(CA_FILE)).unwrap();
        proxy_acceptors(dir.path(), &hosts(&["chatgpt.com"])).unwrap();
        let cert = std::fs::read(dir.path().join(PROXY_CA_FILE)).unwrap();
        let key = std::fs::read(dir.path().join(PROXY_CA_KEY_FILE)).unwrap();
        proxy_acceptors(dir.path(), &hosts(&["chatgpt.com", "api.openai.com"])).unwrap();
        let second = std::fs::read(dir.path().join(PROXY_CA_FILE)).unwrap();
        assert_ne!(second, cert);
        let kept = backups(dir.path());
        assert_eq!(kept.len(), 2);
        let find = |prefix: &str| kept.iter().find(|(n, _)| n.starts_with(prefix)).unwrap();
        let (kept_cert, kept_cert_data) = find("proxy-ca.");
        let (kept_key, kept_key_data) = find("proxy-ca-key.");
        assert_eq!((kept_cert_data, kept_key_data), (&cert, &key));
        // Certificate and key share one replacement stamp.
        assert_eq!(
            &kept_cert["proxy-ca".len()..],
            &kept_key["proxy-ca-key".len()..]
        );
        // The kept pair still verifies as a pair.
        let restored = tempfile::tempdir().unwrap();
        std::fs::write(restored.path().join(PROXY_CA_FILE), &cert).unwrap();
        std::fs::write(restored.path().join(PROXY_CA_KEY_FILE), &key).unwrap();
        assert!(load(restored.path(), &PROXY, &|_| true).is_some());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode();
            let backup = dir.path().join(BACKUP_DIR);
            assert_eq!(mode(&backup) & 0o077, 0);
            assert_eq!(mode(&backup.join(kept_key)) & 0o077, 0);
        }
        // A further replacement keeps both earlier pairs.
        proxy_acceptors(dir.path(), &hosts(&["api.anthropic.com"])).unwrap();
        let kept = backups(dir.path());
        assert_eq!(kept.len(), 4);
        assert!(kept.iter().any(|(_, data)| *data == cert));
        assert!(kept.iter().any(|(_, data)| *data == second));
        // Replacing the proxy CA leaves the loopback CA in place.
        assert!(kept.iter().all(|(name, _)| name.starts_with("proxy-ca")));
        assert_eq!(std::fs::read(dir.path().join(CA_FILE)).unwrap(), loopback);
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

    #[test]
    fn proxy_ca_tracks_exact_public_ip_constraints() {
        let dir = tempfile::tempdir().unwrap();
        let initial = hosts(&["gateway.invalid", "182.92.106.196"]);
        proxy_acceptors(dir.path(), &initial).unwrap();
        let pem = std::fs::read(dir.path().join(PROXY_CA_FILE)).unwrap();
        let der = CertificateDer::from_pem_slice(&pem).unwrap();
        assert!(permits(&der, "182.92.106.196"));
        assert!(!permits(&der, "182.92.106.197"));
        assert!(!permits(&der, "127.0.0.1"));
        proxy_acceptors(dir.path(), &initial).unwrap();
        assert_eq!(std::fs::read(dir.path().join(PROXY_CA_FILE)).unwrap(), pem);
        proxy_acceptors(
            dir.path(),
            &hosts(&["gateway.invalid", "182.92.106.196", "182.92.106.197"]),
        )
        .unwrap();
        assert_ne!(std::fs::read(dir.path().join(PROXY_CA_FILE)).unwrap(), pem);
    }

    // Verified as a client would, since constraints only bind the name types
    // they list: a CA with only DNS hosts must not vouch for any IP, and one
    // with only IP hosts must not vouch for any site.
    #[test]
    fn proxy_ca_cannot_vouch_for_names_outside_its_hosts() {
        use rustls::client::danger::ServerCertVerifier;
        for (names, denied) in [
            (
                &["api.anthropic.com"][..],
                &["www.example.com", "127.0.0.1", "182.92.106.196", "::1"][..],
            ),
            (
                &["182.92.106.196"][..],
                &["www.example.com", "localhost", "127.0.0.1", "::1"][..],
            ),
            (
                &["api.anthropic.com", "182.92.106.196"][..],
                &["www.example.com", "127.0.0.1", "182.92.106.197", "::1"][..],
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let hosts = hosts(names);
            proxy_acceptors(dir.path(), &hosts).unwrap();
            let key = KeyPair::from_pem(
                &std::fs::read_to_string(dir.path().join(PROXY_CA_KEY_FILE)).unwrap(),
            )
            .unwrap();
            let issuer = Issuer::new(proxy_ca_params(&hosts), &key);
            let mut roots = rustls::RootCertStore::empty();
            roots
                .add(CertificateDer::from_pem_file(dir.path().join(PROXY_CA_FILE)).unwrap())
                .unwrap();
            let verifier = rustls::client::WebPkiServerVerifier::builder_with_provider(
                Arc::new(roots),
                Arc::new(rustls::crypto::ring::default_provider()),
            )
            .build()
            .unwrap();
            let verifies = |name: &str| {
                let leaf = leaf(name, "forged", &issuer).unwrap();
                verifier
                    .verify_server_cert(
                        &leaf.cert[0],
                        &[],
                        &rustls::pki_types::ServerName::try_from(name.to_owned()).unwrap(),
                        &[],
                        rustls::pki_types::UnixTime::now(),
                    )
                    .is_ok()
            };
            for name in names {
                assert!(verifies(name), "{names:?} {name}");
            }
            for name in denied {
                assert!(!verifies(name), "{names:?} {name}");
            }
        }
    }

    #[test]
    fn a_stored_proxy_ca_leaving_a_name_type_open_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let only_dns = hosts(&["api.anthropic.com"]);
        // As created before unused name types were closed.
        let key = KeyPair::generate().unwrap();
        let open = ca_template(
            "coport proxy CA",
            vec![GeneralSubtree::DnsName("api.anthropic.com".into())],
        )
        .self_signed(&key)
        .unwrap();
        std::fs::write(dir.path().join(PROXY_CA_KEY_FILE), key.serialize_pem()).unwrap();
        std::fs::write(dir.path().join(PROXY_CA_FILE), open.pem()).unwrap();
        assert!(!covers(open.der(), &only_dns));
        proxy_acceptors(dir.path(), &only_dns).unwrap();
        let replaced = CertificateDer::from_pem_file(dir.path().join(PROXY_CA_FILE)).unwrap();
        assert_ne!(replaced.as_ref(), open.der().as_ref());
        assert!(covers(&replaced, &only_dns));
        // Gaining an IP host lifts the exclusion, so the CA is replaced again.
        assert!(!covers(
            &replaced,
            &hosts(&["api.anthropic.com", "182.92.106.196"])
        ));
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
