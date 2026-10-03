//! Controls the independent proxy daemon; GUI-only probes use a Tokio runtime.

use crate::daemon;
use agent_router::config::Config;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{runtime::Runtime, task::JoinHandle};

pub type Notify = Arc<dyn Fn() + Send + Sync>;

#[derive(Clone, PartialEq, Debug)]
pub enum Phase {
    Stopped,
    Running { port: u16, since: Instant },
    Failed(String),
}

#[derive(Clone)]
pub enum Probe {
    Pending,
    Reachable {
        latency: Duration,
        /// Looked up for proxies on this machine only.
        exit: Option<Exit>,
    },
    Unreachable(String),
}

/// Where traffic through a proxy leaves for the internet.
#[derive(Clone, Debug, PartialEq)]
pub struct Exit {
    pub ip: String,
    /// ISO 3166-1 alpha-2 country or region code.
    pub country: Option<String>,
}

/// Cloudflare's diagnostics endpoint: no key needed, answers with the
/// caller's address (`ip=`) and country or region (`loc=`). The IPv4 literal
/// makes dual-stack exits report their (shorter, more familiar) IPv4 address.
const TRACE_URL: &str = "https://1.1.1.1/cdn-cgi/trace";

#[derive(Clone)]
pub struct CheckResult {
    pub ok: bool,
    pub message: String,
}

#[derive(Default)]
struct Shared {
    phase: Option<Phase>,
    generation: u64,
    check: Option<CheckResult>,
    checking: bool,
    probes: BTreeMap<String, (Probe, Instant)>,
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
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("proxy")
            .build()
            .expect("Tokio runtime");
        let mut controller = Self {
            rt,
            shared: Default::default(),
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
        if let Err(error) = Config::read(config_path) {
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
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let client = monitor_client.clone();
                let result = tokio::task::spawn_blocking(move || client.status()).await;
                if !matches!(result, Ok(Ok(_))) {
                    let mut shared = shared.lock().unwrap();
                    if shared.generation != generation {
                        break;
                    }
                    shared.phase = Some(Phase::Failed(
                        "Lost contact with the proxy daemon. Restart to reconnect.".into(),
                    ));
                    drop(shared);
                    notify();
                    break;
                }
            }
        });
        self.running = Some(Running { client, monitor });
    }

    pub fn stop(&mut self) -> Result<(), String> {
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

    /// Validates configuration and credential sources, like `--check`.
    pub fn check(&self, config_path: &Path) {
        let path = config_path.to_owned();
        let shared = self.shared.clone();
        let notify = self.notify.clone();
        {
            let mut s = self.lock();
            s.checking = true;
            s.check = None;
        }
        self.rt.spawn(async move {
            let result = match Config::read(&path) {
                Ok(config) => config.check_credentials().await.map(|_| ()),
                Err(e) => Err(e),
            };
            let mut s = shared.lock().unwrap();
            s.checking = false;
            s.check = Some(match result {
                Ok(()) => CheckResult {
                    ok: true,
                    message: "Configuration, credentials and routes are valid.".into(),
                },
                Err(e) => CheckResult {
                    ok: false,
                    message: e.message.to_string(),
                },
            });
            drop(s);
            notify();
        });
        (self.notify)();
    }

    pub fn check_state(&self) -> (bool, Option<CheckResult>) {
        let s = self.lock();
        (s.checking, s.check.clone())
    }

    /// Tests an outbound proxy. Proxies on this machine (typically local
    /// clients that switch nodes) get a request through them to learn the
    /// exit address; other proxies get a TCP connection check.
    pub fn probe(&self, name: &str, endpoint: &str) {
        let Some(addr) = host_port(endpoint) else {
            self.set_probe(name, Probe::Unreachable("Invalid proxy URL".into()));
            return;
        };
        self.set_probe(name, Probe::Pending);
        let shared = self.shared.clone();
        let notify = self.notify.clone();
        let name = name.to_owned();
        let endpoint = endpoint.to_owned();
        self.rt.spawn(async move {
            let probe = if is_local(&endpoint) {
                trace(&endpoint).await
            } else {
                connect(addr).await
            };
            shared
                .lock()
                .unwrap()
                .probes
                .insert(name, (probe, Instant::now()));
            notify();
        });
    }

    /// Probes proxies without a result newer than `max_age`.
    pub fn probe_stale<'a>(
        &self,
        proxies: impl IntoIterator<Item = (&'a String, &'a String)>,
        max_age: Duration,
    ) {
        for (name, endpoint) in proxies {
            let stale = self.lock().probes.get(name).is_none_or(|(probe, at)| {
                !matches!(probe, Probe::Pending) && at.elapsed() > max_age
            });
            if stale {
                self.probe(name, endpoint);
            }
        }
    }

    fn set_probe(&self, name: &str, probe: Probe) {
        self.lock()
            .probes
            .insert(name.to_owned(), (probe, Instant::now()));
    }

    pub fn probes(&self) -> BTreeMap<String, Probe> {
        self.lock()
            .probes
            .iter()
            .map(|(name, (probe, _))| (name.clone(), probe.clone()))
            .collect()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.shared.lock().unwrap()
    }
}

impl Drop for Controller {
    fn drop(&mut self) {
        let _ = self.stop();
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
    let response = match client.get(TRACE_URL).send().await {
        Ok(response) => response,
        Err(e) if e.is_timeout() => return Probe::Unreachable("timed out".into()),
        Err(e) if e.is_connect() => return Probe::Unreachable("cannot connect".into()),
        Err(_) => return Probe::Unreachable("request failed".into()),
    };
    if !response.status().is_success() {
        return Probe::Unreachable(format!("HTTP {}", response.status().as_u16()));
    }
    let text = response.text().await.unwrap_or_default();
    Probe::Reachable {
        latency: started.elapsed(),
        exit: parse_trace(&text),
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

/// A proxy listening on this machine.
pub fn is_local(endpoint: &str) -> bool {
    host_port(endpoint).is_some_and(|(host, _)| {
        host.eq_ignore_ascii_case("localhost")
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
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
    use super::{Controller, Exit, Phase, host_port, is_local, parse_trace};
    use std::sync::Arc;

    #[test]
    fn missing_helper_reports_failure_without_starting_in_process() {
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
    fn recognizes_local_proxies() {
        assert!(is_local("http://127.0.0.1:7890"));
        assert!(is_local("socks5://user:pw@localhost:1080"));
        assert!(is_local("http://[::1]:8080"));
        assert!(!is_local("http://10.156.232.107:10810"));
        assert!(!is_local("https://proxy.example.com:443"));
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
