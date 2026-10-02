#!/usr/bin/env python3
"""Offline HTTP CONNECT and SOCKS5 authentication probes, using synthetic secrets."""
import base64
import http.client
import json
import os
from pathlib import Path
import socket
import socketserver
import subprocess
import tempfile
import threading
import time
from urllib.parse import quote

ROOT = Path(__file__).resolve().parents[1]
BINARY = os.environ.get("AGENT_ROUTER_BINARY", str(ROOT / "target/debug" / ("agent-router.exe" if os.name == "nt" else "agent-router")))
USER, PASSWORD = 'test@user', 'p:ss@word'


def receive(sock, count):
    data = b''
    while len(data) < count:
        chunk = sock.recv(count - len(data))
        if not chunk:
            raise EOFError()
        data += chunk
    return data


class Handler(socketserver.BaseRequestHandler):
    def handle(self):
        self.request.settimeout(4)
        try:
            if self.server.scheme == 'socks5':
                version, count = receive(self.request, 2)
                assert version == 5
                methods = receive(self.request, count)
                assert 2 in methods, 'Client must offer username/password authentication'
                self.request.sendall(b'\x05\x02')
                version, length = receive(self.request, 2)
                assert version == 1
                username = receive(self.request, length).decode()
                password = receive(self.request, receive(self.request, 1)[0]).decode()
                self.server.credentials.append((username, password))
                good = (username, password) == (USER, self.server.expected_password)
                self.request.sendall(b'\x01\x00' if good else b'\x01\x01')
                if good:
                    head = receive(self.request, 4)
                    assert head[:3] == b'\x05\x01\x00'
                    if head[3] == 3:
                        host = receive(self.request, receive(self.request, 1)[0])
                        assert host == b'api.openai.com'
                    elif head[3] == 1:
                        receive(self.request, 4)
                    else:
                        receive(self.request, 16)
                    receive(self.request, 2)
                    self.server.authorized.append(True)
                    self.request.sendall(b'\x05\x04\x00\x01\x00\x00\x00\x00\x00\x00')
            else:
                data = b''
                while b'\r\n\r\n' not in data:
                    data += receive(self.request, 1)
                    assert len(data) < 65536
                assert data.startswith(b'CONNECT api.openai.com:443 ')
                assert b'model-secret' not in data
                fields = dict(line.split(b':', 1) for line in data.split(b'\r\n')[1:] if b':' in line)
                auth = next((value.strip() for key, value in fields.items() if key.lower() == b'proxy-authorization'), b'')
                expected = b'Basic ' + base64.b64encode((USER + ':' + self.server.expected_password).encode())
                if auth:
                    decoded = base64.b64decode(auth.split()[1]).decode().split(':', 1)
                    self.server.credentials.append(tuple(decoded))
                if auth == expected:
                    self.server.authorized.append(True)
                    self.request.sendall(b'HTTP/1.1 502 Probe Complete\r\nContent-Length: 0\r\nConnection: close\r\n\r\n')
                else:
                    self.request.sendall(b'HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm="probe"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n')
        except (EOFError, OSError):
            pass
        except Exception as error:
            self.server.errors.append(type(error).__name__)


class Probe(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True


def exercise(scheme, password, expected_password=PASSWORD):
    with Probe(('127.0.0.1', 0), Handler) as probe, tempfile.TemporaryDirectory() as directory:
        probe.scheme, probe.credentials, probe.authorized, probe.errors = scheme, [], [], []
        probe.expected_password = expected_password
        threading.Thread(target=probe.serve_forever, daemon=True).start()
        with socket.socket() as sock:
            sock.bind(('127.0.0.1', 0))
            port = sock.getsockname()[1]
        folder = Path(directory)
        endpoint = f'{scheme}://{quote(USER, safe="")}:{quote(password, safe="")}@127.0.0.1:{probe.server_address[1]}'
        config = folder/'config.yaml'
        config.write_text(f'listen_port: {port}\nrequest_timeout_seconds: 3\nproxies:\n  authenticated: "{endpoint}"\ncodex:\n  routing:\n    api_key_fallback: authenticated\n')
        process = subprocess.Popen([BINARY, '--config', str(config)], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            for _ in range(100):
                try:
                    with socket.create_connection(('127.0.0.1',port),timeout=.1): break
                except OSError: time.sleep(.03)
            connection = http.client.HTTPConnection('127.0.0.1',port,timeout=8)
            connection.request('GET','/v1/models',headers={'Authorization':'Bearer model-secret'})
            response = connection.getresponse()
            assert response.status == 502
            response.read(); connection.close()
            assert not probe.errors, probe.errors
            assert (USER,password) in probe.credentials, f'{scheme}: credential exchange missing'
            assert bool(probe.authorized) == (password == expected_password)
            log = (folder/'logs/proxy.log').read_text()
            for secret in [USER,PASSWORD,quote(USER,safe=''),quote(password,safe=''),'model-secret',base64.b64encode((USER+':'+password).encode()).decode()]:
                assert not secret or secret not in log, 'Credentials leaked into logs'
            records = [json.loads(line) for line in log.splitlines()]
            routed = [r for r in records if r['event']=='route_selected']
            assert routed and routed[-1]['proxy_endpoint'] == f'{scheme}://127.0.0.1:{probe.server_address[1]}'
            print(f'PASS: {scheme}, '+('accepted credentials' if password == expected_password else 'rejected wrong password')+', redacted logs')
        finally:
            process.terminate(); process.wait(timeout=5); probe.shutdown()


if __name__ == '__main__':
    for scheme in ['http','socks5']:
        for password in [PASSWORD,'wrong-password']:
            exercise(scheme,password)
    exercise('http', '', expected_password='')
