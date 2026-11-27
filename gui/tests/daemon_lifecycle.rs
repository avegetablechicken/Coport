use coport_gui::{
    daemon,
    daemon_lifecycle::{Launch, Lifecycle, Progress, Wait},
    proxy::Controller,
};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio_rustls::rustls;

fn helper() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_coportd"))
}

/// Starts the daemon at `binary` for `dir` now, as a Restart does.
fn start(
    binary: &Path,
    dir: &Path,
    config: &Path,
    log: &Path,
) -> std::io::Result<(daemon::Client, daemon::Status)> {
    Lifecycle::new(dir.to_owned(), binary.to_owned()).spawn(Launch { config, log })
}

fn controller(dir: &Path) -> Controller {
    Controller::with_daemon(Arc::new(|| {}), dir.to_owned(), helper())
}

/// A free loopback port for the daemon to bind later. It lies below every
/// platform's ephemeral range, so connections opened by tests running in
/// parallel cannot take it before the daemon binds, and is issued only once.
fn free_port() -> u16 {
    use std::hash::BuildHasher;
    static ISSUED: std::sync::Mutex<std::collections::BTreeSet<u16>> =
        std::sync::Mutex::new(std::collections::BTreeSet::new());
    let random = std::collections::hash_map::RandomState::new();
    for attempt in 0..1000u32 {
        let port = 20000 + (random.hash_one(attempt) % 10000) as u16;
        if ISSUED.lock().unwrap().insert(port) && TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return port;
        }
    }
    panic!("no free loopback port");
}

fn fixture(dir: &Path) -> u16 {
    let port = free_port();
    let config = format!(
        "listen_port: {port}\nrequest_timeout_seconds: 30\ncodex:\n  homes: []\nclaude:\n  config_dirs: []\n"
    );
    std::fs::write(dir.join("config.yaml"), config).unwrap();
    port
}

fn read_head(stream: &mut impl Read) -> Vec<u8> {
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        head.push(byte[0]);
        assert!(head.len() < 4096);
    }
    head
}

type Tls<S> = rustls::StreamOwned<rustls::ClientConnection, S>;

/// Completes TLS with the listener, trusting only the daemon's loopback CA:
/// binary records cross the connection in both directions, and nothing
/// contacts an upstream.
fn tls<S: Read + Write>(dir: &Path, stream: S) -> Tls<S> {
    use rustls::pki_types::{CertificateDer, pem::PemObject};
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(CertificateDer::from_pem_file(dir.join("tls").join("ca.pem")).unwrap())
        .unwrap();
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    let connection =
        rustls::ClientConnection::new(Arc::new(config), "localhost".try_into().unwrap()).unwrap();
    let mut tls = rustls::StreamOwned::new(connection, stream);
    while tls.conn.is_handshaking() {
        tls.conn.complete_io(&mut tls.sock).unwrap();
    }
    tls
}

/// The forwarding helper's stdio as one stream, recording the bytes it relays.
struct Pipe {
    input: std::process::ChildStdin,
    output: std::process::ChildStdout,
    relayed: Arc<std::sync::Mutex<Vec<u8>>>,
}
impl Read for Pipe {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.output.read(buf)?;
        self.relayed.lock().unwrap().extend_from_slice(&buf[..n]);
        Ok(n)
    }
}
impl Write for Pipe {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.input.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.input.flush()
    }
}

/// A request the daemon answers itself, closing the connection after it.
fn health<S: Read + Write>(tls: &mut Tls<S>) {
    tls.write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .unwrap();
    tls.flush().unwrap();
    let head = String::from_utf8(read_head(tls)).unwrap();
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
}

fn wait_for(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !predicate() {
        assert!(
            Instant::now() < deadline,
            "Timed out waiting for daemon lifecycle"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

// Executed in a separate OS process by the tests below, using exactly the
// controller and exit policy called by the GUI, without requiring a display.
#[test]
fn short_lived_gui_controller() {
    let Some(dir) = std::env::var_os("COPORT_TEST_PARENT_DIR") else {
        return;
    };
    let dir = PathBuf::from(dir);
    let mut gui = controller(&dir);
    gui.start(&dir.join("config.yaml"), dir.join("proxy.log"));
    assert!(gui.is_running(), "{:?}", gui.phase());
    std::fs::write(dir.join("ready"), "").unwrap();
    wait_for(|| dir.join("exit").exists());
    let keep = std::env::var("COPORT_TEST_KEEP").unwrap() == "true";
    gui.on_app_exit(keep).unwrap();
}

struct Cleanup(PathBuf);
impl Drop for Cleanup {
    fn drop(&mut self) {
        if let Some((client, _)) = daemon::Client::discover(&self.0) {
            let _ = client.stop();
        }
    }
}

#[cfg(unix)]
#[test]
fn ssh_discovery_survives_removal_of_running_daemon_installation() {
    let home = tempfile::tempdir().unwrap();
    let state = if cfg!(target_os = "macos") {
        home.path()
            .join("Library/Application Support/io.github.coport.gui")
    } else {
        home.path().join(".config/io.github.coport.gui")
    };
    std::fs::create_dir_all(&state).unwrap();
    let _cleanup = Cleanup(state.clone());
    fixture(&state);
    let installed = home.path().join("coportd");
    std::fs::copy(helper(), &installed).unwrap();
    let (client, before) = start(
        &installed,
        &state,
        &state.join("config.yaml"),
        &state.join("proxy.log"),
    )
    .unwrap();
    std::fs::remove_file(installed).unwrap();
    let record = std::fs::read(state.join("summary-executable")).unwrap();
    let script = include_str!("../src/ssh_summary.sh").replace("\r\n", "\n");
    for operation in ["--summary", "--capabilities", "--summary"] {
        let output = Command::new("/bin/sh")
            .args(["-c", &script.replace("--summary", operation)])
            .env("HOME", home.path())
            .env("XDG_CONFIG_HOME", home.path().join(".config"))
            .env("PATH", home.path().join("empty-path"))
            .output()
            .unwrap();
        assert!(output.status.success(), "{:?}", output);
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        if operation == "--capabilities" {
            assert_eq!(value["running"], true);
        }
        assert_eq!(client.status().unwrap().pid, before.pid);
        assert_eq!(
            std::fs::read(state.join("summary-executable")).unwrap(),
            record
        );
    }
}

fn parent(dir: &Path, keep: bool) -> std::process::Child {
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "short_lived_gui_controller", "--nocapture"])
        .env("COPORT_TEST_PARENT_DIR", dir)
        .env("COPORT_TEST_KEEP", keep.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap()
}

fn surviving_exit(force: bool) {
    let dir = tempfile::tempdir().unwrap();
    let _cleanup = Cleanup(dir.path().to_owned());
    let port = fixture(dir.path());
    let mut parent = parent(dir.path(), true);
    wait_for(|| dir.path().join("ready").exists());
    let (client, before) = daemon::Client::discover(dir.path()).unwrap();
    assert_ne!(before.pid, parent.id());
    let stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    // The TLS session is established before the GUI exits.
    let mut tls = tls(dir.path(), stream);
    if force {
        parent.kill().unwrap();
    } else {
        std::fs::write(dir.path().join("exit"), "").unwrap();
    }
    let exit = parent.wait().unwrap();
    if !force {
        assert!(exit.success());
    }
    assert_eq!(client.status().unwrap().pid, before.pid);
    health(&mut tls);

    // A fresh controller attaches without starting another listener or changing PID.
    let mut reopened = controller(dir.path());
    assert!(reopened.is_running());
    assert_eq!(reopened.daemon_status().unwrap().pid, before.pid);
    reopened.stop().unwrap();
    assert!(daemon::Client::discover(dir.path()).is_none());
    TcpListener::bind(("127.0.0.1", port)).unwrap();
    drop(tls);
}

#[test]
fn quit_really_ends_parent_and_preserves_daemon_and_connections() {
    surviving_exit(false);
}

#[test]
fn force_killing_gui_preserves_daemon_and_connections() {
    surviving_exit(true);
}

#[test]
fn disabled_option_stops_daemon_on_gui_exit() {
    let dir = tempfile::tempdir().unwrap();
    let _cleanup = Cleanup(dir.path().to_owned());
    let port = fixture(dir.path());
    let mut parent = parent(dir.path(), false);
    wait_for(|| dir.path().join("ready").exists());
    assert!(daemon::Client::discover(dir.path()).is_some());
    std::fs::write(dir.path().join("exit"), "").unwrap();
    assert!(parent.wait().unwrap().success());
    assert!(daemon::Client::discover(dir.path()).is_none());
    TcpListener::bind(("127.0.0.1", port)).unwrap();
}

#[test]
fn switching_configuration_changes_live_paths_and_rejects_invalid_yaml_before_stopping() {
    let dir = tempfile::tempdir().unwrap();
    let _cleanup = Cleanup(dir.path().to_owned());
    fixture(dir.path());
    let mut gui = controller(dir.path());
    gui.try_start(
        &dir.path().join("config.yaml"),
        dir.path().join("proxy.log"),
    )
    .unwrap();
    let initial = gui.daemon_status().unwrap().clone();
    let invalid = dir.path().join("invalid.yaml");
    std::fs::write(&invalid, "listen_port: [").unwrap();
    assert!(
        gui.try_start(&invalid, dir.path().join("other.log"))
            .is_err()
    );
    assert!(gui.is_running());
    assert_eq!(gui.daemon_status().unwrap().pid, initial.pid);
    let new_dir = dir.path().join("new");
    std::fs::create_dir(&new_dir).unwrap();
    let new_port = fixture(&new_dir);
    let config = new_dir.join("config.yaml").canonicalize().unwrap();
    let log = new_dir.canonicalize().unwrap().join("proxy.log");
    gui.try_start(&config, log.clone()).unwrap();
    let (_, live) = daemon::Client::discover(dir.path()).unwrap();
    assert_ne!(live.pid, initial.pid);
    assert_eq!(live.port, new_port);
    assert_eq!(live.config_path, config);
    assert_eq!(live.log_path, log);
    gui.stop().unwrap();
}

#[test]
fn daemon_crash_is_reported_and_restart_recovers_stale_registration() {
    let dir = tempfile::tempdir().unwrap();
    let _cleanup = Cleanup(dir.path().to_owned());
    fixture(dir.path());
    let mut daemon = Command::new(helper())
        .arg(dir.path())
        .arg(dir.path().join("config.yaml"))
        .arg(dir.path().join("proxy.log"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_for(|| daemon::Client::discover(dir.path()).is_some());
    let mut gui = controller(dir.path());
    assert!(gui.is_running());
    daemon.kill().unwrap();
    daemon.wait().unwrap();
    assert!(dir.path().join("daemon.json").exists());
    wait_for(|| !gui.is_running());
    gui.start(
        &dir.path().join("config.yaml"),
        dir.path().join("proxy.log"),
    );
    assert!(gui.is_running(), "{:?}", gui.phase());
    assert_ne!(gui.daemon_status().unwrap().pid, daemon.id());
    gui.stop().unwrap();
}

#[test]
fn restart_replaces_daemon_and_stale_discovery_is_recovered() {
    let dir = tempfile::tempdir().unwrap();
    let _cleanup = Cleanup(dir.path().to_owned());
    let port = fixture(dir.path());
    let path = dir.path().join("daemon.json");
    let stale = b"stale invalid endpoint";
    std::fs::write(&path, stale).unwrap();
    // Hold a discovery read open across publication to force the overlap.
    let mut reader = std::fs::File::open(&path).unwrap();
    let mut gui = controller(dir.path());
    gui.try_start(
        &dir.path().join("config.yaml"),
        dir.path().join("proxy.log"),
    )
    .expect("start with stale discovery");
    let first = gui.daemon_status().unwrap().pid;
    let mut old = Vec::new();
    reader.read_to_end(&mut old).unwrap();
    assert_eq!(old, stale);
    assert_eq!(daemon::Client::discover(dir.path()).unwrap().1.pid, first);
    gui.try_start(
        &dir.path().join("config.yaml"),
        dir.path().join("proxy.log"),
    )
    .unwrap();
    assert_ne!(gui.daemon_status().unwrap().pid, first);
    assert_eq!(gui.daemon_status().unwrap().port, port);
    gui.stop().unwrap();
}

#[test]
fn unauthenticated_stop_is_rejected_and_duplicate_daemon_cannot_bind() {
    let dir = tempfile::tempdir().unwrap();
    let _cleanup = Cleanup(dir.path().to_owned());
    fixture(dir.path());
    let (client, status) = start(
        &helper(),
        dir.path(),
        &dir.path().join("config.yaml"),
        &dir.path().join("proxy.log"),
    )
    .unwrap();
    let path = dir.path().join("daemon.json");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    let endpoint: serde_json::Value =
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    for bytes in [
        br#"{"token":"wrong","action":"Stop"}"#.as_slice(),
        br#"{"token":"wrong","action":{"Probe":{"name":"test"}}}"#.as_slice(),
        br#"{"token":"wrong","action":"AccountStates"}"#.as_slice(),
        br#"{"token":"wrong","action":"CredentialLabels"}"#.as_slice(),
    ] {
        let mut control =
            TcpStream::connect(("127.0.0.1", endpoint["port"].as_u64().unwrap() as u16)).unwrap();
        control
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        control
            .write_all(&(bytes.len() as u32).to_be_bytes())
            .unwrap();
        control.write_all(bytes).unwrap();
        let mut length = [0; 4];
        control.read_exact(&mut length).unwrap();
        let mut response = vec![0; u32::from_be_bytes(length) as usize];
        control.read_exact(&mut response).unwrap();
        assert!(
            String::from_utf8(response)
                .unwrap()
                .contains("Unauthorized")
        );
    }
    let duplicate = Command::new(helper())
        .arg(dir.path())
        .arg(dir.path().join("config.yaml"))
        .arg(dir.path().join("proxy.log"))
        .output()
        .unwrap();
    assert!(!duplicate.status.success());
    assert_eq!(client.status().unwrap().pid, status.pid);
    client.stop().unwrap();
}

#[test]
fn proxy_tests_use_daemon_configuration_and_authenticated_control() {
    use coport_gui::proxy::Probe;
    let dir = tempfile::tempdir().unwrap();
    let _cleanup = Cleanup(dir.path().into());
    fixture(dir.path());
    let proxy = TcpListener::bind("127.0.0.1:0").unwrap();
    proxy.set_nonblocking(true).unwrap();
    let endpoint = format!("http://user:password@{}", proxy.local_addr().unwrap());
    let config_path = dir.path().join("config.yaml");
    let mut config = std::fs::read_to_string(&config_path).unwrap();
    config.push_str(&format!("proxies:\n  test: {endpoint}\n"));
    std::fs::write(&config_path, config).unwrap();
    let (client, status) = start(
        &helper(),
        dir.path(),
        &config_path,
        &dir.path().join("proxy.log"),
    )
    .unwrap();
    assert!(status.proxy_probe_supported);
    assert!(
        client
            .probe("missing")
            .unwrap_err()
            .to_string()
            .contains("running daemon configuration")
    );
    let mut gui = controller(dir.path());
    gui.probe("test", &endpoint);
    assert!(matches!(gui.probes()["test"].0, Probe::Pending));
    let mut incoming = None;
    wait_for(|| {
        incoming = proxy.accept().ok();
        incoming.is_some()
    });
    let (mut socket, _) = incoming.unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        socket.read_exact(&mut byte).unwrap();
        head.push(byte[0]);
        assert!(head.len() < 8192);
    }
    let head = String::from_utf8(head).unwrap().to_ascii_lowercase();
    assert!(head.starts_with("connect 1.1.1.1:443 "));
    assert!(head.contains("proxy-authorization: basic dxnlcjpwyxnzd29yza=="));
    // While the daemon waits for the test endpoint, control stays responsive.
    assert_eq!(client.status().unwrap().pid, status.pid);
    socket.write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
    drop(socket);
    wait_for(|| matches!(gui.probes()["test"].0, Probe::Unreachable(_)));
    // A config edit must not silently test the daemon's old endpoint.
    std::fs::write(&config_path, "invalid config").unwrap();
    // Windows requires write access when changing file timestamps.
    std::fs::OpenOptions::new()
        .write(true)
        .open(&config_path)
        .unwrap()
        .set_modified(status.config_modified.unwrap() + Duration::from_secs(2))
        .unwrap();
    gui.probe("test", &endpoint);
    wait_for(
        || matches!(&gui.probes()["test"].0, Probe::Unavailable(e) if e.contains("updated configuration")),
    );
    gui.on_app_exit(true).unwrap();
    client.stop().unwrap();
}

#[test]
fn metadata_uses_daemon_then_local_only_after_daemon_stops() {
    let dir = tempfile::tempdir().unwrap();
    let _cleanup = Cleanup(dir.path().into());
    let port = free_port();
    let config_path = dir.path().join("config.yaml");
    let daemon_home = dir.path().join("daemon-home");
    std::fs::create_dir(&daemon_home).unwrap();
    let daemon_home_yaml = serde_json::to_string(&daemon_home).unwrap();
    let daemon_config = format!(
        "listen_port: {port}\nrequest_timeout_seconds: 3\ncodex:\n  homes: [{daemon_home_yaml}]\n  routing:\n    account: {{daemon-account: none}}\nclaude:\n  config_dirs: []\n"
    );
    std::fs::write(&config_path, &daemon_config).unwrap();
    let (client, status) = start(
        &helper(),
        dir.path(),
        &config_path,
        &dir.path().join("proxy.log"),
    )
    .unwrap();
    assert!(status.metadata_supported);
    // Synthetic local credentials make an accidental local fallback observable.
    let home = dir.path().join("local-home");
    std::fs::create_dir(&home).unwrap();
    std::fs::write(
        home.join("auth.json"),
        r#"{"tokens":{"account_id":"local-account","access_token":"test-only-token"}}"#,
    )
    .unwrap();
    let local_text = daemon_config
        .replace("daemon-account", "local-account")
        .replacen(
            &format!("homes: [{daemon_home_yaml}]"),
            &format!("homes: [{}]", serde_json::to_string(&home).unwrap()),
            1,
        );
    let local = coport::config::Config::parse(&local_text).unwrap();
    let tasks = coport_gui::tasks::Tasks::new(dir.path().into(), config_path.clone(), local);
    let rt = tokio::runtime::Runtime::new().unwrap();
    let states = rt.block_on(tasks.account_states()).unwrap();
    assert!(states[0].contains_key("daemon-account"));
    assert!(!states[0].contains_key("local-account"));
    let labels = rt.block_on(tasks.credential_labels()).unwrap();
    assert_eq!(
        labels
            .get(&("Codex".into(), "daemon-account".into()))
            .map(String::as_str),
        Some("daemon-account")
    );
    assert!(!labels.contains_key(&("Codex".into(), "local-account".into())));
    // Windows requires write access when changing file timestamps.
    std::fs::OpenOptions::new()
        .write(true)
        .open(&config_path)
        .unwrap()
        .set_modified(status.config_modified.unwrap() + Duration::from_secs(2))
        .unwrap();
    assert!(
        rt.block_on(tasks.account_states())
            .unwrap_err()
            .contains("updated configuration")
    );
    assert!(
        rt.block_on(tasks.credential_labels())
            .unwrap_err()
            .contains("updated configuration")
    );
    client.stop().unwrap();
    let states = rt.block_on(tasks.account_states()).unwrap();
    assert!(states[0].contains_key("local-account"));
    assert!(!states[0].contains_key("daemon-account"));
    let labels = rt.block_on(tasks.credential_labels()).unwrap();
    assert_eq!(
        labels
            .get(&("Codex".into(), "local-account".into()))
            .map(String::as_str),
        Some("local-account")
    );
}

#[test]
fn stdio_forwarding_uses_the_running_daemon_and_preserves_response_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let _cleanup = Cleanup(dir.path().to_owned());
    fixture(dir.path());
    let mut gui = controller(dir.path());
    gui.start(
        &dir.path().join("config.yaml"),
        dir.path().join("proxy.log"),
    );
    assert!(gui.is_running());
    let original = gui.daemon_status().unwrap().pid;
    let mut child = Command::new(helper())
        .arg("--forward")
        .arg("--state-dir")
        .arg(dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Move all pipe I/O to a worker so a broken helper cannot hang CI.
    let input = child.stdin.take().unwrap();
    let output = child.stdout.take().unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    let state = dir.path().to_owned();
    let io = std::thread::spawn(move || {
        let mut output = output;
        let mut ready = [0; b"COPORT-FORWARD/1\n".len()];
        // Read the protocol marker separately from HTTP bytes.
        let marker = b"COPORT-FORWARD/1\n";
        output.read_exact(&mut ready[..marker.len()]).unwrap();
        assert_eq!(&ready[..marker.len()], marker);
        let relayed = Arc::new(std::sync::Mutex::new(Vec::new()));
        let pipe = Pipe {
            input,
            output,
            relayed: relayed.clone(),
        };
        // TLS fails on any altered byte in either direction.
        let mut tls = tls(&state, pipe);
        health(&mut tls);
        // Keep stdin open: a closed remote socket must still terminate the helper.
        let mut rest = Vec::new();
        tls.sock.read_to_end(&mut rest).unwrap();
        let response = relayed.lock().unwrap().clone();
        sender.send(response).unwrap();
    });
    let response = receiver.recv_timeout(Duration::from_secs(10));
    if response.is_err() {
        let _ = child.kill();
    }
    let response = response.unwrap();
    io.join().unwrap();
    // The daemon's bytes start with its TLS handshake record.
    assert_eq!(response.first(), Some(&0x16));
    assert!(!response.windows(7).any(|bytes| bytes == b"COPORT-"));
    wait_for(|| child.try_wait().unwrap().is_some());
    assert!(child.wait().unwrap().success());
    assert_eq!(
        daemon::Client::discover(dir.path()).unwrap().1.pid,
        original
    );
    gui.stop().unwrap();
}

#[cfg(unix)]
#[test]
fn unified_forwarding_keeps_port_and_tls_and_never_falls_back_on_ssh_failure() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let _cleanup = Cleanup(dir.path().to_owned());
    let port = fixture(dir.path());
    let ssh = dir.path().join("ssh");
    let ssh_script = r#"#!/bin/sh
for argument do command="$argument"; done
case "$command" in *--capabilities*)
  printf '%s\n' '{"protocol":1,"version":"test-capabilities","runningVersion":"test-capabilities","running":true,"statistics":true,"forwarding":true,"forwardingAvailable":true}'
  exit 0 ;;
esac
printf 'COPORT-FORWARD/1\n'
IFS= read -r request || exit 0
printf 'HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 6\r\n\r\nremote'
"#;
    std::fs::write(&ssh, ssh_script).unwrap();
    std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o700)).unwrap();
    let quote = |path: &Path| format!("'{}'", path.display().to_string().replace('\'', "'\\''"));
    let wrapper = dir.path().join("start-daemon");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nexport PATH={}:$PATH\nexec {} \"$@\"\n",
            quote(dir.path()),
            quote(&helper())
        ),
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
    let (client, status) = start(
        &wrapper,
        dir.path(),
        &dir.path().join("config.yaml"),
        &dir.path().join("proxy.log"),
    )
    .unwrap();
    assert!(status.forwarding_supported);
    let rt = tokio::runtime::Runtime::new().unwrap();
    let client = rt.block_on(async {
        // The daemon checks the devices the GUI names, in the background.
        let device = coport_gui::remote::Device {
            name: "test-device".into(),
            host: "fake-host".into(),
            binary: "coportd".into(),
        };
        let started=Instant::now();
        loop {
            let checks=client.device_checks(vec![device.clone()]).await.unwrap();
            if let Some(caps)=checks.first().and_then(|check|check.capabilities.as_ref()) {
                assert!(caps.forwarding && caps.forwarding_available);
                assert_eq!(caps.version,"test-capabilities");
                break;
            }
            assert!(started.elapsed()<Duration::from_secs(10));
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let cert =
            reqwest::Certificate::from_pem(&std::fs::read(dir.path().join("tls/ca.pem")).unwrap())
                .unwrap();
        let http = reqwest::Client::builder()
            .no_proxy()
            .add_root_certificate(cert)
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap();
        let urls = [
            format!("http://127.0.0.1:{port}/unified-test"),
            format!("https://127.0.0.1:{port}/unified-test"),
        ];
        let before = http.get(&urls[0]).send().await.unwrap().status();
        assert_ne!(before.as_u16(), 200);
        let target = coport_gui::remote_forward::Target {
            device_id: "test-device".into(),
            name: "Remote".into(),
            connection: coport_gui::remote::Device {
                name: "test".into(),
                host: "fake-host".into(),
                binary: "coportd".into(),
            },
        };
        client.set_forwarding(Some(target)).await.unwrap();
        for url in &urls {
            let response = http.get(url).send().await.unwrap();
            assert_eq!(response.status(), 200);
            assert_eq!(response.text().await.unwrap(), "remote");
        }
        let during = client.status().unwrap();
        assert_eq!(during.port, port);
        assert_eq!(during.pid, status.pid);
        assert_eq!(during.forwarding.unwrap().device_id, "test-device");
        // A failed SSH connection must produce 502, even though local routing works.
        std::fs::write(&ssh, "#!/bin/sh\nexit 1\n").unwrap();
        for url in &urls {
            let response = http.get(url).send().await.unwrap();
            assert_eq!(response.status(), 502);
            assert_eq!(
                response.text().await.unwrap(),
                "Remote SSH proxy unavailable.\n"
            );
        }
        let failed = client.status().unwrap().forwarding.unwrap();
        assert!(failed.error.is_some());
        assert_eq!(failed.health.state, "disconnected");
        assert!(failed.traffic.upload_bytes > 0 && failed.traffic.download_bytes > 0);
        assert_eq!(failed.traffic.connections, 4); // health probes are excluded
        client.set_forwarding_restore(true).await.unwrap();
        client.stop().unwrap();
        let (client, restored) = start(&wrapper, dir.path(), &dir.path().join("config.yaml"), &dir.path().join("proxy.log")).unwrap();
        assert!(client.status().unwrap().forwarding_restore);
        assert!(restored.forwarding.is_some());
        // Restore selects remote mode before accepting the first request, even offline.
        for url in &urls { assert_eq!(http.get(url).send().await.unwrap().status(), 502); }
        std::fs::write(&ssh, "#!/bin/sh\nprintf 'COPORT-FORWARD/1\\n'\nIFS= read -r request || exit 0\nprintf 'HTTP/1.1 200 OK\\r\\nConnection: close\\r\\nContent-Length: 6\\r\\n\\r\\nremote'\n").unwrap();
        assert_eq!(http.get(&urls[0]).send().await.unwrap().text().await.unwrap(), "remote");
        let recovered = client.status().unwrap().forwarding.unwrap();
        assert_eq!(recovered.health.state, "connected");
        assert!(recovered.health.recovered_at.is_some());
        assert!(recovered.error.is_none());
        client.set_forwarding(None).await.unwrap();
        assert!(client.status().unwrap().forwarding.is_none());
        for url in &urls {
            assert_eq!(http.get(url).send().await.unwrap().status(), before);
        }
        assert_eq!(client.status().unwrap().port, port);
        client
    });
    client.stop().unwrap();
}

#[test]
fn summary_stream_helper_respects_platform_support_without_starting_daemon() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    std::fs::create_dir(dir.path().join("logs")).unwrap();
    std::fs::write(dir.path().join("logs/proxy.log"), "").unwrap();
    coport_gui::data_api::prepare_identity(dir.path()).unwrap();
    let output = Command::new(helper())
        .args(["--summary-stream", "--window", "1440", "all", "--state-dir"])
        .arg(dir.path())
        .output()
        .unwrap();
    #[cfg(unix)]
    {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let frames: Vec<coport_gui::data_api::Summary> = String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(frames.len(), 2);
        for frame in &frames {
            frame.validate().unwrap();
        }
        assert_eq!(frames[0].schema_version, 4);
        assert_eq!(frames[0].windows.len(), 1);
        assert_eq!(frames[0].windows[0].minutes, 1440);
        assert_eq!(
            frames[0].windows[0].scope,
            coport_gui::traffic::TrafficScope::All
        );
        assert_eq!(frames[1].schema_version, 3);
        assert_eq!(frames[1].windows.len(), 12);
    }
    #[cfg(not(unix))]
    {
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("unavailable on this platform"));
        assert!(output.stdout.is_empty());
    }
    assert!(daemon::Client::discover(dir.path()).is_none());
}

#[cfg(unix)]
#[test]
fn display_metadata_and_summary_helpers_never_contact_account_upstream() {
    let dir = tempfile::tempdir().unwrap();
    let _cleanup = Cleanup(dir.path().to_owned());
    let port = fixture(dir.path());
    let forbidden = TcpListener::bind("127.0.0.1:0").unwrap();
    forbidden.set_nonblocking(true).unwrap();
    let upstream = format!("http://{}", forbidden.local_addr().unwrap());
    let credentials = dir.path().join("claude");
    std::fs::create_dir(&credentials).unwrap();
    std::fs::write(
        credentials.join(".credentials.json"),
        r#"{"claudeAiOauth":{"accessToken":"fixture-only-secret"}}"#,
    )
    .unwrap();
    let config_text = format!(
        "listen_port: {port}\nrequest_timeout_seconds: 1\ncodex:\n  homes: []\nclaude:\n  config_dirs: [{}]\n  base_url: https://upstream.invalid\n  routing:\n    account: {{unresolved@example.invalid: none}}\n    account_probe: lookup\nproxies:\n  lookup: {upstream}\n",
        serde_json::to_string(&credentials).unwrap()
    );
    std::fs::write(dir.path().join("config.yaml"), &config_text).unwrap();
    let log = dir.path().join("proxy.log");
    let timestamp = (chrono::Utc::now() - chrono::Duration::minutes(3)).to_rfc3339();
    let rows: String = (0..100).map(|index| format!("{}\n", serde_json::json!({"event":"model_call_finished","timestamp":timestamp,"model_call_id":format!("fixture-{index}"),"service":"claude","status":"200","input_tokens":"10","output_tokens":"5"}))).collect();
    std::fs::write(&log, rows).unwrap();
    let (client, _) = start(&helper(), dir.path(), &dir.path().join("config.yaml"), &log).unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    // The explicit account_probe authorizes one background discovery attempt.
    // Wait for that cycle, then prove display reads cause no additional I/O.
    rt.block_on(client.credential_reports()).unwrap();
    assert!(
        forbidden.accept().is_ok(),
        "Background identity discovery did not run"
    );
    rt.block_on(async {
        let config = coport::config::Config::parse(&config_text).unwrap();
        let mut cache = coport_gui::data_client::MergeCache::default();
        // First entry, repeat entry and timer reads use exactly the display-only metadata path.
        for _ in 0..3 {
            // Both old and new RPCs must stay offline even with missing OAuth identity.
            client.credential_labels().await.unwrap();
            client.cached_credential_labels().await.unwrap();
            let tasks = coport_gui::tasks::Tasks::new(
                dir.path().into(),
                dir.path().join("config.yaml"),
                config.clone(),
            );
            let labels = tasks.cached_credential_labels().await;
            cache
                .views(
                    Vec::new(),
                    this_device(config.clone(), log.clone(), labels),
                    false,
                )
                .await
                .unwrap();
        }
        // Explicit statistics refresh also uses local/cached identity evidence.
        for minutes in [30, 360, 1440] {
            let config_labels = config.local_traffic_credential_labels().await;
            cache
                .refresh_progressive(
                    Vec::new(),
                    this_device(config.clone(), log.clone(), config_labels),
                    (minutes, coport_gui::traffic::TrafficScope::Model),
                    |_| Ok(()),
                )
                .await
                .unwrap();
        }
    });
    let output = Command::new(helper())
        .args(["--summary-stream", "--window", "30", "model", "--state-dir"])
        .arg(dir.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        output
            .stdout
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .count(),
        2
    );
    assert!(
        matches!(forbidden.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
        "Display/statistics access connected to an upstream account endpoint"
    );
    client.stop().unwrap();
    // The legacy GUI fallback with no daemon must not reintroduce profile probes.
    let tasks = coport_gui::tasks::Tasks::new(
        dir.path().into(),
        dir.path().join("config.yaml"),
        coport::config::Config::parse(&config_text).unwrap(),
    );
    rt.block_on(async {
        for _ in 0..3 {
            tasks.credential_labels().await.unwrap();
            tasks.cached_credential_labels().await;
        }
    });
    assert!(
        matches!(forbidden.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
        "Stopped-daemon metadata fallback contacted an upstream"
    );
}

/// A configuration whose requests to `upstream.invalid` go through a proxy
/// that accepts connections and never answers, so they stay in flight. The
/// returned listener is that proxy.
fn hanging_fixture(dir: &Path) -> (u16, TcpListener) {
    let hang = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = free_port();
    let config = format!(
        "listen_port: {port}\nrequest_timeout_seconds: 60\nproxies:\n  hang: http://127.0.0.1:{}\ncodex:\n  homes: []\n  routing:\n    api_key:\n      upstream.invalid: hang\nclaude:\n  config_dirs: []\n",
        hang.local_addr().unwrap().port()
    );
    std::fs::write(dir.join("config.yaml"), config).unwrap();
    (port, hang)
}

/// A request to the daemon on `port` that stays in flight until dropped.
fn hanging_request(port: u16, client: &daemon::Client) -> TcpStream {
    let before = client.status().unwrap().requests_in_flight.unwrap();
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .write_all(b"GET /codex/https://upstream.invalid/v1/models HTTP/1.1\r\nHost: local\r\nAuthorization: Bearer secret\r\n\r\n")
        .unwrap();
    wait_for(|| client.status().unwrap().requests_in_flight == Some(before + 1));
    stream
}

#[test]
fn a_restart_waits_for_requests_in_flight_and_a_forced_restart_joins_it() {
    let dir = tempfile::tempdir().unwrap();
    let _cleanup = Cleanup(dir.path().to_owned());
    let (port, _hang) = hanging_fixture(dir.path());
    let config = dir.path().join("config.yaml");
    let log = dir.path().join("proxy.log");
    let (client, first) = start(&helper(), dir.path(), &config, &log).unwrap();
    let request = hanging_request(port, &client);
    let restart = |wait: Wait, heard: std::sync::mpsc::Sender<Progress>| {
        let (dir, config, log) = (dir.path().to_owned(), config.clone(), log.clone());
        std::thread::spawn(move || {
            Lifecycle::new(dir, helper()).restart(
                Launch {
                    config: &config,
                    log: &log,
                },
                wait,
                &mut |progress| heard.send(progress).unwrap(),
            )
        })
    };

    // Requests that outlast the wait cancel the Restart.
    let (heard, progress) = std::sync::mpsc::channel();
    let cancelled = restart(Wait::Idle(Duration::from_secs(1)), heard);
    let error = cancelled.join().unwrap().err().unwrap().to_string();
    assert!(error.contains("1 request(s) still in flight"), "{error}");
    assert_eq!(progress.iter().collect::<Vec<_>>(), [Progress::Waiting(1)]);
    assert_eq!(client.status().unwrap().pid, first.pid);

    // The Restart goes ahead once they end.
    let (heard, progress) = std::sync::mpsc::channel();
    let waiting = restart(Wait::Idle(Duration::from_secs(30)), heard);
    assert_eq!(progress.recv().unwrap(), Progress::Waiting(1));
    std::thread::sleep(Duration::from_millis(500));
    assert!(!waiting.is_finished());
    drop(request);
    let second = waiting.join().unwrap().unwrap().status;
    assert_ne!(second.pid, first.pid);
    assert_eq!(
        progress.iter().collect::<Vec<_>>(),
        [Progress::Stopping, Progress::Stopped]
    );

    // The GUI's Restart never waits: it forces the waiting one and joins it.
    let (client, _) = daemon::Client::discover(dir.path()).unwrap();
    let mut request = hanging_request(port, &client);
    let (heard, progress) = std::sync::mpsc::channel();
    let waiting = restart(Wait::Idle(Duration::from_secs(60)), heard);
    assert_eq!(progress.recv().unwrap(), Progress::Waiting(1));
    let mut gui = controller(dir.path());
    gui.try_start(&config, log.clone()).unwrap();
    let third = waiting.join().unwrap().unwrap().status;
    assert_ne!(third.pid, second.pid);
    assert_eq!(gui.daemon_status().unwrap().pid, third.pid);
    // The forced Restart interrupted the request.
    request
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    assert!(request.read(&mut [0; 64]).is_ok());
}

#[test]
fn a_second_control_restart_forces_the_waiting_one() {
    use std::io::BufRead;
    let dir = tempfile::tempdir().unwrap();
    let _cleanup = Cleanup(dir.path().to_owned());
    let (port, _hang) = hanging_fixture(dir.path());
    let (client, first) = start(
        &helper(),
        dir.path(),
        &dir.path().join("config.yaml"),
        &dir.path().join("proxy.log"),
    )
    .unwrap();
    let _request = hanging_request(port, &client);
    let control = |args: &[&str]| {
        Command::new(helper())
            .arg("--control")
            .args(args)
            .arg("--state-dir")
            .arg(dir.path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    };
    let mut waiting = control(&["restart", "--wait-idle", "60"]);
    let mut notes = std::io::BufReader::new(waiting.stderr.take().unwrap()).lines();
    let note = notes.next().unwrap().unwrap();
    assert!(
        note.starts_with("Waiting for 1 request(s) in flight"),
        "{note}"
    );
    let forced = control(&["restart", "--force"]).wait_with_output().unwrap();
    assert!(forced.status.success(), "{forced:?}");
    assert!(String::from_utf8_lossy(&forced.stderr).contains("Another restart is in progress"));
    let waited = waiting.wait_with_output().unwrap();
    assert!(waited.status.success(), "{waited:?}");
    let notes: Vec<_> = notes.map(Result::unwrap).collect();
    assert_eq!(
        notes,
        [
            "Stopping the proxy daemon.",
            "Starting the new proxy daemon."
        ]
    );
    let pid = |output: &std::process::Output| {
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()["daemon"]["pid"]
            .as_u64()
            .unwrap() as u32
    };
    assert_eq!(pid(&forced), pid(&waited));
    assert_ne!(pid(&waited), first.pid);
    assert_eq!(
        daemon::Client::discover(dir.path()).unwrap().1.pid,
        pid(&waited)
    );
    assert!(
        !Command::new(helper())
            .args(["--control", "restart", "--wait-idle", "0", "--state-dir"])
            .arg(dir.path())
            .output()
            .unwrap()
            .status
            .success()
    );
}

/// A child that another thread forks keeps a copy of a just-written
/// executable's write handle until it execs, and starting the executable
/// meanwhile fails with "Text file busy". Here the test holds that handle.
#[cfg(target_os = "linux")]
#[test]
fn a_daemon_executable_still_open_for_writing_starts_once_it_is_closed() {
    let dir = tempfile::tempdir().unwrap();
    let _cleanup = Cleanup(dir.path().to_owned());
    fixture(dir.path());
    let copy = dir.path().join("coportd");
    std::fs::copy(helper(), &copy).unwrap();
    let writer = std::fs::OpenOptions::new().write(true).open(&copy).unwrap();
    let release = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        drop(writer);
    });
    let started = start(
        &copy,
        dir.path(),
        &dir.path().join("config.yaml"),
        &dir.path().join("proxy.log"),
    );
    release.join().unwrap();
    started.unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn a_new_daemon_that_fails_to_start_gives_way_to_the_previous_program() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let _cleanup = Cleanup(dir.path().to_owned());
    fixture(dir.path());
    let config = dir.path().join("config.yaml");
    let log = dir.path().join("proxy.log");
    let previous = dir.path().join("previous-install/coportd");
    std::fs::create_dir_all(previous.parent().unwrap()).unwrap();
    std::fs::copy(helper(), &previous).unwrap();
    let (_, first) = start(&previous, dir.path(), &config, &log).unwrap();
    // Only the running image is left of the previous program.
    std::fs::remove_dir_all(previous.parent().unwrap()).unwrap();
    let broken = dir.path().join("broken-coportd");
    std::fs::write(&broken, "#!/bin/sh\necho broken build >&2\nexit 3\n").unwrap();
    std::fs::set_permissions(&broken, std::fs::Permissions::from_mode(0o755)).unwrap();
    let restarted = Lifecycle::new(dir.path().to_owned(), broken)
        .restart(
            Launch {
                config: &config,
                log: &log,
            },
            Wait::Force,
            &mut |_| {},
        )
        .unwrap();
    assert!(restarted.rolled_back.unwrap().contains("broken build"));
    assert_ne!(restarted.status.pid, first.pid);
    assert_eq!(
        daemon::Client::discover(dir.path()).unwrap().1.pid,
        restarted.status.pid
    );
}

/// This device as a merge reads it, with a node ID no peer has.
#[cfg(unix)]
fn this_device(
    config: coport::config::Config,
    log: PathBuf,
    labels: std::collections::BTreeMap<(String, String), String>,
) -> coport_gui::data_client::ThisDevice {
    coport_gui::data_client::ThisDevice {
        daemon: coport_gui::daemon::Lookup::new(log.parent().unwrap().into()),
        config,
        log,
        labels,
        node_id: "local".into(),
    }
}
