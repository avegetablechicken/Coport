#![cfg(target_os = "linux")]
mod common;
use coport::service::{Action, Platform, Service};
use std::{
    fs,
    net::TcpStream,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::Command,
    thread,
    time::{Duration, Instant},
};
struct Cleanup(Service);
impl Drop for Cleanup {
    fn drop(&mut self) {
        if self.0.registration.exists() {
            let _ = self.0.manage(Action::Uninstall, Path::new(""), None);
        }
    }
}
fn ready(port: u16) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(Instant::now() < deadline, "service did not start");
        thread::sleep(Duration::from_millis(100));
    }
    let (status, body) = common::request(port, None, None, "/health", "GET");
    assert_eq!(status, 200);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({"ok":true})
    );
}
fn pid(unit: &str) -> String {
    let output = Command::new("systemctl")
        .args(["--user", "show", unit, "-p", "MainPID", "--value"])
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap().trim().into()
}
#[test]
#[ignore = "requires Linux with a working systemd user session; installs a temporary user service"]
fn isolated_service_lifecycle() {
    assert!(
        Command::new("systemctl")
            .args(["--user", "show-environment"])
            .output()
            .unwrap()
            .status
            .success()
    );
    let dir = tempfile::tempdir().unwrap();
    let label = format!("local.coport.test-{}", uuid::Uuid::new_v4().simple());
    let unit = format!("{label}.service");
    let service = Cleanup(Service {
        platform: Platform::Linux,
        label,
        runtime: dir.path().join("runtime with spaces"),
        registration: dirs::config_dir().unwrap().join("systemd/user").join(&unit),
    });
    let s = &service.0;
    fs::create_dir(&s.runtime).unwrap();
    fs::set_permissions(&s.runtime, fs::Permissions::from_mode(0o700)).unwrap();
    let environment = s.runtime.join("service.env");
    fs::write(&environment, "CODING_PROXY_SYSTEMD_TEST_VALUE=loaded\n").unwrap();
    fs::set_permissions(environment, fs::Permissions::from_mode(0o600)).unwrap();
    let port = common::port();
    let config = dir.path().join("config.yaml");
    fs::write(
        &config,
        format!("listen_port: {port}\nrequest_timeout_seconds: 3\n"),
    )
    .unwrap();
    let binary = common::binary().canonicalize().unwrap();
    assert_eq!(
        s.manage(Action::Install, &binary, Some(&config)).unwrap(),
        0
    );
    ready(port);
    assert_eq!(
        common::request(port, None, None, "/responses", "GET").0,
        401
    );
    let before = pid(&unit);
    assert_ne!(before, "0");
    assert!(
        fs::read(format!("/proc/{before}/environ"))
            .unwrap()
            .split(|b| *b == 0)
            .any(|v| v == b"CODING_PROXY_SYSTEMD_TEST_VALUE=loaded")
    );
    let preserved = fs::read(s.runtime.join("config.yaml")).unwrap();
    fs::write(&config, "invalid checkout settings\n").unwrap();
    assert_eq!(s.manage(Action::Update, &binary, Some(&config)).unwrap(), 0);
    assert_eq!(fs::read(s.runtime.join("config.yaml")).unwrap(), preserved);
    assert_eq!(pid(&unit), before);
    assert_eq!(
        s.manage(Action::Restart, &binary, Some(&config)).unwrap(),
        0
    );
    ready(port);
    let after = pid(&unit);
    assert_ne!(after, "0");
    assert_ne!(after, before);
    assert_eq!(
        fs::metadata(s.runtime.join("logs/proxy.log"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(s.manage(Action::Stop, &binary, Some(&config)).unwrap(), 0);
    assert!(TcpStream::connect(("127.0.0.1", port)).is_err());
    assert_eq!(
        s.manage(Action::Uninstall, &binary, Some(&config)).unwrap(),
        0
    );
    assert!(!s.registration.exists());
    assert_eq!(fs::read(s.runtime.join("config.yaml")).unwrap(), preserved);
}
