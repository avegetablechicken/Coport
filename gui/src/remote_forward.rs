//! Explicit, session-scoped access to a remote daemon's loopback proxy over SSH.
//! No remote credentials or configuration are copied to the local machine.
use crate::remote::{Device, helper_command};
use serde::{Deserialize, Serialize};
use std::{
    io,
    net::Ipv4Addr,
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
    process::{Child, Command},
};

#[path = "forwarding_status.rs"]
mod telemetry;
use telemetry::{Active, CountWriter, Counters};
pub use telemetry::{Health, RemoteTraffic, Traffic};

const READY: &[u8] = b"COPORT-FORWARD/1\n";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(25);

/// The destination helper resolves only the running daemon's proxy, never its
/// control socket or an arbitrary host/port supplied by the SSH client.
pub async fn serve_stdio(dir: &Path) -> io::Result<()> {
    let dir = dir.to_owned();
    let (_, status) = tokio::task::spawn_blocking(move || crate::daemon::Client::discover(&dir))
        .await
        .map_err(io::Error::other)?
        .ok_or_else(|| {
            io::Error::other("Start Coport on the destination before forwarding requests.")
        })?;
    if status.forwarding.is_some() {
        return Err(io::Error::other(
            "The destination already forwards to another device; chained forwarding is not supported.",
        ));
    }
    let socket = tokio::time::timeout(
        Duration::from_secs(5),
        TcpStream::connect((Ipv4Addr::LOCALHOST, status.port)),
    )
    .await
    .map_err(io::Error::other)??;
    let mut output = tokio::io::stdout();
    output.write_all(READY).await?;
    output.flush().await?;
    let (reader, writer) = socket.into_split();
    relay(tokio::io::stdin(), output, reader, writer).await
}

async fn relay<A, B, C, D>(mut a: A, mut b: B, mut c: C, mut d: D) -> io::Result<()>
where
    A: AsyncRead + Unpin,
    B: AsyncWrite + Unpin,
    C: AsyncRead + Unpin,
    D: AsyncWrite + Unpin,
{
    let upload = async move {
        tokio::io::copy(&mut a, &mut d).await?;
        d.shutdown().await?;
        // A child's stdin pipe ignores shutdown and only closes when dropped.
        drop(d);
        io::Result::Ok(())
    };
    let download = async {
        tokio::io::copy(&mut c, &mut b).await?;
        b.shutdown().await
    };
    tokio::pin!(upload, download);
    tokio::select! {
        result = &mut upload => { result?; download.await },
        result = &mut download => result,
    }
}

struct Diagnostics(tokio::task::JoinHandle<Vec<u8>>);
impl Drop for Diagnostics {
    fn drop(&mut self) {
        self.0.abort();
    }
}
struct Channel {
    child: Child,
    input: tokio::process::ChildStdin,
    output: tokio::process::ChildStdout,
    _diagnostics: Diagnostics,
}
impl Channel {
    async fn open(device: &Device, executable: &Path) -> Result<Self, String> {
        device.validate()?;
        let mut child = Command::new(executable)
            .args([
                "-T",
                "-a",
                "-x",
                "-o",
                "ClearAllForwardings=yes",
                "-o",
                "PermitLocalCommand=no",
                "-o",
                "RemoteCommand=none",
                "-o",
                "BatchMode=yes",
                "-o",
                "StrictHostKeyChecking=yes",
                "-o",
                "ConnectTimeout=5",
                "-o",
                "ServerAliveInterval=5",
                "-o",
                "ServerAliveCountMax=1",
                "--",
                &device.host,
                &helper_command(&device.binary, "--forward"),
            ])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            // Diagnostics are bounded and never enter the request stream.
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("Cannot start SSH: {e}"))?;
        let mut stderr = child.stderr.take().unwrap();
        let mut diagnostics = Diagnostics(tokio::spawn(async move {
            let mut saved = Vec::new();
            let mut buffer = [0; 4096];
            while let Ok(size) = stderr.read(&mut buffer).await {
                if size == 0 {
                    break;
                }
                let keep = size.min(4096usize.saturating_sub(saved.len()));
                saved.extend_from_slice(&buffer[..keep]);
            }
            saved
        }));
        let input = child.stdin.take().unwrap();
        let mut output = child.stdout.take().unwrap();
        let mut ready = [0; READY.len()];
        let result = tokio::time::timeout(CONNECT_TIMEOUT, output.read_exact(&mut ready)).await;
        if !matches!(result, Ok(Ok(_))) || ready != READY {
            let _ = child.kill().await;
            let reason =
                match tokio::time::timeout(Duration::from_secs(1), &mut diagnostics.0).await {
                    Ok(Ok(bytes)) => String::from_utf8_lossy(&bytes)
                        .chars()
                        .filter(|c| !c.is_control() || c.is_whitespace())
                        .take(1024)
                        .collect::<String>()
                        .trim()
                        .to_owned(),
                    _ => String::new(),
                };
            let message = if result.is_err() {
                "SSH forwarding connection timed out."
            } else {
                "Cannot open remote proxy. Start or update Coport on the destination; summary-only SSH keys cannot forward requests."
            };
            return Err(if reason.is_empty() {
                message.into()
            } else {
                format!("{message} {reason}")
            });
        }
        Ok(Self {
            child,
            input,
            output,
            _diagnostics: diagnostics,
        })
    }
    async fn transfer(
        self,
        socket: coport::server::Connection,
        counters: &Counters,
    ) -> io::Result<()> {
        let Self {
            mut child,
            input,
            mut output,
            _diagnostics,
        } = self;
        let (reader, writer) = tokio::io::split(socket);
        // The relay owns stdin so a client half-close reaches the remote helper.
        let result = relay(
            reader,
            CountWriter(writer, counters.download.clone()),
            &mut output,
            CountWriter(input, counters.upload.clone()),
        )
        .await;
        // A remote helper must not outlive its local request, even on failure.
        let _ = child.kill().await;
        result
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Target {
    pub device_id: String,
    pub name: String,
    pub connection: Device,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub device_id: String,
    pub name: String,
    pub error: Option<String>,
    #[serde(default)]
    pub health: Health,
    #[serde(default)]
    pub traffic: Traffic,
    #[serde(default)]
    pub capabilities: Option<crate::remote::Capabilities>,
    #[serde(default)]
    pub capabilities_error: Option<String>,
    #[serde(default)]
    pub remote_traffic: Option<RemoteTraffic>,
    #[serde(default)]
    pub remote_traffic_error: Option<String>,
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Preferences {
    restore: bool,
    target: Option<Target>,
}
struct Mode {
    target: Option<Target>,
    health: Mutex<Health>,
    sequence: AtomicU64,
    counters: Arc<Counters>,
    metadata: Mutex<Metadata>,
}
#[derive(Default)]
struct Metadata {
    capabilities: Option<crate::remote::Capabilities>,
    capabilities_error: Option<String>,
    traffic: Option<RemoteTraffic>,
    traffic_error: Option<String>,
}
impl Mode {
    fn new(target: Option<Target>) -> Self {
        Self {
            target,
            health: Mutex::new(Health::checking()),
            sequence: 0.into(),
            counters: Arc::new(Counters::default()),
            metadata: Mutex::new(Metadata::default()),
        }
    }
    fn next(&self) -> u64 {
        self.sequence.fetch_add(1, Ordering::Relaxed) + 1
    }
    fn observe(&self, sequence: u64, result: Result<u64, String>) {
        self.health.lock().unwrap().observe(sequence, result);
    }
}
/// Owned by coportd. A failed remote connection never changes the selected mode.
pub struct Routing {
    mode: tokio::sync::watch::Sender<Arc<Mode>>,
    updates: tokio::sync::Mutex<()>,
    executable: std::path::PathBuf,
    preferences: Mutex<Preferences>,
    store: Option<std::path::PathBuf>,
    pub device_checks: Arc<crate::device_capabilities::Checks>,
}
impl Default for Routing {
    fn default() -> Self {
        Self {
            mode: tokio::sync::watch::channel(Arc::new(Mode::new(None))).0,
            updates: Default::default(),
            executable: "ssh".into(),
            preferences: Mutex::new(Preferences::default()),
            store: None,
            device_checks: Arc::new(Default::default()),
        }
    }
}
impl Routing {
    pub fn with_store(dir: &Path) -> io::Result<Self> {
        let path = dir.join("forwarding.json");
        let bytes = (|| -> io::Result<Vec<u8>> {
            use std::io::Read;
            let file = std::fs::File::open(&path)?;
            let mut bytes = Vec::new();
            file.take(65537).read_to_end(&mut bytes)?;
            Ok(bytes)
        })();
        let preferences: Preferences = match bytes {
            Ok(bytes) if bytes.len() <= 65536 => serde_json::from_slice(&bytes).map_err(|_| io::Error::other("Invalid forwarding preferences; repair forwarding.json before starting the proxy."))?,
            Ok(_) => return Err(io::Error::other("Forwarding preferences exceed the size limit")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Preferences::default(),
            Err(error) => return Err(error),
        };
        if let Some(target) = &preferences.target {
            validate_target(target).map_err(io::Error::other)?;
        }
        let routing = Self {
            store: Some(path),
            device_checks: Arc::new(crate::device_capabilities::Checks::new(dir)),
            ..Self::default()
        };
        if preferences.restore {
            routing
                .mode
                .send_replace(Arc::new(Mode::new(preferences.target.clone())));
        }
        *routing.preferences.lock().unwrap() = preferences;
        Ok(routing)
    }
    pub fn stored_restore(dir: &Path) -> Result<bool, String> {
        Self::with_store(dir)
            .map(|routing| routing.restore_enabled())
            .map_err(|e| e.to_string())
    }
    pub fn configure_stopped(
        dir: &Path,
        restore: Option<bool>,
        clear_device: Option<&str>,
    ) -> Result<(), String> {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        let lock = crate::daemon::lock_file(&dir.join("daemon.lock")).map_err(|e| e.to_string())?;
        lock.try_lock().map_err(|_| {
            "The daemon is starting or unavailable; retry when it responds.".to_owned()
        })?;
        let routing = Self::with_store(dir).map_err(|e| e.to_string())?;
        let mut preferences = routing.preferences.lock().unwrap().clone();
        let mut changed = false;
        if let Some(restore) = restore {
            preferences.restore = restore;
            changed = true;
        }
        if clear_device.is_some_and(|id| {
            preferences
                .target
                .as_ref()
                .is_some_and(|target| target.device_id == id)
        }) {
            preferences.target = None;
            changed = true;
        }
        if changed {
            routing.save(preferences)?;
        }
        Ok(())
    }
    fn save(&self, preferences: Preferences) -> Result<(), String> {
        if let Some(path) = &self.store {
            crate::settings::write_private(
                path,
                &serde_json::to_vec_pretty(&preferences).map_err(|e| e.to_string())?,
            )
            .map_err(|e| e.to_string())?;
        }
        *self.preferences.lock().unwrap() = preferences;
        Ok(())
    }
    pub fn restore_enabled(&self) -> bool {
        self.preferences.lock().unwrap().restore
    }
    pub async fn set_restore(&self, enabled: bool) -> Result<(), String> {
        let _guard = self.updates.lock().await;
        self.save(Preferences {
            restore: enabled,
            target: self.mode.borrow().target.clone(),
        })
    }
    pub fn status(&self) -> Option<Status> {
        let mode = self.mode.borrow();
        mode.target.as_ref().map(|target| {
            let health = mode.health.lock().unwrap().clone();
            let metadata = mode.metadata.lock().unwrap();
            Status {
                device_id: target.device_id.clone(),
                name: target.name.clone(),
                error: health.error.clone(),
                health,
                traffic: mode.counters.snapshot(),
                capabilities: metadata.capabilities.clone(),
                capabilities_error: metadata.capabilities_error.clone(),
                remote_traffic: metadata.traffic.clone(),
                remote_traffic_error: metadata.traffic_error.clone(),
            }
        })
    }
    pub async fn set(&self, target: Option<Target>) -> Result<(), String> {
        let _guard = self.updates.lock().await;
        let mode = Arc::new(Mode::new(target.clone()));
        if let Some(target) = &target {
            validate_target(target)?;
            let started = Instant::now();
            let mut probe = Channel::open(&target.connection, &self.executable).await?;
            mode.observe(mode.next(), Ok(started.elapsed().as_millis() as u64));
            let _ = probe.child.kill().await;
        }
        self.save(Preferences {
            restore: self.restore_enabled(),
            target,
        })?;
        self.mode.send_replace(mode);
        Ok(())
    }
    async fn check(&self, mode: &Arc<Mode>) {
        if let Some(target) = &mode.target {
            let sequence = mode.next();
            let started = Instant::now();
            match Channel::open(&target.connection, &self.executable).await {
                Ok(mut channel) => {
                    mode.observe(sequence, Ok(started.elapsed().as_millis() as u64));
                    let _ = channel.child.kill().await;
                }
                Err(error) => mode.observe(sequence, Err(error)),
            }
        }
    }
    /// One task per daemon, cancelled on shutdown. Mode changes discard all late
    /// health/metadata results, and no retry ever changes the routing destination.
    pub async fn monitor(&self) {
        let mut changes = self.mode.subscribe();
        loop {
            let mode = changes.borrow_and_update().clone();
            let work = async {
                let Some(target) = &mode.target else {
                    std::future::pending::<()>().await;
                    return;
                };
                let health = async {
                    loop {
                        self.check(&mode).await;
                        tokio::time::sleep(Duration::from_secs(15)).await;
                    }
                };
                let metadata = async {
                    loop {
                        let (capabilities, summary) = tokio::join!(
                            crate::remote::capabilities(&target.connection),
                            crate::remote::summary(&target.connection)
                        );
                        {
                            let mut metadata = mode.metadata.lock().unwrap();
                            match capabilities {
                                Ok(value) => {
                                    metadata.capabilities = Some(value);
                                    metadata.capabilities_error = None;
                                }
                                Err(error) => metadata.capabilities_error = Some(error),
                            }
                            match summary.and_then(RemoteTraffic::from_summary) {
                                Ok(value) => {
                                    metadata.traffic = Some(value);
                                    metadata.traffic_error = None;
                                }
                                Err(error) => metadata.traffic_error = Some(error),
                            }
                        }
                        tokio::time::sleep(Duration::from_secs(30)).await;
                    }
                };
                tokio::join!(health, metadata);
            };
            tokio::select! { _ = changes.changed() => {}, _ = work => {} }
        }
    }
    pub fn handler(self: &Arc<Self>) -> coport::server::ConnectionHandler {
        let routing = self.clone();
        Arc::new(move |server, socket| {
            let routing = routing.clone();
            // Subscribe before spawning so a mode change also cancels queued work.
            let mut changes = routing.mode.subscribe();
            let mode = changes.borrow_and_update().clone();
            Box::pin(async move {
                tokio::select! {
                    biased;
                    _ = changes.changed() => {},
                    _ = async {
                        if let Some(target) = &mode.target {
                            let mut socket = socket;
                            let _active = Active::new(mode.counters.clone());
                            let sequence = mode.next(); let started = Instant::now();
                            match Channel::open(&target.connection, &routing.executable).await {
                                Ok(channel) => {
                                    mode.observe(sequence, Ok(started.elapsed().as_millis() as u64));
                                    if channel.transfer(socket, &mode.counters).await.is_err() {
                                        // A local client abort is not proof that the remote is offline.
                                        mode.counters.failures.fetch_add(1,Ordering::Relaxed);
                                    }
                                }
                                Err(error) => {
                                    mode.counters.failures.fetch_add(1,Ordering::Relaxed);
                                    mode.observe(sequence, Err(error));
                                    // A completed TLS handshake does not mean the HTTP client has
                                    // sent a request yet. An unsolicited response can be rejected
                                    // by its HTTP state machine (hyper UnexpectedMessage).
                                    if !matches!(tokio::time::timeout(Duration::from_secs(30),
                                        read_rejected_request_head(&mut socket)).await, Ok(Ok(()))) {
                                        return;
                                    }
                                    // Reject after headers, without waiting for a body or routing locally.
                                    let response = b"HTTP/1.1 502 Bad Gateway\r\nConnection: close\r\nContent-Length: 30\r\n\r\nRemote SSH proxy unavailable.\n";
                                    let _ = socket.write_all(response).await;
                                    let _ = socket.shutdown().await;
                                    // Drain a bounded amount of the rejected request so an unread
                                    // body cannot turn the 502 into a TCP reset on macOS/Windows.
                                    let _ = tokio::time::timeout(Duration::from_millis(250),
                                        tokio::io::copy(&mut socket.take(32 * 1024 * 1024 + 65536), &mut tokio::io::sink())).await;
                                }
                            }
                        } else {
                            server.connection(socket).await;
                        }
                    } => {},
                }
            })
        })
    }
}

async fn read_rejected_request_head(socket: &mut (impl AsyncRead + Unpin)) -> io::Result<()> {
    let mut head = Vec::new();
    while head.len() < 65536 {
        let mut chunk = [0; 4096];
        let remaining = (65536 - head.len()).min(chunk.len());
        let count = socket.read(&mut chunk[..remaining]).await?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Incomplete request headers",
            ));
        }
        let boundary = head.len().saturating_sub(3);
        head.extend_from_slice(&chunk[..count]);
        if head[boundary..]
            .windows(4)
            .any(|bytes| bytes == b"\r\n\r\n")
        {
            return Ok(());
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "Request headers exceed 64 KiB",
    ))
}

fn validate_target(target: &Target) -> Result<(), String> {
    if target.device_id.is_empty()
        || target.device_id.len() > 128
        || target.name.is_empty()
        || target.name.len() > 128
    {
        return Err("Invalid forwarding device".into());
    }
    target.connection.validate()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn relay_preserves_binary_streams_and_half_close() {
        let (mut client, local) = tokio::io::duplex(64);
        let (remote, mut server) = tokio::io::duplex(64);
        let (a, b) = tokio::io::split(local);
        let (c, d) = tokio::io::split(remote);
        let task = tokio::spawn(relay(a, b, c, d));
        let payload = [0, 255, 13, 10, 0, 17];
        client.write_all(&payload).await.unwrap();
        client.shutdown().await.unwrap();
        let mut request = Vec::new();
        server.read_to_end(&mut request).await.unwrap();
        assert_eq!(request, payload);
        // A response after client EOF must still reach the client.
        server
            .write_all(b"data: first\n\ndata: second\n\n")
            .await
            .unwrap();
        server.shutdown().await.unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        assert_eq!(response, b"data: first\n\ndata: second\n\n");
        task.await.unwrap().unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn relay_closes_child_stdin_after_client_half_close() {
        let mut child = tokio::process::Command::new("cat")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let output = child.stdout.take().unwrap();
        let (mut client, local) = tokio::io::duplex(64);
        let (a, b) = tokio::io::split(local);
        let task = tokio::spawn(relay(a, b, output, input));
        client.write_all(b"request").await.unwrap();
        client.shutdown().await.unwrap();
        // `cat` only exits, ending the download, once its stdin reaches EOF.
        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(10), client.read_to_end(&mut response))
            .await
            .expect("the remote helper never saw the client's EOF")
            .unwrap();
        assert_eq!(response, b"request");
        task.await.unwrap().unwrap();
    }

    #[cfg(unix)]
    fn fake_ssh(dir: &Path, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let executable = dir.join("ssh");
        std::fs::write(&executable, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        executable
    }
    #[cfg(unix)]
    fn device() -> Device {
        Device {
            name: "test".into(),
            host: "test-host".into(),
            binary: "coportd".into(),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unavailable_ssh_waits_for_request_headers_before_replying() {
        let dir = tempfile::tempdir().unwrap();
        let routing = Arc::new(Routing {
            executable: fake_ssh(dir.path(), "exit 1"),
            ..Default::default()
        });
        routing.mode.send_replace(Arc::new(Mode::new(Some(Target {
            device_id: "offline".into(),
            name: "Offline".into(),
            connection: device(),
        }))));
        let config = coport::config::Config::parse("listen_port: 8787\nrequest_timeout_seconds: 30\ncodex:\n  homes: []\nclaude:\n  config_dirs: []\n").unwrap();
        let server = Arc::new(coport::server::Server::new(
            config,
            Arc::new(coport::logger::Logger::new(dir.path().join("test.log"))),
        ));
        let (mut client, socket) = tokio::io::duplex(1024);
        let task = tokio::spawn(routing.handler()(server, Box::new(socket)));
        tokio::time::timeout(Duration::from_secs(5), async {
            while routing.status().unwrap().traffic.failures == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let mut byte = [0];
        assert!(
            tokio::time::timeout(Duration::from_millis(100), client.read(&mut byte))
                .await
                .is_err(),
            "Server replied before the client sent its request"
        );
        client
            .write_all(b"POST / HTTP/1.1\r\nHost: test\r\nContent-Length: 100\r\n")
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(30), client.read(&mut byte))
                .await
                .is_err(),
            "Server replied before request headers completed"
        );
        client.write_all(b"\r\n").await.unwrap();
        // An early rejection must not wait for the request body (e.g. Expect: 100-continue).
        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), client.read_to_end(&mut response))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(response, b"HTTP/1.1 502 Bad Gateway\r\nConnection: close\r\nContent-Length: 30\r\n\r\nRemote SSH proxy unavailable.\n");
        drop(client);
        task.await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_preflight_preserves_the_previous_mode() {
        let dir = tempfile::tempdir().unwrap();
        let executable = fake_ssh(dir.path(), "printf 'COPORT-FORWARD/1\\n'; exec cat");
        let routing = Routing {
            executable,
            ..Default::default()
        };
        let target = Target {
            device_id: "device".into(),
            name: "Remote".into(),
            connection: device(),
        };
        routing.set(Some(target.clone())).await.unwrap();
        assert_eq!(routing.status().unwrap().device_id, "device");
        fake_ssh(dir.path(), "exit 1");
        assert!(routing.set(Some(target)).await.is_err());
        assert_eq!(routing.status().unwrap().device_id, "device");
        routing.set(None).await.unwrap();
        assert!(routing.status().is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn mode_switch_closes_an_active_remote_stream() {
        let dir = tempfile::tempdir().unwrap();
        let executable = fake_ssh(dir.path(), "printf 'COPORT-FORWARD/1\\n'; exec cat");
        let routing = Arc::new(Routing {
            executable,
            ..Default::default()
        });
        routing
            .set(Some(Target {
                device_id: "remote".into(),
                name: "Remote".into(),
                connection: device(),
            }))
            .await
            .unwrap();
        let config = coport::config::Config::parse("listen_port: 8787\nrequest_timeout_seconds: 30\ncodex:\n  homes: []\nclaude:\n  config_dirs: []\n").unwrap();
        let server = Arc::new(coport::server::Server::new(
            config,
            Arc::new(coport::logger::Logger::new(dir.path().join("test.log"))),
        ));
        let (mut client, socket) = tokio::io::duplex(128);
        let connection = tokio::spawn(routing.handler()(server, Box::new(socket)));
        client.write_all(b"active").await.unwrap();
        let mut response = [0; 6];
        tokio::time::timeout(Duration::from_secs(5), client.read_exact(&mut response))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&response, b"active");
        routing.set(None).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), connection)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(client.read(&mut response).await.unwrap(), 0);
    }

    #[test]
    fn restore_loads_offline_target_and_invalid_preferences_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let preferences = Preferences {
            restore: true,
            target: Some(Target {
                device_id: "offline".into(),
                name: "Offline".into(),
                connection: Device {
                    name: "offline".into(),
                    host: "offline-host".into(),
                    binary: "coportd".into(),
                },
            }),
        };
        std::fs::write(
            dir.path().join("forwarding.json"),
            serde_json::to_vec(&preferences).unwrap(),
        )
        .unwrap();
        let routing = Routing::with_store(dir.path()).unwrap();
        assert!(routing.restore_enabled());
        assert_eq!(routing.status().unwrap().device_id, "offline");
        assert_eq!(routing.status().unwrap().health.state, "checking");
        std::fs::write(dir.path().join("forwarding.json"), b"invalid").unwrap();
        assert!(Routing::with_store(dir.path()).is_err());
    }
    #[test]
    fn stopped_preferences_preserve_selection_and_clear_removed_devices_under_lock() {
        let _guard = crate::daemon::spawn_guard();
        let dir = tempfile::tempdir().unwrap();
        let preferences = Preferences {
            restore: true,
            target: Some(Target {
                device_id: "selected".into(),
                name: "Selected".into(),
                connection: Device {
                    name: "selected".into(),
                    host: "host".into(),
                    binary: "coportd".into(),
                },
            }),
        };
        std::fs::write(
            dir.path().join("forwarding.json"),
            serde_json::to_vec(&preferences).unwrap(),
        )
        .unwrap();
        let lock = crate::daemon::lock_file(&dir.path().join("daemon.lock")).unwrap();
        lock.try_lock().unwrap();
        assert!(Routing::configure_stopped(dir.path(), Some(false), None).is_err());
        drop(lock);
        Routing::configure_stopped(dir.path(), Some(false), None).unwrap();
        assert!(!Routing::stored_restore(dir.path()).unwrap());
        Routing::configure_stopped(dir.path(), Some(true), None).unwrap();
        assert_eq!(
            Routing::with_store(dir.path())
                .unwrap()
                .status()
                .unwrap()
                .device_id,
            "selected"
        );
        Routing::configure_stopped(dir.path(), None, Some("unrelated")).unwrap();
        assert!(Routing::with_store(dir.path()).unwrap().status().is_some());
        Routing::configure_stopped(dir.path(), None, Some("selected")).unwrap();
        let restored = Routing::with_store(dir.path()).unwrap();
        assert!(restored.restore_enabled());
        assert!(restored.status().is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn health_probe_recovers_without_a_client_request_or_resetting_counters() {
        let dir = tempfile::tempdir().unwrap();
        let executable = fake_ssh(dir.path(), "printf 'COPORT-FORWARD/1\\n'; exec cat");
        let routing = Routing {
            executable,
            ..Default::default()
        };
        routing
            .set(Some(Target {
                device_id: "device".into(),
                name: "Remote".into(),
                connection: device(),
            }))
            .await
            .unwrap();
        let mode = routing.mode.borrow().clone();
        fake_ssh(dir.path(), "exit 1");
        routing.check(&mode).await;
        assert_eq!(routing.status().unwrap().health.state, "disconnected");
        fake_ssh(dir.path(), "printf 'COPORT-FORWARD/1\\n'; exec cat");
        routing.check(&mode).await;
        let status = routing.status().unwrap();
        assert_eq!(status.health.state, "connected");
        assert!(status.health.recovered_at.is_some());
        assert_eq!(status.traffic.connections, 0);
        assert_eq!(status.traffic.upload_bytes, 0);
    }

    #[tokio::test]
    async fn forwarding_does_not_start_an_absent_daemon() {
        let dir = tempfile::tempdir().unwrap();
        assert!(serve_stdio(dir.path()).await.is_err());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}
