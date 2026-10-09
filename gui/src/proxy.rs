//! Prefer daemon tests; fall back locally only when no daemon is running.

use crate::daemon;
use coport::config::Config;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime},
};
use tokio::{runtime::Runtime, task::JoinHandle};

#[path = "background.rs"]
mod background;
pub use background::{BackgroundController, Completion};

pub type Notify = Arc<dyn Fn() + Send + Sync>;

#[derive(Clone, PartialEq, Debug)]
pub enum Phase {
    Stopped,
    Running { port: u16, since: Instant },
    Failed(String),
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum Probe {
    Pending,
    Reachable {
        latency: Duration,
        /// Looked up for loopback and local-network proxies.
        exit: Option<Exit>,
    },
    Unreachable(String),
    /// The test could not establish reachability (submission or lookup failure).
    Unavailable(String),
}

/// Where traffic through a proxy leaves for the internet.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Exit {
    pub ip: String,
    /// ISO 3166-1 alpha-2 country or region code.
    pub country: Option<String>,
}

/// Cloudflare's diagnostics endpoint: no key needed, answers with the
/// caller's address (`ip=`) and country or region (`loc=`). The IPv4 literal
/// makes dual-stack exits report their (shorter, more familiar) IPv4 address.
const TRACE_URL: &str = "https://1.1.1.1/cdn-cgi/trace";

#[derive(Default)]
struct Shared {
    phase: Option<Phase>,
    generation: u64,
    /// Each result with when its test started: monotonic time for its age,
    /// wall-clock time to order it against logged observations.
    probes: BTreeMap<String, (Probe, Instant, SystemTime)>,
    /// Tests still running, by proxy. A background refresh keeps the last
    /// result shown meanwhile instead of a pending state.
    testing: BTreeMap<String, usize>,
}

struct Running {
    client: daemon::Client,
    monitor: JoinHandle<()>,
}

pub struct Controller {
    rt: Runtime,
    shared: Arc<Mutex<Shared>>,
    running: Option<Running>,
    notify: Notify,
    daemon_dir: PathBuf,
    daemon_binary: PathBuf,
    attached: Option<daemon::Status>,
}

impl Controller {
    pub fn new(notify: Notify) -> Self {
        Self::with_daemon(
            notify,
            crate::settings::app_dir(),
            daemon::binary_path().unwrap_or_default(),
        )
    }

    pub fn with_daemon(notify: Notify, daemon_dir: PathBuf, daemon_binary: PathBuf) -> Self {
        Self::with_shared(notify, daemon_dir, daemon_binary, Default::default())
    }

    fn with_shared(
        notify: Notify,
        daemon_dir: PathBuf,
        daemon_binary: PathBuf,
        shared: Arc<Mutex<Shared>>,
    ) -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("proxy")
            .build()
            .expect("Tokio runtime");
        let mut controller = Self {
            rt,
            shared,
            running: None,
            notify,
            daemon_dir,
            daemon_binary,
            attached: None,
        };
        if let Some((client, status)) = daemon::Client::discover(&controller.daemon_dir) {
            controller.attach(client, status);
        }
        controller
    }

    pub fn phase(&self) -> Phase {
        self.lock().phase.clone().unwrap_or(Phase::Stopped)
    }

    pub fn is_running(&self) -> bool {
        matches!(self.phase(), Phase::Running { .. })
    }

    pub fn daemon_status(&self) -> Option<&daemon::Status> {
        self.attached.as_ref()
    }

    pub fn start(&mut self, config_path: &Path, log_path: PathBuf) {
        let _ = self.try_start(config_path, log_path);
    }

    pub fn try_start(&mut self, config_path: &Path, log_path: PathBuf) -> Result<(), String> {
        // Validate before stopping the working listener.
        if let Err(error) =
            Config::read(config_path).and_then(|config| config.check_external_data())
        {
            let message = error.message.to_string();
            if !self.is_running() {
                self.lock().phase = Some(Phase::Failed(message.clone()));
                (self.notify)();
            }
            return Err(message);
        }
        // Never launch a replacement unless the old listener has stopped.
        self.stop()?;
        let result = daemon::start(
            &self.daemon_binary,
            &self.daemon_dir,
            config_path,
            &log_path,
        )
        .map_err(|e| e.to_string());
        match result {
            Ok((client, status)) => self.attach(client, status),
            Err(message) => {
                self.lock().phase = Some(Phase::Failed(message.clone()));
                (self.notify)();
                return Err(message);
            }
        }
        (self.notify)();
        Ok(())
    }

    fn attach(&mut self, client: daemon::Client, status: daemon::Status) {
        let mut shared = self.lock();
        shared.generation += 1;
        let generation = shared.generation;
        shared.phase = Some(Phase::Running {
            port: status.port,
            since: Instant::now()
                .checked_sub(Duration::from_millis(status.uptime_ms))
                .unwrap_or_else(Instant::now),
        });
        drop(shared);
        self.attached = Some(status);
        let shared = self.shared.clone();
        let notify = self.notify.clone();
        let monitor_client = client.clone();
        let monitor = self.rt.spawn(async move {
            // One slow reply is not a lost daemon, and contact may come back.
            const LOST_AFTER: u32 = 3;
            let mut failures = 0;
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let client = monitor_client.clone();
                let result = tokio::task::spawn_blocking(move || client.status()).await;
                let mut shared = shared.lock().unwrap();
                if shared.generation != generation {
                    break;
                }
                match result {
                    Ok(Ok(status)) => {
                        let lost = matches!(shared.phase, Some(Phase::Failed(_)));
                        failures = 0;
                        if !lost {
                            continue;
                        }
                        shared.phase = Some(Phase::Running {
                            port: status.port,
                            since: Instant::now()
                                .checked_sub(Duration::from_millis(status.uptime_ms))
                                .unwrap_or_else(Instant::now),
                        });
                    }
                    _ => {
                        failures += 1;
                        if failures != LOST_AFTER {
                            continue;
                        }
                        shared.phase = Some(Phase::Failed(
                            "Lost contact with the proxy daemon. Restart to reconnect.".into(),
                        ));
                    }
                }
                drop(shared);
                notify();
            }
        });
        self.running = Some(Running { client, monitor });
    }

    pub fn stop(&mut self) -> Result<(), String> {
        // A daemon this GUI lost track of, such as one that answered discovery
        // too slowly at launch, would otherwise keep its port and never stop.
        if self.running.is_none()
            && let Some((client, status)) = daemon::Client::discover(&self.daemon_dir)
        {
            self.attach(client, status);
        }
        self.stop_attached()
    }

    /// Stops only the daemon this controller is attached to, if any.
    fn stop_attached(&mut self) -> Result<(), String> {
        if let Some(running) = self.running.as_ref() {
            if let Err(error) = running.client.stop() {
                let message = format!("Cannot stop proxy daemon: {error}");
                self.lock().phase = Some(Phase::Failed(message.clone()));
                (self.notify)();
                return Err(message);
            }
            if let Some(running) = self.running.take() {
                running.monitor.abort();
            }
            self.attached = None;
            let mut shared = self.lock();
            shared.generation += 1;
            shared.phase = Some(Phase::Stopped);
            drop(shared);
            (self.notify)();
        }
        Ok(())
    }

    /// Release the GUI's control connection without stopping the independent process.
    fn detach(&mut self) {
        self.lock().generation += 1;
        if let Some(running) = self.running.take() {
            running.monitor.abort();
        }
        self.attached = None;
    }

    pub fn on_app_exit(&mut self, keep_running: bool) -> Result<(), String> {
        if keep_running {
            self.detach();
            Ok(())
        } else {
            self.stop()
        }
    }

    /// Tests an outbound proxy, showing it as pending until the result.
    /// Loopback and local-network proxies get a request through them to
    /// learn the exit address; other proxies
    /// get a TCP connection check.
    pub fn probe(&self, name: &str, endpoint: &str) {
        self.test(name, endpoint, true);
    }

    fn test(&self, name: &str, endpoint: &str, show_progress: bool) {
        let Some(_) = host_port(endpoint) else {
            self.set_probe(name, Probe::Unreachable("Invalid proxy URL".into()));
            return;
        };
        let directory = self.daemon_dir.clone();
        let endpoint = endpoint.to_owned();
        let generation = self.lock().generation;
        let started = (Instant::now(), SystemTime::now());
        {
            let mut shared = self.lock();
            *shared.testing.entry(name.to_owned()).or_default() += 1;
            if show_progress {
                shared
                    .probes
                    .insert(name.to_owned(), (Probe::Pending, started.0, started.1));
            }
        }
        let shared = self.shared.clone();
        let notify = self.notify.clone();
        let name = name.to_owned();
        self.rt.spawn(async move {
            let result = async {
                match daemon::task_backend(directory).await? {
                    Some((client, status)) => {
                        if !status.proxy_probe_supported {
                            return Err(std::io::Error::other(
                                "Restart the proxy daemon to enable proxy tests.",
                            ));
                        }
                        daemon::check_task_config(&status)?;
                        client.probe_async(&name).await
                    }
                    None => Ok(run_probe(&endpoint).await),
                }
            }
            .await;
            let probe = result.unwrap_or_else(|error| Probe::Unavailable(error.to_string()));
            let mut shared = shared.lock().unwrap();
            if let Some(running) = shared.testing.get_mut(&name) {
                *running -= 1;
                if *running == 0 {
                    shared.testing.remove(&name);
                }
            }
            // A test started later, such as a manual one, has the newer result.
            if shared.generation == generation
                && shared
                    .probes
                    .get(&name)
                    .is_none_or(|(_, at, _)| *at <= started.0)
            {
                shared.probes.insert(name, (probe, started.0, started.1));
            }
            drop(shared);
            notify();
        });
    }

    /// Tests, in the background, proxies without a result newer than
    /// `max_age`; the last result stays shown until the new one arrives.
    pub fn probe_stale<'a>(
        &self,
        proxies: impl IntoIterator<Item = (&'a String, &'a String)>,
        max_age: Duration,
    ) {
        for (name, endpoint) in proxies {
            let stale = {
                let shared = self.lock();
                // A wall clock set back would make every result look new.
                !shared.testing.contains_key(name)
                    && shared
                        .probes
                        .get(name)
                        .is_none_or(|(_, started, _)| started.elapsed() > max_age)
            };
            if stale {
                self.test(name, endpoint, false);
            }
        }
    }

    fn set_probe(&self, name: &str, probe: Probe) {
        self.lock()
            .probes
            .insert(name.to_owned(), (probe, Instant::now(), SystemTime::now()));
    }

    pub fn probes(&self) -> BTreeMap<String, (Probe, SystemTime)> {
        self.lock()
            .probes
            .iter()
            .map(|(name, (probe, _, at))| (name.clone(), (probe.clone(), *at)))
            .collect()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.shared.lock().unwrap()
    }
}

impl Drop for Controller {
    fn drop(&mut self) {
        // A detached daemon was deliberately kept running.
        let _ = self.stop_attached();
    }
}

/// The shared test used by the daemon and by the GUI when no daemon exists.
pub(crate) async fn run_probe(endpoint: &str) -> Probe {
    let Some(addr) = host_port(endpoint) else {
        return Probe::Unreachable("Invalid proxy URL".into());
    };
    if is_local_network(endpoint) {
        trace(endpoint).await
    } else {
        connect(addr).await
    }
}

async fn connect(addr: (String, u16)) -> Probe {
    let started = Instant::now();
    match tokio::time::timeout(Duration::from_secs(3), tokio::net::TcpStream::connect(addr)).await {
        Ok(Ok(_)) => Probe::Reachable {
            latency: started.elapsed(),
            exit: None,
        },
        Ok(Err(e)) => Probe::Unreachable(e.kind().to_string()),
        Err(_) => Probe::Unreachable("timed out".into()),
    }
}

/// Fetches the trace endpoint through the proxy; the latency covers the whole
/// request, so it reflects the proxy's real path rather than a local connect.
async fn trace(endpoint: &str) -> Probe {
    trace_at(endpoint, TRACE_URL).await
}

async fn trace_at(endpoint: &str, trace_url: &str) -> Probe {
    let client = reqwest::Proxy::all(endpoint).and_then(|proxy| {
        reqwest::Client::builder()
            .proxy(proxy)
            .timeout(Duration::from_secs(8))
            .build()
    });
    let Ok(client) = client else {
        return Probe::Unreachable("Invalid proxy URL".into());
    };
    let started = Instant::now();
    let response = match client.get(trace_url).send().await {
        Ok(response) => response,
        Err(error) => {
            // A failed lookup may be a destination/TLS failure. Only report
            // the proxy unreachable if its own socket also cannot be reached.
            let mut cause: Option<&(dyn std::error::Error + 'static)> = Some(&error);
            while let Some(current) = cause {
                if current
                    .to_string()
                    .ends_with("proxy authorization required")
                {
                    return Probe::Unreachable("Proxy authentication required (HTTP 407).".into());
                }
                cause = current.source();
            }
            // The lookup and fallback must fit inside the daemon's 10s reply deadline.
            if let Some(addr) = host_port(endpoint)
                && let Ok(unreachable @ Probe::Unreachable(_)) =
                    tokio::time::timeout(Duration::from_secs(1), connect(addr)).await
            {
                return unreachable;
            }
            return Probe::Unavailable(if error.is_timeout() {
                "Exit IP lookup timed out; proxy reachability is unverified.".into()
            } else {
                "Exit IP lookup failed; proxy reachability is unverified.".into()
            });
        }
    };
    if response.status() == reqwest::StatusCode::PROXY_AUTHENTICATION_REQUIRED {
        return Probe::Unreachable("Proxy authentication required (HTTP 407).".into());
    }
    if !response.status().is_success() {
        return Probe::Unavailable(format!(
            "Exit IP lookup failed: HTTP {}.",
            response.status().as_u16()
        ));
    }
    let Ok(text) = response.text().await else {
        return Probe::Unavailable("Cannot read the exit IP lookup response.".into());
    };
    let Some(exit) = parse_trace(&text) else {
        return Probe::Unavailable("Exit IP lookup returned no valid IP address.".into());
    };
    Probe::Reachable {
        latency: started.elapsed(),
        exit: Some(exit),
    }
}

fn parse_trace(text: &str) -> Option<Exit> {
    let field = |key: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
            .map(str::trim)
    };
    let ip = field("ip")?.parse::<std::net::IpAddr>().ok()?.to_string();
    let country = field("loc")
        .filter(|c| c.len() == 2 && c.bytes().all(|b| b.is_ascii_uppercase()) && *c != "XX")
        .map(str::to_owned);
    Some(Exit { ip, country })
}

/// A proxy on loopback, a private network, or a link-local address.
pub fn is_local_network(endpoint: &str) -> bool {
    host_port(endpoint).is_some_and(|(host, _)| {
        host.eq_ignore_ascii_case("localhost")
            || host.parse::<std::net::IpAddr>().is_ok_and(|ip| match ip {
                std::net::IpAddr::V4(ip) => {
                    ip.is_loopback() || ip.is_private() || ip.is_link_local()
                }
                std::net::IpAddr::V6(ip) => ip.to_ipv4_mapped().map_or_else(
                    || ip.is_loopback() || ip.is_unique_local() || ip.is_unicast_link_local(),
                    |ip| ip.is_loopback() || ip.is_private() || ip.is_link_local(),
                ),
            })
    })
}

fn host_port(endpoint: &str) -> Option<(String, u16)> {
    let rest = endpoint.split_once("://")?.1;
    let authority = rest.split('/').next()?;
    let authority = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let (host, port) = authority.rsplit_once(':')?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    Some((host.to_owned(), port.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::{Controller, Exit, Phase, Probe, host_port, is_local_network, parse_trace};
    use std::sync::Arc;

    fn wait_for(mut predicate: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !predicate() {
            assert!(std::time::Instant::now() < deadline, "timed out");
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
    }

    #[tokio::test]
    async fn exit_lookup_failures_do_not_mark_a_reachable_proxy_down() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for (status, body) in [
            (403, ""),
            (429, ""),
            (500, ""),
            (200, "invalid"),
            (200, "ip=203.0.113.8\nloc=JP\n"),
            (407, ""),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut head = Vec::new();
                while !head.ends_with(b"\r\n\r\n") {
                    head.push(socket.read_u8().await.unwrap());
                }
                socket.write_all(format!("HTTP/1.1 {status} Response\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            });
            let result = super::trace_at(&endpoint, "http://trace.invalid/cdn-cgi/trace").await;
            server.await.unwrap();
            if status == 407 {
                assert!(matches!(result, Probe::Unreachable(_)));
            } else if body.contains("ip=") {
                assert!(matches!(result, Probe::Reachable { exit: Some(_), .. }));
            } else {
                assert!(
                    matches!(result, Probe::Unavailable(_)),
                    "{status}: {result:?}"
                );
            }
        }
    }

    #[test]
    fn a_stalled_daemon_is_lost_only_after_repeated_failures_and_recovers() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let dir = tempfile::tempdir().unwrap();
        let up = Arc::new(AtomicBool::new(true));
        crate::daemon::tests::fake_daemon(dir.path(), up.clone());
        let controller = Controller::with_daemon(
            Arc::new(|| {}),
            dir.path().to_owned(),
            dir.path().join("missing-daemon"),
        );
        assert!(controller.is_running());
        up.store(false, Ordering::SeqCst);
        std::thread::sleep(std::time::Duration::from_millis(1500));
        assert!(controller.is_running(), "{:?}", controller.phase());
        wait_for(|| matches!(controller.phase(), Phase::Failed(_)));
        up.store(true, Ordering::SeqCst);
        wait_for(|| controller.is_running());
    }

    #[test]
    fn stop_finds_a_daemon_that_was_not_attached() {
        let _guard = crate::daemon::spawn_guard();
        let dir = tempfile::tempdir().unwrap();
        let mut controller = Controller::with_daemon(
            Arc::new(|| {}),
            dir.path().to_owned(),
            dir.path().join("missing-daemon"),
        );
        assert!(controller.daemon_status().is_none());
        let (_signal, daemon) = crate::daemon::tests::serve_in_thread(dir.path());
        crate::daemon::tests::wait_for_daemon(dir.path());
        controller.stop().unwrap();
        wait_for(|| daemon.is_finished());
        daemon.join().unwrap().unwrap();
        assert!(crate::daemon::Client::discover(dir.path()).is_none());
        assert!(matches!(controller.phase(), Phase::Stopped));
    }

    #[test]
    fn results_age_by_monotonic_time_even_if_the_wall_clock_is_set_back() {
        let dir = tempfile::tempdir().unwrap();
        let controller = Controller::with_daemon(
            Arc::new(|| {}),
            dir.path().to_owned(),
            dir.path().join("missing-daemon"),
        );
        let started = std::time::Instant::now()
            .checked_sub(std::time::Duration::from_secs(2))
            .unwrap_or_else(std::time::Instant::now);
        // Recorded an hour "ahead", as after the wall clock was set back.
        let recorded = std::time::SystemTime::now() + std::time::Duration::from_secs(3600);
        controller.lock().probes.insert(
            "proxy".into(),
            (Probe::Unreachable("timed out".into()), started, recorded),
        );
        let (name, endpoint) = ("proxy".to_owned(), "not a proxy URL".to_owned());
        controller.probe_stale([(&name, &endpoint)], std::time::Duration::from_secs(1));
        assert!(matches!(
            &controller.probes()["proxy"].0,
            Probe::Unreachable(error) if error == "Invalid proxy URL"
        ));
    }

    #[test]
    fn background_refreshes_keep_the_last_result_shown() {
        let _guard = crate::daemon::spawn_guard();
        let dir = tempfile::tempdir().unwrap();
        // The daemon, not the controller, contacts this silent local proxy.
        let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", silent.local_addr().unwrap());
        let (_signal, daemon) = crate::daemon::tests::serve_with_proxies(
            dir.path(),
            &format!("proxies:\n  local: {endpoint}\n"),
        );
        crate::daemon::tests::wait_for_daemon(dir.path());
        let mut controller = Controller::with_daemon(
            Arc::new(|| {}),
            dir.path().to_owned(),
            dir.path().join("unused"),
        );
        let name = "local".to_owned();
        let earlier = std::time::Instant::now()
            .checked_sub(std::time::Duration::from_secs(2))
            .unwrap_or_else(std::time::Instant::now);
        let reachable = Probe::Reachable {
            latency: std::time::Duration::from_millis(5),
            exit: None,
        };
        controller.lock().probes.insert(
            name.clone(),
            (reachable, earlier, std::time::SystemTime::now()),
        );
        let refresh =
            || controller.probe_stale([(&name, &endpoint)], std::time::Duration::from_secs(1));
        refresh();
        assert!(matches!(
            controller.probes()[&name].0,
            Probe::Reachable { .. }
        ));
        // A refresh already running is not started again.
        refresh();
        assert_eq!(controller.lock().testing[&name], 1);
        // A manual test still shows its progress.
        controller.probe(&name, &endpoint);
        assert!(matches!(controller.probes()[&name].0, Probe::Pending));
        assert_eq!(controller.lock().testing[&name], 2);
        // A slow test must not block the daemon's status or stop commands.
        assert!(controller.running.as_ref().unwrap().client.status().is_ok());
        controller.stop().unwrap();
        daemon.join().unwrap().unwrap();
    }

    #[test]
    fn only_an_absent_daemon_permits_gui_networking() {
        use std::io::{Read, Write};
        let dir = tempfile::tempdir().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        {
            let controller = Controller::with_daemon(
                Arc::new(|| {}),
                dir.path().into(),
                dir.path().join("unused"),
            );
            controller.probe("test", &endpoint);
            let mut incoming = None;
            wait_for(|| {
                incoming = listener.accept().ok();
                incoming.is_some()
            });
            let (mut socket, _) = incoming.unwrap();
            socket
                .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                .unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                socket.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
                assert!(request.len() < 8192);
            }
            assert!(request.starts_with(b"CONNECT 1.1.1.1:443 "));
            socket.write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
            drop(socket);
            wait_for(|| matches!(controller.probes()["test"].0, Probe::Unreachable(_)));
        }
        let up = Arc::new(std::sync::atomic::AtomicBool::new(true));
        crate::daemon::tests::fake_daemon(dir.path(), up.clone());
        let mut controller = Controller::with_daemon(
            Arc::new(|| {}),
            dir.path().into(),
            dir.path().join("unused"),
        );
        controller.probe("test", &endpoint);
        wait_for(
            || matches!(&controller.probes()["test"].0, Probe::Unavailable(e) if e.contains("Restart")),
        );
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        // A process holding registration must not be treated as absent merely
        // because it stops answering control requests.
        let lock = crate::daemon::lock_file(&dir.path().join("daemon.lock")).unwrap();
        lock.try_lock().unwrap();
        up.store(false, std::sync::atomic::Ordering::SeqCst);
        controller.probe("test", &endpoint);
        wait_for(
            || matches!(&controller.probes()["test"].0, Probe::Unavailable(e) if e.contains("control channel")),
        );
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        controller.on_app_exit(true).unwrap();
    }

    #[test]
    fn missing_helper_reports_failure_without_starting_in_process() {
        let _guard = crate::daemon::spawn_guard();
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.yaml");
        std::fs::write(&config, include_str!("../../config.example.yaml")).unwrap();
        let mut controller = Controller::with_daemon(
            Arc::new(|| {}),
            dir.path().to_owned(),
            dir.path().join("missing-daemon"),
        );
        controller.start(&config, dir.path().join("proxy.log"));
        assert!(
            matches!(controller.phase(), Phase::Failed(message) if message.contains("Cannot start"))
        );
    }

    #[test]
    fn parses_trace_responses() {
        let text = "fl=1\nh=www.cloudflare.com\nip=203.0.113.5\nts=1\nloc=JP\ncolo=NRT\n";
        assert_eq!(
            parse_trace(text),
            Some(Exit {
                ip: "203.0.113.5".into(),
                country: Some("JP".into())
            })
        );
        let v6 = parse_trace("ip=2001:db8::1\nloc=XX\n").unwrap();
        assert_eq!((v6.ip.as_str(), v6.country), ("2001:db8::1", None));
        assert_eq!(parse_trace("ip=1.2.3.4\nloc=T1\n").unwrap().country, None);
        assert_eq!(parse_trace("ip=not-an-ip\nloc=JP\n"), None);
        assert_eq!(parse_trace("<html>blocked</html>"), None);
    }

    #[test]
    fn recognizes_loopback_and_local_network_proxies() {
        for endpoint in [
            "http://127.0.0.1:7890",
            "socks5://user:pw@localhost:1080",
            "http://[::1]:8080",
            "http://10.156.232.107:10810",
            "http://172.16.0.1:8080",
            "http://172.31.255.254:8080",
            "http://192.168.1.1:8080",
            "http://169.254.1.1:8080",
            "http://[fc00::1]:8080",
            "http://[fd12::1]:8080",
            "http://[fe80::1]:8080",
            "http://[::ffff:192.168.1.1]:8080",
        ] {
            assert!(is_local_network(endpoint), "{endpoint}");
        }
        for endpoint in [
            "https://proxy.example.com:443",
            "http://172.15.255.255:8080",
            "http://172.32.0.1:8080",
            "http://192.169.1.1:8080",
            "http://8.8.8.8:8080",
            "http://[2606:4700:4700::1111]:8080",
            "http://[::ffff:8.8.8.8]:8080",
            "http://0.0.0.0:8080",
            "http://[::]:8080",
            "invalid",
        ] {
            assert!(!is_local_network(endpoint), "{endpoint}");
        }
    }

    #[test]
    fn parses_proxy_endpoints() {
        assert_eq!(
            host_port("http://127.0.0.1:7890"),
            Some(("127.0.0.1".into(), 7890))
        );
        assert_eq!(
            host_port("socks5://user:pw@proxy.local:1080/"),
            Some(("proxy.local".into(), 1080))
        );
        assert_eq!(host_port("http://[::1]:8080"), Some(("::1".into(), 8080)));
        assert_eq!(host_port("none"), None);
    }
}
