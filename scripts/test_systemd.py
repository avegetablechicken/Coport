#!/usr/bin/env python3
"""Exercise the Linux service manager using an isolated, temporary user service.

Run after cargo build --release, with a working systemd user session. This test
does not use the normal service label, runtime configuration, or listening port.
"""
import http.client
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time
import uuid

import service_rust as service


def request(port, path="/health"):
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=2)
    try:
        connection.request("GET", path)
        response = connection.getresponse()
        return response.status, response.read()
    finally:
        connection.close()


def wait_ready(port):
    for _ in range(100):
        try:
            status, body = request(port)
            if status == 200 and json.loads(body) == {"ok": True}:
                return
        except (OSError, http.client.HTTPException):
            pass
        time.sleep(0.1)
    raise AssertionError("Temporary systemd service did not become healthy")


def main():
    if not sys.platform.startswith("linux"):
        raise SystemExit("This test requires Linux with a systemd user session.")
    subprocess.run(["systemctl", "--user", "show-environment"],
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=True)
    binary = Path(os.environ.get("AGENT_ROUTER_BINARY",
                                service.ROOT / "target/release/agent-router")).resolve()
    service.LABEL = "local.agent-router.test-" + uuid.uuid4().hex
    unit = service.LABEL + ".service"
    registration = Path(os.environ.get("XDG_CONFIG_HOME", Path.home() / ".config")) / "systemd/user" / unit
    with tempfile.TemporaryDirectory(prefix="coding-proxy-systemd-") as directory:
        folder = Path(directory)
        runtime = folder / "runtime with spaces"
        runtime.mkdir(mode=0o700)
        (runtime / "service.env").write_text("CODING_PROXY_SYSTEMD_TEST_VALUE=loaded\n")
        (runtime / "service.env").chmod(0o600)
        service.runtime_dir = lambda: runtime
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        config = folder / "config.yaml"
        config.write_text(f"listen_port: {port}\nrequest_timeout_seconds: 3\n")
        try:
            assert service.manage("install", binary, config) == 0
            wait_ready(port)
            assert request(port, "/responses")[0] == 401
            before = subprocess.check_output(
                ["systemctl", "--user", "show", unit, "-p", "MainPID", "--value"], text=True).strip()
            assert before != "0"
            assert b"CODING_PROXY_SYSTEMD_TEST_VALUE=loaded" in Path(f"/proc/{before}/environ").read_bytes().split(b"\0")
            preserved = (runtime / "config.yaml").read_bytes()
            config.write_text("invalid checkout settings\n")
            assert service.manage("update", binary, config) == 0
            assert (runtime / "config.yaml").read_bytes() == preserved
            after_update = subprocess.check_output(
                ["systemctl", "--user", "show", unit, "-p", "MainPID", "--value"], text=True).strip()
            assert after_update == before, "update must not restart the service"
            assert service.manage("restart", binary, config) == 0
            wait_ready(port)
            after_restart = subprocess.check_output(
                ["systemctl", "--user", "show", unit, "-p", "MainPID", "--value"], text=True).strip()
            assert after_restart not in ("0", before)
            assert (runtime / "logs/proxy.log").stat().st_mode & 0o777 == 0o600
            assert service.manage("stop", binary, config) == 0
            try:
                request(port)
            except OSError:
                pass
            else:
                raise AssertionError("Stopped service still accepts requests")
        finally:
            if registration.exists():
                service.manage("uninstall", binary, config)
        assert not registration.exists()
        assert (runtime / "config.yaml").read_bytes() == preserved
    print("PASS: systemd install, health, auth rejection, update without restart, restart, stop, uninstall, private logs and paths with spaces")


if __name__ == "__main__":
    main()
