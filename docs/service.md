# Run as a background service

Build the Rust release executable, prepare `config.yaml`, and run:

```sh
target/release/coport service install
target/release/coport service status
```

On Windows, use `target/release/coport.exe`. The built-in service manager copies
the executable and initial configuration into a per-user runtime directory.
The service runs the native executable directly; Python is not required.

| Platform | Background runner | Runtime directory |
| --- | --- | --- |
| macOS | launchd user agent | `~/Library/Application Support/coport-rust` |
| Linux | systemd user service | `$XDG_DATA_HOME/coport-rust`, default `~/.local/share/coport-rust` |
| Windows | Task Scheduler task at user logon | `%LOCALAPPDATA%/coport-rust` |

The Windows task runs in the logged-in user's session; it is not a system service
that runs before login. The runtime copy of `coport.exe` is marked as a Windows
GUI-subsystem program (as `editbin /SUBSYSTEM:WINDOWS` does), so the task opens no
console window that could be closed to stop it; run CLI commands with the original
executable, since the runtime copy prints no console output. Linux requires an
available systemd user manager. On all
platforms, manage mihomo/Clash separately; the Rust service manager does not start
an external proxy core. The Linux registration retains systemd 245-compatible
path quoting. An opt-in Linux test covers the complete service lifecycle;
Windows task registration still needs native verification.

The service/task name is `local.coport.rust`. Stop any existing
listener on the configured port before installing. The service command manages only its
own service registration and runtime directory.
Use `--binary /path/to/executable` and `--config /path/to/config.yaml` to install
from other locations. By default, installation uses the executable running
the command and `config.yaml` in the current directory. Installation validates
the configuration first. A runtime copy retained by an earlier install is kept;
a different `--config` is refused until that copy is edited or removed.

Edit the runtime copy of `config.yaml`, then restart to apply changes. Updates
preserve that copy. Configure allowed credential directories in YAML. Linux services can load exported API Keys from a private `service.env` file beside
that runtime config, using systemd EnvironmentFile syntax. Codex API Keys can
also be stored in `.env` in a configured home on all platforms.

```sh
cargo build --locked --release
target/release/coport service update
target/release/coport service restart
```

On Windows, run `stop` before `update` because Windows locks running executables,
then run `restart`. On Unix, `update` stages the new executable without restarting.
`stop`, `status`, `restart`, and `uninstall` operate only on the Rust registration.
`uninstall` retains configuration and logs. Logs are under `logs/proxy.log` in the
runtime directory. Restart briefly interrupts active requests.

```sh
curl --noproxy '*' http://127.0.0.1:7889/health
```

Health confirms that the listener is running, not upstream connectivity.
