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

fn fixture(dir: &Path, upstream: Option<u16>) -> u16 {
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
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
    let mut control =
        TcpStream::connect(("127.0.0.1", endpoint["port"].as_u64().unwrap() as u16)).unwrap();
    control
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let bytes = br#"{"token":"wrong","action":"Stop"}"#;
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
