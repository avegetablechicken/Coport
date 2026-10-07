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
    pub port: u16,
    pub uptime_ms: u64,
    pub config_path: PathBuf,
    pub log_path: PathBuf,
    pub config_modified: Option<SystemTime>,
}

#[derive(Serialize, Deserialize)]
enum Action {
    Status,
    Stop,
}

#[derive(Serialize, Deserialize)]
struct Request {
    token: String,
    action: Action,
}

#[derive(Serialize, Deserialize)]
enum Response {
    Status(Status),
    Stopped,
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
            Response::Status(status) => Ok(status),
            _ => Err(io::Error::other("Unexpected daemon status response")),
        }
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
    stops: tokio::sync::mpsc::UnboundedSender<tokio::net::TcpStream>,
) {
    let request = tokio::time::timeout(Duration::from_millis(500), read_request(&mut stream)).await;
    let Ok(Ok(request)) = request else {
        return;
    };
    let response = if request.token != *token {
        Response::Error("Unauthorized".into())
    } else {
        match request.action {
            Action::Status => Response::Status(Status {
                uptime_ms: started.elapsed().as_millis() as u64,
                ..Status::clone(&status)
            }),
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
    let proxy = TcpListener::bind((Ipv4Addr::LOCALHOST, config.listen_port)).await?;
    let port = proxy.local_addr()?.port();
    let control = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let endpoint = Endpoint {
        port: control.local_addr()?.port(),
        token: uuid::Uuid::new_v4().to_string(),
    };
    let logger = Arc::new(Logger::new(log_path.clone()));
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
    let mut serving = tokio::spawn(async move {
        server
            .serve(proxy, async {
                let _ = stop.await;
            })
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
        port,
        uptime_ms: 0,
        config_path,
        log_path,
        config_modified,
    });
    let (stops, mut stop_requests) = tokio::sync::mpsc::unbounded_channel();
    tokio::pin!(signal);
    let mut stop_client = None;
    let result = loop {
        tokio::select! {
            _ = &mut signal => break Ok(()),
            result = &mut serving => {
                // Do not poll the completed JoinHandle again during cleanup.
                startup.abort();
                logger.write("server_stopped", Default::default());
                return result.map_err(io::Error::other)?;
            }
            Some(stream) = stop_requests.recv() => { stop_client = Some(stream); break Ok(()); }
            accepted = control.accept() => {
                let (stream, _) = match accepted { Ok(pair) => pair, Err(e) => break Err(e) };
                tokio::spawn(handle_control(stream, token.clone(), status.clone(), started, stops.clone()));
            }
        }
    };
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
                let bytes = serde_json::to_vec(&Response::Status(Status {
                    pid: 1,
                    port,
                    uptime_ms: 0,
                    config_path: dir.join("config.yaml"),
                    log_path: dir.join("proxy.log"),
                    config_modified: None,
                }))
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
        let port = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let config = dir.join("config.yaml");
        std::fs::write(
            &config,
            format!(
                "listen_port: {port}\nrequest_timeout_seconds: 30\ncodex:\n  homes: []\nclaude:\n  config_dirs: []\n"
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
