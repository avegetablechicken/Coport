//! Explicit, session-scoped access to a remote daemon's loopback proxy over SSH.
//! No remote credentials or configuration are copied to the local machine.
use crate::remote::{Device, helper_command};
use serde::{Deserialize, Serialize};
use std::{
    io,
    net::Ipv4Addr,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
    process::{Child, Command},
};

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
    let upload = async {
        tokio::io::copy(&mut a, &mut d).await?;
        d.shutdown().await
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

struct Channel {
    child: Child,
    input: tokio::process::ChildStdin,
    output: tokio::process::ChildStdout,
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
            // Never let SSH diagnostics or a remote banner enter the request stream.
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("Cannot start SSH: {e}"))?;
        let input = child.stdin.take().unwrap();
        let mut output = child.stdout.take().unwrap();
        let mut ready = [0; READY.len()];
        let result = tokio::time::timeout(CONNECT_TIMEOUT, output.read_exact(&mut ready)).await;
        if !matches!(result, Ok(Ok(_))) || ready != READY {
            let _ = child.kill().await;
            return Err("Cannot open remote proxy. Check SSH access, start Coport on the destination, and update it to support request forwarding. A summary-only SSH key cannot forward requests.".into());
        }
        Ok(Self {
            child,
            input,
            output,
        })
    }
    async fn transfer(mut self, socket: coport::server::Connection) -> io::Result<()> {
        let (reader, writer) = tokio::io::split(socket);
        let result = relay(reader, writer, &mut self.output, &mut self.input).await;
        // A remote helper must not outlive its local request, even on failure.
        let _ = self.child.kill().await;
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
}
struct Mode {
    target: Option<Target>,
    error: Mutex<Option<String>>,
}
/// Owned by coportd. A failed remote connection never changes the selected mode.
pub struct Routing {
    mode: tokio::sync::watch::Sender<Arc<Mode>>,
    updates: tokio::sync::Mutex<()>,
    executable: std::path::PathBuf,
}
impl Default for Routing {
    fn default() -> Self {
        Self {
            mode: tokio::sync::watch::channel(Arc::new(Mode {
                target: None,
                error: Mutex::new(None),
            }))
            .0,
            updates: Default::default(),
            executable: "ssh".into(),
        }
    }
}
impl Routing {
    pub fn status(&self) -> Option<Status> {
        let mode = self.mode.borrow();
        mode.target.as_ref().map(|target| Status {
            device_id: target.device_id.clone(),
            name: target.name.clone(),
            error: mode.error.lock().unwrap().clone(),
        })
    }
    pub async fn set(&self, target: Option<Target>) -> Result<(), String> {
        let _guard = self.updates.lock().await;
        if let Some(target) = &target {
            if target.device_id.is_empty()
                || target.device_id.len() > 128
                || target.name.is_empty()
                || target.name.len() > 128
            {
                return Err("Invalid forwarding device".into());
            }
            let mut probe = Channel::open(&target.connection, &self.executable).await?;
            let _ = probe.child.kill().await;
        }
        self.mode.send_replace(Arc::new(Mode {
            target,
            error: Mutex::new(None),
        }));
        Ok(())
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
                            match Channel::open(&target.connection, &routing.executable).await {
                                Ok(channel) => {
                                    *mode.error.lock().unwrap() = None;
                                    if let Err(error) = channel.transfer(socket).await {
                                        *mode.error.lock().unwrap() = Some(error.to_string());
                                    }
                                }
                                Err(error) => {
                                    *mode.error.lock().unwrap() = Some(error);
                                    // The local TLS handshake already completed. Return an HTTP
                                    // gateway error instead of silently routing through local accounts.
                                    let response = b"HTTP/1.1 502 Bad Gateway\r\nConnection: close\r\nContent-Length: 30\r\n\r\nRemote SSH proxy unavailable.\n";
                                    let _ = socket.write_all(response).await;
                                    let _ = socket.shutdown().await;
                                    // Drain a bounded amount of the rejected request so an unread
                                    // body cannot turn the 502 into a TCP reset on macOS/Windows.
                                    let _ = tokio::time::timeout(Duration::from_millis(250),
                                        tokio::io::copy(&mut socket.take(65536), &mut tokio::io::sink())).await;
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

    #[tokio::test]
    async fn forwarding_does_not_start_an_absent_daemon() {
        let dir = tempfile::tempdir().unwrap();
        assert!(serve_stdio(dir.path()).await.is_err());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}
