use coport_gui::{daemon, proxy::Controller};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};

fn helper() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_coportd"))
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

fn fixture(dir: &Path, upstream: Option<u16>) -> u16 {
    let port = free_port();
    let mut config = format!(
        "listen_port: {port}\nrequest_timeout_seconds: 30\ncodex:\n  homes: []\nclaude:\n  config_dirs: []\n"
    );
    if let Some(upstream) = upstream {
        config.push_str(&format!("connect:\n  \"127.0.0.1:{upstream}\": none\n"));
    }
    std::fs::write(dir.join("config.yaml"), config).unwrap();
    port
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

fn exchange(stream: &mut TcpStream, bytes: &[u8]) {
    stream.write_all(bytes).unwrap();
    let mut echo = vec![0; bytes.len()];
    stream.read_exact(&mut echo).unwrap();
    assert_eq!(echo, bytes);
}

fn surviving_exit(force: bool) {
    let dir = tempfile::tempdir().unwrap();
    let _cleanup = Cleanup(dir.path().to_owned());
    let upstream = TcpListener::bind("127.0.0.1:0").unwrap();
    let upstream_port = upstream.local_addr().unwrap().port();
    let port = fixture(dir.path(), Some(upstream_port));
    let echo = std::thread::spawn(move || {
        let (mut stream, _) = upstream.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut bytes = [0; 128];
        while let Ok(n) = stream.read(&mut bytes) {
            if n == 0 {
                break;
            }
            if stream.write_all(&bytes[..n]).is_err() {
                break;
            }
        }
    });
    let mut parent = parent(dir.path(), true);
    wait_for(|| dir.path().join("ready").exists());
    let (client, before) = daemon::Client::discover(dir.path()).unwrap();
    assert_ne!(before.pid, parent.id());
    let mut tunnel = TcpStream::connect(("127.0.0.1", port)).unwrap();
    tunnel
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write!(
        tunnel,
        "CONNECT 127.0.0.1:{upstream_port} HTTP/1.1\r\nHost: 127.0.0.1:{upstream_port}\r\n\r\n"
    )
    .unwrap();
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        tunnel.read_exact(&mut byte).unwrap();
        head.push(byte[0]);
        assert!(head.len() < 4096);
    }
    assert!(String::from_utf8(head).unwrap().starts_with("HTTP/1.1 200"));
    exchange(&mut tunnel, b"before GUI exit");
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
    exchange(&mut tunnel, b"after GUI process ended");

    // A fresh controller attaches without starting another listener or changing PID.
    let mut reopened = controller(dir.path());
    assert!(reopened.is_running());
    assert_eq!(reopened.daemon_status().unwrap().pid, before.pid);
    reopened.stop().unwrap();
    assert!(daemon::Client::discover(dir.path()).is_none());
    TcpListener::bind(("127.0.0.1", port)).unwrap();
    drop(tunnel);
    echo.join().unwrap();
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
    let port = fixture(dir.path(), None);
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
    fixture(dir.path(), None);
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
    let new_port = fixture(&new_dir, None);
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
    fixture(dir.path(), None);
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
    let port = fixture(dir.path(), None);
    std::fs::write(dir.path().join("daemon.json"), b"stale invalid endpoint").unwrap();
    let mut gui = controller(dir.path());
    gui.start(
        &dir.path().join("config.yaml"),
        dir.path().join("proxy.log"),
    );
    let first = gui.daemon_status().unwrap().pid;
    gui.start(
        &dir.path().join("config.yaml"),
        dir.path().join("proxy.log"),
    );
    assert_ne!(gui.daemon_status().unwrap().pid, first);
    assert_eq!(gui.daemon_status().unwrap().port, port);
    gui.stop().unwrap();
}

#[test]
fn unauthenticated_stop_is_rejected_and_duplicate_daemon_cannot_bind() {
    let dir = tempfile::tempdir().unwrap();
    let _cleanup = Cleanup(dir.path().to_owned());
    fixture(dir.path(), None);
    let (client, status) = daemon::start(
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
    fixture(dir.path(), None);
    let proxy = TcpListener::bind("127.0.0.1:0").unwrap();
    proxy.set_nonblocking(true).unwrap();
    let endpoint = format!("http://user:password@{}", proxy.local_addr().unwrap());
    let config_path = dir.path().join("config.yaml");
    let mut config = std::fs::read_to_string(&config_path).unwrap();
    config.push_str(&format!("proxies:\n  test: {endpoint}\n"));
    std::fs::write(&config_path, config).unwrap();
    let (client, status) = daemon::start(
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
    let (client, status) = daemon::start(
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
    let upstream = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let upstream_port = upstream.local_addr().unwrap().port();
    fixture(dir.path(), Some(upstream_port));
    let upstream_task = std::thread::spawn(move || {
        let (mut socket, _) = upstream.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut request = [0; 6];
        socket.read_exact(&mut request).unwrap();
        assert_eq!(&request, b"hello\0");
        socket.write_all(b"remote-response\0\xff").unwrap();
    });
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
    let io = std::thread::spawn(move || {
        let mut input = input;
        let mut output = output;
        let mut ready = [0; b"COPORT-FORWARD/1\n".len()];
        // Read the protocol marker separately from HTTP bytes.
        let marker = b"COPORT-FORWARD/1\n";
        output.read_exact(&mut ready[..marker.len()]).unwrap();
        assert_eq!(&ready[..marker.len()], marker);
        write!(
            input,
            "CONNECT 127.0.0.1:{upstream_port} HTTP/1.1\r\nHost: 127.0.0.1:{upstream_port}\r\n\r\n"
        )
        .unwrap();
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            output.read_exact(&mut byte).unwrap();
            head.push(byte[0]);
            assert!(head.len() < 4096);
        }
        assert!(head.starts_with(b"HTTP/1.1 200"));
        input.write_all(b"hello\0").unwrap();
        let mut response = Vec::new();
        // Keep stdin open: a closed remote socket must still terminate the helper.
        output.read_to_end(&mut response).unwrap();
        sender.send(response).unwrap();
    });
    let response = receiver.recv_timeout(Duration::from_secs(10));
    if response.is_err() {
        let _ = child.kill();
    }
    let response = response.unwrap();
    io.join().unwrap();
    assert_eq!(response, b"remote-response\0\xff");
    upstream_task.join().unwrap();
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
    let port = fixture(dir.path(), None);
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
    std::fs::write(dir.path().join("gui.json"), r#"{"managed_devices":[{"id":"test-device","name":"Remote","ssh":{"host":"fake-host","binary":"coportd"},"data":null}]}"#).unwrap();
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
    let (client, status) = daemon::start(
        &wrapper,
        dir.path(),
        &dir.path().join("config.yaml"),
        &dir.path().join("proxy.log"),
    )
    .unwrap();
    assert!(status.forwarding_supported);
    let rt = tokio::runtime::Runtime::new().unwrap();
    let client = rt.block_on(async {
        // The daemon checks configured devices without any GUI request to inspect them.
        let started=Instant::now();
        loop {
            let checks=client.device_capabilities().await.unwrap();
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
        let (client, restored) = daemon::start(&wrapper, dir.path(), &dir.path().join("config.yaml"), &dir.path().join("proxy.log")).unwrap();
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
    fixture(dir.path(), None);
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
