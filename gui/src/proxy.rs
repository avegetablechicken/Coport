//! Runs the proxy server in-process on a background Tokio runtime.

use coding_agent_proxy::{config::Config, logger::Logger, server::Server};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{runtime::Runtime, sync::oneshot, task::JoinHandle};

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
    check: Option<CheckResult>,
    checking: bool,
    probes: BTreeMap<String, (Probe, Instant)>,
}

struct Running {
    shutdown: oneshot::Sender<()>,
    task: JoinHandle<()>,
}

pub struct Controller {
    rt: Runtime,
    shared: Arc<Mutex<Shared>>,
    running: Option<Running>,
    notify: Notify,
}

impl Controller {
    pub fn new(notify: Notify) -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("proxy")
            .build()
            .expect("Tokio runtime");
        Self {
            rt,
            shared: Default::default(),
            running: None,
            notify,
        }
    }

    pub fn phase(&self) -> Phase {
        self.lock().phase.clone().unwrap_or(Phase::Stopped)
    }

    pub fn is_running(&self) -> bool {
        matches!(self.phase(), Phase::Running { .. })
    }

    pub fn start(&mut self, config_path: &Path, log_path: PathBuf) {
        self.stop();
        let phase = match self.spawn(config_path, log_path) {
            Ok(phase) => phase,
            Err(message) => Phase::Failed(message),
        };
        self.lock().phase = Some(phase);
        (self.notify)();
    }

    fn spawn(&mut self, config_path: &Path, log_path: PathBuf) -> Result<Phase, String> {
        let config = Config::read(config_path).map_err(|e| e.message.to_string())?;
        let port = config.listen_port;
        let listener = self
            .rt
            .block_on(tokio::net::TcpListener::bind((
                std::net::Ipv4Addr::LOCALHOST,
                port,
            )))
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::AddrInUse => {
                    format!("Port {port} is already in use.")
                }
                _ => format!("Cannot listen on 127.0.0.1:{port}: {e}"),
            })?;
        let logger = Arc::new(Logger::new(log_path));
        logger.write("server_started", Default::default());
        let server = Arc::new(Server::new(config, logger.clone()));
        let (shutdown, stop) = oneshot::channel::<()>();
        let shared = self.shared.clone();
        let notify = self.notify.clone();
        let task = self.rt.spawn(async move {
            // Startup route reporting can read credentials or a login shell;
            // accept connections meanwhile instead of delaying them.
            let startup = tokio::spawn({
                let server = server.clone();
                async move { server.startup_log().await }
            });
            let result = server
                .serve(listener, async {
                    let _ = stop.await;
                })
                .await;
            startup.abort();
            logger.write("server_stopped", Default::default());
            if let Err(e) = result {
                shared.lock().unwrap().phase = Some(Phase::Failed(format!("Listener failed: {e}")));
                notify();
            }
        });
        self.running = Some(Running { shutdown, task });
        Ok(Phase::Running {
            port,
            since: Instant::now(),
        })
    }

    pub fn stop(&mut self) {
        if let Some(running) = self.running.take() {
            let _ = running.shutdown.send(());
            // Wait so the port is free before a restart binds it again.
            // The timer must be created inside the runtime, not just awaited there.
            let _ = self.rt.block_on(async {
                tokio::time::timeout(Duration::from_secs(5), running.task).await
            });
            let mut shared = self.lock();
            if matches!(shared.phase, Some(Phase::Running { .. })) {
                shared.phase = Some(Phase::Stopped);
            }
            drop(shared);
            (self.notify)();
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
        self.stop();
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
    fn starts_and_stops_outside_any_runtime() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.yaml");
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let example = include_str!("../../config.example.yaml")
            .replace("listen_port: 8787", &format!("listen_port: {port}"));
        std::fs::write(&config, example).unwrap();
        let mut controller = Controller::new(Arc::new(|| {}));
        controller.start(&config, dir.path().join("proxy.log"));
        assert!(
            matches!(controller.phase(), Phase::Running { .. }),
            "{:?}",
            controller.phase()
        );
        controller.stop();
        assert_eq!(controller.phase(), Phase::Stopped);
        // The port is free again once stop returns.
        std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
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
