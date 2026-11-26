//! The accounts access tokens belong to, as Account lookups find them. Each
//! App with lookups keeps one `Directory`: the accounts already known, which
//! of its tokens Account discovery failed to look up, and the policy that
//! discovery is not retried sooner than `FAILED_PROBE_RETRY` after a failure.
//! A `Lookup` names only an App's request, how its answer parses and its
//! error texts.
use super::*;

/// Account discovery does not retry a failed lookup sooner than this.
pub(super) const FAILED_PROBE_RETRY: Duration = Duration::from_secs(600);
/// The accounts and failures a directory keeps before evicting.
const CAPACITY: usize = 128;

/// Accounts by access token, with last use for bounded LRU eviction. A
/// token's account does not depend on routing, so one process can share
/// them across servers rebuilt for configuration changes.
pub struct Profiles<I>(Arc<Mutex<HashMap<String, (Instant, I)>>>);
impl<I> Clone for Profiles<I> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}
impl<I> Default for Profiles<I> {
    fn default() -> Self {
        Self(Default::default())
    }
}
impl<I> Profiles<I> {
    /// Keep `token`'s account, evicting the least recently used other
    /// account at capacity.
    pub fn insert(&self, token: impl Into<String>, identity: I) {
        let token = token.into();
        let mut profiles = self.lock();
        if profiles.len() >= CAPACITY && !profiles.contains_key(&token) {
            if let Some(oldest) = profiles
                .iter()
                .min_by_key(|(_, (last_used, _))| *last_used)
                .map(|(key, _)| key.clone())
            {
                profiles.remove(&oldest);
            }
        }
        profiles.insert(token, (Instant::now(), identity));
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, (Instant, I)>> {
        // An entry is written whole, so a panic elsewhere leaves none half set.
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// What a directory knows of a token's account.
#[derive(Debug, PartialEq)]
pub(super) enum State<I> {
    Known(I),
    /// Account discovery failed to look it up; it stays the latest result
    /// after its retry delay, until a lookup succeeds.
    Failed,
    Unknown,
}

pub(super) struct Directory<I> {
    profiles: Profiles<I>,
    // When Account discovery last failed for a token, until a lookup of it
    // succeeds. The key is the credential, so switching a saved login forces
    // a new check. Rebuilt servers do not share it: their proxies may differ.
    failed: tokio::sync::Mutex<HashMap<String, Instant>>,
    // Serializes discovery, so concurrent runs share one lookup per token.
    discovery: tokio::sync::Mutex<()>,
}
impl<I> Default for Directory<I> {
    fn default() -> Self {
        Self {
            profiles: Default::default(),
            failed: Default::default(),
            discovery: Default::default(),
        }
    }
}

impl<I: Clone> Directory<I> {
    /// Keep the accounts of `profiles`, shared with other servers.
    pub(super) fn share(&mut self, profiles: Profiles<I>) {
        self.profiles = profiles;
    }
    #[cfg(test)]
    pub(super) fn profiles(&self) -> Profiles<I> {
        self.profiles.clone()
    }

    /// The account already known for `token`; `touch` marks it used.
    pub(super) fn known(&self, token: &str, touch: bool) -> Option<I> {
        let mut profiles = self.profiles.lock();
        let (last_used, identity) = profiles.get_mut(token)?;
        if touch {
            *last_used = Instant::now();
        }
        Some(identity.clone())
    }

    /// Every account already known.
    pub(super) fn accounts(&self) -> Vec<I> {
        let profiles = self.profiles.lock();
        profiles.values().map(|(_, i)| i.clone()).collect()
    }

    pub(super) async fn state(&self, token: &str) -> State<I> {
        if let Some(identity) = self.known(token, false) {
            return State::Known(identity);
        }
        if self.failed.lock().await.contains_key(token) {
            State::Failed
        } else {
            State::Unknown
        }
    }

    /// The account `lookup` finds for `token`: kept, and clearing a failure.
    pub(super) async fn remember(
        &self,
        token: &str,
        lookup: impl Future<Output = Result<I>>,
    ) -> Result<I> {
        let identity = lookup.await?;
        self.profiles.insert(token, identity.clone());
        self.failed.lock().await.remove(token);
        Ok(identity)
    }

    /// Account discovery of `token`: its known account, or what `lookup`
    /// finds unless a lookup failed within `FAILED_PROBE_RETRY`. `lookup` is
    /// not run otherwise. A failure is recorded.
    pub(super) async fn discover(
        &self,
        token: &str,
        lookup: impl Future<Output = Result<I>>,
    ) -> Option<I> {
        let _discovery = self.discovery.lock().await;
        if let Some(identity) = self.known(token, false) {
            return Some(identity);
        }
        if self
            .failed
            .lock()
            .await
            .get(token)
            .is_some_and(|at| at.elapsed() < FAILED_PROBE_RETRY)
        {
            return None;
        }
        match self.remember(token, lookup).await {
            Ok(identity) => Some(identity),
            Err(_) => {
                let mut failed = self.failed.lock().await;
                if failed.len() >= CAPACITY {
                    failed.clear();
                }
                failed.insert(token.to_owned(), Instant::now());
                None
            }
        }
    }

    /// Move every recorded time `by` into the past, as if it had passed.
    #[cfg(test)]
    pub(super) async fn backdate(&self, by: Duration) {
        // Windows counts `Instant` from boot, so it may not reach back far.
        let back = |at: &mut Instant| *at = at.checked_sub(by).unwrap_or(*at);
        self.profiles
            .lock()
            .values_mut()
            .for_each(|(at, _)| back(at));
        self.failed.lock().await.values_mut().for_each(back);
    }
    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.profiles.lock().len()
    }
    #[cfg(test)]
    pub(super) async fn failures(&self) -> Vec<String> {
        let mut tokens: Vec<_> = self.failed.lock().await.keys().cloned().collect();
        tokens.sort();
        tokens
    }
}

/// The error texts of an App's Account lookup.
pub(super) struct Texts {
    pub request: &'static str,
    pub failed: &'static str,
    pub read: &'static str,
    pub too_large: &'static str,
    pub json: &'static str,
    pub timeout: &'static str,
}

/// An App's Account lookup: a GET with the token as Bearer authorization,
/// answered with JSON that names the account.
pub(super) trait Lookup {
    type Identity: Clone;
    /// Its `service` in the request log.
    const SERVICE: &'static str;
    /// Request headers besides authorization, accept and accept-encoding.
    const HEADERS: &'static [(&'static str, &'static str)];
    const TEXTS: Texts;
    fn directory(server: &Server) -> &Directory<Self::Identity>;
    fn url(server: &Server) -> Result<Url>;
    fn identity(answer: &serde_json::Value) -> Result<Self::Identity>;
}

impl Server {
    /// The account `token` belongs to: known, or looked up through `proxy`,
    /// never directly nor through a proxy an unverified account picks.
    pub(super) async fn lookup_account<L: Lookup>(
        &self,
        token: &str,
        proxy: &Choice,
        log: &mut RequestLog,
    ) -> Result<L::Identity> {
        let directory = L::directory(self);
        if let Some(identity) = directory.known(token, true) {
            return Ok(identity);
        }
        directory
            .remember(token, self.account_lookup::<L>(token, proxy, log))
            .await
    }

    /// Account discovery of `token` through `proxy`, recorded in its own
    /// request log when it sends a lookup.
    pub(super) async fn discover_account<L: Lookup>(
        &self,
        token: &str,
        proxy: &Choice,
    ) -> Option<L::Identity> {
        L::directory(self)
            .discover(token, async {
                let mut log = RequestLog::auxiliary(
                    self.logger.clone(),
                    "account_probe",
                    json!({"service": L::SERVICE}).as_object().unwrap().clone(),
                    Outcome::Failed,
                );
                let result = tokio::time::timeout(
                    Duration::from_secs_f64(self.config.request_timeout_seconds.min(10.0)),
                    self.account_lookup::<L>(token, proxy, &mut log),
                )
                .await
                .unwrap_or_else(|_| Err(Error::config(L::TEXTS.timeout)));
                match &result {
                    Ok(_) => log.finish(),
                    Err(error) => log.fail(error.message),
                }
                result
            })
            .await
    }

    async fn account_lookup<L: Lookup>(
        &self,
        token: &str,
        proxy: &Choice,
        log: &mut RequestLog,
    ) -> Result<L::Identity> {
        let texts = &L::TEXTS;
        log.field("service", L::SERVICE);
        let mut request = self
            .client("none")?
            .get(L::url(self)?)
            .bearer_auth(token)
            .header("accept", "application/json")
            // The proxy reads this answer itself and does not decompress it.
            .header("accept-encoding", "identity");
        for (name, value) in L::HEADERS {
            request = request.header(*name, *value);
        }
        let request = request.build().map_err(|_| Error::config(texts.request))?;
        let mut response = self
            .send_via(
                proxy,
                request,
                false,
                Deadline::new(self.config.request_timeout_seconds.min(10.0)),
                Upstream::Auxiliary,
                log,
            )
            .await?;
        for (from, to) in [
            ("proxy", "profile_proxy"),
            ("proxy_endpoint", "profile_proxy_endpoint"),
        ] {
            if let Some(value) = log.fields.get(from).cloned() {
                log.fields.insert(to.into(), value);
            }
        }
        let status = response.status();
        record_upstream_response(log, status.as_u16(), Upstream::Auxiliary);
        if !status.is_success() {
            return Err(Error::new(
                if status.is_client_error() {
                    status.as_u16()
                } else {
                    502
                },
                texts.failed,
            ));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| Error::config(texts.read))?
        {
            if bytes.len() + chunk.len() > 65536 {
                return Err(Error::config(texts.too_large));
            }
            bytes.extend_from_slice(&chunk);
        }
        let answer = serde_json::from_slice(&bytes).map_err(|_| Error::config(texts.json))?;
        L::identity(&answer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    async fn failing() -> Result<&'static str> {
        Err(Error::config("lookup failed"))
    }

    #[tokio::test]
    async fn eviction_drops_only_the_least_recently_used_other_account() {
        let directory = Directory::<&str>::default();
        for index in 0..CAPACITY {
            directory
                .remember(&format!("token-{index}"), async { Ok("id") })
                .await
                .unwrap();
        }
        // Older than every other entry, without reaching back past boot on Windows.
        {
            let mut profiles = directory.profiles.lock();
            let entry = &mut profiles.get_mut("token-42").unwrap().0;
            *entry = entry.checked_sub(Duration::from_secs(1)).unwrap_or(*entry);
        }
        // Replacing a known token at capacity evicts no other account.
        directory
            .remember("token-0", async { Ok("id") })
            .await
            .unwrap();
        assert_eq!(directory.len(), CAPACITY);
        directory
            .remember("new-token", async { Ok("id") })
            .await
            .unwrap();
        assert_eq!(directory.len(), CAPACITY);
        assert!(directory.known("token-42", false).is_none());
        assert!(directory.known("new-token", false).is_some());
        assert!(directory.known("token-0", false).is_some());
    }

    #[tokio::test]
    async fn discovery_retries_a_failure_only_after_its_delay() {
        let directory = Directory::<&str>::default();
        let sent = AtomicUsize::new(0);
        let lookup = |answer: Result<&'static str>| {
            sent.fetch_add(1, Ordering::SeqCst);
            async move { answer }
        };
        assert_eq!(directory.discover("t", failing()).await, None);
        assert_eq!(directory.state("t").await, State::Failed);
        // Within the delay, the lookup is not even started.
        let unsent = async {
            sent.fetch_add(1, Ordering::SeqCst);
            Ok("id")
        };
        assert_eq!(directory.discover("t", unsent).await, None);
        assert_eq!(sent.load(Ordering::SeqCst), 0);
        // A failure stays the latest result after its delay.
        directory
            .backdate(FAILED_PROBE_RETRY + Duration::from_secs(1))
            .await;
        assert_eq!(directory.state("t").await, State::Failed);
        assert_eq!(directory.discover("t", lookup(Ok("id"))).await, Some("id"));
        assert_eq!(sent.load(Ordering::SeqCst), 1);
        assert_eq!(directory.state("t").await, State::Known("id"));
        assert!(directory.failures().await.is_empty());
        // A known account is answered without a lookup.
        assert_eq!(directory.discover("t", failing()).await, Some("id"));
    }

    #[tokio::test]
    async fn any_successful_lookup_clears_a_failure_and_answers_discovery() {
        let directory = Directory::<&str>::default();
        assert_eq!(directory.discover("t", failing()).await, None);
        assert_eq!(directory.failures().await, ["t"]);
        // A request's own lookup succeeds within the retry delay.
        directory.remember("t", async { Ok("id") }).await.unwrap();
        assert!(directory.failures().await.is_empty());
        assert_eq!(directory.discover("t", failing()).await, Some("id"));
        // A request's failed lookup is not a discovery failure.
        assert!(directory.remember("u", failing()).await.is_err());
        assert_eq!(directory.state("u").await, State::Unknown);
    }

    #[tokio::test]
    async fn each_app_keeps_its_own_failures() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::parse("listen_port: 7889\nrequest_timeout_seconds: 3\n").unwrap();
        let server = Server::new(config, Arc::new(Logger::new(dir.path().join("proxy.log"))));
        let failed = async { Err(Error::config("lookup failed")) };
        assert!(
            server
                .claude_identities
                .discover("t", failed)
                .await
                .is_none()
        );
        assert_eq!(server.claude_identities.failures().await, ["t"]);
        assert!(server.antigravity_identities.failures().await.is_empty());
    }

    #[tokio::test]
    async fn failures_beyond_capacity_start_over() {
        let directory = Directory::<&str>::default();
        for index in 0..=CAPACITY {
            directory.discover(&format!("t{index}"), failing()).await;
        }
        assert_eq!(directory.failures().await, [format!("t{CAPACITY}")]);
    }
}
