//! A detached, GUI-free proxy process. Discovery is per-user; control requests
//! use a random capability stored in an owner-only file, never on the proxy port.

use coport::{config::Config, logger::Logger, server::Server};
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    io::{self, Read, Write},
    net::{Ipv4Addr, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

const ENDPOINT: &str = "daemon.json";
const MAX_MESSAGE: usize = 64 * 1024;
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
    pub proxy_probe_supported: bool,
    #[serde(default)]
    pub metadata_supported: bool,
    pub port: u16,
    #[serde(default)]
    pub allow_external_access: bool,
    #[serde(default)]
    pub data_port: Option<u16>,
    pub uptime_ms: u64,
    pub config_path: PathBuf,
    pub log_path: PathBuf,
    pub config_modified: Option<SystemTime>,
}

#[derive(Serialize, Deserialize)]
enum Action {
    Status,
    Stop,
    Probe {
        name: String,
    },
    AccountStates,
    CredentialLabels,
    CachedCredentialLabels,
    SetForwarding {
        target: Option<crate::remote_forward::Target>,
    },
    SetForwardRestore {
        enabled: bool,
    },
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
    AccountStates([std::collections::BTreeMap<String, String>; 2]),
    CredentialLabels(Vec<((String, String), String)>),
    Recorded,
    ForwardingSet,
    Error(String),
}

#[derive(Clone)]
pub struct Client {
    endpoint: Endpoint,
    directory: PathBuf,
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

    pub async fn account_states(
        &self,
    ) -> io::Result<[std::collections::BTreeMap<String, String>; 2]> {
        match self
            .request_async(Action::AccountStates, Duration::from_secs(60))
            .await?
        {
            Response::AccountStates(states) => Ok(states),
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
            if length > MAX_MESSAGE {
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
        lock_file(&self.directory.join("daemon.lock")).is_ok_and(|lock| lock.try_lock().is_ok())
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
        if length > MAX_MESSAGE {
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
        let lock = lock_file(&dir.join("daemon.lock"))?;
        match lock.try_lock() {
            Ok(()) => Ok(None),
            Err(std::fs::TryLockError::WouldBlock) => Err(io::Error::other(
                "Proxy daemon is running but its control channel is unavailable.",
            )),
            Err(std::fs::TryLockError::Error(error)) => Err(error),
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

fn private_file(path: &Path) -> io::Result<File> {
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

/// Detaches at spawn, not at GUI exit. No pipes or runtime tasks owned by the
/// GUI are required to keep the listener or in-flight requests alive.
pub fn start(binary: &Path, dir: &Path, config: &Path, log: &Path) -> io::Result<(Client, Status)> {
    std::fs::create_dir_all(dir)?;
    let stderr = private_file(&dir.join("daemon.stderr.log"))?;
    let mut command = Command::new(binary);
    command
        .arg(dir)
        .arg(config)
        .arg(log)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid is async-signal-safe and does not access Rust state.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0000_0008 | 0x0000_0200); // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP
    }
    let mut child = command.spawn().map_err(|e| {
        io::Error::new(
            e.kind(),
            format!(
                "Cannot start {}: {e}. Keep the daemon executable beside the GUI.",
                binary.display()
            ),
        )
    })?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(code) = child.try_wait()? {
            let path = dir.join("daemon.stderr.log");
            // The log is appended across starts; its last line is this exit's reason.
            let reason = std::fs::read_to_string(&path)
                .ok()
                .and_then(|text| {
                    text.lines()
                        .rev()
                        .find(|l| !l.trim().is_empty())
                        .map(str::to_owned)
                })
                .map(|line| format!(": {line}"))
                .unwrap_or_default();
            return Err(io::Error::other(format!(
                "Proxy daemon exited ({code}){reason}; see {}",
                path.display()
            )));
        }
        if let Some((client, status)) = Client::discover(dir)
            && status.pid == child.id()
        {
            // Reap while the GUI is alive; dropping this thread on GUI exit has
            // no effect on the independently running child.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            return Ok((client, status));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Proxy daemon did not become ready",
            ));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

struct Registration {
    path: PathBuf,
    _lock: File,
}
impl Drop for Registration {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
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
                ..Status::clone(&status)
            })),
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
            Action::AccountStates => Response::AccountStates(server.account_route_states().await.map(|states| states.into_iter().map(|(key, value)| (key, value.to_owned())).collect())),
            Action::CachedCredentialLabels => Response::CredentialLabels(server.cached_traffic_credential_labels().await.into_iter().collect()),
            Action::CredentialLabels => Response::CredentialLabels(server.traffic_credential_labels().await.into_iter().collect()),
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
        _lock: lock,
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
    if let Err(error) = std::env::current_exe()
        .and_then(|binary| crate::remote::register_summary_executable(dir, &binary))
    {
        logger.write(
            "statistics_discovery_unavailable",
            [("reason".to_owned(), error.to_string().into())]
                .into_iter()
                .collect(),
        );
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
    let mut server = Server::new(config, logger.clone());
    // Without TLS, plain-HTTP clients still work; HTTPS base URLs fail to connect.
    match coport::local_tls::acceptor(&coport::local_tls::dir_for(&config_path)) {
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
    // Atomic, owner-only discovery file. Publish only after both listeners bind.
    let mut temp = tempfile::NamedTempFile::new_in(dir)?;
    temp.write_all(&serde_json::to_vec(&endpoint)?)?;
    temp.as_file().sync_all()?;
    temp.persist(&registration.path).map_err(|e| e.error)?;

    let token: Arc<str> = endpoint.token.into();
    let status = Arc::new(Status {
        pid: std::process::id(),
        version: env!("CARGO_PKG_VERSION").into(),
        forwarding_restore_supported: true,
        forwarding_restore: false,
        forwarding_supported: true,
        forwarding: None,
        proxy_probe_supported: true,
        metadata_supported: true,
        port,
        uptime_ms: 0,
        allow_external_access,
        data_port,
        config_path,
        log_path,
        config_modified,
    });
    let (stops, mut stop_requests) = tokio::sync::mpsc::unbounded_channel();
    tokio::pin!(signal);
    let mut controls = tokio::task::JoinSet::new();
    let monitor = routing.monitor();
    tokio::pin!(monitor);
    let mut stop_client = None;
    let result = loop {
        tokio::select! {
            _ = &mut signal => break Ok(()),
            Some(_) = controls.join_next(), if !controls.is_empty() => {},
            _ = &mut monitor => break Err(io::Error::other("Forwarding monitor stopped unexpectedly")),
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

/// A child forked by `start` shares every open file until it execs, including
/// lock handles a parallel test has just dropped. Tests that spawn a daemon or
/// expect a released lock to be free at once hold this guard.
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
                let bytes = serde_json::to_vec(&Response::Status(Box::new(Status {
                    pid: 1,
                    version: String::new(),
                    forwarding_restore_supported: false,
                    forwarding_restore: false,
                    forwarding_supported: false,
                    forwarding: None,
                    proxy_probe_supported: false,
                    metadata_supported: false,
                    port,
                    uptime_ms: 0,
                    allow_external_access: false,
                    data_port: None,
                    config_path: dir.join("config.yaml"),
                    log_path: dir.join("proxy.log"),
                    config_modified: None,
                })))
                .unwrap();
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
