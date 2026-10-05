//! Read-only access to CLI logins kept in the OS credential store.
use crate::{Error, Result};
use std::{
    collections::BTreeMap,
    sync::Mutex,
    time::{Duration, Instant},
};

/// A credential store entry: service and account.
type Key = (String, String);

/// A denied or locked store is not retried for every request, so one refusal
/// does not turn into a stream of system prompts.
const FAILURE_COOLDOWN: Duration = Duration::from_secs(30);

static FAILURES: Mutex<BTreeMap<Key, Instant>> = Mutex::new(BTreeMap::new());

/// The stored secret, or `None` when the store has no such entry.
pub(crate) async fn read(service: &str, account: &str) -> Result<Option<String>> {
    // Reads are serialized: concurrent requests must not raise several prompts.
    static GATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _gate = GATE.lock().await;
    let key = (service.to_owned(), account.to_owned());
    let unavailable = || Error::config("Cannot read the OS credential store.");
    {
        let mut failures = FAILURES.lock().unwrap();
        failures.retain(|_, at| at.elapsed() < FAILURE_COOLDOWN);
        if failures.contains_key(&key) {
            return Err(unavailable());
        }
    }
    let (service, account) = key.clone();
    let result = tokio::task::spawn_blocking(move || load(&service, &account))
        .await
        .unwrap_or(Err(()));
    if result.is_err() {
        FAILURES.lock().unwrap().insert(key, Instant::now());
    }
    result.map_err(|_| unavailable())
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

// Tests never touch the real store; they register entries here instead.
#[cfg(test)]
static TEST_ENTRIES: Mutex<BTreeMap<Key, Option<String>>> = Mutex::new(BTreeMap::new());

#[cfg(test)]
fn load(service: &str, account: &str) -> std::result::Result<Option<String>, ()> {
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

/// Store `value` for an entry; `None` makes reading it fail as a locked store would.
#[cfg(test)]
pub(crate) fn set_test_entry(service: &str, account: &str, value: Option<&str>) {
    TEST_ENTRIES
        .lock()
        .unwrap()
        .insert((service.into(), account.into()), value.map(String::from));
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
