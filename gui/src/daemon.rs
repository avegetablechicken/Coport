//! A detached, GUI-free proxy process. Discovery is per-user; control requests
//! use a random capability stored in an owner-only file, never on the proxy port.

use coport::{config::Config, logger::Logger, server::Server};
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    io::{self, Read, Write},
    net::{Ipv4Addr, TcpStream},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

const ENDPOINT: &str = "daemon.json";
const MAX_MESSAGE: usize = 64 * 1024;
/// A forwarded traffic window can approach the 1 MiB remote summary it comes from.
const MAX_RESPONSE: usize = 1024 * 1024;
const STATUS_TIMEOUT: Duration = Duration::from_millis(750);
const STOP_TIMEOUT: Duration = Duration::from_secs(7);

#[derive(Clone, Serialize, Deserialize)]
struct Endpoint {
    port: u16,
    token: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Status {
    pub pid: u32,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub forwarding_restore_supported: bool,
    #[serde(default)]
    pub forwarding_restore: bool,
    #[serde(default)]
    pub forwarding_supported: bool,
    #[serde(default)]
    pub forwarding: Option<crate::remote_forward::Status>,
    #[serde(default)]
    pub forwarding_traffic_supported: bool,
    #[serde(default)]
    pub proxy_probe_supported: bool,
    #[serde(default)]
    pub metadata_supported: bool,
    #[serde(default)]
    pub credential_checks_supported: bool,
    /// Account states are read from local data and learned identities only;
    /// older daemons probe upstream to answer the same query.
    #[serde(default)]
    pub passive_account_states_supported: bool,
    /// Device checks take the GUI's device list; older daemons read it from
    /// the GUI's preferences.
    #[serde(default)]
    pub device_list_supported: bool,
    pub port: u16,
    #[serde(default)]
    pub allow_external_access: bool,
    #[serde(default)]
    pub data_port: Option<u16>,
    pub uptime_ms: u64,
    /// The proxy-port CA as loaded at start, while credential routing is on.
    #[serde(default)]
    pub proxy_ca: Option<ProxyCa>,
    pub config_path: PathBuf,
    pub log_path: PathBuf,
    pub config_modified: Option<SystemTime>,
    /// Client requests the daemon is serving, counting each connection
    /// forwarded over SSH as one; absent from older daemons.
    #[serde(default)]
    pub requests_in_flight: Option<usize>,
}

/// Credential-routed origins the stored proxy CA does not cover, which stay
/// undecrypted until the user replaces it, and whether it leaves a name type
/// unconstrained.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ProxyCa {
    pub decrypted: Vec<String>,
    pub uncovered: Vec<String>,
    pub unrestricted: bool,
}

#[derive(Serialize, Deserialize)]
enum Action {
    Status,
    Stop,
    Probe {
        name: String,
    },
    AccountStates,
    CredentialReports,
    CredentialLabels,
    CachedCredentialLabels,
    SetForwarding {
        target: Option<crate::remote_forward::Target>,
    },
    SetForwardRestore {
        enabled: bool,
    },
    /// From GUIs before `DeviceChecks`: the known devices' checks.
    DeviceCapabilities,
    /// From GUIs before `DeviceChecks`: check waiting devices now.
    RefreshDeviceCapabilities,
    /// The capability checks of `devices`, the SSH devices the GUI manages.
    DeviceChecks {
        devices: Vec<crate::remote::Device>,
    },
    ForwardingTraffic {
        minutes: u64,
        scope: crate::traffic::TrafficScope,
    },
    ForwardingSummary,
    RecordDeviceQuery(crate::device_events::QueryEvent),
}

#[derive(Serialize, Deserialize)]
struct Request {
    token: String,
    action: Action,
}

#[derive(Serialize, Deserialize)]
enum Response {
    Status(Box<Status>),
    Stopped,
    Probe(crate::proxy::Probe),
    /// Codex, Claude, Antigravity, then Grok. A list, so a daemon that reports
    /// fewer apps still answers; the missing ones have no states.
    AccountStates(Vec<std::collections::BTreeMap<String, String>>),
    CredentialReports(Vec<coport::server::CredentialReport>),
    CredentialLabels(Vec<((String, String), String)>),
    Recorded,
    ForwardingSet,
    DeviceCapabilities(Vec<crate::device_capabilities::Status>),
    ForwardingTraffic(Box<crate::remote_forward::RemoteWindow>),
    ForwardingSummary(Option<Box<crate::remote_forward::RemoteSummary>>),
    Error(String),
}

#[derive(Clone)]
pub struct Client {
    endpoint: Endpoint,
    directory: PathBuf,
}

/// The running Daemon serving `dir`, looked up afresh off the async runtime.
/// Not finding one is no proof that none runs: see `Lifecycle::find`. An
/// error only when the lookup itself could not run.
pub async fn find(dir: &Path) -> io::Result<Option<(Client, Status)>> {
    let dir = dir.to_owned();
    tokio::task::spawn_blocking(move || Client::discover(&dir))
        .await
        .map_err(io::Error::other)
}

/// The running Daemon serving `dir`, looked up when first asked for and kept
/// from then on. Clones share that one lookup: one traffic read takes one,
/// so all it asks of the Daemon goes to the same one.
#[derive(Clone)]
pub struct Lookup {
    dir: PathBuf,
    found: Arc<tokio::sync::OnceCell<Option<(Client, Status)>>>,
}

impl Lookup {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            found: Default::default(),
        }
    }

    /// The Daemon this lookup found; none if it found none or could not look.
    pub async fn get(&self) -> Option<(Client, Status)> {
        self.found
            .get_or_init(|| async { find(&self.dir).await.ok().flatten() })
            .await
            .clone()
    }
}

impl Client {
    pub fn discover(dir: &Path) -> Option<(Self, Status)> {
        let endpoint = serde_json::from_slice(&std::fs::read(dir.join(ENDPOINT)).ok()?).ok()?;
        let client = Self {
            endpoint,
            directory: dir.to_owned(),
        };
        let status = client.status().ok()?;
        Some((client, status))
    }

    pub fn status(&self) -> io::Result<Status> {
        match self.request(Action::Status, STATUS_TIMEOUT)? {
            Response::Status(status) => Ok(*status),
            _ => Err(io::Error::other("Unexpected daemon status response")),
        }
    }

    pub async fn device_capabilities(&self) -> io::Result<Vec<crate::device_capabilities::Status>> {
        match self
            .request_async(Action::DeviceCapabilities, STATUS_TIMEOUT)
            .await?
        {
            Response::DeviceCapabilities(value) => Ok(value),
            _ => Err(io::Error::other(
                "Update the local proxy for automatic capability checks.",
            )),
        }
    }
    /// The checks of `devices`, which the daemon checks from now on: a new
    /// or changed one at once.
    pub async fn device_checks(
        &self,
        devices: Vec<crate::remote::Device>,
    ) -> io::Result<Vec<crate::device_capabilities::Status>> {
        match self
            .request_async(Action::DeviceChecks { devices }, STATUS_TIMEOUT)
            .await?
        {
            Response::DeviceCapabilities(value) => Ok(value),
            _ => Err(io::Error::other("Unexpected daemon device response")),
        }
    }
    pub async fn refresh_device_capabilities(&self) -> io::Result<()> {
        match self
            .request_async(Action::RefreshDeviceCapabilities, STATUS_TIMEOUT)
            .await?
        {
            Response::Recorded => Ok(()),
            Response::Error(error) => Err(io::Error::other(error)),
            _ => Err(io::Error::other(
                "Update the local proxy for automatic capability checks.",
            )),
        }
    }
    /// The forwarded device's statistics as last read by the daemon's monitor.
    pub async fn forwarding_traffic(
        &self,
        minutes: u64,
        scope: crate::traffic::TrafficScope,
    ) -> io::Result<crate::remote_forward::RemoteWindow> {
        match self
            .request_async(Action::ForwardingTraffic { minutes, scope }, STATUS_TIMEOUT)
            .await?
        {
            Response::ForwardingTraffic(window) => Ok(*window),
            _ => Err(io::Error::other("Unexpected daemon traffic response")),
        }
    }
    /// The forwarded device's summary as last read by the daemon's monitor.
    pub async fn forwarding_summary(
        &self,
    ) -> io::Result<Option<crate::remote_forward::RemoteSummary>> {
        match self
            .request_async(Action::ForwardingSummary, STATUS_TIMEOUT)
            .await?
        {
            Response::ForwardingSummary(summary) => Ok(summary.map(|summary| *summary)),
            _ => Err(io::Error::other("Unexpected daemon summary response")),
        }
    }
    pub async fn set_forwarding_restore(&self, enabled: bool) -> io::Result<()> {
        match self
            .request_async(
                Action::SetForwardRestore { enabled },
                Duration::from_secs(60),
            )
            .await?
        {
            Response::ForwardingSet => Ok(()),
            Response::Error(error) => Err(io::Error::other(error)),
            _ => Err(io::Error::other(
                "Update the local daemon to configure forwarding restore.",
            )),
        }
    }

    pub async fn set_forwarding(
        &self,
        target: Option<crate::remote_forward::Target>,
    ) -> io::Result<()> {
        match self
            .request_async(Action::SetForwarding { target }, Duration::from_secs(60))
            .await?
        {
            Response::ForwardingSet => Ok(()),
            Response::Error(error) => Err(io::Error::other(error)),
            _ => Err(io::Error::other(
                "Update and restart the local daemon to enable unified SSH forwarding.",
            )),
        }
    }

    /// Only a configured proxy name crosses the control channel. The daemon
    /// owns the endpoint, authentication and outbound connection.
    pub fn probe(&self, name: &str) -> io::Result<crate::proxy::Probe> {
        match self.request(Action::Probe { name: name.into() }, Duration::from_secs(10))? {
            Response::Probe(probe) => Ok(probe),
            _ => Err(io::Error::other("Unexpected daemon probe response")),
        }
    }

    pub async fn probe_async(&self, name: &str) -> io::Result<crate::proxy::Probe> {
        match self
            .request_async(Action::Probe { name: name.into() }, Duration::from_secs(10))
            .await?
        {
            Response::Probe(probe) => Ok(probe),
            _ => Err(io::Error::other("Unexpected daemon probe response")),
        }
    }

    pub async fn credential_reports(&self) -> io::Result<Vec<coport::server::CredentialReport>> {
        match self
            .request_async(Action::CredentialReports, Duration::from_secs(65))
            .await?
        {
            Response::CredentialReports(reports) => Ok(reports),
            _ => Err(io::Error::other(
                "Credential checks unavailable; restart the proxy daemon.",
            )),
        }
    }

    pub async fn account_states(&self) -> io::Result<crate::tasks::AccountStates> {
        match self
            .request_async(Action::AccountStates, Duration::from_secs(60))
            .await?
        {
            Response::AccountStates(states) => Ok(crate::tasks::account_states(states)),
            _ => Err(io::Error::other("Unexpected daemon account response")),
        }
    }

    pub async fn cached_credential_labels(
        &self,
    ) -> io::Result<std::collections::BTreeMap<(String, String), String>> {
        match self
            .request_async(Action::CachedCredentialLabels, Duration::from_secs(5))
            .await?
        {
            Response::CredentialLabels(labels) => Ok(labels.into_iter().collect()),
            _ => Err(io::Error::other("Cached metadata unavailable")),
        }
    }
    pub async fn credential_labels(
        &self,
    ) -> io::Result<std::collections::BTreeMap<(String, String), String>> {
        match self
            .request_async(Action::CredentialLabels, Duration::from_secs(60))
            .await?
        {
            Response::CredentialLabels(labels) => Ok(labels.into_iter().collect()),
            _ => Err(io::Error::other("Unexpected daemon labels response")),
        }
    }

    async fn request_async(&self, action: Action, timeout: Duration) -> io::Result<Response> {
        tokio::time::timeout(timeout, async {
            let mut stream =
                tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, self.endpoint.port)).await?;
            let bytes = serde_json::to_vec(&Request {
                token: self.endpoint.token.clone(),
                action,
            })?;
            stream.write_u32(bytes.len() as u32).await?;
            stream.write_all(&bytes).await?;
            let length = stream.read_u32().await? as usize;
            if length > MAX_RESPONSE {
                return Err(io::Error::other("Daemon response too large"));
            }
            let mut bytes = vec![0; length];
            stream.read_exact(&mut bytes).await?;
            match serde_json::from_slice(&bytes)? {
                Response::Error(error) => Err(io::Error::other(error)),
                response => Ok(response),
            }
        })
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "Daemon request timed out"))?
    }

    /// The daemon acknowledges only after the proxy has released its listener.
    pub fn stop(&self) -> io::Result<()> {
        match self.request(Action::Stop, STOP_TIMEOUT) {
            Ok(Response::Stopped) => Ok(()),
            // A dead loopback endpoint can time out on Windows instead of
            // refusing the connection. Only a released daemon lock proves
            // that the old process has stopped; a timeout alone does not.
            Err(_) if self.registration_is_unlocked() => Ok(()),
            Err(e) => Err(e),
            _ => Err(io::Error::other("Unexpected daemon stop response")),
        }
    }

    pub(crate) fn record_device_query(
        &self,
        event: crate::device_events::QueryEvent,
    ) -> io::Result<()> {
        match self.request(Action::RecordDeviceQuery(event), STATUS_TIMEOUT)? {
            Response::Recorded => Ok(()),
            _ => Err(io::Error::other("Unexpected device query audit response")),
        }
    }

    fn registration_is_unlocked(&self) -> bool {
        try_hold(&self.directory.join("daemon.lock")).is_ok_and(|held| held.is_some())
    }

    fn request(&self, action: Action, timeout: Duration) -> io::Result<Response> {
        let mut stream = TcpStream::connect_timeout(
            &(Ipv4Addr::LOCALHOST, self.endpoint.port).into(),
            STATUS_TIMEOUT,
        )?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        let bytes = serde_json::to_vec(&Request {
            token: self.endpoint.token.clone(),
            action,
        })?;
        stream.write_all(&(bytes.len() as u32).to_be_bytes())?;
        stream.write_all(&bytes)?;
        let mut length = [0; 4];
        stream.read_exact(&mut length)?;
        let length = u32::from_be_bytes(length) as usize;
        if length > MAX_RESPONSE {
            return Err(io::Error::other("Daemon response too large"));
        }
        let mut bytes = vec![0; length];
        stream.read_exact(&mut bytes)?;
        match serde_json::from_slice(&bytes)? {
            Response::Error(error) => Err(io::Error::other(error)),
            response => Ok(response),
        }
    }
}

/// A failed status request is not proof that the daemon is absent. Only an
/// unlocked registration permits GUI fallback (including stale discovery).
pub async fn task_backend(dir: PathBuf) -> io::Result<Option<(Client, Status)>> {
    tokio::task::spawn_blocking(move || {
        if let Some(found) = Client::discover(&dir) {
            return Ok(Some(found));
        }
        if !dir.exists() {
            return Ok(None);
        }
        match try_hold(&dir.join("daemon.lock"))? {
            Some(_) => Ok(None),
            None => Err(io::Error::other(
                "Proxy daemon is running but its control channel is unavailable.",
            )),
        }
    })
    .await
    .map_err(io::Error::other)?
}

pub fn check_task_config(status: &Status) -> io::Result<()> {
    if std::fs::metadata(&status.config_path)
        .and_then(|m| m.modified())
        .ok()
        != status.config_modified
    {
        return Err(io::Error::other(
            "Restart the proxy daemon to use the updated configuration.",
        ));
    }
    Ok(())
}

pub fn binary_path() -> io::Result<PathBuf> {
    Ok(std::env::current_exe()?.with_file_name(if cfg!(windows) {
        "coportd.exe"
    } else {
        "coportd"
    }))
}

pub(crate) fn private_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

pub(crate) fn lock_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    // Windows cannot lock an append-only handle. Keep write access without
    // truncating the file, since another daemon may already hold its lock.
    options.create(true).truncate(false).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// A file lock this process holds. Dropping it unlocks rather than only
/// closes: a child that another thread forks holds a copy of the handle until
/// it execs, and the lock with it.
pub(crate) struct Held(pub(crate) File);
impl Drop for Held {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

/// The lock at `path` if no one else holds it.
pub(crate) fn try_hold(path: &Path) -> io::Result<Option<Held>> {
    let lock = lock_file(path)?;
    match lock.try_lock() {
        Ok(()) => Ok(Some(Held(lock))),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(error)) => Err(error),
    }
}

struct Registration {
    path: PathBuf,
    lock: File,
}
impl Drop for Registration {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        // Windows releases a lock left on a closed handle only when system
        // resources allow, so an immediate restart could still find it held.
        let _ = self.lock.unlock();
    }
}

async fn read_request(stream: &mut tokio::net::TcpStream) -> io::Result<Request> {
    let length = stream.read_u32().await? as usize;
    if length > MAX_MESSAGE {
        return Err(io::Error::other("Control request too large"));
    }
    let mut bytes = vec![0; length];
    stream.read_exact(&mut bytes).await?;
    Ok(serde_json::from_slice(&bytes)?)
}

async fn respond(stream: &mut tokio::net::TcpStream, response: Response) -> io::Result<()> {
    let bytes = serde_json::to_vec(&response)?;
    stream.write_u32(bytes.len() as u32).await?;
    stream.write_all(&bytes).await
}

/// Answers one control connection. Each runs in its own task, so a peer that
/// connects and never sends cannot delay requests on other connections.
async fn handle_control(
    mut stream: tokio::net::TcpStream,
    token: Arc<str>,
    status: Arc<Status>,
    started: Instant,
    server: Arc<Server>,
    stops: tokio::sync::mpsc::UnboundedSender<tokio::net::TcpStream>,
    routing: Arc<crate::remote_forward::Routing>,
) {
    let request = tokio::time::timeout(Duration::from_millis(500), read_request(&mut stream)).await;
    let Ok(Ok(request)) = request else {
        return;
    };
    let response = if request.token != *token {
        Response::Error("Unauthorized".into())
    } else {
        match request.action {
            Action::Status => Response::Status(Box::new(Status {
                uptime_ms: started.elapsed().as_millis() as u64,
                forwarding: routing.status(),
                forwarding_restore: routing.restore_enabled(),
                // A Restart must not cut a stream forwarded over SSH.
                requests_in_flight: Some(server.requests_in_flight() + routing.connections_in_flight()),
                ..Status::clone(&status)
            })),
            Action::DeviceCapabilities => Response::DeviceCapabilities(routing.device_checks.statuses()),
            Action::DeviceChecks { devices } => match routing.device_checks.set(devices) {
                Ok(()) => Response::DeviceCapabilities(routing.device_checks.statuses()),
                Err(error) => Response::Error(error.to_string()),
            },
            Action::ForwardingSummary => Response::ForwardingSummary(routing.summary().map(Box::new)),
            Action::ForwardingTraffic { minutes, scope } => match routing.traffic_window(minutes, scope) {
                Ok(window) => Response::ForwardingTraffic(Box::new(window)),
                Err(error) => Response::Error(error),
            },
            Action::RefreshDeviceCapabilities => {
                routing.device_checks.refresh();
                Response::Recorded
            }
            Action::SetForwardRestore { enabled } => match routing.set_restore(enabled).await {
                Ok(()) => Response::ForwardingSet,
                Err(error) => Response::Error(error),
            },
            Action::SetForwarding { target } => match routing.set(target).await {
                Ok(()) => Response::ForwardingSet,
                Err(error) => Response::Error(error),
            },
            Action::Probe { name } => match server.config.proxies.get(&name) {
                Some(endpoint) => Response::Probe(crate::proxy::run_probe(endpoint).await),
                None => Response::Error("Proxy is not in the running daemon configuration; restart the daemon after changing proxies.".into()),
            },
            Action::CredentialReports => Response::CredentialReports(server.credential_reports().await),
            Action::AccountStates => Response::AccountStates(server.account_route_states().await.into_iter().map(|states| states.into_iter().map(|(key, value)| (key, value.to_owned())).collect()).collect()),
            Action::CachedCredentialLabels => Response::CredentialLabels(server.cached_traffic_credential_labels().await.into_iter().collect()),
            // Legacy statistics readers must obey the same no-network contract.
            Action::CredentialLabels => Response::CredentialLabels(server.cached_traffic_credential_labels().await.into_iter().collect()),
            Action::RecordDeviceQuery(event) => {
                if event.validate() {
                    server.logger.write(event.event(), event.fields());
                    Response::Recorded
                } else {
                    Response::Error("Invalid device query audit".into())
                }
            }
            // The serve loop acknowledges once the listener has been released.
            Action::Stop => {
                let _ = stops.send(stream);
                return;
            }
        }
    };
    let _ = tokio::time::timeout(Duration::from_millis(500), respond(&mut stream, response)).await;
}

async fn termination() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler");
        tokio::select! { _ = term.recv() => {}, _ = tokio::signal::ctrl_c() => {} }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// Runs without Tauri, a WebView, a tray icon, or any GUI lifecycle hooks.
pub async fn serve(dir: &Path, config: &Path, log: &Path) -> io::Result<()> {
    serve_until(dir, config, log, termination()).await
}

async fn serve_until(
    dir: &Path,
    config: &Path,
    log: &Path,
    signal: impl std::future::Future<Output = ()>,
) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let lock = lock_file(&dir.join("daemon.lock"))?;
    lock.try_lock().map_err(|error| match error {
        std::fs::TryLockError::WouldBlock => io::Error::other("A proxy daemon is already running"),
        std::fs::TryLockError::Error(error) => error,
    })?;
    let registration = Registration {
        path: dir.join(ENDPOINT),
        lock,
    };
    let config_path = std::path::absolute(config)?;
    let log_path = std::path::absolute(log)?;
    let config_modified = std::fs::metadata(&config_path)?.modified().ok();
    let config = Config::read(&config_path).map_err(|e| io::Error::other(e.message))?;
    let logger = Arc::new(Logger::new(log_path.clone()));
    let allow_external_access = config.allow_external_access;
    // SSH summaries are optional when external sharing is disabled. Preserve
    // damaged files for diagnosis instead of blocking the local proxy or
    // silently generating a new identity for a previously known device.
    if let Err(error) = crate::data_api::prepare_identity(dir) {
        if allow_external_access {
            return Err(error);
        }
        logger.write(
            "statistics_unavailable",
            [("reason".to_owned(), error.to_string().into())]
                .into_iter()
                .collect(),
        );
    }
    // Publishing the SSH discovery hint is independent of proxy/HTTP startup.
    // The record is written now; the helper, a copy of the whole executable,
    // is copied beside startup, since discovery runs the recorded executable
    // while it exists.
    let discovery_unavailable = |logger: &Logger, error: io::Error| {
        logger.write(
            "statistics_discovery_unavailable",
            [("reason".to_owned(), error.to_string().into())]
                .into_iter()
                .collect(),
        );
    };
    match std::env::current_exe().and_then(|binary| {
        crate::remote::register_summary_executable(dir, &binary).map(|()| binary)
    }) {
        Ok(binary) => {
            let (dir, logger) = (dir.to_owned(), logger.clone());
            std::thread::spawn(move || {
                if let Err(error) = crate::remote::preserve_summary_executable(&dir, &binary) {
                    discovery_unavailable(&logger, error);
                }
            });
        }
        Err(error) => discovery_unavailable(&logger, error),
    }
    let _data_api = if allow_external_access {
        Some(crate::data_api::start(config.clone(), dir, log_path.clone()).await?)
    } else {
        None
    };
    let data_port = _data_api.as_ref().map(|api| api.port);
    let proxy = TcpListener::bind((Ipv4Addr::LOCALHOST, config.listen_port)).await?;
    let port = proxy.local_addr()?.port();
    let control = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let endpoint = Endpoint {
        port: control.local_addr()?.port(),
        token: uuid::Uuid::new_v4().to_string(),
    };
    let tls_dir = coport::local_tls::dir_for(&config_path);
    // Enabled in Settings; without its CA, proxy-port clients could not be routed.
    let credential_proxy = coport::credential_proxy::CredentialProxy::prepare(&config, &tls_dir)
        .map_err(|e| io::Error::other(e.message))?;
    let proxy_ca = credential_proxy.as_ref().map(|proxy| ProxyCa {
        decrypted: proxy.authorities(),
        uncovered: proxy.uncovered(),
        unrestricted: proxy.ca_unrestricted(),
    });
    let mut server = Server::new(config, logger.clone())
        .with_refresh_routes(coport::refresh_routes::RefreshRoutes::load(
            dir.join(coport::refresh_routes::FILE),
        ))
        .with_grok_routes(coport::refresh_routes::RefreshRoutes::load(
            dir.join(coport::refresh_routes::GROK_FILE),
        ))
        .with_codex_refresh_routes(coport::refresh_routes::RefreshRoutes::load(
            dir.join(coport::refresh_routes::CODEX_FILE),
        ))
        .with_antigravity_refresh_routes(coport::refresh_routes::RefreshRoutes::load(
            dir.join(coport::refresh_routes::ANTIGRAVITY_FILE),
        ));
    if let Some(proxy) = credential_proxy {
        server = server.with_credential_proxy(proxy);
    }
    // Without TLS, plain-HTTP clients still work; HTTPS base URLs fail to connect.
    match coport::local_tls::acceptor(&tls_dir) {
        Ok(tls) => server = server.with_tls(tls),
        Err(e) => logger.write(
            "tls_disabled",
            [("reason".to_owned(), e.message.into())]
                .into_iter()
                .collect(),
        ),
    }
    let server = Arc::new(server);
    let started = Instant::now();
    let (shutdown, stop) = tokio::sync::oneshot::channel::<()>();
    let startup = tokio::spawn({
        let server = server.clone();
        async move { server.startup_log().await }
    });
    let routing = Arc::new(crate::remote_forward::Routing::with_store(dir)?);
    let handler = routing.handler();
    let serving_server = server.clone();
    let mut serving = tokio::spawn(async move {
        serving_server
            .serve_routed(
                proxy,
                async {
                    let _ = stop.await;
                },
                Some(handler),
            )
            .await
    });
    logger.write("server_started", Default::default());
    // On Windows, std::fs::rename can replace a stale file that a discovery
    // reader still has open. NamedTempFile::persist uses only MoveFileExW.
    {
        let staging = tempfile::tempdir_in(dir)?;
        let staged = staging.path().join(ENDPOINT);
        crate::settings::create_private(&staged, &serde_json::to_vec(&endpoint)?)?;
        std::fs::rename(staged, &registration.path)?;
    }

    let token: Arc<str> = endpoint.token.into();
    let status = Arc::new(Status {
        pid: std::process::id(),
        version: env!("CARGO_PKG_VERSION").into(),
        forwarding_restore_supported: true,
        forwarding_restore: false,
        forwarding_supported: true,
        forwarding: None,
        forwarding_traffic_supported: true,
        proxy_probe_supported: true,
        metadata_supported: true,
        credential_checks_supported: true,
        passive_account_states_supported: true,
        device_list_supported: true,
        requests_in_flight: None,
        port,
        uptime_ms: 0,
        proxy_ca,
        allow_external_access,
        data_port,
        config_path,
        log_path,
        config_modified,
    });
    let (stops, mut stop_requests) = tokio::sync::mpsc::unbounded_channel();
    tokio::pin!(signal);
    let checks = routing.device_checks.monitor();
    tokio::pin!(checks);
    let monitor = routing.monitor();
    tokio::pin!(monitor);
    let mut controls = tokio::task::JoinSet::new();
    let mut stop_client = None;
    let result = loop {
        tokio::select! {
            _ = &mut signal => break Ok(()),
            _ = &mut checks => break Err(io::Error::other("Device capability checker stopped unexpectedly")),
            _ = &mut monitor => break Err(io::Error::other("Forwarding monitor stopped unexpectedly")),
            Some(_) = controls.join_next(), if !controls.is_empty() => {},
            result = &mut serving => {
                // Do not poll the completed JoinHandle again during cleanup.
                controls.shutdown().await;
                startup.abort();
                logger.write("server_stopped", Default::default());
                return result.map_err(io::Error::other)?;
            }
            Some(stream) = stop_requests.recv() => { stop_client = Some(stream); break Ok(()); }
            accepted = control.accept() => {
                let (stream, _) = match accepted { Ok(pair) => pair, Err(e) => break Err(e) };
                controls.spawn(handle_control(stream, token.clone(), status.clone(), started, server.clone(), stops.clone(), routing.clone()));
            }
        }
    };
    controls.shutdown().await;
    let _ = shutdown.send(());
    let stopped = match tokio::time::timeout(Duration::from_secs(5), &mut serving).await {
        Ok(result) => result.map_err(io::Error::other).and_then(|result| result),
        Err(_) => {
            serving.abort();
            let _ = serving.await;
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Proxy shutdown timed out",
            ))
        }
    };
    startup.abort();
    logger.write("server_stopped", Default::default());
    // Remove discovery and unlock before acknowledging, so an immediate restart works.
    drop(control);
    drop(registration);
    // Stop requests that raced the first one get the same answer.
    stop_requests.close();
    let mut stop_clients: Vec<_> = stop_client.into_iter().collect();
    while let Ok(stream) = stop_requests.try_recv() {
        stop_clients.push(stream);
    }
    for mut stream in stop_clients {
        let response = match &stopped {
            Ok(()) => Response::Stopped,
            Err(e) => Response::Error(e.to_string()),
        };
        let _ =
            tokio::time::timeout(Duration::from_millis(500), respond(&mut stream, response)).await;
    }
    result.and(stopped)
}

/// A child forked by `start`, or by any test, shares every open file until it
/// execs, including lock handles a parallel test has just dropped. Tests that
/// spawn a child process or expect a released lock to be free at once hold
/// this guard.
#[cfg(test)]
pub(crate) fn spawn_guard() -> std::sync::MutexGuard<'static, ()> {
    static GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());
    GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[tokio::test]
    async fn shutdown_joins_pending_control_connections_before_returning() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.yaml");
        let log = dir.path().join("proxy.log");
        let port = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        std::fs::write(&config,format!("listen_port: {port}\nrequest_timeout_seconds: 3\ncodex:\n  homes: []\nclaude:\n  config_dirs: []\n")).unwrap();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let directory = dir.path().to_owned();
        let server = tokio::spawn(async move {
            serve_until(&directory, &config, &log, async {
                let _ = stopped.await;
            })
            .await
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        while !dir.path().join(ENDPOINT).exists() {
            assert!(Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let endpoint: Endpoint =
            serde_json::from_slice(&std::fs::read(dir.path().join(ENDPOINT)).unwrap()).unwrap();
        let mut idle = tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, endpoint.port))
            .await
            .unwrap();
        let client = Client {
            endpoint,
            directory: dir.path().to_owned(),
        };
        client
            .request_async(Action::Status, Duration::from_secs(2))
            .await
            .unwrap();
        let reports = client.credential_reports().await.unwrap();
        assert_eq!(
            reports.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            ["Codex", "Claude", "Antigravity", "Grok"]
        );
        assert!(reports.iter().all(|report| report.ok));
        stop.send(()).unwrap();
        server.await.unwrap().unwrap();
        let mut byte = [0];
        assert_eq!(
            tokio::time::timeout(Duration::from_millis(100), idle.read(&mut byte))
                .await
                .expect("A control task survived server shutdown")
                .unwrap(),
            0
        );
    }

    /// A forked child shares the lock's open file description until it execs;
    /// `try_clone` shares it the same way. Closing alone leaves the lock with
    /// that copy, which made a following Restart or migration see it held.
    #[tokio::test]
    async fn clones_of_a_lookup_share_its_one_lookup() {
        let dir = tempfile::tempdir().unwrap();
        let lookup = Lookup::new(dir.path().into());
        let clone = lookup.clone();
        assert!(lookup.get().await.is_none());
        fake_daemon_with(
            dir.path(),
            Arc::new(true.into()),
            |_| {},
            Default::default(),
        );
        assert!(clone.get().await.is_none());
        assert!(Lookup::new(dir.path().into()).get().await.is_some());
    }

    #[test]
    fn a_held_lock_is_released_even_while_a_forked_copy_lives() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.lock");
        let lock = lock_file(&path).unwrap();
        lock.try_lock().unwrap();
        let copy = lock.try_clone().unwrap();
        drop(lock);
        assert!(
            try_hold(&path).unwrap().is_none(),
            "closing alone keeps the lock"
        );
        drop(copy);

        let held = try_hold(&path).unwrap().unwrap();
        let copy = held.0.try_clone().unwrap();
        drop(held);
        assert!(try_hold(&path).unwrap().is_some());
        drop(copy);
    }

    /// A daemon is ready once it records its executable; the helper, a copy
    /// of the whole executable, follows without holding up startup.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_discovery_record_is_ready_with_the_daemon_and_the_helper_follows() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.yaml");
        let log = dir.path().join("proxy.log");
        let port = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        std::fs::write(&config, format!("listen_port: {port}\nrequest_timeout_seconds: 3\nallow_external_access: false\ncodex:\n  homes: []\nclaude:\n  config_dirs: []\n")).unwrap();
        let state = dir.path().to_owned();
        let checked = async {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !state.join(ENDPOINT).exists() {
                assert!(Instant::now() < deadline, "Daemon did not become ready");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            let record = std::fs::read(state.join("summary-executable")).unwrap();
            let running = std::env::current_exe().unwrap();
            assert_eq!(record, format!("{}\n", running.display()).as_bytes());
            let helper = state.join("summary-helper");
            let deadline = Instant::now() + Duration::from_secs(60);
            while !helper.exists() {
                assert!(Instant::now() < deadline, "The helper was never preserved");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            assert_eq!(
                std::fs::metadata(&helper).unwrap().len(),
                std::fs::metadata(&running).unwrap().len()
            );
        };
        serve_until(dir.path(), &config, &log, checked)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn disabled_statistics_errors_do_not_prevent_proxy_start() {
        for broken in ["data-node-id", "data-identity.key", "summary-executable"] {
            // Only Unix stores persistent private identity keys and SSH hints.
            if !cfg!(unix) && broken != "data-node-id" {
                continue;
            }
            let dir = tempfile::tempdir().unwrap();
            let config = dir.path().join("config.yaml");
            let log = dir.path().join("proxy.log");
            let port = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
                .unwrap()
                .local_addr()
                .unwrap()
                .port();
            std::fs::write(&config, format!("listen_port: {port}\nrequest_timeout_seconds: 3\nallow_external_access: false\ncodex:\n  homes: []\nclaude:\n  config_dirs: []\n")).unwrap();
            let path = dir.path().join(broken);
            if broken == "summary-executable" {
                std::fs::create_dir(&path).unwrap();
            } else {
                crate::settings::create_private(&path, b"broken-statistics-file").unwrap();
            }
            let result = serve_until(dir.path(), &config, &log, async {}).await;
            assert!(
                result.is_ok(),
                "{broken} prevented local proxy startup: {result:?}"
            );
            let logged = std::fs::read_to_string(&log).unwrap();
            assert!(logged.contains(if broken == "summary-executable" {
                "statistics_discovery_unavailable"
            } else {
                "statistics_unavailable"
            }));
            if broken != "summary-executable" {
                assert_eq!(std::fs::read(&path).unwrap(), b"broken-statistics-file");
            }
        }
    }

    #[tokio::test]
    async fn explicitly_enabled_statistics_reject_invalid_identity() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.yaml");
        std::fs::write(&config, "listen_port: 8787\nrequest_timeout_seconds: 3\nallow_external_access: true\nexternal_data:\n  port: 8788\n  token_env: UNUSED_TEST_DATA_KEY\n  trusted_lan: [10.0.0.0/8]\ncodex:\n  homes: []\nclaude:\n  config_dirs: []\n").unwrap();
        crate::settings::create_private(&dir.path().join("data-node-id"), b"broken-statistics-id")
            .unwrap();
        let result =
            serve_until(dir.path(), &config, &dir.path().join("proxy.log"), async {}).await;
        assert_eq!(result.unwrap_err().to_string(), "Invalid node identity");
        assert!(!dir.path().join(ENDPOINT).exists());
    }

    /// A control endpoint registered in `dir` that answers status requests
    /// while `up` is set and otherwise drops them, like a daemon that stalls.
    pub(crate) fn fake_daemon(dir: &Path, up: Arc<std::sync::atomic::AtomicBool>) -> u16 {
        fake_daemon_with(dir, up, |_| {}, Default::default())
    }

    /// Like `fake_daemon`, with `edit` applied to its status and every
    /// request's action recorded in `actions`.
    pub(crate) fn fake_daemon_with(
        dir: &Path,
        up: Arc<std::sync::atomic::AtomicBool>,
        edit: impl Fn(&mut Status) + Send + 'static,
        actions: Arc<std::sync::Mutex<Vec<String>>>,
    ) -> u16 {
        fake_daemon_answering(dir, up, edit, |_| None, actions)
    }

    /// Like `fake_daemon_with`, answering an action with `answer`'s response,
    /// in the daemon's JSON form, or else with its status.
    pub(crate) fn fake_daemon_answering(
        dir: &Path,
        up: Arc<std::sync::atomic::AtomicBool>,
        edit: impl Fn(&mut Status) + Send + 'static,
        answer: impl Fn(&serde_json::Value) -> Option<serde_json::Value> + Send + 'static,
        actions: Arc<std::sync::Mutex<Vec<String>>>,
    ) -> u16 {
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let endpoint = Endpoint {
            port,
            token: "fake".into(),
        };
        std::fs::write(dir.join(ENDPOINT), serde_json::to_vec(&endpoint).unwrap()).unwrap();
        let dir = dir.to_owned();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                if !up.load(std::sync::atomic::Ordering::SeqCst) {
                    continue;
                }
                let mut length = [0; 4];
                if stream.read_exact(&mut length).is_err() {
                    continue;
                }
                let mut request = vec![0; u32::from_be_bytes(length) as usize];
                if stream.read_exact(&mut request).is_err() {
                    continue;
                }
                let request =
                    serde_json::from_slice::<serde_json::Value>(&request).unwrap_or_default();
                actions.lock().unwrap().push(request["action"].to_string());
                if let Some(response) = answer(&request["action"]) {
                    let bytes = serde_json::to_vec(&response).unwrap();
                    let _ = stream.write_all(&(bytes.len() as u32).to_be_bytes());
                    let _ = stream.write_all(&bytes);
                    continue;
                }
                let mut status = Status {
                    pid: 1,
                    version: String::new(),
                    forwarding_restore_supported: false,
                    forwarding_restore: false,
                    forwarding_supported: false,
                    forwarding: None,
                    forwarding_traffic_supported: false,
                    proxy_probe_supported: false,
                    metadata_supported: false,
                    credential_checks_supported: false,
                    passive_account_states_supported: false,
                    device_list_supported: false,
                    requests_in_flight: None,
                    port,
                    uptime_ms: 0,
                    proxy_ca: None,
                    allow_external_access: false,
                    data_port: None,
                    config_path: dir.join("config.yaml"),
                    log_path: dir.join("proxy.log"),
                    config_modified: None,
                };
                edit(&mut status);
                let bytes = serde_json::to_vec(&Response::Status(Box::new(status))).unwrap();
                let _ = stream.write_all(&(bytes.len() as u32).to_be_bytes());
                let _ = stream.write_all(&bytes);
            }
        });
        port
    }

    /// Serves `dir` in-process until the returned sender fires or a Stop request arrives.
    pub(crate) fn serve_in_thread(
        dir: &Path,
    ) -> (
        tokio::sync::oneshot::Sender<()>,
        std::thread::JoinHandle<io::Result<()>>,
    ) {
        serve_with_proxies(dir, "")
    }

    pub(crate) fn serve_with_proxies(
        dir: &Path,
        proxies: &str,
    ) -> (
        tokio::sync::oneshot::Sender<()>,
        std::thread::JoinHandle<io::Result<()>>,
    ) {
        let port = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let config = dir.join("config.yaml");
        std::fs::write(
            &config,
            format!(
                "listen_port: {port}\nrequest_timeout_seconds: 30\ncodex:\n  homes: []\nclaude:\n  config_dirs: []\n{proxies}"
            ),
        )
        .unwrap();
        let (signal, terminated) = tokio::sync::oneshot::channel::<()>();
        let dir = dir.to_owned();
        let thread = std::thread::spawn(move || {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap()
                .block_on(serve_until(&dir, &config, &dir.join("proxy.log"), async {
                    let _ = terminated.await;
                }))
        });
        (signal, thread)
    }

    pub(crate) fn wait_for_daemon(dir: &Path) -> (Client, Status) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(found) = Client::discover(dir) {
                return found;
            }
            assert!(Instant::now() < deadline, "Daemon did not become ready");
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    #[test]
    fn idle_control_connections_do_not_delay_status_or_stop() {
        let _guard = spawn_guard();
        let dir = tempfile::tempdir().unwrap();
        let (_signal, daemon) = serve_in_thread(dir.path());
        let (client, status) = wait_for_daemon(dir.path());
        // Each would hold a one-at-a-time control loop for its full read timeout.
        let idle: Vec<_> = (0..3)
            .map(|_| TcpStream::connect((Ipv4Addr::LOCALHOST, client.endpoint.port)).unwrap())
            .collect();
        std::thread::sleep(Duration::from_millis(50));
        for _ in 0..3 {
            assert_eq!(client.status().unwrap().pid, status.pid);
        }
        client.stop().unwrap();
        daemon.join().unwrap().unwrap();
        assert!(Client::discover(dir.path()).is_none());
        std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, status.port)).unwrap();
        drop(idle);
    }

    // The GUI warns from this status; the daemon must keep the trusted CA
    // rather than replace it when the configuration gains hosts.
    #[test]
    fn status_reports_hosts_outside_the_trusted_proxy_ca() {
        let _guard = spawn_guard();
        let dir = tempfile::tempdir().unwrap();
        let tls = dir.path().join("tls");
        coport::local_tls::proxy_acceptors(
            &tls,
            &["upstream.invalid", "platform.claude.com"]
                .map(String::from)
                .into(),
        )
        .unwrap();
        let trusted = std::fs::read(tls.join(coport::local_tls::PROXY_CA_FILE)).unwrap();
        // Continues the `claude` section the helper writes, then enables routing.
        let (_signal, daemon) = serve_with_proxies(
            dir.path(),
            "  base_url: https://upstream.invalid\n  routing:\n    account_fallback: none\n    api_key:\n      'https://182.92.106.196:6060': none\nconnect_credential_routing: true\n",
        );
        let (client, status) = wait_for_daemon(dir.path());
        assert_eq!(
            status.proxy_ca,
            Some(ProxyCa {
                decrypted: vec![
                    "platform.claude.com:443".into(),
                    "upstream.invalid:443".into()
                ],
                uncovered: vec!["182.92.106.196:6060".into()],
                unrestricted: false,
            })
        );
        client.stop().unwrap();
        daemon.join().unwrap().unwrap();
        assert_eq!(
            std::fs::read(tls.join(coport::local_tls::PROXY_CA_FILE)).unwrap(),
            trusted
        );
        // Statuses from daemons predating the field still parse.
        let mut older = serde_json::to_value(&status).unwrap();
        older.as_object_mut().unwrap().remove("proxy_ca");
        assert!(
            serde_json::from_value::<Status>(older)
                .unwrap()
                .proxy_ca
                .is_none()
        );
    }

    #[test]
    fn device_query_audit_requires_local_auth_and_accepts_only_bounded_metadata() {
        let _guard = spawn_guard();
        let dir = tempfile::tempdir().unwrap();
        let (_signal, daemon) = serve_in_thread(dir.path());
        let (client, status) = wait_for_daemon(dir.path());
        let event = crate::device_events::QueryEvent {
            device_id: uuid::Uuid::new_v4().to_string(),
            transport: crate::device_events::Protocol::Ssh,
            outcome: crate::device_events::Outcome::Succeeded,
            duration_ms: 42,
            query_count: 1,
        };
        let mut unauthorized = client.clone();
        unauthorized.endpoint.token = "invalid".into();
        assert!(unauthorized.record_device_query(event.clone()).is_err());
        let mut invalid = event.clone();
        invalid.device_id = "https://private.example".into();
        assert!(client.record_device_query(invalid).is_err());
        client.record_device_query(event.clone()).unwrap();
        client.stop().unwrap();
        daemon.join().unwrap().unwrap();
        let raw = std::fs::read_to_string(status.log_path).unwrap();
        let events: Vec<serde_json::Value> = raw
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .filter(|v: &serde_json::Value| v["event"] == "device_statistics_succeeded")
            .collect();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["device_id"], event.device_id);
        assert!(!raw.contains("private.example"));
    }

    #[test]
    fn unreachable_daemon_is_stopped_only_after_its_lock_is_released() {
        let _guard = spawn_guard();
        let dir = tempfile::tempdir().unwrap();
        let lock = lock_file(&dir.path().join("daemon.lock")).unwrap();
        lock.try_lock().unwrap();
        // Reserve a port without listening so control requests cannot succeed.
        let socket = tokio::net::TcpSocket::new_v4().unwrap();
        socket.bind((Ipv4Addr::LOCALHOST, 0).into()).unwrap();
        let client = Client {
            endpoint: Endpoint {
                port: socket.local_addr().unwrap().port(),
                token: "unreachable".into(),
            },
            directory: dir.path().to_owned(),
        };
        assert!(client.stop().is_err());
        drop(lock);
        client.stop().unwrap();
    }

    #[test]
    fn daemon_lock_excludes_another_handle_until_released() {
        let _guard = spawn_guard();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.lock");
        let first = lock_file(&path).unwrap();
        first.try_lock().unwrap();
        let second = lock_file(&path).unwrap();
        assert!(matches!(
            second.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
        drop(first);
        second.try_lock().unwrap();
    }
}
