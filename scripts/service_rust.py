#!/usr/bin/env python3
"""Manage the Rust executable as a per-user launchd/systemd/logon task.

Python is used only for installation and management, never for request routing.
The external proxy (mihomo/Clash/etc.) is managed separately.
"""
import argparse
import os
from pathlib import Path
import plistlib
import shutil
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]
LABEL = "local.coport.rust"


def runtime_dir(platform=sys.platform):
    if platform == "darwin":
        return Path.home() / "Library/Application Support/coport-rust"
    if platform == "win32":
        return Path(os.environ.get("LOCALAPPDATA", Path.home() / "AppData/Local")) / "coport-rust"
    if platform.startswith("linux"):
        return Path(os.environ.get("XDG_DATA_HOME", Path.home() / ".local/share")) / "coport-rust"
    raise RuntimeError("Supported platforms: macOS, Linux and Windows.")


def executable(platform=sys.platform):
    return "coport.exe" if platform == "win32" else "coport"


def private_write(path, data):
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    with tempfile.NamedTemporaryFile(dir=path.parent, delete=False) as output:
        staged = Path(output.name)
        try:
            output.write(data)
            output.flush()
            os.fsync(output.fileno())
        except BaseException:
            staged.unlink(missing_ok=True)
            raise
    try:
        staged.replace(path)
    finally:
        staged.unlink(missing_ok=True)


def stage(runtime, source, config, platform=sys.platform):
    if not source.is_file():
        raise RuntimeError("Run cargo build --release first, or pass --binary.")
    if not (runtime / "config.yaml").exists() and not config.is_file():
        raise RuntimeError("Create config.yaml first, or pass --config.")
    runtime.mkdir(parents=True, exist_ok=True, mode=0o700)
    destination = runtime / executable(platform)
    temporary = destination.with_suffix(".staged")
    try:
        shutil.copy2(source, temporary)
        temporary.chmod(0o700)
        temporary.replace(destination)
    except PermissionError as error:
        raise RuntimeError("Cannot replace the executable. Stop the Rust task, update, then restart (Windows locks running executables).") from error
    finally:
        temporary.unlink(missing_ok=True)
    if not (runtime / "config.yaml").exists():
        private_write(runtime / "config.yaml", config.read_bytes())
    (runtime / "logs").mkdir(exist_ok=True, mode=0o700)


def systemd_quote(value, command=False):
    # systemd performs specifier expansion even inside quotes.
    value = str(value)
    if command:
        value = value.replace('$', '$$')
    return '"' + value.replace('\\', '\\\\').replace('"', '\\"').replace('%', '%%').replace('\n', '\\n').replace('\r', '\\r') + '"'


def systemd_unit(runtime):
    # WorkingDirectory is a literal path, not a shell-like word list. In
    # systemd 245, surrounding quotes become part of the path and invalidate it.
    working_directory = str(runtime).replace('%', '%%')
    if '\n' in working_directory or '\r' in working_directory:
        raise RuntimeError("The systemd runtime directory cannot contain newlines.")
    binary = runtime / executable("linux")
    args = [binary, "--config", runtime / "config.yaml"]
    return ("[Unit]\nDescription=Coport (Rust)\nAfter=network.target\n\n"
            "[Service]\nType=simple\n"
            f"WorkingDirectory={working_directory}/\n"
            f"ExecStart={' '.join(systemd_quote(x, command=True) for x in args)}\n"
            f"EnvironmentFile=-{working_directory}/service.env\n"
            "Restart=on-failure\nRestartSec=10\nUMask=0077\nTimeoutStopSec=10\n\n"
            "[Install]\nWantedBy=default.target\n")


def launchd_plist(runtime):
    return plistlib.dumps({
        "Label": LABEL,
        "ProgramArguments": [str(runtime / executable("darwin")), "--config", str(runtime / "config.yaml")],
        "WorkingDirectory": str(runtime), "RunAtLoad": True, "KeepAlive": True,
        "ThrottleInterval": 10, "ExitTimeOut": 10, "Umask": 0o077,
        "StandardOutPath": str(runtime / "logs/service.stdout.log"),
        "StandardErrorPath": str(runtime / "logs/service.stderr.log"),
    })


def powershell_literal(value):
    return "'" + str(value).replace("'", "''") + "'"


def windows_script(action, runtime):
    name = powershell_literal(LABEL)
    if action == "exists":
        return f"if (Get-ScheduledTask -TaskName {name} -ErrorAction SilentlyContinue) {{ exit 0 }} else {{ exit 1 }}"
    if action == "status":
        return f"Get-ScheduledTask -TaskName {name}; Get-ScheduledTaskInfo -TaskName {name}"
    if action == "stop":
        return f"Stop-ScheduledTask -TaskName {name}"
    if action == "restart":
        return f"Stop-ScheduledTask -TaskName {name}; Start-ScheduledTask -TaskName {name}"
    if action == "uninstall":
        return f"Stop-ScheduledTask -TaskName {name}; Unregister-ScheduledTask -TaskName {name} -Confirm:$false"
    binary = powershell_literal(runtime / executable("win32"))
    args = powershell_literal(subprocess.list2cmdline(["--config", str(runtime / "config.yaml")]))
    workdir = powershell_literal(runtime)
    return (f"$identity = [System.Security.Principal.WindowsIdentity]::GetCurrent().Name; "
            f"$action = New-ScheduledTaskAction -Execute {binary} -Argument {args} -WorkingDirectory {workdir}; "
            "$trigger = New-ScheduledTaskTrigger -AtLogOn -User $identity; "
            "$principal = New-ScheduledTaskPrincipal -UserId $identity -LogonType Interactive -RunLevel Limited; "
            "$settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) "
            "-RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1) -MultipleInstances IgnoreNew "
            "-AllowStartIfOnBatteries -DontStopIfGoingOnBatteries; "
            f"Register-ScheduledTask -TaskName {name} -Action $action -Trigger $trigger -Principal $principal -Settings $settings; "
            f"Start-ScheduledTask -TaskName {name}")


def powershell(script, check=True):
    # EncodedCommand avoids shell interpolation of user paths or account names.
    import base64
    payload = base64.b64encode(("$ErrorActionPreference = 'Stop'; " + script).encode("utf-16le")).decode()
    return subprocess.run(["powershell.exe", "-NoProfile", "-NonInteractive", "-EncodedCommand", payload], check=check)


def manage(action, source, config):
    runtime = runtime_dir()
    if sys.platform == "darwin":
        registration = Path.home() / "Library/LaunchAgents" / f"{LABEL}.plist"
    elif sys.platform.startswith("linux"):
        registration = Path(os.environ.get("XDG_CONFIG_HOME", Path.home() / ".config")) / "systemd/user" / f"{LABEL}.service"
    else:
        registration = None
    if action in ("install", "update"):
        exists = registration.exists() if registration else powershell(windows_script("exists", runtime), check=False).returncode == 0
        if action == "install" and exists:
            raise RuntimeError("Rust service already installed; use update, then restart.")
        if action == "update" and not exists:
            raise RuntimeError("Rust service is not installed; use install first.")
        stage(runtime, source, config)
        if action == "update":
            print(f"Updated executable; retained {runtime / 'config.yaml'}. Restart to apply.")
            return 0
    if sys.platform == "darwin":
        domain = f"gui/{os.getuid()}"
        target = f"{domain}/{LABEL}"
        if action == "install":
            for filename in ("service.stdout.log", "service.stderr.log"):
                path = runtime / "logs" / filename
                if not path.exists():
                    private_write(path, b"")
            private_write(registration, launchd_plist(runtime))
            subprocess.run(["launchctl", "enable", target], check=True)
            subprocess.run(["launchctl", "bootstrap", domain, str(registration)], check=True)
        elif action == "status":
            return subprocess.run(["launchctl", "print", target]).returncode
        elif action == "stop":
            subprocess.run(["launchctl", "bootout", target], check=True)
        elif action == "restart":
            loaded = subprocess.run(["launchctl", "print", target], capture_output=True).returncode == 0
            subprocess.run(["launchctl", "kickstart", "-k", target] if loaded else ["launchctl", "bootstrap", domain, str(registration)], check=True)
        elif action == "uninstall":
            if subprocess.run(["launchctl", "print", target], capture_output=True).returncode == 0:
                subprocess.run(["launchctl", "bootout", target], check=True)
            registration.unlink(missing_ok=True)
    elif sys.platform.startswith("linux"):
        unit = f"{LABEL}.service"
        if action == "install":
            private_write(registration, systemd_unit(runtime).encode())
            subprocess.run(["systemctl", "--user", "daemon-reload"], check=True)
            subprocess.run(["systemctl", "--user", "enable", "--now", unit], check=True)
        elif action == "uninstall":
            # A malformed unit can be registered but not loaded. Still remove
            # its registration; do not mask other stop failures.
            stopped = subprocess.run(["systemctl", "--user", "stop", unit])
            if stopped.returncode not in (0, 5):
                stopped.check_returncode()
            subprocess.run(["systemctl", "--user", "disable", unit], check=True)
            registration.unlink(missing_ok=True)
            subprocess.run(["systemctl", "--user", "daemon-reload"], check=True)
        else:
            return subprocess.run(["systemctl", "--user", action, unit]).returncode
    else:
        powershell(windows_script(action, runtime))
    print(f"{action}: {LABEL}; runtime: {runtime}")
    return 0


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=("install", "update", "status", "stop", "restart", "uninstall"))
    parser.add_argument("--binary", type=Path, default=ROOT / "target/release" / executable())
    parser.add_argument("--config", type=Path, default=ROOT / "config.yaml")
    args = parser.parse_args()
    try:
        sys.exit(manage(args.action, args.binary.resolve(), args.config.resolve()))
    except (RuntimeError, OSError, subprocess.SubprocessError) as error:
        print(f"Service error: {error}", file=sys.stderr)
        sys.exit(1)
