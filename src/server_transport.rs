//! Shared route health, deadlines and outbound request execution.
use super::*;

const PROBE_INTERVAL: Duration = Duration::from_secs(30);
#[derive(Clone, Hash, PartialEq, Eq)]
pub(super) struct ProbeKey {
    pub(super) endpoint: String,
    pub(super) origin: String,
    pub(super) native_tls: bool,
    pub(super) tunnel: bool,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum ProbeResult {
    Healthy,
    Transient,
    HardFailure,
}
#[derive(Default)]
pub(super) struct ProbeHealth {
    pub(super) initialized: bool,
    pub(super) failures: u32,
    pub(super) unavailable: bool,
    pub(super) next_check: Option<Instant>,
    pub(super) revision: u64,
    pub(super) monitored: bool,
    // Serialize cold probes without holding the health state across network I/O.
    initial_probe: Arc<tokio::sync::Mutex<()>>,
}
impl ProbeHealth {
    pub(super) fn observe(&mut self, result: ProbeResult) {
        self.initialized = true;
        self.revision += 1;
        match result {
            ProbeResult::Healthy => {
                self.failures = 0;
                self.unavailable = false;
            }
            ProbeResult::Transient => {
                self.failures = self.failures.saturating_add(1);
                self.unavailable |= self.failures >= 3;
            }
            ProbeResult::HardFailure => {
                self.failures = self.failures.saturating_add(1);
                self.unavailable = true;
            }
        }
        let interval = if self.failures == 0 {
            PROBE_INTERVAL
        } else if !self.unavailable {
            Duration::from_secs(1)
        } else if self.failures < 10 {
            Duration::from_secs(3)
        } else {
            PROBE_INTERVAL
        };
        self.next_check = Some(Instant::now() + interval);
    }
}
pub(super) type ProbeState = Arc<tokio::sync::Mutex<ProbeHealth>>;

impl ProbeKey {
    pub(super) fn http(endpoint: &str, url: &Url, native_tls: bool) -> Self {
        let mut origin = url.clone();
        origin.set_path("/");
        origin.set_query(None);
        origin.set_fragment(None);
        Self {
            endpoint: endpoint.into(),
            origin: origin.to_string(),
            native_tls,
            tunnel: false,
        }
    }
    pub(super) fn tunnel(endpoint: &str, url: &Url) -> Self {
        Self {
            tunnel: true,
            ..Self::http(endpoint, url, false)
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct Deadline(Instant);
impl Deadline {
    pub(super) fn new(seconds: f64) -> Self {
        Self(Instant::now() + Duration::from_secs_f64(seconds))
    }
    fn cap(self, seconds: f64) -> Self {
        Self(self.0.min(Self::new(seconds).0))
    }
    fn remaining(self) -> Result<Duration> {
        let remaining = self.0.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            Err(Error::config("Outbound operation timed out."))
        } else {
            Ok(remaining)
        }
    }
    async fn run<T>(self, future: impl std::future::Future<Output = T>) -> Result<T> {
        tokio::time::timeout(self.remaining()?, future)
            .await
            .map_err(|_| Error::config("Outbound operation timed out."))
    }
}

#[derive(Clone, Copy)]
struct Failure {
    health: ProbeResult,
    reason: &'static str,
    retryable: bool,
}
impl Failure {
    fn http(error: &reqwest::Error) -> Self {
        let hard = hard_probe_failure(error);
        Self {
            health: if hard.is_some() {
                ProbeResult::HardFailure
            } else {
                ProbeResult::Transient
            },
            reason: hard.unwrap_or(if error.is_timeout() {
                "timeout"
            } else if error.is_connect() {
                "connect"
            } else if error.is_request() {
                "request"
            } else {
                "transport_error"
            }),
            retryable: hard != Some("proxy_authentication")
                && (error.is_timeout() || error.is_connect() || error.is_request()),
        }
    }
    fn tunnel(error: &Error) -> Self {
        let hard = error.status == 407 || error.message == "CONNECT connection refused.";
        Self {
            health: if hard {
                ProbeResult::HardFailure
            } else {
                ProbeResult::Transient
            },
            reason: if error.status == 407 {
                "proxy_authentication"
            } else if hard {
                "connection_refused"
            } else if error.message == "Outbound operation timed out." {
                "timeout"
            } else {
                "tunnel_transport_error"
            },
            retryable: !hard,
        }
    }
}
fn response_health(status: reqwest::StatusCode) -> ProbeResult {
    if status == 407 {
        ProbeResult::HardFailure
    } else {
        ProbeResult::Healthy
    }
}

impl Server {
    pub(super) fn client(&self, endpoint: &str) -> Result<reqwest::Client> {
        self.client_transport(endpoint, false)
    }
    pub(super) fn client_transport(
        &self,
        endpoint: &str,
        native_tls: bool,
    ) -> Result<reqwest::Client> {
        let key = if native_tls {
            format!("native-tls:{endpoint}")
        } else {
            endpoint.into()
        };
        let mut clients = self
            .clients
            .lock()
            .map_err(|_| Error::config("Transport unavailable."))?;
        if let Some(c) = clients.get(&key) {
            return Ok(c.clone());
        }
        let mut builder = reqwest::Client::builder()
            .no_proxy()
            .retry(reqwest::retry::never())
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs_f64(self.config.request_timeout_seconds))
            .connect_timeout(Duration::from_secs_f64(self.config.request_timeout_seconds))
            .pool_max_idle_per_host(8);
        // Explicit API gateways can use certificates accepted by Node/OpenSSL
        // but rejected by rustls (e.g. self-signed CA certificates as leaves).
        // Both backends retain chain and hostname/IP verification.
        builder = if native_tls {
            builder.use_native_tls()
        } else {
            builder.use_rustls_tls()
        };
        if endpoint != "none" {
            // Remote DNS keeps destination resolution inside the selected SOCKS tunnel.
            let proxy = if let Some(rest) = endpoint.strip_prefix("socks5://") {
                format!("socks5h://{rest}")
            } else {
                endpoint.into()
            };
            builder = builder.proxy(
                reqwest::Proxy::all(proxy)
                    .map_err(|_| Error::config("Cannot configure outbound proxy."))?,
            );
        }
        let client = builder
            .build()
            .map_err(|_| Error::config("Cannot initialize outbound transport."))?;
        if clients.len() >= 32 {
            clients.clear();
        }
        clients.insert(key, client.clone());
        Ok(client)
    }
    #[cfg(test)]
    pub(super) async fn select(
        &self,
        choice: &Choice,
        destination: &Url,
        log: &mut RequestLog,
    ) -> Result<String> {
        self.select_transport(
            choice,
            destination,
            log,
            false,
            Deadline::new(self.config.request_timeout_seconds),
        )
        .await
    }
    pub(super) async fn select_transport(
        &self,
        choice: &Choice,
        destination: &Url,
        log: &mut RequestLog,
        native_tls: bool,
        deadline: Deadline,
    ) -> Result<String> {
        if let Choice::One(n) = choice {
            return Ok(n.clone());
        }
        for name in choice.names() {
            let endpoint = self.config.endpoint(name);
            let key = ProbeKey::http(endpoint, destination, native_tls);
            let state = self.health_state(&key);
            // Only cold lookups wait for a probe. Concurrent cold requests share it.
            let mut cached = deadline.run(state.lock()).await?;
            cached.monitored = true;
            if !cached.initialized {
                let gate = cached.initial_probe.clone();
                drop(cached);
                let _probe = deadline.run(gate.lock()).await?;
                cached = deadline.run(state.lock()).await?;
                // Another cold probe or a real request may have initialized it.
                if !cached.initialized {
                    let revision = cached.revision;
                    drop(cached);
                    let result = deadline.run(self.probe(&key)).await?;
                    cached = deadline.run(state.lock()).await?;
                    // As with background probes, newer request evidence wins.
                    if cached.revision == revision {
                        cached.observe(result);
                        self.log_health(&key, &cached, "probe");
                    }
                }
            }
            let available = !cached.unavailable;
            drop(cached);
            log.field("proxy", name);
            log.field("proxy_endpoint", redacted_endpoint(endpoint));
            log.field("available", available);
            // Probe events are emitted only for actual network checks.
            if available {
                log.fields.remove("available");
                return Ok(name.clone());
            }
        }
        Err(Error::config(
            "No available outbound proxy in the configured list.",
        ))
    }
    pub(super) async fn probe(&self, key: &ProbeKey) -> ProbeResult {
        let deadline = Deadline::new(self.config.request_timeout_seconds.min(7.0));
        if key.tunnel {
            let (result, reason) = match self.open_tunnel(key, deadline.cap(5.0)).await {
                Ok(_) => (ProbeResult::Healthy, "tunnel_connected"),
                Err(error) => {
                    let failure = Failure::tunnel(&error);
                    (failure.health, failure.reason)
                }
            };
            self.log_probe(key, result, reason, None);
            return result;
        }
        let mut reason = "http_response";
        let mut status = None;
        let result = match self.client_transport(&key.endpoint, key.native_tls) {
            Ok(client) => match client
                .head(&key.origin)
                .timeout(deadline.cap(5.0).remaining().expect("new probe deadline"))
                .send()
                .await
            {
                Ok(response) => {
                    status = Some(response.status().as_u16());
                    if response_health(response.status()) == ProbeResult::HardFailure {
                        reason = "proxy_authentication";
                        ProbeResult::HardFailure
                    } else {
                        // Even a target 5xx proves that the route carried an HTTP response.
                        ProbeResult::Healthy
                    }
                }
                Err(error) => {
                    let failure = Failure::http(&error);
                    reason = failure.reason;
                    failure.health
                }
            },
            Err(_) => {
                reason = "transport_configuration";
                ProbeResult::HardFailure
            }
        };
        // A HEAD failure alone cannot establish that an exit is down. Confirm
        // transport reachability without sending an API request or credentials.
        let result = if result == ProbeResult::Transient {
            match self.open_tunnel(key, deadline.cap(2.0)).await {
                Ok(_) => {
                    reason = "tunnel_connected";
                    ProbeResult::Healthy
                }
                Err(error) => {
                    let failure = Failure::tunnel(&error);
                    if failure.health == ProbeResult::HardFailure {
                        reason = failure.reason;
                        failure.health
                    } else {
                        result
                    }
                }
            }
        } else {
            result
        };
        self.log_probe(key, result, reason, status);
        result
    }

    fn log_probe(&self, key: &ProbeKey, result: ProbeResult, reason: &str, status: Option<u16>) {
        self.logger.write(
            "proxy_probe",
            json!({
                "proxy_endpoint": redacted_endpoint(&key.endpoint),
                "origin": key.origin,
                "transport": if key.tunnel { "connect" } else { "http" },
                "probe_success": (result == ProbeResult::Healthy).to_string(),
                "probe_result": format!("{result:?}"),
                "reason": reason,
                "upstream_status": status,
            })
            .as_object()
            .unwrap()
            .clone(),
        );
    }

    #[cfg(test)]
    pub(super) async fn record_route_success(
        &self,
        endpoint: &str,
        destination: &Url,
        native_tls: bool,
    ) {
        self.record_health(
            &ProbeKey::http(endpoint, destination, native_tls),
            ProbeResult::Healthy,
            "request",
        )
        .await;
    }

    /// Health is a disposable observation cache. Rebuild it after a poisoned
    /// update so list routes probe again; never reuse potentially partial data.
    fn health_cache(&self) -> std::sync::MutexGuard<'_, HashMap<ProbeKey, ProbeState>> {
        match self.probes.lock() {
            Ok(cache) => cache,
            Err(poisoned) => {
                let mut cache = poisoned.into_inner();
                cache.clear();
                self.probes.clear_poison();
                cache
            }
        }
    }

    fn health_state(&self, key: &ProbeKey) -> ProbeState {
        let mut probes = self.health_cache();
        if probes.len() >= 256 && !probes.contains_key(key) {
            if let Some(old) = probes.keys().next().cloned() {
                probes.remove(&old);
            }
        }
        probes.entry(key.clone()).or_default().clone()
    }
    fn log_health(&self, key: &ProbeKey, health: &ProbeHealth, source: &str) {
        self.logger.write("route_health", json!({
            "proxy_endpoint": redacted_endpoint(&key.endpoint), "origin": key.origin,
            "transport": if key.tunnel { "connect" } else { "http" }, "source": source,
            "available": (!health.unavailable).to_string(),
            "health": if health.unavailable { "unavailable" } else if health.failures > 0 { "suspect" } else { "healthy" },
            "consecutive_failures": health.failures.to_string(),
        }).as_object().unwrap().clone());
    }
    async fn record_health(&self, key: &ProbeKey, result: ProbeResult, source: &str) {
        let state = self.health_state(key);
        let mut cached = state.lock().await;
        cached.observe(result);
        self.log_health(key, &cached, source);
    }

    pub(super) async fn refresh_probes(&self) {
        let entries: Vec<_> = self
            .health_cache()
            .iter()
            .map(|(key, state)| (key.clone(), state.clone()))
            .collect();
        futures_util::stream::iter(entries)
            .for_each_concurrent(8, |(key, state)| async move {
                let revision = match state.try_lock() {
                    Ok(cached)
                        if cached.initialized
                            && cached.monitored
                            && cached.next_check.is_some_and(|t| t <= Instant::now()) =>
                    {
                        cached.revision
                    }
                    _ => return,
                };
                let result = self.probe(&key).await;
                let mut cached = state.lock().await;
                // A real request may have succeeded during this probe.
                if cached.revision == revision {
                    cached.observe(result);
                    self.log_health(&key, &cached, "probe");
                }
            })
            .await;
    }

    pub(super) async fn monitor_probes(&self) {
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            self.refresh_probes().await;
        }
    }
}

fn hard_probe_failure(error: &(dyn std::error::Error + 'static)) -> Option<&'static str> {
    let mut current = Some(error);
    while let Some(error) = current {
        if error
            .downcast_ref::<std::io::Error>()
            .is_some_and(|e| e.kind() == std::io::ErrorKind::ConnectionRefused)
        {
            return Some("connection_refused");
        }
        if error.to_string() == "proxy authorization required" {
            return Some("proxy_authentication");
        }
        current = error.source();
    }
    None
}

impl Server {
    /// Selection, retries and response-body reads share this operation's budget.
    /// A logical request contributes one final health observation, not one per retry.
    pub(super) async fn send_via(
        &self,
        choice: &Choice,
        request: reqwest::Request,
        native_tls: bool,
        deadline: Deadline,
        log: &mut RequestLog,
    ) -> Result<reqwest::Response> {
        log.field("upstream_method", request.method());
        log.field("upstream_path", request.url().path());
        let selected = self
            .select_transport(choice, request.url(), log, native_tls, deadline)
            .await?;
        let endpoint = self.config.endpoint(&selected);
        log.field("proxy", &selected);
        log.field("proxy_endpoint", redacted_endpoint(endpoint));
        log.event("route_selected");
        let key = ProbeKey::http(endpoint, request.url(), native_tls);
        let retryable = request.method() == hyper::Method::GET
            && !request.headers().contains_key("upgrade")
            && request
                .body()
                .is_none_or(|body| body.as_bytes().is_some_and(|bytes| bytes.is_empty()));
        let client = self.client_transport(endpoint, native_tls)?;
        let mut failure = Failure {
            health: ProbeResult::Transient,
            reason: "timeout",
            retryable: false,
        };
        for attempt in 1..=3 {
            let Ok(remaining) = deadline.remaining() else {
                break;
            };
            let mut outgoing = request
                .try_clone()
                .ok_or(Error::config("Cannot replay streaming request."))?;
            *outgoing.timeout_mut() = Some(remaining);
            log.field("upstream_attempts", attempt);
            match client.execute(outgoing).await {
                Ok(response) => {
                    self.record_health(&key, response_health(response.status()), "request")
                        .await;
                    return Ok(response);
                }
                Err(error) => {
                    failure = Failure::http(&error);
                }
            }
            let delay = Duration::from_millis(200 * attempt);
            if !retryable
                || !failure.retryable
                || attempt == 3
                || deadline
                    .remaining()
                    .map_or(true, |remaining| remaining <= delay)
            {
                break;
            }
            let mut fields = log.fields.clone();
            fields.insert("transport_error".into(), json!(failure.reason));
            fields.insert(
                "retry_delay_ms".into(),
                json!(delay.as_millis().to_string()),
            );
            self.logger.write("upstream_retry", fields);
            tokio::time::sleep(delay).await;
        }
        log.field("transport_error", failure.reason);
        self.record_health(&key, failure.health, "request").await;
        Err(Error::config(
            "Upstream transport failed; no direct fallback was attempted.",
        ))
    }

    async fn open_tunnel(
        &self,
        key: &ProbeKey,
        deadline: Deadline,
    ) -> Result<crate::tunnel::Socket> {
        let url = Url::parse(&key.origin).map_err(|_| Error::config("Invalid tunnel origin."))?;
        let host = url
            .host_str()
            .unwrap()
            .trim_start_matches('[')
            .trim_end_matches(']');
        deadline
            .run(crate::tunnel::open(
                host,
                url.port_or_known_default().unwrap(),
                &key.endpoint,
            ))
            .await?
    }

    /// Connecting a candidate is the probe itself; never open a second tunnel
    /// just to test a socket which has already been established successfully.
    pub(super) async fn connect_via(
        &self,
        choice: &Choice,
        destination: &Url,
        deadline: Deadline,
        log: &mut RequestLog,
    ) -> Result<crate::tunnel::Socket> {
        for name in choice.names() {
            deadline.remaining()?;
            let endpoint = self.config.endpoint(name);
            let key = ProbeKey::tunnel(endpoint, destination);
            let state = self.health_state(&key);
            {
                let mut cached = state.lock().await;
                cached.monitored |= matches!(choice, Choice::List(_));
                if matches!(choice, Choice::List(_)) && cached.unavailable {
                    continue;
                }
            }
            log.field("proxy", name);
            log.field("proxy_endpoint", redacted_endpoint(endpoint));
            match self.open_tunnel(&key, deadline).await {
                Ok(socket) => {
                    self.record_health(&key, ProbeResult::Healthy, "connect")
                        .await;
                    log.event("route_selected");
                    return Ok(socket);
                }
                Err(error) => {
                    let failure = Failure::tunnel(&error);
                    log.field("transport_error", failure.reason);
                    self.record_health(&key, failure.health, "connect").await;
                }
            }
        }
        Err(Error::config(
            "No configured CONNECT route could establish a tunnel.",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn poisoned_health_cache_is_discarded_without_changing_routing() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::parse("listen_port: 8787\nrequest_timeout_seconds: 3\nproxies:\n  selected: http://127.0.0.1:9000\ncodex:\n  homes: []\n  routing:\n    api_key_fallback: selected\nclaude:\n  config_dirs: []\n").unwrap();
        let server = Arc::new(Server::new(
            config,
            Arc::new(Logger::new(dir.path().join("proxy.log"))),
        ));
        let key = ProbeKey::http(
            "http://127.0.0.1:9000",
            &Url::parse("https://example.invalid/").unwrap(),
            false,
        );
        let state = server.health_state(&key);
        state.try_lock().unwrap().observe(ProbeResult::HardFailure);
        let copy = server.clone();
        let _ = std::thread::spawn(move || {
            let _cache = copy.probes.lock().unwrap();
            panic!("interrupted health update");
        })
        .join();
        let rebuilt = server.health_state(&key);
        assert!(!Arc::ptr_eq(&state, &rebuilt));
        assert!(!rebuilt.try_lock().unwrap().initialized);
        assert!(!server.probes.is_poisoned());
        assert_eq!(
            server
                .config
                .codex
                .routing
                .api_key_fallback
                .as_ref()
                .unwrap()
                .label(),
            "selected"
        );
    }

    #[tokio::test]
    async fn cold_probe_does_not_block_requests_or_overwrite_their_success() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let heads = Arc::new(AtomicUsize::new(0));
        let (began, released, count) = (started.clone(), release.clone(), heads.clone());
        let upstream = tokio::spawn(async move {
            let mut tasks = tokio::task::JoinSet::new();
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let (began, released, count) = (began.clone(), released.clone(), count.clone());
                tasks.spawn(async move {
                    let mut head = Vec::new();
                    while !head.ends_with(b"\r\n\r\n") {
                        match socket.read_u8().await {
                            Ok(byte) => head.push(byte),
                            Err(_) => return,
                        }
                    }
                    let status = if head.starts_with(b"HEAD ") {
                        count.fetch_add(1, Ordering::SeqCst);
                        began.notify_one();
                        released.notified().await;
                        "407 Proxy Authentication Required"
                    } else {
                        "200 OK"
                    };
                    let _ = socket.write_all(format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).await;
                });
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let config = Config::parse("listen_port: 8787\nrequest_timeout_seconds: 3\ncodex:\n  homes: []\nclaude:\n  config_dirs: []\n").unwrap();
        let server = Arc::new(Server::new(
            config,
            Arc::new(Logger::new(dir.path().join("proxy.log"))),
        ));
        let log = || RequestLog {
            logger: server.logger.clone(),
            fields: Default::default(),
            started: Instant::now(),
            status: 200,
            bytes: 0,
            outcome: "test_finished",
        };
        let cold = |mut log| {
            let (server, url) = (server.clone(), url.clone());
            tokio::spawn(async move {
                server
                    .select_transport(
                        &Choice::List(vec!["none".into()]),
                        &url,
                        &mut log,
                        false,
                        Deadline::new(3.0),
                    )
                    .await
            })
        };
        let first = cold(log());
        started.notified().await;
        let second = cold(log());
        // A scalar choice skips probing. Its already-successful response must
        // return while the cold HEAD is still waiting for the release signal.
        let mut request_log = log();
        let response = tokio::time::timeout(
            Duration::from_secs(1),
            server.send_via(
                &Choice::direct(),
                reqwest::Request::new(hyper::Method::GET, url.clone()),
                false,
                Deadline::new(3.0),
                &mut request_log,
            ),
        )
        .await;
        release.notify_one();
        let selected = first.await.unwrap();
        let also_selected = second.await.unwrap();
        upstream.abort();
        assert_eq!(
            response
                .expect("response waited behind the cold probe")
                .unwrap()
                .status(),
            200
        );
        assert_eq!(selected.unwrap(), "none");
        assert_eq!(also_selected.unwrap(), "none");
        assert_eq!(
            heads.load(Ordering::SeqCst),
            1,
            "cold requests must share a probe"
        );
        let state = server.health_state(&ProbeKey::http("none", &url, false));
        assert!(
            !state.lock().await.unavailable,
            "late 407 probe must not replace the successful request"
        );
    }
}
