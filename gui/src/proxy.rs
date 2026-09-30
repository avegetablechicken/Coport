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
    Reachable(Duration),
    Unreachable(String),
}

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
    probes: BTreeMap<String, Probe>,
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
        let config = Config::read(config_path, false).map_err(|e| e.message.to_string())?;
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
            let result = match Config::read(&path, false) {
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

    /// TCP reachability of an outbound proxy endpoint.
    pub fn probe(&self, name: &str, endpoint: &str) {
        let Some(addr) = host_port(endpoint) else {
            self.lock().probes.insert(
                name.to_owned(),
                Probe::Unreachable("Invalid proxy URL".into()),
            );
            return;
        };
        self.lock().probes.insert(name.to_owned(), Probe::Pending);
        let shared = self.shared.clone();
        let notify = self.notify.clone();
        let name = name.to_owned();
        self.rt.spawn(async move {
            let started = Instant::now();
            let result =
                tokio::time::timeout(Duration::from_secs(3), tokio::net::TcpStream::connect(addr))
                    .await;
            let probe = match result {
                Ok(Ok(_)) => Probe::Reachable(started.elapsed()),
                Ok(Err(e)) => Probe::Unreachable(e.kind().to_string()),
                Err(_) => Probe::Unreachable("timed out".into()),
            };
            shared.lock().unwrap().probes.insert(name, probe);
            notify();
        });
    }

    pub fn probes(&self) -> BTreeMap<String, Probe> {
        self.lock().probes.clone()
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
    use super::{Controller, Phase, host_port};
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
