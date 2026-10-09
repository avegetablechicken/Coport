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
        discovery_command(DISCOVERY_SCRIPT, operation)
    } else {
        format!("{} {operation}", quote(binary))
    }
}

fn discovery_command(script: &str, operation: &str) -> String {
    // include_str! preserves checkout line endings; POSIX sh requires LF.
    format!(
        "/bin/sh -c {}",
        quote(&script.replace("\r\n", "\n").replace("--summary", operation))
    )
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

/// One SSH process sends the selected window, then the complete snapshot.
pub async fn summary_progressive(
    device: &Device,
    minutes: u64,
    scope: crate::traffic::TrafficScope,
    mut on_summary: impl FnMut(crate::data_api::Summary) -> Result<(), String>,
) -> Result<(), String> {
    crate::data_api::bucket_minutes(minutes).ok_or("Unsupported traffic range")?;
    let scope = match scope {
        crate::traffic::TrafficScope::Model => "model",
        crate::traffic::TrafficScope::All => "all",
    };
    // Four arguments also make old helpers reject this as an unknown command:
    // three positional arguments would otherwise be interpreted as daemon startup.
    let operation = format!("--summary-stream --window {minutes} {scope}");
    let command = if device.binary.is_empty() || device.binary == "coportd" {
        discovery_command(DISCOVERY_SCRIPT, &operation)
    } else {
        format!("{} {operation}", quote(&device.binary))
    };
    let mut process = ssh_command(device, &command)?
        .spawn()
        .map_err(|e| format!("Cannot start SSH: {e}"))?;
    use tokio::io::AsyncReadExt;
    let stdout = process.stdout.take().unwrap();
    let mut stderr = process.stderr.take().unwrap().take(8193);
    let mut received = false;
    let mut errors = Vec::new();
    let mut exit_code = None;
    let operation = async {
        let read = read_summary_stream(stdout, |summary| {
            received = true;
            on_summary(summary)
        });
        let (result, error_result) = tokio::join!(read, stderr.read_to_end(&mut errors));
        error_result.map_err(|e| e.to_string())?;
        if errors.len() > 8192 {
            return Err("SSH response exceeded the size limit.".into());
        }
        let status = process.wait().await.map_err(|e| e.to_string())?;
        exit_code = status.code();
        result?;
        if !status.success() {
            return Err("SSH statistics request failed.".into());
        }
        Ok(())
    };
    let result = match tokio::time::timeout(std::time::Duration::from_secs(60), operation).await {
        Ok(result) => result,
        Err(_) => Err("SSH statistics request timed out.".into()),
    };
    if result.is_err() {
        let _ = process.kill().await;
        // Older helpers reject the new option without changing any state.
        if legacy_summary_retry(received, exit_code, &errors) {
            let legacy = summary(device).await?;
            legacy.validate()?;
            return on_summary(legacy);
        }
    }
    result
}

fn legacy_summary_retry(received: bool, exit_code: Option<i32>, errors: &[u8]) -> bool {
    // Only a known old helper's usage error proves lack of protocol support.
    // Denied SSH access, timeouts and malformed output must not reconnect.
    !received
        && exit_code == Some(2)
        && errors
            .windows(b"Usage: coportd".len())
            .any(|part| part == b"Usage: coportd")
}

async fn read_summary_stream(
    input: impl tokio::io::AsyncRead + Unpin,
    mut on_summary: impl FnMut(crate::data_api::Summary) -> Result<(), String>,
) -> Result<(), String> {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt};
    let mut input = tokio::io::BufReader::new(input);
    for index in 0..2 {
        let mut bytes = Vec::new();
        (&mut input)
            .take(1024 * 1024 + 2)
            .read_until(b'\n', &mut bytes)
            .await
            .map_err(|_| "Cannot read SSH statistics stream")?;
        if bytes.len() > 1024 * 1024 + 1 || !bytes.ends_with(b"\n") {
            return Err("Invalid or oversized SSH statistics frame.".into());
        }
        let summary: crate::data_api::Summary =
            serde_json::from_slice(&bytes).map_err(|_| "Invalid SSH statistics frame")?;
        summary.validate()?;
        // Forced-command SSH keys may always return the original single DTO.
        let legacy = index == 0 && summary.schema_version < 4;
        if index == 1 && summary.schema_version != 3 {
            return Err("Unexpected SSH statistics frame.".into());
        }
        on_summary(summary)?;
        if legacy {
            break;
        }
    }
    let mut extra = [0];
    if input
        .read(&mut extra)
        .await
        .map_err(|_| "Cannot finish SSH statistics stream")?
        != 0
    {
        return Err("Unexpected extra SSH statistics frame.".into());
    }
    Ok(())
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

    #[tokio::test]
    async fn selected_ssh_frame_arrives_before_the_remaining_statistics() {
        use tokio::io::AsyncWriteExt;
        let end = chrono::Utc::now().timestamp_millis() / 60_000 * 60_000;
        let window = |minutes, scope, at| crate::data_api::Window {
            minutes,
            scope,
            window_start: at - minutes as i64 * 60_000,
            window_end: at,
            bucket_minutes: crate::data_api::bucket_minutes(minutes).unwrap(),
            groups: Vec::new(),
        };
        let mut first = crate::data_api::Summary {
            schema_version: 4,
            node_id: uuid::Uuid::new_v4().to_string(),
            window_start: end - 30 * 60_000,
            window_end: end,
            bucket_minutes: 1,
            groups: Vec::new(),
            windows: vec![window(1440, crate::traffic::TrafficScope::All, end)],
            previous_windows: [60_000, 120_000]
                .into_iter()
                .map(|offset| window(1440, crate::traffic::TrafficScope::All, end - offset))
                .collect(),
        };
        let first_bytes = serde_json::to_vec(&first).unwrap();
        first.schema_version = 3;
        first.windows.clear();
        first.previous_windows.clear();
        for offset in [0, 60_000, 120_000] {
            for minutes in crate::data_api::RANGES {
                for scope in [
                    crate::traffic::TrafficScope::Model,
                    crate::traffic::TrafficScope::All,
                ] {
                    let w = window(minutes, scope, end - offset);
                    if offset == 0 {
                        first.windows.push(w);
                    } else {
                        first.previous_windows.push(w);
                    }
                }
            }
        }
        let rest_bytes = serde_json::to_vec(&first).unwrap();
        let mut legacy = rest_bytes.clone();
        legacy.push(b'\n');
        let mut legacy_versions = Vec::new();
        super::read_summary_stream(legacy.as_slice(), |s| {
            legacy_versions.push(s.schema_version);
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(legacy_versions, [3]);
        let (mut writer, reader) = tokio::io::duplex(4096);
        let (release, ready) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            writer.write_all(&first_bytes).await.unwrap();
            writer.write_all(b"\n").await.unwrap();
            ready.await.unwrap(); // Cannot finish until the caller renders the first frame.
            writer.write_all(&rest_bytes).await.unwrap();
            writer.write_all(b"\n").await.unwrap();
        });
        let mut release = Some(release);
        let mut versions = Vec::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            super::read_summary_stream(reader, |summary| {
                versions.push(summary.schema_version);
                if let Some(release) = release.take() {
                    release.send(()).unwrap();
                }
                Ok(())
            }),
        )
        .await
        .unwrap()
        .unwrap();
        server.await.unwrap();
        assert_eq!(versions, [4, 3]);
        for bytes in [b"{}\n".to_vec(), vec![b'x'; 1024 * 1024 + 2]] {
            assert!(
                super::read_summary_stream(bytes.as_slice(), |_| Ok(()))
                    .await
                    .is_err()
            );
        }
    }
    use super::*;
    #[test]
    fn legacy_fallback_never_retries_denied_or_timed_out_ssh() {
        let usage = b"Usage: coportd ... --summary";
        assert!(legacy_summary_retry(false, Some(2), usage));
        for code in [None, Some(0), Some(1), Some(255)] {
            assert!(!legacy_summary_retry(false, code, usage));
        }
        assert!(!legacy_summary_retry(false, Some(2), b"Permission denied"));
        assert!(!legacy_summary_retry(true, Some(2), usage));
    }
    #[test]
    fn discovery_command_is_independent_of_checkout_line_endings() {
        let lf = DISCOVERY_SCRIPT.replace("\r\n", "\n");
        let crlf = lf.replace('\n', "\r\n");
        for operation in ["--summary", "--forward"] {
            let command = discovery_command(&crlf, operation);
            assert_eq!(command, discovery_command(&lf, operation));
            assert!(!command.contains('\r'));
        }
    }
    #[cfg(unix)]
    #[test]
    fn lf_and_crlf_discovery_scripts_execute_the_registered_helper() {
        let (home, _, _) = discovery_fixture();
        let lf = DISCOVERY_SCRIPT.replace("\r\n", "\n");
        for script in [lf.clone(), lf.replace('\n', "\r\n")] {
            let output = std::process::Command::new("/bin/sh")
                .args(["-c", &discovery_command(&script, "--summary")])
                .env("HOME", home.path())
                .env("XDG_CONFIG_HOME", home.path().join(".config"))
                .env("PATH", "/usr/bin:/bin")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(output.stdout, b"{\"result\":\"summary-only\"}\n");
        }
    }
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
