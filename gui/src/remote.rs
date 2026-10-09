//! Read-only SSH summary transport. Discovery stays on the destination; only
//! processed statistics cross SSH. Local CLI lifecycle helpers are separate.
use serde::{Deserialize, Serialize};
use std::{io, path::Path};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Device {
    pub name: String,
    pub host: String,
    #[serde(default)]
    pub binary: String,
}

impl Device {
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty()
            || self.name.len() > 128
            || self.name.chars().any(char::is_control)
        {
            return Err("Provide a device name (up to 128 bytes).".into());
        }
        if self.host.is_empty()
            || self.host.len() > 255
            || self.host.starts_with('-')
            || !self
                .host
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"._-@:".contains(&c))
        {
            return Err("Use an SSH config alias or user@hostname.".into());
        }
        if self.binary.len() > 4096 || self.binary.chars().any(char::is_control) {
            return Err("Provide the remote coportd executable path.".into());
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Status,
    Start,
    Stop,
    Restart,
}
impl Action {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "status" => Some(Self::Status),
            "start" => Some(Self::Start),
            "stop" => Some(Self::Stop),
            "restart" => Some(Self::Restart),
            _ => None,
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct ControlStatus {
    pub running: bool,
    pub daemon: Option<crate::daemon::Status>,
}

/// Executed on the destination machine, using that user's existing GUI data.
pub fn control(action: Action) -> io::Result<ControlStatus> {
    control_in(action, &crate::settings::app_dir())
}

pub fn control_in(action: Action, dir: &Path) -> io::Result<ControlStatus> {
    control_at(action, dir, &std::env::current_exe()?)
}

fn control_at(action: Action, dir: &Path, binary: &Path) -> io::Result<ControlStatus> {
    let found = crate::daemon::Client::discover(dir);
    // Never start over an unresponsive daemon which still holds its lock.
    if found.is_none() && dir.join("daemon.lock").exists() {
        let lock = crate::daemon::lock_file(&dir.join("daemon.lock"))?;
        lock.try_lock().map_err(|_| {
            io::Error::other("The daemon is unresponsive; inspect it on the remote device.")
        })?;
    }
    if matches!(action, Action::Restart) || (matches!(action, Action::Start) && found.is_none()) {
        coport::config::Config::read(&dir.join("config.yaml"))
            .and_then(|config| config.check_external_data())
            .map_err(io::Error::other)?;
    }
    match action {
        Action::Status => {}
        Action::Stop | Action::Restart => {
            if let Some((client, _)) = &found {
                client.stop()?;
            }
        }
        Action::Start => {}
    }
    if matches!(action, Action::Restart) || (matches!(action, Action::Start) && found.is_none()) {
        let (_, status) = crate::daemon::start(
            binary,
            dir,
            &dir.join("config.yaml"),
            &dir.join("logs/proxy.log"),
        )?;
        return Ok(ControlStatus {
            running: true,
            daemon: Some(status),
        });
    }
    let status = if matches!(action, Action::Stop) {
        None
    } else {
        found.map(|(_, status)| status)
    };
    Ok(ControlStatus {
        running: status.is_some(),
        daemon: status,
    })
}

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

const DISCOVERY_SCRIPT: &str = include_str!("ssh_summary.sh");

fn summary_command(binary: &str) -> String {
    helper_command(binary, "--summary")
}

pub(crate) fn helper_command(binary: &str, operation: &str) -> String {
    assert!(matches!(operation, "--summary" | "--forward"));
    if binary.is_empty() || binary == "coportd" {
        format!(
            "/bin/sh -c {}",
            quote(&DISCOVERY_SCRIPT.replace("--summary", operation))
        )
    } else {
        format!("{} {operation}", quote(binary))
    }
}

/// Published privately at daemon startup, never by a summary request.
pub(crate) fn register_summary_executable(dir: &Path, binary: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let path = binary.as_os_str().as_bytes();
        if !binary.is_absolute() || path.len() > 4096 || path.iter().any(u8::is_ascii_control) {
            return Err(io::Error::other(
                "Invalid Coport executable path for SSH discovery",
            ));
        }
        let mut record = path.to_vec();
        record.push(b'\n');
        crate::settings::write_private(&dir.join("summary-executable"), &record)?;
    }
    #[cfg(not(unix))]
    let _ = (dir, binary);
    Ok(())
}

fn ssh_command(device: &Device, command: &str) -> Result<tokio::process::Command, String> {
    device.validate()?;
    let mut child = tokio::process::Command::new("ssh");
    child
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
            "ConnectionAttempts=1",
            "-o",
            "ServerAliveInterval=5",
            "-o",
            "ServerAliveCountMax=1",
            "--",
            &device.host,
            command,
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    Ok(child)
}

/// A login check only: no summary query, forwarding channel, or daemon lifecycle action.
pub async fn check_connection(device: &Device) -> Result<(), String> {
    check_command(
        ssh_command(device, "exit 0")?,
        std::time::Duration::from_secs(10),
    )
    .await
}

async fn check_command(
    mut command: tokio::process::Command,
    timeout: std::time::Duration,
) -> Result<(), String> {
    let mut child = command
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("Cannot start SSH: {e}"))?;
    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) if status.success() => Ok(()),
        Ok(Ok(_)) => {
            Err("SSH connection failed; check the host, authentication and known host key.".into())
        }
        Ok(Err(error)) => Err(format!("Cannot check SSH: {error}")),
        Err(_) => {
            let _ = child.kill().await;
            Err("SSH connection check timed out.".into())
        }
    }
}

pub async fn summary(device: &Device) -> Result<crate::data_api::Summary, String> {
    let limit = 1024 * 1024;
    let mut child = ssh_command(device, &summary_command(&device.binary))?;
    let mut process = child
        .spawn()
        .map_err(|e| format!("Cannot start SSH: {e}"))?;
    use tokio::io::AsyncReadExt;
    // Bounded reads prevent an unexpected executable/banner filling memory.
    let mut stdout = process.stdout.take().unwrap().take(limit as u64 + 1);
    let mut stderr = process.stderr.take().unwrap().take(8193);
    let operation = async {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let (a, b) = tokio::join!(stdout.read_to_end(&mut out), stderr.read_to_end(&mut err));
        a.map_err(|e| e.to_string())?;
        b.map_err(|e| e.to_string())?;
        if out.len() > limit || err.len() > 8192 {
            return Err("SSH response exceeded the size limit.".into());
        }
        let status = process.wait().await.map_err(|e| e.to_string())?;
        if !status.success() {
            return Err(format!(
                "SSH statistics request failed: {}",
                String::from_utf8_lossy(&err).trim()
            ));
        }
        serde_json::from_slice(&out)
            .map_err(|_| "Invalid statistics response; update Coport on the destination.".into())
    };
    match tokio::time::timeout(std::time::Duration::from_secs(25), operation).await {
        Ok(result) => {
            if result.is_err() {
                let _ = process.kill().await;
            }
            result
        }
        Err(_) => {
            let _ = process.kill().await;
            Err("SSH operation timed out.".into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ssh_availability_uses_noninteractive_login_without_remote_services() {
        let device = Device {
            name: "test".into(),
            host: "user@host".into(),
            binary: "/unused/coportd".into(),
        };
        let command = ssh_command(&device, "exit 0").unwrap();
        let args: Vec<_> = command
            .as_std()
            .get_args()
            .map(|s| s.to_str().unwrap())
            .collect();
        for option in [
            "BatchMode=yes",
            "StrictHostKeyChecking=yes",
            "ClearAllForwardings=yes",
            "PermitLocalCommand=no",
            "RemoteCommand=none",
            "ConnectionAttempts=1",
        ] {
            assert!(args.contains(&option));
        }
        assert_eq!(&args[args.len() - 3..], &["--", "user@host", "exit 0"]);
        assert!(!args.iter().any(|arg| arg.contains("coportd")
            || arg.contains("--summary")
            || arg.contains("--forward")));
    }

    #[test]
    fn ssh_check_fixture() {
        match std::env::var("COPORT_SSH_CHECK_FIXTURE").as_deref() {
            Ok("success") => std::process::exit(0),
            Ok("failure") => std::process::exit(7),
            Ok("timeout") => std::thread::sleep(std::time::Duration::from_secs(60)),
            _ => {}
        }
    }

    #[tokio::test]
    async fn ssh_check_reports_failure_and_times_out_without_network_access() {
        for (mode, expected) in [
            ("success", None),
            ("failure", Some("SSH connection failed")),
            ("timeout", Some("timed out")),
        ] {
            let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
            command
                .args(["--exact", "remote::tests::ssh_check_fixture"])
                .env("COPORT_SSH_CHECK_FIXTURE", mode);
            let timeout = if mode == "timeout" {
                std::time::Duration::from_millis(100)
            } else {
                std::time::Duration::from_secs(10)
            };
            let result = check_command(command, timeout).await;
            if let Some(expected) = expected {
                assert!(result.unwrap_err().contains(expected));
            } else {
                result.unwrap();
            }
        }
    }

    #[test]
    fn rejects_ssh_options_and_shell_syntax_in_host() {
        for host in [
            "-oProxyCommand=evil",
            "host;evil",
            "user@host\n",
            "$(evil)",
            "",
        ] {
            assert!(
                Device {
                    name: "test".into(),
                    host: host.into(),
                    binary: "coportd".into()
                }
                .validate()
                .is_err()
            );
        }
        assert!(
            Device {
                name: "test".into(),
                host: "user@my-host".into(),
                binary: "/path with spaces/coportd".into()
            }
            .validate()
            .is_ok()
        );
        assert_eq!(quote("/tmp/a'b"), "'/tmp/a'\\''b'");
    }
    #[cfg(unix)]
    fn discovery_fixture() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let dir = if cfg!(target_os = "macos") {
            home.path()
                .join("Library/Application Support/io.github.coport.gui")
        } else {
            home.path().join(".config/io.github.coport.gui")
        };
        let binary = home.path().join("build with spaces ' $literal/coportd");
        std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
        std::fs::write(&binary, b"#!/bin/sh\n[ \"$#\" = 1 ] && [ \"$1\" = --summary ] || exit 9\nprintf '%s\\n' '{\"result\":\"summary-only\"}'\n").unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        register_summary_executable(&dir, &binary).unwrap();
        (home, dir, binary)
    }

    #[cfg(unix)]
    fn run_discovery(home: &Path, binary: &str) -> std::process::Output {
        std::process::Command::new("/bin/sh")
            .args(["-c", &summary_command(binary)])
            .env("HOME", home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("PATH", "/usr/bin:/bin")
            .output()
            .unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn discovery_uses_private_registration_without_returning_paths_or_changing_files() {
        use std::os::unix::fs::PermissionsExt;
        let (home, dir, binary) = discovery_fixture();
        let marker = dir.join("summary-executable");
        let before = std::fs::read(&marker).unwrap();
        assert_eq!(
            std::fs::metadata(&marker).unwrap().permissions().mode() & 0o777,
            0o600
        );
        for mode in ["", "coportd", binary.to_str().unwrap()] {
            let output = run_discovery(home.path(), mode);
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(output.stdout, b"{\"result\":\"summary-only\"}\n");
            assert!(output.stderr.is_empty());
            assert_eq!(std::fs::read(&marker).unwrap(), before);
        }
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn discovery_falls_back_to_path_only_when_registration_is_absent() {
        let (home, dir, binary) = discovery_fixture();
        std::fs::remove_file(dir.join("summary-executable")).unwrap();
        let output = std::process::Command::new("/bin/sh")
            .args(["-c", &summary_command("")])
            .env("HOME", home.path())
            .env("XDG_CONFIG_HOME", home.path().join(".config"))
            .env("PATH", binary.parent().unwrap())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"{\"result\":\"summary-only\"}\n");
        assert!(!dir.join("summary-executable").exists());
    }
    #[cfg(unix)]
    #[test]
    fn discovery_rejects_public_symlinked_and_stale_records_without_falling_back() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let (home, dir, binary) = discovery_fixture();
        let marker = dir.join("summary-executable");
        std::fs::set_permissions(&marker, std::fs::Permissions::from_mode(0o644)).unwrap();
        let output = run_discovery(home.path(), "");
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("must be private"));
        std::fs::remove_file(&marker).unwrap();
        symlink(&binary, &marker).unwrap();
        assert!(!run_discovery(home.path(), "").status.success());
        std::fs::remove_file(&marker).unwrap();
        register_summary_executable(&dir, &binary).unwrap();
        std::fs::remove_file(&binary).unwrap();
        let output = run_discovery(home.path(), "");
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("unavailable"));
    }

    #[cfg(unix)]
    #[test]
    fn registration_rejects_control_characters_and_updates_after_relocation() {
        let (home, dir, binary) = discovery_fixture();
        assert!(register_summary_executable(&dir, Path::new("/tmp/bad\n/coportd")).is_err());
        assert!(register_summary_executable(&dir, Path::new("relative/coportd")).is_err());
        let moved = binary.with_file_name("new-coportd");
        std::fs::rename(&binary, &moved).unwrap();
        register_summary_executable(&dir, &moved).unwrap();
        let output = run_discovery(home.path(), "");
        assert!(output.status.success());
        assert_eq!(output.stdout, b"{\"result\":\"summary-only\"}\n");
        assert_eq!(
            std::fs::read(dir.join("summary-executable")).unwrap(),
            format!("{}\n", moved.display()).as_bytes()
        );
    }
    #[test]
    fn an_unresponsive_daemon_is_not_reported_as_stopped_or_replaced() {
        let _guard = crate::daemon::spawn_guard();
        let dir = tempfile::tempdir().unwrap();
        let lock = crate::daemon::lock_file(&dir.path().join("daemon.lock")).unwrap();
        lock.try_lock().unwrap();
        for action in [Action::Status, Action::Start, Action::Stop, Action::Restart] {
            assert!(control_at(action, dir.path(), Path::new("missing-binary")).is_err());
        }
    }
    #[test]
    fn control_adopts_and_stops_an_existing_daemon() {
        let _guard = crate::daemon::spawn_guard();
        let dir = tempfile::tempdir().unwrap();
        let (_signal, thread) = crate::daemon::tests::serve_in_thread(dir.path());
        let (_, status) = crate::daemon::tests::wait_for_daemon(dir.path());
        for action in [Action::Status, Action::Start] {
            let result = control_at(action, dir.path(), Path::new("missing-binary")).unwrap();
            assert!(result.running);
            assert_eq!(result.daemon.unwrap().pid, status.pid);
        }
        assert!(
            !control_at(Action::Stop, dir.path(), Path::new("missing-binary"))
                .unwrap()
                .running
        );
        thread.join().unwrap().unwrap();
    }
    #[test]
    fn status_and_stop_do_not_create_a_daemon() {
        let dir = tempfile::tempdir().unwrap();
        for action in [Action::Status, Action::Stop] {
            assert!(
                !control_at(action, dir.path(), Path::new("missing-binary"))
                    .unwrap()
                    .running
            );
        }
    }
}
