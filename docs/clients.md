# Connecting clients

## Connect Codex

Top-level Codex settings (before any TOML table):

```toml
openai_base_url = "http://127.0.0.1:7889/v1"
chatgpt_base_url = "https://127.0.0.1:7889/backend-api"
```

The first setting controls model requests; the second independently controls
ChatGPT backend requests. Codex only adds `/backend-api` automatically for
recognized official hostnames, so include it for a loopback URL. Restart Codex
after editing. HTTP/SSE and HTTP/1.1 WebSocket upgrades use the same credential
and upstream routes.

Signed in with ChatGPT, Codex 0.156 and later refuse a plain-HTTP
`chatgpt_base_url` ("workspace backend must use an HTTPS origin without
credentials"); `openai_base_url` may stay HTTP. The listening port therefore
also accepts TLS: a connection that starts with a TLS handshake is decrypted,
anything else is served as plain HTTP. On first start coport creates a private
CA in `tls/` next to the configuration file (`tls/ca.pem`, with an owner-only
`tls/ca-key.pem`) and issues a certificate for `127.0.0.1` and `localhost`
from it on every start. Point Codex at that CA in the environment it starts
from; it is trusted in addition to the system roots. For the Linux service:

```sh
export CODEX_CA_CERTIFICATE="$HOME/.local/share/coport-rust/tls/ca.pem"
```

Use the `tls/ca.pem` beside your own configuration file; the GUI's setup
snippets show the exact path. Deleting `tls/` creates a new CA on the next
start.

A shell export does not reach the Codex desktop app. Both read `~/.codex/.env`,
but Codex ignores `CODEX_*` variables there; set `SSL_CERT_FILE` instead, which
Codex also adds to its system roots. Commands Codex runs do not inherit it under
`shell_environment_policy.inherit = "core"`:

```sh
echo "SSL_CERT_FILE=$HOME/.local/share/coport-rust/tls/ca.pem" >> ~/.codex/.env
```

Codex 0.160 TUIs attach to a shared managed app-server that reads `.env` only
when it starts, so run `codex app-server daemon restart` after changing it.

Alternatively, add the CA to the system trust store; Codex reads it, so neither
variable is needed. The CA carries critical name constraints permitting only
`localhost` and `127.0.0.1`, so it cannot vouch for any other host. On
Debian/Ubuntu:

```sh
sudo cp ~/.local/share/coport-rust/tls/ca.pem /usr/local/share/ca-certificates/coport.crt
sudo update-ca-certificates
```

On macOS, add it to the System keychain as a trusted root (the GUI's default
configuration lives in `~/Library/Application Support/io.github.coport.gui/`, the service's in
`~/Library/Application Support/coport-rust/`):

```sh
sudo security add-trusted-cert -d -r trustRoot -k /Library/Keychains/System.keychain \
  "$HOME/Library/Application Support/io.github.coport.gui/tls/ca.pem"
security verify-cert -c "$HOME/Library/Application Support/io.github.coport.gui/tls/ca.pem"
```

The serving certificate is valid for one year from each start, within Apple's
825-day limit. Recreating `tls/` requires trusting the new CA again.

Each serving certificate carries a single name, chosen by SNI: `localhost`, or
`127.0.0.1` when the client sends none. BoringSSL clients, including Claude Code,
reject an IP name under the CA's IP constraint, so give them `localhost`, e.g.
`ANTHROPIC_BASE_URL=https://localhost:7889/anthropic`. Plain HTTP needs no CA.
Running Claude Code sessions apply `settings.json` changes immediately but keep the
CA list they loaded at startup, so restart sessions started before the CA was
trusted before switching them to HTTPS.

For matched ChatGPT credentials, `/responses`, `/v1/responses` and
`/backend-api/codex/responses` map to the same model endpoint. Official
`/backend-api/...` paths retain their full path, including plugin listing at
`/backend-api/ps/plugins/installed` and analytics events. The account endpoints
`/backend-api/wham/usage`, `/backend-api/wham/profiles/me` and
`/backend-api/wham/rate-limit-reset-credits` require GET, and
`/backend-api/wham/rate-limit-reset-credits/consume` requires POST; all need a
matched ChatGPT login. API Keys cannot read subscription limits or account
profiles, or consume reset credits.

ChatGPT login refreshes can also use the saved account's proxy. Codex reads the
refresh URL from the environment rather than `config.toml`:

```sh
export CODEX_REFRESH_TOKEN_URL_OVERRIDE="http://127.0.0.1:7889/oauth/token"
```

`POST /oauth/token` (or `/https://auth.openai.com/oauth/token`) is forwarded to
`https://auth.openai.com/oauth/token`. The request has no Bearer token, so the
`refresh_token` in the JSON or form body selects the Codex `auth.json` login
whose `refresh_token` matches, and that account's proxy is used. Unmatched refresh
tokens use `account_fallback` only when `account_auth_file_only` is false;
otherwise they are rejected with 403 before any connection. The body is sent
unchanged and no credential is added; the refresh token is never logged.

The explicit upstream URL form also works, for example:
`http://127.0.0.1:7889/https://chatgpt.com/backend-api/codex/responses` or
`http://127.0.0.1:7889/https://provider.example.com/v1/responses`.
The HTTPS origin and API path must match the credential's configured upstream.
There is no arbitrary unauthenticated URL forwarding or `?base_url=` parameter.

### Codex API URL routes

`codex.routing.api_key` accepts upstream URLs as well as provider IDs and
environment variables:

```yaml
codex:
  routing:
    api_key:
      "provider.example.com/v1": [jp, us]
      "https://182.92.106.196:6060": none
```

Set the Codex client base URL to
`http://127.0.0.1:8787/codex/https://provider.example.com/v1`.
URL routes require a Bearer token, which the upstream validates; they do not
require a local provider credential source or a fallback. They preserve request
paths, queries, bodies and streaming responses. `--check` recognizes
URL selectors without looking them up as environment variables.

In routing keys, both `https://` and the API path may be omitted. A key such as
`api.example.com` defaults to HTTPS and matches every path on that origin;
`api.example.com/v1` is more specific and wins for `/v1` and `/v1/...`, but not
`/v1-other`. The longest matching path wins and the request path is not rewritten.
Ports still match exactly (omitted means 443). `//api.example.com/v1` is also
accepted. Equivalent spellings cannot be configured twice. Keys containing a
dot, colon or slash are interpreted as URL selectors; plain provider IDs and
environment-variable names remain credential selectors. Client base URLs still
use the explicit `http://127.0.0.1:8787/.../https://...` form.

Codex and Claude share the same origin/path matcher, public-IP validation, ordered
proxy selection and verified TLS transport for explicit URL routes. If both apps
configure matching URL routes, an unprefixed `/https://...` request returns 409;
use `/codex/https://...` or `/anthropic/https://...` to select the application.
A URL declared for only one app also works with the unprefixed form.

### OpenAI documentation MCP

```toml
[mcp_servers.openaiDeveloperDocs]
url = "http://127.0.0.1:7889/mcp/openaiDeveloperDocs"
enabled = true
```

No helper or per-session configuration is required. The destination is fixed to
`https://developers.openai.com/mcp`. A matching optional Bearer credential selects
the same route as model requests. Otherwise `codex.routing.mcp_fallback` applies;
with the URL-only configuration above, requests normally use that fallback.
Model tokens, account IDs, cookies and other private headers are not sent to the
public MCP upstream. MCP session/protocol headers, JSON and SSE are preserved.

## HTTP CONNECT and WebSocket

WebSocket clients use the same local paths, credentials and upstream routing as
HTTP requests, with `ws://` in place of the local `http://` URL. The proxy forwards
the handshake over HTTPS and, after a valid `101` response, relays frames in both
directions, including binary data, ping/pong, close frames, negotiated subprotocols
and extensions. Upstream handshake errors keep their HTTP status and body.

The listener answers CONNECT only for the API hosts it decrypts with proxy port
credential routing (below), and routes each request inside by credential. It never
tunnels a destination without reading its requests: every other CONNECT returns
403, and a top-level `connect` section is rejected as configuration.

### Credential routing on the proxy port

To route proxy-port clients by credential, as for base URL clients, enable
**Settings → Network → Proxy port credential routing** or set:

```yaml
connect_credential_routing: true # Default false; restart to apply.
```

CONNECT requests to the API hosts that the `codex` and `claude` sections route by
credential are then answered locally instead of tunnelled: the client's TLS is
terminated with a certificate from a second private CA, `tls/proxy-ca.pem` (with
an owner-only `tls/proxy-ca-key.pem`), and each request inside is handled exactly
like the explicit URL form `/anthropic/https://<host>/...` or
`/codex/https://<host>/...`. Account, API Key and URL routes, fallbacks, OAuth
refresh and WebSocket upgrades therefore select the outbound proxy as they do for
base URL clients, and unmatched credentials are rejected before any outbound
connection. These hosts are decrypted:

| Service | Hosts |
| --- | --- |
| Claude, with any account or non-URL API Key route or fallback | `claude.base_url` and `platform.claude.com` |
| Codex, with account routes or `account_fallback` | `codex.base_url.account` and `auth.openai.com` |
| Codex, with non-URL API Key routes or `api_key_fallback` | `codex.base_url.api_key` |
| Codex, for each named provider or its `env_key` selector | provider upstream hosts and ports from the configured Codex homes |
| Claude, for each named settings selector | the selected settings file's upstream host and port |
| Either, for each URL route | that route's host and port, including public IPv4 addresses |

A host routed by both services uses the unprefixed `/https://` form and its usual
conflict rules. Destination discovery reads local provider/settings definitions
at startup without resolving credentials or contacting upstreams. Restart after changing those
upstream addresses. Unavailable definitions contribute no destinations; valid
definitions in other configured homes still apply. All other destinations are
denied. Requests inside a decrypted tunnel must target
its destination (origin-form, or absolute-form naming that host), and the client
must not name another host in SNI (IP clients normally omit SNI).

The proxy CA is separate from the loopback CA and carries critical name
constraints permitting only the decrypted hostnames and exact IP addresses, so
trusting it does not cover loopback or any other site. Constraints bind only the
name types they list, so without a decrypted IP address every IP address is
excluded, and without a decrypted hostname only the reserved `invalid` domain is
permitted. A stored CA is never replaced automatically, since clients trust it.
Hosts a later configuration adds outside its constraints are not decrypted:
their CONNECT is refused with that reason, while base URLs and the hosts the CA
covers keep working. The GUI warns on the Proxy card and offers **Replace &
Trust**, which adds a new CA covering every host to your trust store (macOS asks
for your login password, Windows shows a security warning; on Linux trust it
yourself) before installing it, then restarts the proxy; **Keep Current CA**
reduces the warning to a note until the uncovered hosts change. The CLI prints
the same warning, and `coport --config <path> --replace-proxy-ca` replaces the
CA. A CA created before IP constraints is kept the same way and reported as not
limited to its hosts. A replaced certificate and its key are moved to
`tls/backup/` (for example `proxy-ca.<millis>-<id>.pem` with
`proxy-ca-key.<millis>-<id>.pem`), never overwritten; the loopback CA is kept
the same way. BoringSSL clients such
as Claude Code reject IP certificates under IP constraints, so they cannot use
the proxy port for an IP upstream; use its base URL instead. Point clients at
the CA rather than replacing their CA bundles:

```sh
export HTTPS_PROXY=http://127.0.0.1:8787
export NODE_EXTRA_CA_CERTS="$HOME/.local/share/coport-rust/tls/proxy-ca.pem" # Claude Code
export CODEX_CA_CERTIFICATE="$HOME/.local/share/coport-rust/tls/proxy-ca.pem" # Codex
```

Clients keep their original HTTPS API URLs. The GUI's setup snippets show the
exact path while the setting is on. Apps started from Finder, such as Copool,
see no shell variables; for those, add the proxy CA to the trust store as for the
loopback CA above (its name constraints limit it to the decrypted hosts), then
restart the app. A client that does not trust it, or still trusts a replaced
proxy CA, refuses the handshake; the CONNECT is then logged with reason
`client_rejected_proxy_ca` and nothing is forwarded. A decrypted tunnel that
closes or times out without a valid request is logged as `request_rejected`
with its `client_connect` host. Like base URL connections, a decrypted
connection serves further requests after a forwarded response, so a client
reuses its tunnel instead of opening a CONNECT per request; it closes after 30
seconds without a new request, after a locally generated error response, or when
a response stream fails midway, so a truncated body is never taken as complete.
Request logs include `client_connect` with the decrypted host.

Outbound HTTP and HTTPS proxies support Basic proxy authentication; SOCKS5
supports optional username/password authentication and resolves destination
hostnames remotely. HTTPS proxy certificates are verified using native system
trust.

CONNECT and upgraded WebSocket connections count toward the 128-connection
limit, close on service shutdown, and expire after `request_timeout_seconds`
without data progress in either direction. Active tunnels have no total lifetime
limit. Failure logs retain transferred byte counts and sanitized error side,
operation, and kind (without payloads or credentials). They preserve buffered
early data and TCP half-closes. This listener supports CONNECT authority-form and
API origin-form requests. While proxy port credential routing (below) is on,
absolute-form requests that name this listener on a loopback address are also
served, as proxy clients send for loopback base URLs (for example
`GET http://127.0.0.1:8787/backend-api/wham/usage`). The listener never forwards
plain HTTP to other hosts, and with that setting off every absolute-form target
returns 400.

## Connect Claude Code / Anthropic

Configure `claude.config_dirs` and `claude.routing` as shown in [Configuration](configuration.md), then set this
persistent environment entry in Claude Code's `~/.claude/settings.json` (merge
with existing settings):

```json
{
  "env": {
    "ANTHROPIC_BASE_URL": "http://127.0.0.1:8787/anthropic"
  }
}
```

Use your configured `listen_port`. Start `claude` normally. Outbound proxies
belong in this service's YAML; no proxy environment variables are needed in the
Claude configuration. For a one-time invocation:

```sh
ANTHROPIC_BASE_URL=http://127.0.0.1:8787/anthropic claude
```

The `/anthropic` prefix is removed before forwarding (`/claude` remains an alias).
Messages, token counting, model listing and other native API paths retain `/v1`
and query strings. Unprefixed `/v1/messages`, `/v1/messages/...`, and `/api/oauth/...`
also select Claude. Use `/anthropic/v1/models` for the ambiguous models path.
Explicit URLs must match either a declared API URL route or the default
`claude.base_url` origin and base path. See third-party routing below.

API credentials preserve the incoming `x-api-key` or `Authorization: Bearer ...`
header; subscription credentials use Bearer authentication. Requests containing both are rejected. Client
`anthropic-version`, beta flags, user agent, JSON and SSE are preserved.
Missing `anthropic-version` defaults to `2023-06-01`; OAuth account requests merge
`oauth-2025-04-20` into existing beta flags. Cookies, proxy credentials and
ChatGPT account headers are removed. Upstream errors and rate-limit headers pass
through unchanged. `GET /anthropic/api/oauth/usage` requires a matched local OAuth account or an
account identified by the profile API in `false` mode. API Keys cannot use it.

The OAuth client remains responsible for initiating refreshes and saving
new credentials. The proxy forwards refresh requests without changing credential
files or keychain entries; it does not implement login or
OpenAI-to-Anthropic conversion.
OAuth clients must still satisfy Anthropic's upstream client requirements.

### Claude OAuth usage and refresh

Clients can send `GET /anthropic/api/oauth/usage` with an OAuth Bearer token
and `anthropic-beta: oauth-2025-04-20`, and
`POST /anthropic/v1/oauth/token` with a JSON or form refresh payload containing
`refresh_token`. Usage goes to `claude.base_url` (default
`https://api.anthropic.com`); refresh always goes to
`https://platform.claude.com/v1/oauth/token`. JSON, status codes and `Retry-After`
are passed through. The refresh `client_id`, `scope` and token are unchanged;
no access token is injected.

A refresh token cannot query the profile API. Instead, the proxy learns from
each successful token response it relays which account the new refresh token
belongs to (the response's `account`, else the account of the refresh token it
replaced). A refresh is routed by, in order:

1. the account learned for its refresh token, routed as a looked-up access token
   is: by `claude.routing.account`, else `account_fallback`;
2. the saved Claude login of the configured directories whose
   `claudeAiOauth.refreshToken` matches, by that account's identity, label or
   fallback route (duplicate matches fail);
3. for a token known to neither, the profile lookup's route, `account_probe`
   (or `account_fallback` without one); its response teaches the account.

Learned routes and the probe apply only with
`claude.account_auth_file_only: false`. Learned routes keep only SHA-256 digests of
refresh tokens, with the account ID and email, in `claude-refresh-routes.json`
beside the configuration (the desktop app's data directory for `coportd`), and
survive restarts. To read the response, the proxy asks the token endpoint for
an uncompressed body. Usage still identifies unmatched access tokens via the
profile API before routing.

The unprefixed `/v1/oauth/token` and explicit
`/https://platform.claude.com/v1/oauth/token` paths are also supported, including
`/anthropic` and `/claude` prefixes. Use the local URL as an OAuth base URL,
or enable proxy port credential routing before using the listener as an HTTP
proxy.

Implementation references: Sub2api's [Anthropic forwarding](https://github.com/Wei-Shaw/sub2api/blob/main/backend/internal/service/gateway_anthropic_passthrough.go)
and [Claude header definitions](https://github.com/Wei-Shaw/sub2api/blob/main/backend/internal/pkg/claude/constants.go).

### Third-party Claude APIs

A custom shell command or `claude --settings <file>` may load settings from any
path. This service does not assume a `profiles` directory and does not infer a
settings filename from an opaque token. Declare the third-party upstream directly:

```yaml
claude:
  base_url: "https://api.anthropic.com"
  config_dirs: ["~/.claude"] # Default when omitted.
  account_auth_file_only: true
  routing:
    account:
      "you@example.com": claude_official
    api_key:
      "https://182.92.106.196:6060": none
```

Point the third-party client's `ANTHROPIC_BASE_URL` to:

```text
http://127.0.0.1:8787/https://182.92.106.196:6060
```

`/anthropic/https://...` is also supported. The local listener uses **HTTP**;
the embedded upstream uses **HTTPS**. The URL route applies to Messages, token
counting, model discovery, and other paths under that configured base. The
longest matching base path wins; scheme, host, port and path boundaries must
match. Undeclared destinations cannot use another route. A configured public
IPv4 upstream is supported; private, loopback and link-local IPs are rejected.

These routes require one nonempty `Authorization: Bearer ...` or `x-api-key`
header. They select the proxy by the declared upstream; the upstream validates
the forwarded API credential. They are independent of `account_auth_file_only`
and require no account/API fallback. `none` explicitly selects direct access.
Requests and SSE pass through unchanged, with no OAuth conversion or model
rewriting. Environment-variable API selectors continue to match credentials
locally. Explicit URL routes need no token in this service's configuration.

A custom CA supplied to the Claude client does not automatically become trusted
by the proxy. Explicit API routes use native TLS for compatibility with Node/OpenSSL;
Codex providers with their own `base_url` count as explicit API routes; account routes, API Keys for
the default API base and default Claude routes retain rustls. Both verify certificates and
hostname/IP identity. On Linux, supply a self-signed API certificate through the
service's `SSL_CERT_FILE` CA bundle (include the system CAs), and restart. Other
platforms use their native certificate stores. Linux builds vendor OpenSSL and
require a C toolchain, make and Perl; no system libssl runtime is needed. No settings-directory discovery or disabled TLS verification is needed.
