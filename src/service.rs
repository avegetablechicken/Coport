//! Per-user service installation. Registrations launch the native executable directly.
use base64::{Engine, engine::general_purpose::STANDARD};
use clap::{Args, ValueEnum};
use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Action {
    Install,
    Update,
    Status,
    Stop,
    Restart,
    Uninstall,
}

#[derive(Args)]
pub struct ServiceArgs {
    #[arg(value_enum)]
    pub action: Action,
    /// Executable to install (defaults to this executable).
    #[arg(long)]
    pub binary: Option<PathBuf>,
    #[arg(long, default_value = "config.yaml")]
    pub config: PathBuf,
}

#[derive(Clone, Copy, Debug)]
pub enum Platform {
    Mac,
    Linux,
    Windows,
}

pub struct Service {
    pub platform: Platform,
    pub label: String,
    pub runtime: PathBuf,
    pub registration: PathBuf,
}

impl Service {
    pub fn current() -> Result<Self> {
        let home = dirs::home_dir().ok_or("Cannot locate home directory")?;
        let label = "local.coport.rust".to_owned();
        let (platform, runtime, registration) = if cfg!(target_os = "macos") {
            (
                Platform::Mac,
                home.join("Library/Application Support/coport-rust"),
                home.join("Library/LaunchAgents")
                    .join(format!("{label}.plist")),
            )
        } else if cfg!(target_os = "linux") {
            (
                Platform::Linux,
                dirs::data_local_dir()
                    .ok_or("Cannot locate data directory")?
                    .join("coport-rust"),
                dirs::config_dir()
                    .ok_or("Cannot locate config directory")?
                    .join("systemd/user")
                    .join(format!("{label}.service")),
            )
        } else if cfg!(target_os = "windows") {
            (
                Platform::Windows,
                dirs::data_local_dir()
                    .ok_or("Cannot locate data directory")?
                    .join("coport-rust"),
                PathBuf::new(),
            )
        } else {
            return Err("Supported platforms: macOS, Linux and Windows".into());
        };
        Ok(Self {
            platform,
            label,
            runtime,
            registration,
        })
    }

    pub fn manage(&self, action: Action, source: &Path, config: &Path) -> Result<i32> {
        self.manage_with(action, source, config, &mut |program, args, quiet| {
            let mut command = Command::new(program);
            command.args(args);
            if quiet {
                command.stdout(Stdio::null()).stderr(Stdio::null());
            }
            Ok(command.status()?.code().unwrap_or(1))
        })
    }

    fn manage_with(
        &self,
        action: Action,
        source: &Path,
        config: &Path,
        run: &mut impl FnMut(&str, &[String], bool) -> io::Result<i32>,
    ) -> Result<i32> {
        if matches!(action, Action::Install | Action::Update) {
            let exists = match self.platform {
                Platform::Windows => {
                    run(
                        "powershell.exe",
                        &powershell_args(&self.windows_script("exists")),
                        true,
                    )? == 0
                }
                _ => self.registration.exists(),
            };
            if action == Action::Install && exists {
                return Err("Service already installed; use update, then restart".into());
            }
            if action == Action::Update && !exists {
                return Err("Service is not installed; use install first".into());
            }
            stage(&self.runtime, source, config, self.platform)?;
            if action == Action::Update {
                println!(
                    "Updated executable; retained {}. Restart to apply.",
                    self.runtime.join("config.yaml").display()
                );
                return Ok(0);
            }
        }
        let mut execute =
            |program: &str, args: Vec<String>, quiet: bool, allowed: &[i32]| -> Result<i32> {
                let code = run(program, &args, quiet)?;
                if !allowed.contains(&code) {
                    return Err(format!("{program} failed with exit code {code}").into());
                }
                Ok(code)
            };
        match self.platform {
            Platform::Linux => {
                let unit = format!("{}.service", self.label);
                let args = |parts: &[&str]| {
                    let mut args = vec!["--user".into()];
                    args.extend(parts.iter().map(|s| s.to_string()));
                    args
                };
                match action {
                    Action::Install => {
                        private_write(&self.registration, self.systemd_unit()?.as_bytes())?;
                        execute("systemctl", args(&["daemon-reload"]), false, &[0])?;
                        execute("systemctl", args(&["enable", "--now", &unit]), false, &[0])?;
                    }
                    Action::Uninstall => {
                        execute("systemctl", args(&["stop", &unit]), false, &[0, 5])?;
                        execute("systemctl", args(&["disable", &unit]), false, &[0])?;
                        remove_if_exists(&self.registration)?;
                        execute("systemctl", args(&["daemon-reload"]), false, &[0])?;
                    }
                    _ => {
                        return Ok(run(
                            "systemctl",
                            &args(&[action_name(action), &unit]),
                            false,
                        )?);
                    }
                }
            }
            Platform::Mac => {
                #[cfg(unix)]
                let uid = unsafe { libc::getuid() };
                #[cfg(not(unix))]
                let uid = 0;
                let domain = format!("gui/{uid}");
                let target = format!("{domain}/{}", self.label);
                let registration = self.registration.to_string_lossy().into_owned();
                let args = |parts: &[&str]| parts.iter().map(|s| s.to_string()).collect();
                match action {
                    Action::Install => {
                        for name in ["service.stdout.log", "service.stderr.log"] {
                            let path = self.runtime.join("logs").join(name);
                            if !path.exists() {
                                private_write(&path, b"")?;
                            }
                        }
                        private_write(&self.registration, self.launchd_plist().as_bytes())?;
                        execute("launchctl", args(&["enable", &target]), false, &[0])?;
                        execute(
                            "launchctl",
                            args(&["bootstrap", &domain, &registration]),
                            false,
                            &[0],
                        )?;
                    }
                    Action::Status => {
                        return Ok(run("launchctl", &args(&["print", &target]), false)?);
                    }
                    Action::Stop => {
                        execute("launchctl", args(&["bootout", &target]), false, &[0])?;
                    }
                    Action::Restart | Action::Uninstall => {
                        let loaded = run("launchctl", &args(&["print", &target]), true)? == 0;
                        let parts = if action == Action::Restart {
                            if loaded {
                                vec!["kickstart", "-k", &target]
                            } else {
                                vec!["bootstrap", &domain, &registration]
                            }
                        } else if loaded {
                            vec!["bootout", &target]
                        } else {
                            vec![]
                        };
                        if !parts.is_empty() {
                            let code = run("launchctl", &args(&parts), false)?;
                            if code != 0 {
                                return Err(
                                    format!("launchctl failed with exit code {code}").into()
                                );
                            }
                        }
                        if action == Action::Uninstall {
                            remove_if_exists(&self.registration)?;
                        }
                    }
                    Action::Update => unreachable!(),
                }
            }
            Platform::Windows => {
                return Ok(run(
                    "powershell.exe",
                    &powershell_args(&self.windows_script(action_name(action))),
                    false,
                )?);
            }
        }
        println!(
            "{}: {}; runtime: {}",
            action_name(action),
            self.label,
            self.runtime.display()
        );
        Ok(0)
    }

    pub fn systemd_unit(&self) -> Result<String> {
        let work = self.runtime.to_string_lossy().replace('%', "%%");
        if work.contains(['\n', '\r']) {
            return Err("The systemd runtime directory cannot contain newlines".into());
        }
        let binary = systemd_quote(&self.runtime.join("coport").to_string_lossy());
        let config = systemd_quote(&self.runtime.join("config.yaml").to_string_lossy());
        Ok(format!(
            "[Unit]\nDescription=Coport (Rust)\nAfter=network.target\n\n[Service]\nType=simple\nWorkingDirectory={work}/\nExecStart={binary} --config {config}\nEnvironmentFile=-{work}/service.env\nRestart=on-failure\nRestartSec=10\nUMask=0077\nTimeoutStopSec=10\n\n[Install]\nWantedBy=default.target\n"
        ))
    }

    fn launchd_plist(&self) -> String {
        let path = |name: &str| xml(&self.runtime.join(name).to_string_lossy());
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict>\n<key>Label</key><string>{}</string>\n<key>ProgramArguments</key><array><string>{}</string><string>--config</string><string>{}</string></array>\n<key>WorkingDirectory</key><string>{}</string>\n<key>RunAtLoad</key><true/><key>KeepAlive</key><true/>\n<key>ThrottleInterval</key><integer>10</integer><key>ExitTimeOut</key><integer>10</integer><key>Umask</key><integer>63</integer>\n<key>StandardOutPath</key><string>{}</string>\n<key>StandardErrorPath</key><string>{}</string>\n</dict></plist>\n",
            xml(&self.label),
            path("coport"),
            path("config.yaml"),
            xml(&self.runtime.to_string_lossy()),
            path("logs/service.stdout.log"),
            path("logs/service.stderr.log")
        )
    }

    fn windows_script(&self, action: &str) -> String {
        let name = ps_literal(&self.label);
        match action {
            "exists" => {
                return format!(
                    "if (Get-ScheduledTask -TaskName {name} -ErrorAction SilentlyContinue) {{ exit 0 }} else {{ exit 1 }}"
                );
            }
            "status" => {
                return format!(
                    "Get-ScheduledTask -TaskName {name}; Get-ScheduledTaskInfo -TaskName {name}"
                );
            }
            "stop" => return format!("Stop-ScheduledTask -TaskName {name}"),
            "restart" => {
                return format!(
                    "Stop-ScheduledTask -TaskName {name}; Start-ScheduledTask -TaskName {name}"
                );
            }
            "uninstall" => {
                return format!(
                    "Stop-ScheduledTask -TaskName {name}; Unregister-ScheduledTask -TaskName {name} -Confirm:$false"
                );
            }
            _ => {}
        }
        let binary = ps_literal(&self.runtime.join("coport.exe").to_string_lossy());
        let config = self.runtime.join("config.yaml");
        let args = ps_literal(&format!("--config \"{}\"", config.to_string_lossy()));
        let work = ps_literal(&self.runtime.to_string_lossy());
        format!(
            "$identity = [System.Security.Principal.WindowsIdentity]::GetCurrent().Name; $action = New-ScheduledTaskAction -Execute {binary} -Argument {args} -WorkingDirectory {work}; $trigger = New-ScheduledTaskTrigger -AtLogOn -User $identity; $principal = New-ScheduledTaskPrincipal -UserId $identity -LogonType Interactive -RunLevel Limited; $settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) -RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1) -MultipleInstances IgnoreNew -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries; Register-ScheduledTask -TaskName {name} -Action $action -Trigger $trigger -Principal $principal -Settings $settings; Start-ScheduledTask -TaskName {name}"
        )
    }
}

fn action_name(action: Action) -> &'static str {
    match action {
        Action::Install => "install",
        Action::Update => "update",
        Action::Status => "status",
        Action::Stop => "stop",
        Action::Restart => "restart",
        Action::Uninstall => "uninstall",
    }
}
fn ps_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}
fn powershell_args(script: &str) -> Vec<String> {
    let bytes: Vec<_> = format!("$ErrorActionPreference = 'Stop'; {script}")
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect();
    vec![
        "-NoProfile".into(),
        "-NonInteractive".into(),
        "-EncodedCommand".into(),
        STANDARD.encode(bytes),
    ]
}
fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
fn systemd_quote(value: &str) -> String {
    format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
            .replace('$', "$$")
            .replace('\n', "\\n")
            .replace('\r', "\\r")
    )
}
fn remove_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}
fn private_dir(path: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}
fn private_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or("Missing parent directory")?;
    private_dir(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist(path)?;
    Ok(())
}
fn stage(runtime: &Path, source: &Path, config: &Path, platform: Platform) -> Result<()> {
    if !source.is_file() {
        return Err("Executable not found; build first or pass --binary".into());
    }
    let destination_config = runtime.join("config.yaml");
    if !destination_config.exists() && !config.is_file() {
        return Err("Create config.yaml first or pass --config".into());
    }
    private_dir(runtime)?;
    let destination = runtime.join(if matches!(platform, Platform::Windows) {
        "coport.exe"
    } else {
        "coport"
    });
    let mut staged = tempfile::NamedTempFile::new_in(runtime)?;
    io::copy(&mut fs::File::open(source)?, staged.as_file_mut())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        staged
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o700))?;
    }
    staged.as_file().sync_all()?;
    staged.persist(destination).map_err(|e| {
        format!("Cannot replace executable (stop the task before updating on Windows): {e}")
    })?;
    if !destination_config.exists() {
        private_write(&destination_config, &fs::read(config)?)?;
    }
    private_dir(&runtime.join("logs"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn service(root: &Path) -> Service {
        Service {
            platform: Platform::Linux,
            label: "test.coport".into(),
            runtime: root.join("runtime"),
            registration: root.join("units/test.service"),
        }
    }
    #[test]
    fn update_preserves_config_and_never_restarts() {
        let dir = tempfile::tempdir().unwrap();
        let s = service(dir.path());
        fs::create_dir_all(&s.runtime).unwrap();
        fs::write(s.runtime.join("config.yaml"), "private settings").unwrap();
        private_write(&s.registration, b"installed").unwrap();
        let binary = dir.path().join("binary");
        fs::write(&binary, b"new executable").unwrap();
        assert_eq!(
            s.manage_with(
                Action::Update,
                &binary,
                &dir.path().join("missing"),
                &mut |_, _, _| panic!("update invoked service manager")
            )
            .unwrap(),
            0
        );
        assert_eq!(
            fs::read_to_string(s.runtime.join("config.yaml")).unwrap(),
            "private settings"
        );
        assert_eq!(
            fs::read(s.runtime.join("coport")).unwrap(),
            b"new executable"
        );
    }
    #[test]
    fn registrations_escape_paths_and_launch_native_binary() {
        let mut s = service(Path::new("/home/test user/100% \"quoted\"/&proxy"));
        s.runtime = PathBuf::from("/home/test user/100% \"quoted\"/&proxy/runtime");
        let unit = s.systemd_unit().unwrap();
        assert!(unit.contains("100%% \\\"quoted\\\""));
        assert!(
            unit.contains("WorkingDirectory=/home/test user/100%% \"quoted\"/&proxy/runtime/\n")
        );
        assert!(unit.contains(
            "EnvironmentFile=-/home/test user/100%% \"quoted\"/&proxy/runtime/service.env\n"
        ));
        assert!(unit.contains("Restart=on-failure"));
        let plist = s.launchd_plist();
        assert!(plist.contains("&amp;proxy"));
        assert!(plist.contains("<integer>63</integer>"));
        s.runtime = PathBuf::from("C:/Users/O'Brien/Proxy App");
        let script = s.windows_script("install");
        assert!(script.contains("O''Brien"));
        assert!(script.contains("-LogonType Interactive -RunLevel Limited"));
        assert!(script.contains("coport.exe"));
        assert!(script.contains("[TimeSpan]::Zero"));
        let encoded = powershell_args(&script);
        let decoded = STANDARD.decode(&encoded[3]).unwrap();
        let units: Vec<_> = decoded
            .chunks_exact(2)
            .map(|p| u16::from_le_bytes([p[0], p[1]]))
            .collect();
        assert_eq!(
            String::from_utf16(&units).unwrap(),
            format!("$ErrorActionPreference = 'Stop'; {script}")
        );
    }
    #[test]
    fn uninstall_unloaded_unit_but_propagate_other_stop_errors() {
        for code in [5, 1] {
            let dir = tempfile::tempdir().unwrap();
            let s = service(dir.path());
            private_write(&s.registration, b"invalid unit").unwrap();
            let result = s.manage_with(
                Action::Uninstall,
                Path::new(""),
                Path::new(""),
                &mut |_, args, _| Ok(if args[1] == "stop" { code } else { 0 }),
            );
            assert_eq!(result.is_ok(), code == 5);
            assert_eq!(s.registration.exists(), code != 5);
        }
    }
    #[test]
    fn install_is_private_and_does_not_overwrite_registration() {
        let dir = tempfile::tempdir().unwrap();
        let s = service(dir.path());
        let binary = dir.path().join("binary");
        let config = dir.path().join("config");
        fs::write(&binary, "executable").unwrap();
        fs::write(&config, "settings").unwrap();
        let mut calls = vec![];
        s.manage_with(Action::Install, &binary, &config, &mut |_, args, _| {
            calls.push(args.to_vec());
            Ok(0)
        })
        .unwrap();
        assert_eq!(calls.len(), 2);
        assert!(
            s.manage_with(Action::Install, &binary, &config, &mut |_, _, _| panic!())
                .is_err()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(s.runtime.join("config.yaml"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }
}
