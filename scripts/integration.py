#!/usr/bin/env python3
"""Offline integration: real listener, HTTP CONNECT, startup configuration, fail-closed routing.
Uses only synthetic credentials and loopback sockets. Run after cargo build; COPORT_BINARY can select another executable.
"""
import http.client
import json
import os
import pathlib
import socket
import socketserver
import subprocess
import tempfile
import threading
import time

ROOT = pathlib.Path(__file__).resolve().parents[1]
BINARY = os.environ.get("COPORT_BINARY", str(ROOT / "target/debug" / ("coport.exe" if os.name == "nt" else "coport")))

class Probe(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True
    def __init__(self):
        self.requests = []
        super().__init__(("127.0.0.1", 0), Handler)
        threading.Thread(target=self.serve_forever, daemon=True).start()

class Handler(socketserver.BaseRequestHandler):
    def handle(self):
        self.request.settimeout(3)
        data = self.request.recv(65536)
        self.server.requests.append(data)
        self.request.sendall(b"HTTP/1.1 502 Test Proxy Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")


def main():
    probes = [Probe(), Probe()]
    a, b = probes
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    process = None
    try:
        with tempfile.TemporaryDirectory(prefix="coport-test-") as temp:
            temp = pathlib.Path(temp)
            home = temp / "home"
            home.mkdir()
            auth = home / "auth.json"
            config = temp / "config.yaml"
            def login(account, token):
                staged = home / "auth.next"
                staged.write_text(json.dumps({"tokens": {"account_id": account, "access_token": token}}))
                staged.replace(auth)
            def configure(a_proxy):
                value = f'''listen_port: {port}
request_timeout_seconds: 3
proxies:
  us: "http://127.0.0.1:{a_proxy}"
  jp: "http://127.0.0.1:{b.server_address[1]}"
codex:
  homes: [{json.dumps(str(home))}]
  base_url:
    account: "https://upstream.invalid/backend-api"
  routing:
    account:
      account-a: us
      account-b: jp
'''
                staged = temp / "config.next"
                staged.write_text(value)
                staged.replace(config)
            def request(token="token-a", account=None, path="/responses", method=None):
                conn = http.client.HTTPConnection("127.0.0.1", port, timeout=8)
                headers = {"Content-Type": "application/json"}
                if token is not None:
                    headers["Authorization"] = "Bearer " + token
                if account is not None:
                    headers["ChatGPT-Account-Id"] = account
                conn.request(method or ("GET" if path == "/health" else "POST"), path, body=b'{"private":"secret-body"}', headers=headers)
                response = conn.getresponse()
                result = response.status, response.read()
                conn.close()
                return result
            login("account-a", "token-a")
            configure(a.server_address[1])
            subprocess.run([BINARY, "--config", str(config), "--check"], check=True)
            codex_home = temp / "codex"
            codex_home.mkdir()
            (codex_home / "config.toml").write_text('[model_providers.reverse]\nenv_key = "REVERSE_TEST_KEY"\nbase_url = "https://provider-a.invalid/v1"\n')
            ignored_home = temp / "ignored-home"
            ignored_home.mkdir()
            (ignored_home / "config.toml").write_text("malformed = [")
            (ignored_home / ".credentials.json").write_text("invalid JSON")
            test_environment = dict(os.environ, HOME=str(temp), USERPROFILE=str(temp), CODEX_HOME=str(ignored_home), CLAUDE_CONFIG_DIR=str(ignored_home), REVERSE_TEST_KEY="provider-key-one", EXTRA_KEY_A="extra-key-a", EXTRA_KEY_B="extra-key-b")
            def restart():
                nonlocal process
                if process is not None:
                    process.terminate()
                    process.communicate(timeout=5)
                process = subprocess.Popen([BINARY, "--config", str(config)], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, env=test_environment)
                for _ in range(100):
                    try:
                        assert request(path="/health")[0] == 200
                        break
                    except OSError:
                        time.sleep(0.05)
                else:
                    raise AssertionError("Listener did not start")
            restart()
            assert request(token="wrong")[0] == 401
            assert not a.requests and not b.requests
            result = request(path="/responses?private_query=secret-query")
            assert result[0] == 502, result
            assert a.requests and not b.requests, f"account-a must use US proxy: response={result!r}"
            assert a.requests[0].startswith(b"CONNECT "), a.requests[0]
            assert b"token-a" not in a.requests[0], "Bearer must not leak into CONNECT"
            previous_a = len(a.requests)
            login("account-b", "token-b")
            assert request(token="token-a")[0] == 401
            assert request(token="token-b", account="account-a")[0] == 409
            assert request(token="token-b", account="account-b")[0] == 502
            assert b.requests and len(a.requests) == previous_a
            total = len(a.requests) + len(b.requests)
            login("unmapped", "token-c")
            assert request(token="token-c")[0] == 502
            assert total == len(a.requests) + len(b.requests)
            login("account-a", "token-a")
            configure(b.server_address[1])
            previous_a, previous_b = len(a.requests), len(b.requests)
            assert request()[0] == 502
            assert len(a.requests) > previous_a and len(b.requests) == previous_b, "YAML changes must not affect the running process"
            restart()
            assert request()[0] == 502
            assert len(b.requests) > previous_b, "Restart must load the changed YAML"
            with socket.socket() as unused:
                unused.bind(("127.0.0.1", 0))
                dead_port = unused.getsockname()[1]
            configure(dead_port)
            restart()
            before_failure = len(a.requests) + len(b.requests)
            assert request()[0] == 502
            assert len(a.requests) + len(b.requests) == before_failure, "No fallback to another proxy"
            config.write_text("invalid: [")
            assert request()[0] == 502
            assert len(a.requests) + len(b.requests) == before_failure
            invalid = subprocess.run([BINARY, "--config", str(config)], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            assert invalid.returncode != 0, "Invalid YAML must fail at startup"
            config.unlink()
            assert request()[0] == 502
            assert len(a.requests) + len(b.requests) == before_failure
            assert request(path="/health")[0] == 200
            log_path = temp / "logs/proxy.log"
            for _ in range(100):
                records = [json.loads(line) for line in log_path.read_text().splitlines()]
                terminal = [r for r in records if r["event"] in ("request_finished", "request_failed", "request_rejected")]
                if len(terminal) == 11:
                    break
                time.sleep(0.01)
            assert len(terminal) == 11
            current = next(r for r in records if r["event"] == "current_route")
            assert current["account_id"] == "account-a" and current["proxy"] == "us"
            routes = [r for r in records if r["event"] == "route_selected"]
            assert [(r["account_id"], r["proxy"]) for r in routes] == [("account-a", "us"), ("account-b", "jp")] + [("account-a", "us")] * 5
            assert all(r.get("request_id") and r.get("duration_ms") is not None for r in terminal)
            assert {r["status"] for r in terminal} == {"401", "409", "502"}
            raw_log = log_path.read_text()
            for secret in ["token-a", "token-b", "token-c", "secret-query", "secret-body"]:
                assert secret not in raw_log, "Sensitive request data must not appear in logs"
            assert all(r.get("path") != "/health" for r in records)
            key_file = home / ".env"
            key_file.write_text("API_PROVIDER_KEY=provider-key-one\n")
            config.write_text(f'''listen_port: {port}
request_timeout_seconds: 3
proxies:
  chosen: "http://127.0.0.1:{a.server_address[1]}"
  unused: "http://127.0.0.1:{b.server_address[1]}"
codex:
  homes: [{json.dumps(str(home))}]
  base_url:
    api_key: "https://api-provider.invalid/v1"
  routing:
    api_key:
      API_PROVIDER_KEY: chosen
''')
            restart()
            auth.unlink()  # API Key mode must not depend on ChatGPT credentials.
            subprocess.run([BINARY, "--config", str(config), "--check"], check=True)
            previous_a, previous_b = len(a.requests), len(b.requests)
            assert request(token="wrong")[0] == 401
            assert len(a.requests) == previous_a and len(b.requests) == previous_b
            assert request(token="provider-key-one", account="irrelevant", path="/v1/responses")[0] == 502
            assert len(a.requests) > previous_a and len(b.requests) == previous_b
            assert a.requests[-1].startswith(b"CONNECT api-provider.invalid:443 ")
            assert b"provider-key-one" not in a.requests[-1]
            key_file.write_text("API_PROVIDER_KEY=provider-key-two\n")
            assert request(token="provider-key-one")[0] == 401
            assert request(token="provider-key-two")[0] == 502
            key_file.unlink()
            total = len(a.requests) + len(b.requests)
            assert request(token="provider-key-two")[0] == 502
            assert len(a.requests) + len(b.requests) == total
            raw_log = log_path.read_text()
            assert "provider-key-one" not in raw_log and "provider-key-two" not in raw_log
            login("account-a", "chat-mixed-token")
            (home / "config.toml").write_text("".join(
                f'[model_providers.{name}]\nenv_key = "{env}"\nbase_url = "https://{host}/v1"\n'
                for name, env, host in [("reverse", "REVERSE_TEST_KEY", "provider-a.invalid"),
                                        ("provider-b", "PROVIDER_B_KEY", "provider-b.invalid"),
                                        ("extra-a", "EXTRA_KEY_A", "provider-a.invalid"),
                                        ("extra-b", "EXTRA_KEY_B", "provider-a.invalid")]))
            key_b = home / ".env"
            key_b.write_text("PROVIDER_B_KEY=provider-key-two\n")
            def mixed(api_base=None, **fallbacks):
                base = f"    api_key: {api_base}\n" if api_base else ""
                extra = "".join(f"    {key}: {value}\n" for key, value in fallbacks.items())
                config.write_text(f'''listen_port: {port}
request_timeout_seconds: 3
proxies:
  us: "http://127.0.0.1:{a.server_address[1]}"
  jp: "http://127.0.0.1:{b.server_address[1]}"
codex:
  homes: [{json.dumps(str(home))}]
  base_url:
    account: "https://chatgpt-mixed.invalid/backend-api"
{base}  routing:
    account:
      account-a: us
    api_key:
      REVERSE_TEST_KEY: us
      provider-b: jp
      EXTRA_KEY_A: us
      EXTRA_KEY_B: jp
{extra}''')
            mixed()
            restart()
            for token, probe, host in [("chat-mixed-token", a, "chatgpt-mixed.invalid"),
                                        ("provider-key-one", a, "provider-a.invalid"),
                                        ("provider-key-two", b, "provider-b.invalid"),
                                        ("extra-key-a", a, "provider-a.invalid"),
                                        ("extra-key-b", b, "provider-a.invalid")]:
                count = len(probe.requests)
                assert request(token=token, path="/v1/responses")[0] == 502
                assert len(probe.requests) > count
                assert probe.requests[-1].startswith(f"CONNECT {host}:443 ".encode())
                other = b if probe is a else a
                before, other_before = len(probe.requests), len(other.requests)
                assert request(token=token, path="/mcp/openaiDeveloperDocs")[0] == 502
                assert len(probe.requests) > before and len(other.requests) == other_before
                assert probe.requests[-1].startswith(b"CONNECT developers.openai.com:443 ")
                assert token.encode() not in probe.requests[-1]
            mixed(mcp_fallback="jp")
            restart()
            for credential in [None, "unknown"]:
                previous_a, previous_b = len(a.requests), len(b.requests)
                assert request(token=credential, path="/mcp/openaiDeveloperDocs")[0] == 502
                assert len(a.requests) == previous_a and len(b.requests) > previous_b
                assert b.requests[-1].startswith(b"CONNECT developers.openai.com:443 ")
            mixed(mcp_fallback="us")
            restart()
            previous_a, previous_b = len(a.requests), len(b.requests)
            assert request(token=None, path="/mcp/openaiDeveloperDocs")[0] == 502
            assert len(a.requests) > previous_a and len(b.requests) == previous_b
            print("PASS: MCP follows matched ChatGPT/API routes, missing credentials use independent fallback loaded at restart")
            for path in [
                "/backend-api/wham/usage",
                "/backend-api/wham/profiles/me",
                "/backend-api/wham/rate-limit-reset-credits",
            ]:
                previous_a, previous_b = len(a.requests), len(b.requests)
                assert request(token="chat-mixed-token", path=path, method="GET")[0] == 502
                assert len(a.requests) > previous_a and len(b.requests) == previous_b
                assert a.requests[-1].startswith(b"CONNECT chatgpt-mixed.invalid:443 ")
                total_before = len(a.requests) + len(b.requests)
                assert request(token="provider-key-one", path=path, method="GET")[0] == 403
                assert len(a.requests) + len(b.requests) == total_before
            print("PASS: account usage/profile/credits use matched ChatGPT proxy; API keys rejected before CONNECT")
            total = len(a.requests) + len(b.requests)
            assert request(token="unknown")[0] == 401
            key_b.write_text("PROVIDER_B_KEY=provider-key-one\n")
            assert request(token="provider-key-one")[0] == 409
            key_b.write_text("PROVIDER_B_KEY=chat-mixed-token\n")
            assert request(token="chat-mixed-token")[0] == 409
            assert len(a.requests) + len(b.requests) == total
            print("PASS: shared listener routes ChatGPT and two API providers; collisions rejected before CONNECT")
            mixed(api_key_fallback="jp")
            restart()
            # Ambiguous known credentials must still be rejected with fallback enabled.
            assert request(token="chat-mixed-token")[0] == 409
            assert len(a.requests) + len(b.requests) == total
            previous_a, previous_b = len(a.requests), len(b.requests)
            assert request(token="unmatched-openai-key")[0] == 502
            assert len(a.requests) == previous_a and len(b.requests) > previous_b
            assert b.requests[-1].startswith(b"CONNECT api.openai.com:443 ")
            assert b"unmatched-openai-key" not in b.requests[-1]
            auth.unlink()
            previous_b = len(b.requests)
            assert request(token="another-unmatched-token")[0] == 502
            assert len(b.requests) > previous_b
            assert b.requests[-1].startswith(b"CONNECT api.openai.com:443 ")
            total = len(a.requests) + len(b.requests)
            assert request(token="unmatched-openai-key", path="/https://other.invalid/v1/responses")[0] == 502
            assert len(a.requests) + len(b.requests) == total
            mixed("https://fallback-default.invalid/v1", api_key_fallback="jp")
            restart()
            previous_b = len(b.requests)
            assert request(token="unmatched-openai-key")[0] == 502
            assert len(b.requests) > previous_b
            assert b.requests[-1].startswith(b"CONNECT fallback-default.invalid:443 ")
            total = len(a.requests) + len(b.requests)
            mixed("https://fallback-default.invalid/v1", api_key_fallback="missing")
            assert request(token="unmatched-openai-key")[0] == 502
            assert len(a.requests) + len(b.requests) > total, "Running process retains valid startup configuration"
            invalid = subprocess.run([BINARY, "--config", str(config)], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            assert invalid.returncode != 0
            login("fallback-account", "fallback-account-token")
            (home / "config.toml").write_text((codex_home / "config.toml").read_text())
            key_b.unlink()
            config.write_text(f'''listen_port: {port}
request_timeout_seconds: 3
proxies:
  us: http://127.0.0.1:{a.server_address[1]}
  jp: http://127.0.0.1:{b.server_address[1]}
codex:
  homes: [{json.dumps(str(home))}]
  base_url:
    account: https://chatgpt-mixed.invalid/backend-api
    api_key: https://fallback-default.invalid/v1
  routing:
    api_key:
      reverse: us
      EXTRA_KEY_B: jp
    account_fallback: us
    api_key_fallback: jp
    mcp_fallback: us
''')
            restart()
            for credential, path, probe, host in [
                ("fallback-account-token", "/v1/responses", a, "chatgpt-mixed.invalid"),
                ("fallback-account-token", "/backend-api/ps/plugins/installed", a, "chatgpt-mixed.invalid"),
                ("unknown-api-token", "/v1/responses", b, "fallback-default.invalid"),
                ("provider-key-one", "/v1/responses", a, "provider-a.invalid"),
                ("extra-key-b", "/v1/responses", b, "fallback-default.invalid"),
                (None, "/mcp/openaiDeveloperDocs", a, "developers.openai.com")
            ]:
                before = len(probe.requests)
                assert request(token=credential, path=path)[0] == 502
                assert len(probe.requests) > before
                assert probe.requests[-1].startswith(f"CONNECT {host}:443 ".encode())
            print("PASS: nested base_url/routing schema, independent account/API/MCP fallbacks")
            # Provider auth selection mirrors Codex: env > explicit bearer > saved
            # OpenAI auth (only when requires_openai_auth is true). All synthetic.
            config.write_text(f'''listen_port: {port}
request_timeout_seconds: 3
proxies:
  chosen: http://127.0.0.1:{a.server_address[1]}
codex:
  homes: [{json.dumps(str(codex_home))}]
  routing:
    api_key:
      custom: chosen
''')
            provider_config = codex_home / "config.toml"
            provider_auth = codex_home / "auth.json"
            provider_auth.write_text(json.dumps({"OPENAI_API_KEY": "saved-provider-key"}))
            def set_provider(flag, extra=""):
                setting = "" if flag is None else f"requires_openai_auth = {str(flag).lower()}\n"
                provider_config.write_text(
                    '[model_providers.custom]\nbase_url = "https://custom-provider.invalid/v1"\n'
                    + setting + extra
                )
            def reaches_provider(token, account=None):
                before, other = len(a.requests), len(b.requests)
                assert request(token=token, account=account)[0] == 502
                assert len(a.requests) > before and len(b.requests) == other
                assert a.requests[-1].startswith(b"CONNECT custom-provider.invalid:443 ")
            def refused(token, status=502):
                before = len(a.requests) + len(b.requests)
                assert request(token=token)[0] == status
                assert len(a.requests) + len(b.requests) == before
            set_provider(True)
            restart()
            subprocess.run([BINARY, "--config", str(config), "--check"], env=test_environment, check=True)
            reaches_provider("saved-provider-key")
            refused("wrong", 401)
            for flag in [True, False, None]:
                set_provider(flag, 'env_key = "REVERSE_TEST_KEY"\nexperimental_bearer_token = "explicit-provider-key"\n')
                reaches_provider("provider-key-one")
                refused("explicit-provider-key", 401)
                refused("saved-provider-key", 401)
                set_provider(flag, 'experimental_bearer_token = "explicit-provider-key"\n')
                reaches_provider("explicit-provider-key")
                refused("saved-provider-key", 401)
                set_provider(flag)
                if flag is True:
                    reaches_provider("saved-provider-key")
                else:
                    refused("saved-provider-key")
                    refused(None, 401)
            set_provider(True, 'env_key = "REVERSE_TEST_KEY"\nexperimental_bearer_token = "explicit-provider-key"\n')
            dotenv = codex_home / ".env"
            dotenv.write_text("BASE=dotenv\nnot valid\nREVERSE_TEST_KEY=first\nexport REVERSE_TEST_KEY=${BASE}-key # last wins\n")
            original_pid = process.pid
            subprocess.run([BINARY, "--config", str(config), "--check"], env=test_environment, check=True)
            reaches_provider("dotenv-key")  # File overrides the inherited process value.
            refused("provider-key-one", 401)
            dotenv.write_text('REVERSE_TEST_KEY="rotated-dotenv-key"\n')
            reaches_provider("rotated-dotenv-key")
            assert process.pid == original_pid
            refused("dotenv-key", 401)
            test_environment["REVERSE_TEST_KEY"] = ""
            restart()
            reaches_provider("rotated-dotenv-key")  # Even an empty process value is overridden.
            test_environment["REVERSE_TEST_KEY"] = "provider-key-one"
            dotenv.write_text('REVERSE_TEST_KEY=\n')
            for token in ["saved-provider-key", "explicit-provider-key", "rotated-dotenv-key", "provider-key-one"]:
                refused(token)  # An empty file value does not fall back.
            set_provider(True, 'env_key = "CODEX_FILTERED"\n')
            test_environment["CODEX_FILTERED"] = "process-protected-key"
            dotenv.write_text('CODEX_FILTERED=file-protected-key\nCoDeX_IGNORED=ignored\n')
            restart()
            reaches_provider("process-protected-key")
            refused("file-protected-key", 401)
            del test_environment["CODEX_FILTERED"]
            dotenv.unlink()
            print("PASS: Codex dotenv precedence, duplicates, interpolation, skipped errors, prefix filtering and live reload")
            set_provider(True)
            provider_auth.write_text(json.dumps({"auth_mode": "chatgpt", "OPENAI_API_KEY": "stale-key",
                                                "tokens": {"access_token": "saved-chat-token", "account_id": "custom-account"}}))
            reaches_provider("saved-chat-token", "custom-account")
            refused("stale-key", 401)
            before = len(a.requests)
            assert request(token="saved-chat-token", account="wrong-account")[0] == 409
            assert len(a.requests) == before
            provider_auth.write_text(json.dumps({"OPENAI_API_KEY": "rotated-provider-key"}))
            reaches_provider("rotated-provider-key")
            refused("saved-chat-token", 401)
            provider_auth.unlink()
            refused("rotated-provider-key")
            print("PASS: custom provider true/false/omitted auth, precedence, saved API/ChatGPT credentials and rotation")
            print("PASS: unmatched credentials use only configured OpenAI fallback proxy; ambiguity and invalid proxy refused")
            print("PASS: API Key mode, no auth.json dependency, designated CONNECT route, key rotation, missing-key refusal")
            print("PASS: startup route, per-request accounts/proxies, failures, request IDs, durations, credential/body/query exclusion")
            print("PASS: loopback listener, bearer validation, account mismatch, CONNECT routing, credential refresh and YAML snapshot/restart, unmapped account, invalid YAML, unavailable proxy without cross-proxy fallback")
    finally:
        if process:
            process.terminate()
            try:
                process.communicate(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.communicate()
                raise AssertionError("SIGTERM did not stop server")
        for probe in probes:
            probe.shutdown()
            probe.server_close()

if __name__ == "__main__":
    main()
