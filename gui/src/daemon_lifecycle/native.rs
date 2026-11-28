//! This computer's operating system.
use super::Platform;
use std::{io, path::Path, process::Command};

#[derive(Clone, Copy)]
pub struct Native;

impl Platform for Native {
    fn detach(&self, command: &mut Command) {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // SAFETY: setsid and signal are async-signal-safe and do not
            // access Rust state.
            unsafe {
                command.pre_exec(|| {
                    if libc::setsid() == -1 {
                        return Err(io::Error::last_os_error());
                    }
                    // An ignored signal stays ignored across exec: a Restart
                    // ignores these while it stops the old Daemon.
                    for signal in [libc::SIGINT, libc::SIGHUP, libc::SIGTERM] {
                        libc::signal(signal, libc::SIG_DFL);
                    }
                    Ok(())
                });
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0000_0008 | 0x0000_0200); // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP
        }
    }

    fn supervisor(&self, pid: u32) -> Option<String> {
        #[cfg(target_os = "linux")]
        {
            supervisor(
                &std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?,
                pid,
                |unit, user| {
                    let mut command = Command::new("systemctl");
                    if user {
                        command.arg("--user");
                    }
                    let output = command
                        .args(["show", "-p", "MainPID", "--value", "--", unit])
                        .output()
                        .ok()?;
                    output
                        .status
                        .success()
                        .then(|| String::from_utf8_lossy(&output.stdout).trim().parse().ok())
                        .flatten()
                },
            )
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = pid;
            None
        }
    }

    fn copy_program(&self, pid: u32, to: &Path) -> io::Result<bool> {
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::fs::PermissionsExt;
            // /proc names the running image even after a build replaced its file.
            let _ = std::fs::remove_file(to);
            std::fs::copy(format!("/proc/{pid}/exe"), to)?;
            std::fs::set_permissions(to, std::fs::Permissions::from_mode(0o700))?;
            Ok(true)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (pid, to);
            Ok(false)
        }
    }

    fn connections(&self, port: u16) -> Option<usize> {
        #[cfg(target_os = "linux")]
        {
            let tables = ["/proc/net/tcp", "/proc/net/tcp6"]
                .map(|path| std::fs::read_to_string(path).unwrap_or_default());
            Some(tables.iter().map(|table| established(table, port)).sum())
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = port;
            None
        }
    }
}

/// A cgroup is inherited by children, including detached Daemons launched
/// by CI runners or terminals. Only the service's main process is supervised.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn supervisor(
    cgroup: &str,
    pid: u32,
    mut main_pid: impl FnMut(&str, bool) -> Option<u32>,
) -> Option<String> {
    let unit = service_unit(cgroup)?;
    [false, true]
        .into_iter()
        .any(|user| main_pid(&unit, user) == Some(pid))
        .then_some(unit)
}

/// The systemd service in a `/proc/PID/cgroup` listing, unless it is a
/// desktop session or an app the desktop launched: those run the process
/// without starting it again.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn service_unit(cgroup: &str) -> Option<String> {
    cgroup.lines().find_map(|line| {
        let mut parts = line.splitn(3, ':');
        let (_, controllers, path) = (parts.next()?, parts.next()?, parts.next()?);
        let unit = path.rsplit('/').next()?;
        let desktop = unit.starts_with("gnome-session-manager@") || unit.starts_with("app-");
        ((controllers.is_empty() || controllers == "name=systemd")
            && unit.ends_with(".service")
            && !desktop)
            .then(|| unit.to_owned())
    })
}

/// Established connections on local `port` in a `/proc/net/tcp` table.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(super) fn established(table: &str, port: u16) -> usize {
    table
        .lines()
        .skip(1)
        .filter(|line| {
            let fields: Vec<_> = line.split_whitespace().collect();
            fields.get(3) == Some(&"01")
                && fields
                    .get(1)
                    .and_then(|local| local.rsplit(':').next())
                    .and_then(|hex| u16::from_str_radix(hex, 16).ok())
                    == Some(port)
        })
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn a_detached_program_does_not_inherit_ignored_signals() {
        // SAFETY: ignoring a signal installs no handler code.
        let previous = unsafe { libc::signal(libc::SIGHUP, libc::SIG_IGN) };
        let mut command = Command::new("grep");
        command.args(["^SigIgn:", "/proc/self/status"]);
        Native.detach(&mut command);
        let output = command.output();
        // SAFETY: restores the disposition read above.
        unsafe { libc::signal(libc::SIGHUP, previous) };
        let line = String::from_utf8(output.unwrap().stdout).unwrap();
        let ignored = u64::from_str_radix(line.trim_start_matches("SigIgn:").trim(), 16).unwrap();
        assert_eq!(ignored & (1 << (libc::SIGHUP - 1)), 0, "{line}");
    }

    #[test]
    fn only_a_service_that_is_no_desktop_session_supervises() {
        let unified = |path: &str| format!("0::{path}\n");
        for (cgroup, unit) in [
            (
                unified("/user.slice/user-1000.slice/user@1000.service/app.slice/coportd.service"),
                Some("coportd.service"),
            ),
            (
                "12:cpu:/\n1:name=systemd:/system.slice/coport.service\n".to_owned(),
                Some("coport.service"),
            ),
            (
                unified(
                    "/user.slice/user-1000.slice/user@1000.service/session.slice/gnome-session-manager@ubuntu.service",
                ),
                None,
            ),
            (
                unified(
                    "/user.slice/user-1000.slice/user@1000.service/app.slice/app-gnome-coport\\x2dgui@autostart.service",
                ),
                None,
            ),
            (
                unified(
                    "/user.slice/user-1000.slice/user@1000.service/app.slice/app-gnome-coport-4242.scope",
                ),
                None,
            ),
            (unified("/user.slice/user-1000.slice/session-3.scope"), None),
            ("4:memory:/system.slice/other.service\n".to_owned(), None),
        ] {
            assert_eq!(service_unit(&cgroup).as_deref(), unit, "{cgroup}");
        }
    }

    #[test]
    fn inherited_service_membership_does_not_supervise_a_child() {
        let cgroup = "0::/system.slice/hosted-compute-agent.service\n";
        assert_eq!(supervisor(cgroup, 42, |_, _| Some(7)), None);
        assert_eq!(supervisor(cgroup, 42, |_, _| None), None);
        assert_eq!(
            supervisor(cgroup, 7, |_, user| (!user).then_some(7)).as_deref(),
            Some("hosted-compute-agent.service")
        );
        let cgroup = "0::/user.slice/user-1000.slice/user@1000.service/app.slice/coportd.service\n";
        assert_eq!(
            supervisor(cgroup, 42, |_, user| user.then_some(42)).as_deref(),
            Some("coportd.service")
        );
    }

    #[test]
    fn only_established_connections_on_the_port_count() {
        let table = "  sl  local_address rem_address   st tx_queue rx_queue\n\
            0: 0100007F:1ED5 00000000:0000 0A 00000000:00000000\n\
            1: 0100007F:1ED5 0100007F:C350 01 00000000:00000000\n\
            2: 0100007F:C350 0100007F:1ED5 01 00000000:00000000\n\
            3: 0100007F:1ED5 0100007F:C351 06 00000000:00000000\n\
            4: 0100007F:1ED5 0100007F:C352 01 00000000:00000000\n";
        assert_eq!(established(table, 0x1ED5), 2);
        assert_eq!(established(table, 0xC350), 1);
        assert_eq!(established("", 0x1ED5), 0);
    }
}
