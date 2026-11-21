//! Access to CLI logins kept in the OS credential store. Writes only occur
//! for explicitly enabled OAuth rotation.
use crate::{Error, Result};
use std::{
    collections::BTreeMap,
    sync::{Arc, LazyLock, Mutex},
    time::{Duration, Instant},
};

/// A credential store entry: service and account.
type Key = (String, String);

/// A denied or locked store is not retried for every request, so one refusal
/// does not turn into a stream of system prompts.
const FAILURE_COOLDOWN: Duration = Duration::from_secs(30);

static GATE: LazyLock<Arc<tokio::sync::Mutex<()>>> = LazyLock::new(Default::default);

static FAILURES: Mutex<BTreeMap<Key, Instant>> = Mutex::new(BTreeMap::new());

/// The stored secret, or `None` when the store has no such entry.
///
/// Reads are serialized: concurrent requests must not raise several prompts.
/// Waiting for the gate holds no thread. The gate then moves into the read,
/// so a caller that gives up mid-prompt neither lets another read start nor
/// loses a denial, and the store never occupies more than one blocking thread.
pub(crate) async fn read(service: &str, account: &str) -> Result<Option<String>> {
    let gate = GATE.clone().lock_owned().await;
    let key = (service.to_owned(), account.to_owned());
    tokio::task::spawn_blocking(move || {
        let _gate = gate;
        read_blocking(key, load)
    })
    .await
    .unwrap_or(Err(()))
    .map_err(|_| Error::config("Cannot read the OS credential store."))
}

/// An entry a Go program saved with go-keyring, as UTF-8 bytes: the same
/// generic password on macOS and Secret Service item elsewhere, but on Windows
/// the credential `service:account`, whose blob is not UTF-16. Reads share
/// the gate and cooldown of [`read`].
pub(crate) async fn read_go_keyring(service: &str, account: &str) -> Result<Option<String>> {
    let gate = GATE.clone().lock_owned().await;
    let key = (service.to_owned(), account.to_owned());
    tokio::task::spawn_blocking(move || {
        let _gate = gate;
        let value = read_blocking(key.clone(), load_go_keyring)?;
        if let Ok(mut recent) = RECENT.lock() {
            recent.insert(key, (Instant::now(), value.clone()));
        }
        Ok(value)
    })
    .await
    .unwrap_or(Err(()))
    .map_err(|_| Error::config("Cannot read the OS credential store."))
}

/// [`read_go_keyring`], answered from its last successful read when that is
/// at most `max_age` old.
pub(crate) async fn read_go_keyring_recent(
    service: &str,
    account: &str,
    max_age: Duration,
) -> Result<Option<String>> {
    let key = (service.to_owned(), account.to_owned());
    let recent = RECENT
        .lock()
        .ok()
        .and_then(|recent| recent.get(&key).cloned())
        .filter(|(at, _)| at.elapsed() <= max_age);
    match recent {
        Some((_, value)) => Ok(value),
        None => read_go_keyring(service, account).await,
    }
}

/// The last successful go-keyring read of each entry, and when it was made.
type Recent = BTreeMap<Key, (Instant, Option<String>)>;
static RECENT: LazyLock<Mutex<Recent>> = LazyLock::new(Default::default);

/// Compare and replace under the same gate as reads; never log credential values.
pub(crate) async fn replace(
    service: &str,
    account: &str,
    expected: serde_json::Value,
    updated: String,
) -> Result<()> {
    let gate = GATE.clone().lock_owned().await;
    let key = (service.to_owned(), account.to_owned());
    tokio::task::spawn_blocking(move || {
        let _gate = gate;
        let current = read_blocking(key.clone(), load)?
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok());
        if current.as_ref() != Some(&expected) {
            return Err(());
        }
        save(&key.0, &key.1, &updated)
    })
    .await
    .unwrap_or(Err(()))
    .map_err(|_| Error::config("Cannot save refreshed login in the OS credential store."))
}
#[cfg(all(
    not(test),
    any(target_os = "macos", target_os = "linux", target_os = "windows")
))]
fn save(service: &str, account: &str, value: &str) -> std::result::Result<(), ()> {
    keyring::Entry::new(service, account)
        .map_err(|_| ())?
        .set_password(value)
        .map_err(|_| ())
}
#[cfg(all(
    not(test),
    not(any(target_os = "macos", target_os = "linux", target_os = "windows"))
))]
fn save(_: &str, _: &str, _: &str) -> std::result::Result<(), ()> {
    Err(())
}
#[cfg(test)]
fn save(service: &str, account: &str, value: &str) -> std::result::Result<(), ()> {
    set_test_entry(service, account, Some(value));
    Ok(())
}

type Loader = fn(&str, &str) -> std::result::Result<Option<String>, ()>;

fn read_blocking(key: Key, load: Loader) -> std::result::Result<Option<String>, ()> {
    {
        let mut failures = FAILURES.lock().unwrap();
        failures.retain(|_, at| at.elapsed() < FAILURE_COOLDOWN);
        if failures.contains_key(&key) {
            return Err(());
        }
    }
    let result = load(&key.0, &key.1);
    if result.is_err() {
        FAILURES.lock().unwrap().insert(key, Instant::now());
    }
    result
}

#[cfg(all(
    not(test),
    any(target_os = "macos", target_os = "linux", target_os = "windows")
))]
fn load(service: &str, account: &str) -> std::result::Result<Option<String>, ()> {
    let entry = keyring::Entry::new(service, account).map_err(|_| ())?;
    match entry.get_password() {
        Ok(value) => Ok(Some(value)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(_) => Err(()),
    }
}

#[cfg(all(
    not(test),
    not(any(target_os = "macos", target_os = "linux", target_os = "windows"))
))]
fn load(_: &str, _: &str) -> std::result::Result<Option<String>, ()> {
    Err(())
}

/// On macOS go-keyring reads and writes through `/usr/bin/security`, and each
/// write replaces the item's partition list with the writer's: a write by
/// Antigravity locks out any app reading it itself, and one by such an app
/// locks out Antigravity, each then facing a Keychain prompt. Read the same
/// way, so the item stays Apple's tool's for everyone.
#[cfg(all(not(test), target_os = "macos"))]
fn load_go_keyring(service: &str, account: &str) -> std::result::Result<Option<String>, ()> {
    use std::io::Read;
    /// errSecItemNotFound, as `security` reports it.
    const NOT_FOUND: i32 = 44;
    /// Long enough to answer a Keychain prompt, so none is left hanging.
    const PROMPT_TIMEOUT: Duration = Duration::from_secs(120);
    let mut child = std::process::Command::new("/usr/bin/security")
        .args(["find-generic-password", "-s", service, "-a", account, "-w"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|_| ())?;
    let started = Instant::now();
    let status = loop {
        match child.try_wait().map_err(|_| ())? {
            Some(status) => break status,
            None if started.elapsed() > PROMPT_TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(());
            }
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    };
    let mut output = Vec::new();
    child
        .stdout
        .take()
        .ok_or(())?
        .read_to_end(&mut output)
        .map_err(|_| ())?;
    match status.code() {
        Some(0) => secret_line(output).map(Some),
        Some(NOT_FOUND) => Ok(None),
        _ => Err(()),
    }
}

/// The secret `security find-generic-password -w` printed: as text, or as
/// hex when it is not plain ASCII, as an unwrapped login with non-ASCII text
/// stored by another app is. A login is JSON or `go-keyring-base64:…`, never
/// hex digits alone.
#[cfg(any(test, target_os = "macos"))]
fn secret_line(mut output: Vec<u8>) -> std::result::Result<String, ()> {
    if output.last() == Some(&b'\n') {
        output.pop();
    }
    let digits = output.strip_prefix(b"0x").unwrap_or(&output);
    if !digits.is_empty() && digits.len() % 2 == 0 && digits.iter().all(u8::is_ascii_hexdigit) {
        output = digits
            .chunks(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap_or(""), 16))
            .collect::<std::result::Result<_, _>>()
            .map_err(|_| ())?;
    }
    String::from_utf8(output).map_err(|_| ())
}

/// go-keyring's entry: on Windows the credential `service:account`; on Linux
/// the Secret Service item with these attributes, read there directly. The
/// default Linux entry would answer from a kernel keyring copy it makes on
/// its first read, and so miss every later write of the Go program.
#[cfg(all(not(test), any(target_os = "linux", target_os = "windows")))]
fn go_keyring_entry(service: &str, account: &str) -> std::result::Result<keyring::Entry, ()> {
    #[cfg(target_os = "windows")]
    let entry = keyring::Entry::new_with_target(&format!("{service}:{account}"), service, account);
    #[cfg(not(target_os = "windows"))]
    let entry = keyring::secret_service::SsCredential::new_with_no_target(service, account)
        .map(|credential| keyring::Entry::new_with_credential(Box::new(credential)));
    entry.map_err(|_| ())
}

#[cfg(all(not(test), any(target_os = "linux", target_os = "windows")))]
fn load_go_keyring(service: &str, account: &str) -> std::result::Result<Option<String>, ()> {
    match go_keyring_entry(service, account)?.get_secret() {
        Ok(bytes) => String::from_utf8(bytes).map(Some).map_err(|_| ()),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(_) => Err(()),
    }
}

#[cfg(all(
    not(test),
    not(any(target_os = "macos", target_os = "linux", target_os = "windows"))
))]
fn load_go_keyring(_: &str, _: &str) -> std::result::Result<Option<String>, ()> {
    Err(())
}

// Tests register go-keyring entries in the same map, and count their reads.
#[cfg(test)]
static GO_KEYRING_LOADS: Mutex<BTreeMap<String, usize>> = Mutex::new(BTreeMap::new());
#[cfg(test)]
fn load_go_keyring(service: &str, account: &str) -> std::result::Result<Option<String>, ()> {
    *GO_KEYRING_LOADS
        .lock()
        .unwrap()
        .entry(service.to_owned())
        .or_default() += 1;
    load(service, account)
}
/// How often go-keyring entries of `service` were read.
#[cfg(test)]
pub(crate) fn test_loads(service: &str) -> usize {
    GO_KEYRING_LOADS
        .lock()
        .unwrap()
        .get(service)
        .copied()
        .unwrap_or(0)
}

// Tests never touch the real store; they register entries here instead.
#[cfg(test)]
static TEST_ENTRIES: Mutex<BTreeMap<Key, Option<String>>> = Mutex::new(BTreeMap::new());

#[cfg(test)]
static TEST_LOADS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
fn load(service: &str, account: &str) -> std::result::Result<Option<String>, ()> {
    if service.starts_with("keychain-test-slow") {
        // Stands in for a system prompt the user has not answered yet.
        if service == "keychain-test-slow" {
            TEST_LOADS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    match TEST_ENTRIES
        .lock()
        .unwrap()
        .get(&(service.to_owned(), account.to_owned()))
    {
        Some(Some(value)) => Ok(Some(value.clone())),
        Some(None) => Err(()),
        None => Ok(None),
    }
}

/// Replace an entry as another process would, unknown to a recent read.
#[cfg(test)]
pub(crate) fn write_test_entry_elsewhere(service: &str, account: &str, value: &str) {
    TEST_ENTRIES
        .lock()
        .unwrap()
        .insert((service.into(), account.into()), Some(value.into()));
}

/// Store `value` for an entry; `None` makes reading it fail as a locked store would.
#[cfg(test)]
pub(crate) fn set_test_entry(service: &str, account: &str, value: Option<&str>) {
    let key: Key = (service.into(), account.into());
    // As if the entry had been written since it was last read.
    RECENT.lock().unwrap().remove(&key);
    TEST_ENTRIES
        .lock()
        .unwrap()
        .insert(key, value.map(String::from));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn security_prints_the_secret_on_one_line() {
        assert_eq!(
            secret_line(b"go-keyring-base64:e30=\n".to_vec()).unwrap(),
            "go-keyring-base64:e30="
        );
        // Only the newline `security` adds is removed.
        assert_eq!(secret_line(b"{} \n\n".to_vec()).unwrap(), "{} \n");
        assert_eq!(secret_line(b"{}".to_vec()).unwrap(), "{}");
        assert!(secret_line(vec![0xff, b'\n']).is_err());
        // Not plain ASCII: printed as hex, with or without `0x`.
        let login = r#"{"id_token_email":"李@example.com"}"#;
        let hex: String = login.bytes().map(|b| format!("{b:02x}")).collect();
        assert_eq!(secret_line(format!("{hex}\n").into_bytes()).unwrap(), login);
        assert_eq!(
            secret_line(format!("0x{hex}\n").into_bytes()).unwrap(),
            login
        );
        assert!(secret_line(b"0x\n".to_vec()).unwrap() == "0x");
    }

    #[tokio::test]
    async fn recent_go_keyring_reads_are_reused_until_they_age_or_the_entry_changes() {
        let (service, account) = ("keychain-test-recent", "login");
        set_test_entry(service, account, Some("first"));
        let long = Duration::from_secs(60);
        let read = || read_go_keyring_recent(service, account, long);
        assert_eq!(read().await.unwrap().as_deref(), Some("first"));
        let loads = test_loads(service);
        // Written elsewhere: a recent read still answers, until it ages.
        write_test_entry_elsewhere(service, account, "second");
        assert_eq!(read().await.unwrap().as_deref(), Some("first"));
        assert_eq!(test_loads(service), loads);
        assert_eq!(
            read_go_keyring_recent(service, account, Duration::ZERO)
                .await
                .unwrap()
                .as_deref(),
            Some("second")
        );
        // A fresh read always reads, and is what recent reads reuse next.
        write_test_entry_elsewhere(service, account, "third");
        assert_eq!(
            read_go_keyring(service, account).await.unwrap().as_deref(),
            Some("third")
        );
        assert_eq!(read().await.unwrap().as_deref(), Some("third"));
        assert_eq!(test_loads(service), loads + 2);
    }

    #[tokio::test]
    async fn missing_entries_are_not_failures_but_failures_cool_down() {
        assert_eq!(read("keychain-test", "missing").await.unwrap(), None);
        set_test_entry("keychain-test", "present", Some("secret"));
        assert_eq!(
            read("keychain-test", "present").await.unwrap().as_deref(),
            Some("secret")
        );
        set_test_entry("keychain-test", "locked", None);
        assert!(read("keychain-test", "locked").await.is_err());
        // Even once readable, a recent failure is not retried immediately.
        set_test_entry("keychain-test", "locked", Some("secret"));
        assert!(read("keychain-test", "locked").await.is_err());
    }

    #[tokio::test]
    async fn cancelled_reads_still_hold_the_gate_and_record_failures() {
        set_test_entry("keychain-test-slow", "denied", None);
        let cancelled = tokio::time::timeout(
            Duration::from_millis(50),
            read("keychain-test-slow", "denied"),
        )
        .await;
        assert!(cancelled.is_err());
        // The abandoned read still finishes before the next one starts, and
        // its denial cools the entry down instead of prompting again.
        assert!(read("keychain-test-slow", "denied").await.is_err());
        assert_eq!(TEST_LOADS.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn waiting_reads_hold_no_blocking_threads() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .max_blocking_threads(2)
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            set_test_entry("keychain-test-slow-pool", "prompt", Some("secret"));
            let reads: Vec<_> = (0..4)
                .map(|_| tokio::spawn(read("keychain-test-slow-pool", "prompt")))
                .collect();
            tokio::time::sleep(Duration::from_millis(50)).await;
            // Reads waiting behind an unanswered prompt leave the pool free.
            let other = tokio::time::timeout(
                Duration::from_millis(200),
                tokio::task::spawn_blocking(|| ()),
            )
            .await;
            assert!(other.is_ok());
            for read in reads {
                assert_eq!(read.await.unwrap().unwrap().as_deref(), Some("secret"));
            }
        });
    }
}
