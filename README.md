# Coport

A cross-platform Rust loopback reverse proxy for Codex, Claude Code, ChatGPT account APIs and
OpenAI documentation MCP. It selects an outbound proxy by matching account or
API Key credentials. HTTP/SSE and WebSocket data are passed through without model
or protocol conversion. Explicit destination routes also support HTTP CONNECT tunnels.

## Configuration

Build with Rust 1.85+ and Cargo on macOS, Linux, or Windows:

```sh
cargo build --locked --release
cp config.example.yaml config.yaml
```

On Windows PowerShell, use `Copy-Item` and `coport.exe`.
The executable, service manager, tests, and packaging tools do not require Python.

The top level contains shared server settings and named proxies. **Codex and
Claude each have their own credential directories, `base_url`, and `routing` settings**:

```yaml
listen_port: 8787
request_timeout_seconds: 300

proxies:
  us: "http://127.0.0.1:7891"
  claude_official: "http://127.0.0.1:7893"

codex:
  base_url:
    account: "https://chatgpt.com/backend-api"
  homes: ["~/.codex"] # Default when omitted.
  account_auth_file_only: true
  routing:
    account:
      default: us
    # api_key:
    #   OPENAI_API_KEY: us

claude:
  base_url: "https://api.anthropic.com"
  config_dirs: ["~/.claude"] # Default when omitted.
  account_auth_file_only: true
  routing:
    account:
      default: claude_official
    # api_key:
    #   ANTHROPIC_API_KEY: claude_official
```

Either service section can be omitted. Within each section:

| Field | Purpose |
| --- | --- |
| `base_url` | Claude: one upstream root for both OAuth and API Keys |
| `base_url.account` | Codex: ChatGPT account upstream |
| `base_url.api_key` | Codex: API Key upstream; defaults to `https://api.openai.com/v1` |
| `codex.homes` / `claude.config_dirs` | Lists of directories allowed for local credential discovery |
| `auth_env` | Alternative: environment variable containing the account access token |
| `routing.account.<label>` | Proxy choice for that account source |
| `routing.api_key.<selector>` | Proxy choice for an API Key environment variable, Codex provider ID, or Claude settings name |
| `routing.account_fallback` | Proxy choice for an account without an explicit mapping |
| `routing.account_probe` | Claude OAuth account identity probe route; omitted uses `account_fallback` for compatibility |
| `routing.api_key_fallback` | Proxy choice for an unmatched API Key credential |

`codex.homes` and `claude.config_dirs` are lists of directory paths. When
omitted, they default to `["~/.codex"]` and `["~/.claude"]` respectively. An explicit
list replaces the default; `[]` disables directory discovery. Paths must be
absolute (`~` is supported). The proxy's `CODEX_HOME` and `CLAUDE_CONFIG_DIR`
environment variables do not change these lists. These settings control local
credential discovery; HTTP requests do not identify the client's configuration
directory, and matching still uses the supplied token.

With account routing enabled, each listed directory supplies the login its CLI
saved there, under the source label `default`. Account ID/email matching retains
its existing priority; use those identities to distinguish accounts from
different directories, or `default`/`account_fallback` for a shared route:

```yaml
codex:
  homes: [~/.codex, ~/.codex-work]
  routing:
    account:
      personal@example.com: us
      work@example.com: jp
```

Logins are read where the CLI keeps them:

- **Codex** follows `cli_auth_credentials_store` in each home's `config.toml`:
  `file` (the default) reads `auth.json`; `keyring` reads the OS credential
  store entry Codex wrote for that home; `auto` tries the credential store, then
  `auth.json`; `ephemeral` logins exist only inside Codex and cannot be routed.
  Codex's encrypted `secret_auth_storage` feature (on by default on Windows) is not
  supported; use `file` there.
- **Claude** reads the macOS keychain entry Claude Code wrote for that config
  directory first, then `.credentials.json`, as Claude Code itself does. Other
  platforms use `.credentials.json` only. Entries are found by directory path, so
  list the same path that `CLAUDE_CONFIG_DIR` names.

The credential store is the macOS keychain, Windows Credential Manager, or the
Linux Secret Service/kernel keyring. An entry written by the CLI belongs to that
CLI, so on macOS the first read asks for permission; choose **Always Allow**.
Reads are serialized, and a denied or locked store is retried after 30 seconds.

`auth_file` is ignored: both CLIs save logins under fixed names, so list another
directory instead. For API-only Codex routing, list `homes` and
`routing.api_key` without account routes. Provider configuration and its
`.env`/saved provider authentication always come from the same home. Multiple
distinct keys for a provider are accepted; a shared key resolving to different
upstreams is rejected as ambiguous. Identical provider credentials with the same
upstream are deduplicated within a route.

`auth_env` is an alternative account source that replaces directory logins;
Codex also checks `.env` in the listed homes for that variable. Different
account-token values for the same variable across homes are rejected as
ambiguous. Claude reads saved OAuth credentials and local account metadata from
each configured directory; it does not execute `apiKeyHelper` or load project
settings. Saved logins are read per request, so their rotation takes effect
without restarting; `.env` changes also take effect on the next credential
lookup. The clients handle login and token refresh.

Only configure `routing.api_key` when needed.

### Service-specific matching

Codex account routing tries account ID, then email/preferred username/name from
the login tokens, then the matched source label, then `codex.routing.account_fallback`.
A supplied `ChatGPT-Account-Id` must match the token's actual account ID.
Access-token profile metadata takes precedence over top-level claims and ID-token
metadata. These are routing hints; upstream authentication still validates tokens.

Both services support `account_auth_file_only`, defaulting to `true`:

| Mode | Codex | Claude |
| --- | --- | --- |
| `true` | Require a configured saved token and read its account claims | Require a configured saved token and read the local CLI account metadata |
| `false` | Also accept other ChatGPT tokens using their JWT claims | Also accept other OAuth tokens after a successful profile lookup |

For Claude's standard `~/.claude/.credentials.json`, account metadata comes from
`~/.claude.json` → `oauthAccount`. Custom credential directories use their own
`.claude.json`; they do not inherit another login's home metadata. UUID, email,
display name, and full name are matched in that order, followed by the source
label (`default`) and `claude.routing.account_fallback`.
Metadata refreshes per request. Missing/malformed metadata leaves explicit source
label and fallback routing available. An environment-backed account source has
no associated metadata file and uses its configured label/fallback. If a saved
token has no matching local identity, source-label, or fallback route, an explicit
`account_probe` resolves its identity before account routing. This includes stale
local metadata that no longer matches a configured account.
This also applies in `account_auth_file_only: true` mode, since the token already
matches a saved credential. Existing label/fallback routes do not require a probe.

In `true` mode, even an explicit account fallback cannot admit an unmatched token.
In `false` mode, saved logins may be absent or unavailable and `--check` skips saved
account requirements, just as for Codex. Matched local tokens still use local
metadata; it is never reused for a different incoming token.

Claude tokens are opaque, so an unknown token requires `GET /api/oauth/profile`
on `claude.base_url`. Before identity is known, **`claude.routing.account_probe`
provides the lookup proxy** (including ordered candidates or explicit `none` for
direct access). When omitted, `account_fallback` supplies the lookup proxy for
backward compatibility. If neither is configured, lookup fails; no implicit direct
route or another account's proxy is used. `account_probe` does not authorize model
requests: after identity lookup, an account mapping or `account_fallback` is required. Only the
Bearer token and profile-request headers are sent, without model payload, cookies
or client headers. The account returned by the API selects the final UUID/email
route for the model request. Example:

```yaml
claude:
  base_url: "https://api.anthropic.com"
  account_auth_file_only: false
  routing:
    account:
      "you@example.com": claude_official
    account_probe: claude_official
    # account_fallback: claude_official # Optional: allow other verified accounts.
```

Successful profile identities are cached in memory per token without time-based
expiry, up to 128 entries. At capacity, inserting a new token evicts only the
least recently used entry. A new token, an evicted entry, or a server restart
requires another probe. Cached identity does not bypass upstream token validation. Tokens without profile permission, rejected tokens, malformed
profiles and transport failures do not forward the model payload and are not
cached. Lookup redirects are not followed. Lookup errors never trigger a direct
retry. Profiles are limited to 64 KiB and lookup time to 10 seconds or the configured
request timeout, whichever is lower. API Key routes are unaffected by this flag.
Missing authentication never activates any fallback.

Claude API Key selectors also accept settings names:

```yaml
claude:
  config_dirs: ["~/.claude"] # Default when omitted.
  routing:
    api_key:
      api: none # Finds api.json; forwards directly to its configured upstream.
```

Example `~/.claude/api.json`:

```json
{
  "env": {
    "ANTHROPIC_BASE_URL": "https://provider.example.com",
    "ANTHROPIC_API_KEY": "your-api-key"
  }
}
```

The client sends the same key to `http://127.0.0.1:8787/anthropic`.
The proxy reads the original HTTPS upstream from the selected file; do not
replace that file's upstream with the local client URL. `ANTHROPIC_AUTH_TOKEN`
is also supported and takes precedence over `ANTHROPIC_API_KEY`. Fields are read
from `env` first, then from the JSON root. The selected file must contain both
an API credential and `ANTHROPIC_BASE_URL`.

Search is recursive across `claude.config_dirs`, without following child symlinks.
As in OpenQuota, JSON filenames containing both `settings` and the selector take
priority; otherwise the filename or its stem must equal the selector (`api` or
`api.json`). Multiple matching files are an error; use a more specific filename.
File credentials and upstreams are reloaded for requests; which file a selector
names is searched for again at most every 10 seconds (sooner if it is removed),
and `.git` directories are skipped. Only when no file
matches is the selector treated as an environment variable, using `claude.base_url`.
Malformed or incomplete selected files never fall back to environment values.
Domain/URL selectors retain their explicit-upstream routing behavior.

Codex API Key URL selectors match explicit upstream requests (see below).
Other selectors first match Codex provider IDs; otherwise they are treated
as environment variable names and may reverse-match a provider by `env_key`.
Multiple reverse matches are rejected. The built-in `openai` provider and unmatched
variables use `codex.base_url.api_key`; custom providers use the `base_url` in
Codex's `config.toml`, including supported explicit local wrapper URLs.
Custom provider credentials are selected in this order: the configured `env_key`,
then `experimental_bearer_token`, then saved OpenAI authentication only when
`requires_openai_auth = true`. An unset flag means `false`, matching Codex; neither
`false` nor omission falls back to saved OpenAI authentication. A configured but
missing or empty environment key is an error, not a fallback trigger.
Codex `.env` loading follows the Codex CLI's `dotenvy`/`set_filtered` behavior:
file entries override inherited process values (including empty values), later
duplicate entries win, and variables starting with `CODEX_` are ignored without
regard to case. Missing/unreadable files and individual parse errors are ignored.
Quoted values, comments, `export` and variable interpolation use dotenvy 0.15.7
syntax; later references see earlier accepted overrides, just as in Codex.

On every credential lookup, the proxy copies the inherited environment into an
isolated map for each configured home, then simulates Codex's `.env` loading in
file order. Edits take effect on the next lookup without restarting. This does
not mutate the process environment or affect Claude credentials or other homes.
The parser/quoted-line code is adapted from dotenvy under its MIT license so that
variable interpolation uses the simulated environment. No helper process is
used. An absent credential still uses the proxy's existing login-shell lookup
where supported, whose result (found or not) is reused for 30 seconds; an
explicitly empty value is an error and never triggers fallback.
Saved provider authentication is the login Codex saved for the same listed home,
read from its configured credential store as described above:
`auth_mode = "apikey"` selects `OPENAI_API_KEY`, and
`auth_mode = "chatgpt"` selects `tokens.access_token` and the optional
`tokens.account_id`. Without `auth_mode`, a stored API key takes precedence.
ChatGPT provider credentials retain the account header and the provider's upstream.
This discovery reads saved API key/ChatGPT login state; it does not emulate
Codex's in-memory authentication, command auth, or environment-only login
overrides. Use an explicit environment credential source for those cases.
Providers with no discoverable Bearer credential cannot use credential matching;
unauthenticated incoming requests remain rejected. A shared token across multiple
configured providers or an account route remains ambiguous and is rejected.
Claude API Key selectors can be named settings files, environment variable names
(using `claude.base_url` when no file matches), or explicit HTTPS upstream bases. Matched API credentials
may use Bearer or `x-api-key`; they do not trigger OAuth profile lookup or receive
injected OAuth beta flags. Requests matching multiple credentials in a
namespace are rejected. The two services never use each other's fallbacks.

Codex alone supports `codex.routing.mcp_fallback` for public OpenAI documentation
MCP. Omitted/null/`none` means direct MCP access. Explicit matched routes take
priority. Account/API fallbacks default to rejection when omitted/null.

### Upstreams, proxies and ordered candidates

Default upstreams must be HTTPS public hostnames; explicitly declared third-party
API routes and Codex custom provider upstreams may also use public IPv4 addresses. Codex's account base is the ChatGPT
backend root: model requests use `/codex`, usage, profile and reset-credit queries
use `/wham`, and plugin APIs use `/ps`; a `/backend-api/codex` base is rejected.
Claude's single base URL is a root **without `/v1`**: native `/v1` and
`/api/oauth` paths are preserved. The Claude base may point to a compatible
Anthropic gateway; no model or protocol conversion is performed.

`proxies` defines named HTTP CONNECT, HTTPS CONNECT or SOCKS5 endpoints. URLs
require explicit ports. Reserved `none` selects direct access; a proxy alias may
also map to `none`. Transport clients disable inherited/system proxy discovery:
all outbound selection happens in this service, independently of client proxy
environment variables.

Every routing value accepts a proxy name, `none`, or an ordered list.
Proxy candidate lists are tried in order, for example `[jp_lab, jp]`:

```yaml
codex:
  routing:
    api_key:
      OPENAI_API_KEY: [us, jp]
    api_key_fallback: [us, jp, none]
    mcp_fallback: [jp, none]
claude:
  routing:
    api_key:
      ANTHROPIC_API_KEY: claude_official
```

Merge those entries into the appropriate service sections, with proxy names
defined under `proxies`. Scalar routes send directly through the selected proxy.
Lists probe candidates with unauthenticated `HEAD /` requests to the upstream
origin, without model tokens, account headers, bodies or query parameters.
Any HTTP response except proxy authentication failure (407) proves reachability;
a target 5xx does not mark the exit down. Redirects are not followed. HEAD checks
are limited to 5 seconds (or the shorter request timeout). After a transient HEAD
failure, a TCP/CONNECT/SOCKS tunnel check gets up to 2 seconds to confirm transport
reachability without relying on the website homepage. The combined check shares
one budget of at most 7 seconds, capped by `request_timeout_seconds` and, for
a cold lookup, by the remaining request budget.

Results are isolated by proxy endpoint, upstream origin, TLS mode and transport
(HTTP versus CONNECT, since a tunnel does not verify TLS/HTTP behavior; up to 256
entries); concurrent initial lookups share a probe. One or two transient failures
leave the exit eligible and schedule rechecks after 1 second. Three consecutive
failures disable it; connection refusal and proxy authentication failure disable
it immediately. Disabled exits are rechecked after 3 seconds, slowing to 30 seconds
after ten failures. Healthy exits are checked after 30 seconds. The background
scheduler runs every second with at most eight concurrent probes; checks can take
longer under load. One successful check restores the exit immediately.

Regular API, Claude profile and OAuth refresh requests use the same outbound
sender. Each logical operation contributes one final health result: response
headers other than 407 clear failures; an exhausted transport failure adds one
failure, regardless of how many attempts were made. CONNECT establishment also
updates health using the same state machine. A late background probe cannot
overwrite newer request feedback. Scalar routes still use their configured exit
without health filtering or background probes.

`proxy_probe.probe_success` describes the latest network check;
`route_health.available` describes whether the route remains eligible, with
`health` set to `healthy`, `suspect` or `unavailable`. Logs include the sanitized
failure category and HTTP status without credentials. Requests use the first
eligible candidate in configured order.
Empty lists and
unknown names are rejected at startup. All failed candidates return 502.
Payloads never replay on another proxy. Bodyless GETs, including profile lookups,
may retry the same proxy before receiving response headers; POSTs (including
OAuth refresh) and WebSocket upgrades are sent only once.

YAML is loaded at startup. Restart after changing routes, upstreams, proxies or
timeouts. Codex provider metadata and account credential files refresh per request.
Environment-backed credentials use process variables first. On macOS/Linux, an
absent variable is read from the user's login/interactive zsh, bash or sh, with a
3-second timeout and no logged values. Windows uses process variables only.
Shell startup must not require terminal interaction. Saved Codex account matches
do not launch a shell to resolve unrelated API Key providers.

```sh
target/release/coport --config config.yaml --check
target/release/coport --config config.yaml
```

`--check` validates configuration and enabled credential sources, not network
reachability. Stop with Ctrl-C or SIGTERM.

### Command-line configuration overrides

Use repeatable `-c PATH=VALUE` arguments to override YAML settings. Dots
separate nested mapping keys; names match the YAML fields, including underscores.

```sh
target/release/coport --config config.yaml \
  -c listen_port=8888 \
  -c request_timeout_seconds=120 \
  -c codex.account_auth_file_only=false \
  -c 'codex.routing.account_fallback=[us, jp]' \
  -c 'claude.routing.account.you@example\.com=jp'
```

Values use YAML syntax: numbers, booleans, strings, lists, maps and `null`.
Quote the entire argument for shell-sensitive values; to force a YAML string,
use e.g. `-c 'proxies.office="none"'`. An empty value (`PATH=`) is an empty
string. Escape literal dots in keys with `\.` and backslashes with `\\`.
Replace lists as a whole, e.g. `-c 'codex.homes=["~/.codex"]'`.

Overrides apply in command-line order (the last assignment wins), create missing
mapping sections, and undergo the same validation as the configuration file.
Paths cannot traverse existing scalar, null or list values. Unknown settings and
invalid types are rejected. Overrides affect startup and `--check` without
modifying the source file.

### Older configurations

Earlier layouts are no longer read. Top-level Codex settings (`auth_file`,
`base_url`, `routing`, `accounts`, `api_key_providers`, `*_upstream_base_url`,
`*_fallback_proxy`), nested `accounts` sources and Claude inline routing fields
or split base URLs are rejected at startup. Move Codex settings under `codex:`,
Claude routing under `claude.routing`, and replace credential paths with
`homes`/`config_dirs`. `codex.auth_file` and `claude.auth_file` are accepted but
ignored.

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

To use the listener as an HTTP CONNECT proxy, configure exact destinations at
the top level:

```yaml
connect:
  "api.anthropic.com:443": claude_official
  "platform.claude.com:443": claude_official
  "chatgpt.com:443": [us, jp]
```

Values refer to names in `proxies`; `none` explicitly selects direct TCP.
Unlisted destinations return 403. Hostnames are case-insensitive; ports must
match. IPv6 literals use `[address]:port`. Configure any additional destinations
needed by the client.

Clients that support an HTTP proxy can then use
`HTTPS_PROXY=http://127.0.0.1:8787` with their original HTTPS URLs. This also carries
secure WebSocket connections through CONNECT. TLS stays between client and
server, so the proxy cannot inspect the API path or token, apply per-account
routing, or change headers inside the tunnel. CONNECT routes use the destination
alone and are independent of `codex.routing` and `claude.routing`.

Outbound HTTP and HTTPS proxies support Basic proxy authentication; SOCKS5
supports optional username/password authentication and resolves destination
hostnames remotely. HTTPS proxy certificates are verified using native system
trust. Ordered route lists attempt tunnel establishment in order and retain the
first successful connection; payloads are never replayed after establishment.
All CONNECT candidates share one establishment timeout budget. Candidate lists
use the same health state machine and background recovery schedule as HTTP; a
real connection attempt serves as the initial check, so successful sockets are
reused rather than opened twice.

CONNECT and upgraded WebSocket connections count toward the 128-connection
limit, close on service shutdown, and expire after `request_timeout_seconds`
without data progress in either direction. Active tunnels have no total lifetime
limit. Failure logs retain transferred byte counts and sanitized error side,
operation, and kind (without payloads or credentials). They preserve buffered
early data and TCP half-closes. This listener supports CONNECT authority-form and
API origin-form requests; it does not accept absolute-form plain HTTP proxy requests.

## Connect Claude Code / Anthropic

Configure `claude.config_dirs` and `claude.routing` as shown above, then set this
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

Refresh routing matches `claudeAiOauth.refreshToken` in the saved Claude logins
of the configured directories and uses that account's identity/label/fallback
proxy route. Duplicate matches fail. For credentials held only by the client,
set `claude.account_auth_file_only: false` and explicitly configure
`claude.routing.account_fallback` for refreshes. `account_probe` alone cannot
route refreshes because a refresh token cannot query the profile API. Usage
still identifies unmatched access tokens via the profile API before routing.

The unprefixed `/v1/oauth/token` and explicit
`/https://platform.claude.com/v1/oauth/token` paths are also supported, including
`/anthropic` and `/claude` prefixes. Use the local URL as an OAuth base URL,
or configure the separate CONNECT destination routes before using the listener
as an HTTP proxy.

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

## Desktop app (menu bar / tray)

`coport-gui` controls a separate `coportd` process from a status icon in
the macOS menu bar (no Dock icon), the Windows notification area, or a Linux
AppIndicator. Clicking the icon drops a panel below it, like a menu bar extra;
clicking elsewhere or pressing Esc closes it. The panel is built with Tauri 2:
the Rust side owns the proxy, and the UI in `gui/ui` is plain HTML/CSS/JS
rendered by the system WebView, so text uses the platform's native fonts. No
Node.js toolchain is needed; `cargo build` embeds the UI. Rust 1.89+ is required;
the command-line proxy keeps its own minimum.

```sh
cargo build --locked --release -p coport-gui
target/release/coport-gui
```

The panel follows the layout of native menu bar utilities such as eul: on macOS
it uses the system popover material, AppKit semantic colors and system fonts.
The main page stacks blocks for the proxy (switch, address, uptime, config
state), 30-minute traffic, client base URLs with setup snippets, recent requests,
outbound proxies (each with a color label used in the routing view, a
reachability status and, for proxies on this machine, the exit address and its
country or region looked up through `https://1.1.1.1/cdn-cgi/trace`), and routing. The header links open:

| Page | Content |
| --- | --- |
| Activity | Traffic charts, and a searchable log for a time range chosen in the Log header (Last hour, Last 6 hours, Today, Yesterday or a custom From–To range; newest first, 500 rows per page with Load More for older entries; the range is read once and reused by filters and searches); click a request for all fields |
| Settings | Launch at Login, auto-start, keep running after quit, appearance, config file status and `--check` equivalent, log folder |

The Activity log keeps its Requests, Models, Errors and All Events tabs. Use the
search condition dropdown for Keyword (all string fields), Path (contains),
Proxy (exact name, or `direct` for a direct connection), Status (exact
three-digit HTTP status), or Credential (contains; the configuration name that
routed the request: an account route such as an email or `default`, an API Key
variable or Codex provider ID, a Claude settings/profile name, or a URL route). Searches ignore case and surrounding whitespace; an
empty search shows all entries in the selected tab and time range.

At app startup, configured SSH devices receive one noninteractive login check,
with at most four checks running concurrently and a ten-second timeout per check.
This check does not retrieve statistics, start forwarding, or manage the remote daemon.
Device indicators show its result until a fresh statistics result is available.
Opening the panel again does not repeat the startup check.

Configured proxies are also tested automatically at app startup. Reopening the
panel does not repeat these checks; use **Test** or **Test All** to run them again. Entering Settings reads local device definitions,
uses cached account status, and does not poll remote device statistics. Entering
or revisiting Devices and its periodic refresh also read local caches only.
Use the Devices **Refresh** action to explicitly retrieve remote statistics. These UI controls do not stop health checks
for routes already in use by the running proxy.

Enable **Settings → General → Keep proxy running after quit** to leave the
independent proxy daemon running when choosing Quit (including the tray menu and
keyboard shortcut). The GUI process actually exits: its WebView, tray icon, and
GUI instance lock are released. The daemon has its own PID and detached process
session, so quitting or force-killing the GUI does not interrupt proxy connections.
Launching the GUI again discovers and authenticates to the existing daemon,
restores its status and logs, and can stop or restart it without a port conflict.
Auto-start does not restart an already-running daemon, even when disabled.
With the option off (the default), normal Quit stops the daemon before exiting.
Closing only the panel still leaves the GUI running in the tray.
The helper must be next to the GUI executable (included in the macOS app bundle).
Its control endpoint and private authentication token are stored in `daemon.json`
in the GUI's application directory; stale discovery data is replaced after a crash.
This does not install a login service or provide automatic crash recovery.

The configuration is edited in your text editor; the panel validates it whenever
the file changes and offers a restart when the running proxy is out of date.
Settings can also reveal it in Finder/Explorer, or import another YAML file to
replace it; an invalid file is rejected and leaves the configuration unchanged.
The request log can be opened or revealed the same way. The
right-click menu offers Open Panel, Start/Stop, Restart and Quit.

With `account_auth_file_only: true`, Routing shows account activation from the
configured credentials. Claude accounts without a local route match are checked
through `account_probe` using their saved OAuth token. A successful probe keeps
the warning icon to indicate the missing local match; a failed probe shows
`Probe failed` alongside the icon. Probe results are cached for 30 seconds;
changing the token or configuration triggers a new check. Without a token, a
probe cannot authenticate and the account remains inactive.

The GUI keeps its configuration, certificates, logs, preferences and daemon discovery
in the persistent per-user `io.github.coport.gui` directory: `~/Library/Application Support/io.github.coport.gui/`
on macOS, `$XDG_CONFIG_HOME/io.github.coport.gui/` (normally `~/.config/io.github.coport.gui/`) on Linux, or
`%APPDATA%/io.github.coport.gui/` on Windows. Its fixed configuration is `config.yaml`, its
certificate and private key are in `tls/`, and request logs are in `logs/proxy.log`
(with rotated history alongside them). These files are application data, not disposable caches.
The GUI never searches the working directory for configuration. It offers to create a
missing configuration from `config.example.yaml` or to import a YAML file (`.yaml` or
`.yml`). Paths saved by earlier versions are ignored. The CLI's default remains `./config.yaml`.

On the first upgrade from the old `io.github.coport.gui` cache directory, stop the
proxy in the old app and quit it before opening the new version. The GUI copies
`config.yaml`, `tls/` and `logs/` to the persistent directory without deleting the
originals or changing the CA identity. An active daemon or conflicting destination
files stop migration with an error; matching files permit retry after interruption.
Later launches do not reimport the old copy. Data already removed by cache cleanup
cannot be restored by this migration.

The CLI continues to log beside its
configuration unless `--log-file` is supplied. Opening the app shows the
panel; Launch at Login starts it with `--background`. Only one instance runs per
user, and launching again opens its panel. Do not run the desktop app and the
background service on the same port at the same time.

- macOS: `scripts/bundle_macos.sh` builds `target/Coport.app`
  (`LSUIElement`, ad-hoc signed). A Rust helper assembles ICNS icons without
  `iconutil` or Python. The previous app is retained until packaging and signature checks
  succeed. A full menu bar hides status items behind the
  notch while apps with long menus are frontmost; the panel then opens at the
  top-right corner when launched or reopened.
- Windows: requires the WebView2 runtime (included with Windows 10 21H2+ and 11).
- Linux: install the WebKitGTK and AppIndicator libraries first, for example on
  Ubuntu 22.04+:
  `sudo apt install libwebkit2gtk-4.1-dev libayatana-appindicator3-dev librsvg2-dev libxdo-dev`.
  GNOME needs the AppIndicator extension (enabled by default on Ubuntu); KDE
  supports it natively.

## Run as a background service

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

## Proxy username/password authentication

Proxy entries retain their string format and may include credentials:

```yaml
proxies:
  authenticated_http: "http://proxy-user:proxy-password@proxy.example.com:8080"
  authenticated_https: "https://proxy-user:proxy-password@proxy.example.com:8443"
  authenticated_socks: "socks5://proxy-user:proxy-password@proxy.example.com:1080"
```

The Rust HTTP transport supplies the credentials to the proxy, separately
from the model request's Bearer token. Percent-encode reserved characters in
usernames/passwords: for example, `user@example` and `p:ss@word` become
`user%40example:p%3Ass%40word`. Supply both fields; an empty password is accepted
for HTTP(S), while SOCKS5 requires 1–255 UTF-8 bytes per field. HTTP usernames
cannot contain a colon. Credentials containing control characters are rejected.

Proxy endpoint logs omit both username and password. Unauthenticated URLs and
`none` remain supported. Restart the service after changing proxy credentials
in YAML. An authentication failure does not switch to a different proxy or direct
access.

## Multiple proxy services with mihomo and Clash

[mihomo](https://github.com/MetaCubeX/mihomo) is the proxy core used by compatible Clash clients. One core can expose several local proxy services at once, each pinned to a different outbound node. For example:

| Account mapping | Local endpoint | Fixed outbound node |
| --- | --- | --- |
| `us` | `127.0.0.1:8101` | `US-Node` |
| `jp` | `127.0.0.1:8102` | `JP-Node` |

Use a Clash client with a **mihomo-compatible core**. Older Clash cores may not support custom `listeners`. System Proxy and TUN mode are not required for these explicit local connections.

### 1. Add dedicated listeners

Add the following to your mihomo configuration, or use your Clash client's persistent profile override/merge mechanism. If a `listeners` list already exists, append these entries instead of creating a duplicate YAML key. Replace `US-Node` and `JP-Node` with exact names of nodes available in the active configuration.

```yaml
listeners:
  - name: coding-us
    type: mixed
    listen: 127.0.0.1
    port: 8101
    udp: false
    users: []
    proxy: US-Node

  - name: coding-jp
    type: mixed
    listen: 127.0.0.1
    port: 8102
    udp: false
    users: []
    proxy: JP-Node
```

Each `mixed` listener accepts HTTP CONNECT and SOCKS5. Its `proxy` field sends traffic directly to the named outbound node. Using a concrete node keeps the route independent of changes to a shared selector group. If you deliberately use a proxy group instead, its selection and fallback policy determine the actual exit; keep it restricted to the intended region and exclude `DIRECT` when direct access must be prevented.

`users: []` disables inbound authentication for these loopback listeners, for the unauthenticated local proxy URLs in these examples. Authentication to a remote proxy can still be handled by mihomo itself.

### 2. Configure the outbound nodes

If your Clash profile already supplies the nodes, keep its existing definitions and use their exact names above. If your provider supplies authenticated HTTPS CONNECT proxies, the following illustrates the equivalent mihomo `proxies` entries:

```yaml
proxies:
  - name: US-Node
    type: http
    server: us-proxy.example.com
    port: 443
    username: REPLACE_WITH_US_USERNAME
    password: REPLACE_WITH_US_PASSWORD
    tls: true

  - name: JP-Node
    type: http
    server: jp-proxy.example.com
    port: 443
    username: REPLACE_WITH_JP_USERNAME
    password: REPLACE_WITH_JP_PASSWORD
    tls: true
```

These reserved example domains and credentials are placeholders, not working proxies. Use the protocol, port, TLS settings, and credentials required by your provider. Other mihomo-supported node types can be used behind the same listeners. Keep real node definitions and subscription URLs in your private mihomo/Clash configuration.

### 3. Load the configuration

**Clash client:** save the override, reload the profile or restart its core, and inspect the effective configuration to confirm both listeners are present. Exact menu names vary by client. Use persistent overrides because subscription refreshes can replace direct edits to a downloaded profile.

**Standalone mihomo on macOS:** install with Homebrew if needed:

```sh
brew install mihomo
```

The Homebrew configuration directory is usually `/opt/homebrew/etc/mihomo` on Apple Silicon or `/usr/local/etc/mihomo` on Intel. For a standalone configuration, combine the listener and node sections above with these top-level settings in a private `config.yaml`:

```yaml
allow-lan: false
bind-address: 127.0.0.1
mode: rule
log-level: info
rules:
  - MATCH,REJECT
```

The named listener routes select their configured nodes directly; the final rule rejects traffic that reaches ordinary rule routing. This rule is for the standalone example, not a replacement for an existing Clash profile's rules.

Validate and run your configuration, replacing the directory with its actual location:

```sh
mihomo -t -d /path/to/private/mihomo -f /path/to/private/mihomo/config.yaml
mihomo -d /path/to/private/mihomo -f /path/to/private/mihomo/config.yaml
```

If Homebrew manages the configuration at its default location, you can run it as a service instead:

```sh
brew services start mihomo
# After editing the configuration of an existing service:
brew services restart mihomo
```

Choose either a Clash-managed core or a standalone core for these ports. Do not start two processes listening on the same addresses and ports.

### 4. Verify each exit and connect the application

Test each listener independently:

```sh
curl --noproxy '' --proxy http://127.0.0.1:8101 --max-time 20 https://api.ipify.org
curl --noproxy '' --proxy socks5h://127.0.0.1:8102 --max-time 20 https://api.ipify.org
```

These commands contact an external IP-check service through the selected node. Compare the results with your provider's expected exits; an IP response alone does not establish a country or guarantee access to the Codex upstream. `socks5h` is curl's remote-DNS option; use `socks5://` in this application's YAML.

The `proxies` entries in the quick-start configuration already match these listener ports. Add the real account mappings, run `--check`, start the application, and inspect `route_selected` events to confirm the account and local endpoint selected for each request.

For another region, add a node, give it a listener on a unique port, and add the corresponding application proxy label and account mapping.

Reference: [mihomo listener fields](https://wiki.metacubex.one/en/config/inbound/listeners/) and [official configuration examples](https://github.com/MetaCubeX/mihomo/blob/Meta/docs/config.yaml).

## Logs

Logs are written to `logs/proxy.log` beside the application configuration file. Override the file location with `--log-file /path/to/proxy.log`.
Records are also mirrored to stderr when it is a terminal, so a foreground run
shows them as before. Background runners (launchd, systemd, the desktop daemon)
and redirected stderr receive records only while the log file cannot be opened
or written; startup errors and log maintenance failures always go to stderr.
File writes, rotation and pruning run on a dedicated logging thread. Producers do
not wait for disk I/O. Queues hold at most 1024 records per logger, with an 8 MiB
process-wide buffer budget and a 256 KiB limit per record. Saturation emits a
`log_records_dropped` count instead of blocking requests. Normal shutdown drains
accepted records; forcibly killing a process can lose records still queued.

```sh
tail -F logs/proxy.log
```

Each line is JSON with a UTC timestamp. Log files rotate at 5 MiB, keeping the
latest backup as `proxy.log.1` and moving older backups into `logs/history/` as
timestamped JSONL archives. Archives are kept for at least 30 days after creation;
cleanup runs at startup and rotation, and retains files modified within the last
30 days. Retention is based on age, with no file-count cap that could discard busy
days early. Traffic reads the active file, latest backup, and historical archives,
deduplicating imported request records. History from before this policy can only
be shown if the source logs still exist. Unix files use mode `0600`; Windows files
inherit directory ACLs, so keep them in your private user directory. If archiving
fails, logging continues in the current file; failures are also reported on stderr.

Traffic groups API configurations by service and the logged `upstream_base_url`,
matching the complete base (including its path and port) against the current
configuration; account identities remain separate. A logged name and base that
match one configuration count under it. Records from a renamed configuration
(same base), one whose base URL changed (same name), or records logged before
base URLs were recorded (name only) are merged into the current configuration
and marked for review in the panel. Records whose base is used by several
configurations, or whose name and base match none, count as `Unidentified` and
are also marked. For each marked name and base, the panel can assign the
records to a current configuration or to `Unidentified`. These choices are
stored in `traffic-compatibility.json` beside the GUI settings (Settings →
Files → Traffic Compatibility); deleting it restores automatic matching. This
reconciliation is read-only and never rewrites retained logs.

| Event | Meaning |
| --- | --- |
| `current_route` | Account ID and proxy configured at startup; not a connectivity test. |
| `route_unavailable` | Startup credentials or mapping could not be read; later requests retry credential loading using the startup configuration. |
| `request_received` | Request method, path without query, and unique request ID. |
| `route_selected` | Account and proxy actually selected for this request. |
| `upstream_response` | Upstream HTTP status and time to response headers (`headers_ms`). |
| `route_health` | Effective route eligibility, consecutive failures and feedback source (`probe`, `request` or `connect`). |
| `upstream_retry` | A bodyless GET transport failure before response headers; includes `upstream_attempts`, redacted `transport_error` category, and `retry_delay_ms`. |
| `request_finished` | Transfer completed, including status, duration, and received bytes; check status for upstream errors. |
| `request_cancelled` | Request processing was dropped (for example, client disconnect or shutdown). No status is logged if no response was established; the UI shows CANCEL. |
| `request_rejected` / `request_failed` | Authentication, configuration, connection, or streaming failure with diagnostic context. |
| `model_call_started` / `model_call_updated` | One HTTP model request or WebSocket `response.create`, identified by `model_call_id` separately from its connection's `request_id`. |
| `model_call_finished` / `model_call_failed` / `model_call_cancelled` | Per-call outcome, model, response ID, duration, and reported input/output/cache token usage when available. WebSocket success requires a model terminal event delivered to the client. |
| `model_call_incomplete` | The model ended the response early with `response.incomplete` (for example `max_output_tokens`, recorded as `incomplete_reason`). Not counted as an error. |
| `model_call_unknown` / `model_observation_gap` | A call outcome could not be observed reliably, or observation exceeded a bounded parser limit; never counted as a successful model response. |

Request logs keep the proxy-generated `request_id` separate from the client's
`client_request_id` and `session_id`. Session headers are checked in this order:
`session_id`, `x-session-id`, `x-codex-session-id`, `x-claude-code-session-id`.
Client request headers are checked in this order: `x-client-request-id`,
`x-request-id`, `request-id`, `request_id`. The corresponding `*_source` fields
identify the selected header or body field. IDs must be nonempty printable ASCII,
without spaces, and at most 256 bytes; ambiguous duplicate headers are skipped.
Missing or invalid IDs are recorded as `null`, never generated as client IDs.

For uncompressed JSON model requests, absent header IDs can be filled from
`session_id` or `metadata.session_id`, and `client_request_id` or `request_id`.
Claude's `metadata.user_id` JSON-string session field and legacy
`user_<hash>_account_<id>_session_<uuid>` form are also recognized; the user/account
portion is never logged. Body IDs appear in subsequent lifecycle records, since
`request_received` and HTTP `model_call_started` precede body parsing. Compressed
request bodies are not decoded, so their IDs must be supplied in headers.
WebSocket calls inherit the handshake session ID unless the individual
`response.create` supplies one. Each call reads its own request ID from
`client_request_id`, `request_id`, or `event_id`, in that order; the handshake ID
is retained separately as `connection_client_request_id` and is never reused as
a per-call client request ID. Upstream response IDs remain `response_id`.

The GUI's **Model Calls** scope counts each HTTP generation/compaction request and
each WebSocket generation, including active calls. **All** counts network requests
and connections instead. Model calls are deduplicated by call ID across log rotation.
New model calls are read from explicit `model_call_*` events with a `model_call_id`.
Historical HTTP generation/compaction requests also count as one call each;
requests already carrying a call ID are excluded from this fallback to avoid double
counting. WebSocket connection records are never used to infer calls.
The client's `Accept-Encoding` is forwarded unchanged, so responses arrive compressed
as the client negotiated and `received_bytes` counts the bytes actually transferred.
HTTP model calls also record `response_content_type` and `response_content_encoding`
when present. Compressed responses (`gzip`, `deflate`, `br`, `zstd`) are decoded
incrementally as they stream through, keeping only the fields above; forwarded bytes
are unchanged and no body is stored. Compressed request bodies are forwarded without
decoding, so the model is taken from the response. Event streams are recognized by
their content even without a `text/event-stream` content type. When a response
cannot be observed, `model_observation` records why: `unsupported_encoding`,
`decode_error`, `message_limit` or `memory_budget`.
Token totals include only reported usage; missing usage displays as unknown, not zero.
These totals are not provider quota or billing measurements. No prompts, generated
text, or tool arguments are written to these logs.

Responses WebSocket sessions have independent `websocket` timeout settings:
`first_message_seconds` (30), `first_output_seconds` (900), `read_seconds` (900),
`write_seconds` (120), and `inter_turn_idle_seconds` (300). Values are seconds;
only inter-turn idle accepts 0 to disable its timer. The first-output deadline
waits for semantic output, not a handshake, `response.created`, or heartbeats.
After output begins, upstream model messages refresh the read deadline. Successful
terminal delivery starts the between-turn idle timer; new calls start fresh timers.
Writes have a separate timeout budget, including across fragments. Idle sessions
close with code 1000; first-message/output/read timeouts use 1001 and write failures
use 1011. Close frames are sent where the transport permits, never inside a partial
data frame. Connection logs record the stage, direction, close code, frame size and
whether downstream bytes were written. An unfinished call remains failed even when
the upstream closes normally. CONNECT and other opaque tunnels retain the shared
byte-activity timeout from `request_timeout_seconds`.

The WebSocket observer handles masking, fragmentation and negotiated
`permessage-deflate` (including context takeover). Frames and reconstructed or
inflated messages are limited to 32 MiB. Exceeding observation limits records a gap
and closes the model WebSocket; it does not silently fabricate successful calls.
HTTP observation buffers, WebSocket frames and reconstructed/inflated messages
share a process-wide 128 MiB allocation budget. HTTP observation stops and releases
its buffers when that budget is exhausted; forwarded bytes are unchanged.
A model WebSocket closes in a controlled way instead of disabling its phase timers:
frame allocation exhaustion uses close code 1013, and observation exhaustion records
a gap before closing. This budget covers these buffers, not total process RSS.

The GUI's exit-IP lookup and proxy-port diagnostic are display-only checks.
They do not feed the daemon's per-destination route-health cache; a reachable
proxy or successful IP lookup does not prove that a specific API origin is reachable.

Logs contain **full account IDs and proxy endpoints**. They do not record tokens, authentication headers, query parameters, or request/response bodies. Keep logs private. `/health` requests are excluded from request logs.

## Limits and troubleshooting

- Default upstreams require HTTPS and a public service hostname. Explicit API URL routes and Codex custom provider upstreams also accept public IPv4 addresses; private addresses and local hostnames remain rejected.
- Proxy URLs require an explicit port and support `http`, `https`, or `socks5`. Optional username/password authentication uses `scheme://username:password@host:port`. Credentials are removed from logged proxy URLs.
- Inbound limits: 32 MiB request body, 64 KiB headers, 128 concurrent connections, and a 30-second read timeout. Content-Length and chunked uploads are supported; each connection handles one request.
- Repeated list-valued `Accept`, `Accept-Encoding`, `Accept-Language`, `Cache-Control`, `Connection`, `Pragma`, and `Via` headers are supported. Duplicate authentication, routing, Host, framing, and other unknown headers are rejected.
- `Expect: 100-continue` returns HTTP 417. Only WebSocket version 13 GET upgrades are supported; other Upgrade requests return HTTP 426.
- Upstream response chunks, including SSE, are forwarded as they arrive with backpressure. There is no whole-response buffering or automatic decompression. Content-Encoding is preserved when returned by an upstream.
- Bodyless GET requests (excluding WebSocket upgrades) retry connection, timeout, or request transport failures before response headers at most twice, with 200 ms and 400 ms backoff. Attempts use the same selected proxy and share one deadline with route selection, cold probes, backoff and response streaming. Claude profile lookups use the same sender with a 10-second maximum budget. CONNECT candidates likewise share one establishment deadline; the relay idle timeout starts separately after connection establishment. HTTP error responses, response-body failures, and other methods are not retried. Proxy selection failures are not retried. Request logs include `upstream_attempts`; final transport failures also include a sanitized `transport_error` category.
- `request_timeout_seconds` accepts 1–3600 seconds and configures the HTTP request timeout and the idle timeout for established WebSocket/CONNECT tunnels. Tunnel activity in either direction resets the idle timer; active tunnels have no total lifetime limit. A failure after streaming starts closes the connection without inserting a JSON error into the stream.
- Local HTTP 401 means the Bearer token is missing/malformed, or no credential matches and OpenAI fallback is disabled. HTTP 409 means the account header does not match or the token matches multiple routes. HTTP 502 indicates a routing/configuration or upstream connection failure. Upstream HTTP errors retain their original status and body.
- If a mihomo listener refuses connections, confirm the effective profile contains it, its node name is valid, and the port is not occupied. If the exit changes unexpectedly, inspect the listener's node/group selection.
- After rebuilding a running service, restart that process to use the new executable.

## Share and merge processed statistics

The desktop daemon can expose an independent, authenticated **read-only data API**.
The model proxy and daemon management port always remain on loopback. Sharing is
opt-in and does not expose API forwarding, CONNECT, WebSockets, configuration,
raw logs, credentials, or lifecycle operations.

Create a separate 256-bit sharing key without printing it:

```sh
umask 077
openssl rand -hex 32 > ~/.coport-data.key
```

Protect this file and privately provide the same key to an authorized receiving
device. Sharing keys are separate from model account/API credentials and SSH
keys. The desktop app reads the key file at daemon startup. Rotate the file and
restart to revoke the previous key; configure the receiver with the new key.

```yaml
allow_external_access: true
external_data:
  port: 8788
  token_file: "~/.coport-data.key"
  # Or token_env: COPORT_DATA_KEY; exactly one key source is required.
  trusted_lan: [10.42.0.0/24]
  # Configure these to allow public HTTPS clients:
  # tls:
  #   certificate: "/absolute/path/to/fullchain.pem"
  #   private_key: "/absolute/path/to/private.key"
```

**LAN HTTP** is accepted only from explicitly trusted RFC1918 CIDRs; the peer's
actual socket address is used, not forwarded headers. **Public clients require
HTTPS**, with a server certificate valid for the hostname/IP they connect to.
All data requests, including local ones, require `Authorization: Bearer <sharing-key>`.
HTTP transmits the sharing key in plaintext: use it only on an isolated, trusted
LAN; use HTTPS on shared or untrusted networks. Configured TLS errors fail startup
instead of falling back to HTTP. On Unix, sharing-key and TLS-key files must be
owned by the daemon's user with mode 600 or 400. Private-file permission checking
is currently supported on Linux/macOS; other platforms can use environment keys
for LAN sharing, but cannot load server TLS private keys through this interface.
The existing loopback CA is not used for public HTTPS.

Only **`GET /v1/summary`** is implemented, with no query parameters or request
body. The versioned JSON contains a random installation ID, aligned traffic
windows (default: the last 30 completed minutes of model calls), and numeric statistics grouped
by service and opaque proxy/upstream references. No proxy names/addresses, upstream
URLs, account IDs, email addresses, configuration paths, per-request IDs or raw
log entries are included. Private proxy/upstream references use domain-separated
HMAC-SHA256 with a separate, server-private
identity key. A data reader cannot derive them from the sharing key or use it
for offline guesses of unknown configuration values. The identity key is never
returned by either API. On Linux/macOS it persists in the owner-only
`data-identity.key`; other platforms use an in-memory key renewed on restart.

The exporter reuses the local traffic processor's deduplication and token-counting
rules. Summaries refresh every 15 seconds; read failures expire the cached result
after 90 seconds. Requests and connections have fixed size/concurrency/time limits.
Unsupported routes and methods are rejected, and the interface has no code path
to mutate files or control the proxy on behalf of a request.

In the last **Settings → Devices** block, add a device and select **HTTP / HTTPS**. Enter its origin
(`http://10.42.0.196:8788` or `https://host:8788`), the receiver's local key-file
path, and optionally a private CA certificate. The client verifies HTTPS names and
certificates, disables redirects and environment proxies, and pins validated LAN
DNS results before sending a key over HTTP. Strict schema, freshness and size
validation precede merging. Duplicate installation IDs are counted once. The receiver automatically chooses
the newest complete window shared by the devices and retries minute-boundary races. Latency sums/sample counts
are merged, rather than averaging averages; unreported token usage remains unknown.

The snapshot contains all six Traffic ranges (30 minutes, 6/12 hours,
1/7/30 days), for both All requests and Models. Each range uses aligned completed
minutes and the same bucket sizes as local Traffic. The client selects one window
and never sums overlapping windows. The Devices page shows the combined Traffic
and a full Traffic block for each device: charts, account usage, request/error
counts, received bytes, weighted latency, reported tokens and weighted cache hit
rate. Each window retains up to 24 detailed groups; remaining groups fold into
anonymous service totals without losing counts. Snapshots remain capped at 1 MiB; large snapshots reduce detail to eight groups
per window and preserve remaining counts in anonymous service totals.
Version three also retains two preceding minute boundaries so clock offsets and
cached responses align automatically. Updating keeps the previous display visible
and needs no manual refresh or clock adjustment. Version-one peers still support the 30-minute Models view and require an update
for the other ranges/categories.

This Device uses the same local identities, history evidence and configuration
assignments as Activity Traffic, without anonymizing and re-identifying its own
data. Already observed local account IDs can identify the same known accounts
on peers. Unknown peer traffic is one Unidentified group per service; different
proxies/endpoints do not manufacture additional apparent accounts. All unknown
labels in the application are English. Settings Devices uses icon add/remove
actions and shows green only after a recent successful statistics retrieval.

A refresh prioritizes the GUI's selected time range and Models/All category.
Each device's selected statistics render as soon as they arrive; slower devices
do not hold up the others. SSH uses one connection to send the selected window
first, then computes and sends the complete snapshot. HTTP requests the selected
cached window with an optional `X-Coport-Window: MINUTES:model|all` header, then
fetches the full cache. Partial responses use schema version four and retain
the same two alignment boundaries and privacy/size limits.
Once all twelve views are cached, range/category switches are immediate.
Background failures retain statistics already displayed. Page entry, periodic
refresh and range/category switches read cached snapshots only; the explicit
**Refresh** action retrieves remote statistics for the current selection. Local
traffic assignments re-merge cached snapshots without reconnecting to devices.
Expanded upstream accounts use the same order as Activity
Traffic: known accounts by received bytes descending, unidentified accounts last.

Each device selects one read-only transport: **HTTP/HTTPS** or **SSH**.
HTTP/HTTPS requests the authenticated service described above. SSH invokes
`coportd --summary-stream --window MINUTES model|all`, flushing two newline-delimited
DTOs (selected, then complete) without requiring an HTTP listener or sharing key.
Older helpers automatically fall back to `coportd --summary`; keys restricted to
that command still return the complete snapshot, without progressive delivery. New devices default to SSH; entering only the SSH
config alias uses it as the display name and automatically discovers the executable.
SSH statistics calls do not invoke lifecycle operations,
read raw credentials, request configuration matching, or modify files.

Unknown proxy/upstream references display **Unidentified proxy / Unidentified** and never create
or modify local configuration. The server-private identity key is never exported.
Random UUIDv4 account IDs additionally produce a service-separated SHA-256 reference.
The receiver matches it only against accounts already known to its own configuration
and uses its own display name. Emails, names, URLs and other guessable identifiers
never produce a shared reference. Unknown accounts stay anonymous. This allows the
same known account on multiple devices to merge without exporting IDs or labels.

Statistics use local metadata and identities already observed by real requests.
Reading traffic or account status never initiates a profile request, runs a login
shell, or probes an upstream provider. Accounts without local/cached evidence remain
anonymous until normal request routing establishes their identity.

Named API providers such as ShareCoder additionally use a domain-separated
HMAC-SHA256 proof keyed by their high-entropy API credential and bound to the
service/upstream. The receiver restores its own configuration name only when its
configured credential/upstream produces that proof. Emails, provider names, URLs
and keys are not exported. New traffic records retain the credential proof; older
records can match an unambiguous current provider with the same logged name and
upstream. Weak/missing credentials and ambiguous histories remain anonymous.

For the SSH route, create a dedicated read-only SSH key restricted on the server
to the summary command, for example an `authorized_keys` entry beginning with
`restrict,command="/absolute/path/to/coportd --summary"`. This prevents that key
from running management commands, shells, PTYs, or port/agent/X11 forwarding.
The service creates its private identity during startup; summary requests only
read existing identity, configuration and processed log results.

This option is provided by desktop `coportd`; standalone `coport` remains a local
model proxy. Its `--check` validates sharing credentials and TLS configuration,
but starting it with sharing enabled reports that the data API requires `coportd`.

## Read statistics from existing devices over SSH

Configure up to 32 devices in **Settings → Devices**; **Request forwarding** follows it. Choose one
connection: SSH or HTTP/HTTPS. New devices default to SSH. Enter the SSH config
alias (for example `mbp16`) and select **Add Device**; no URL or sharing key is
required. The display name defaults to the alias. Leave the executable blank for
automatic discovery; `coportd` in older preferences also means automatic discovery.
An explicit executable path overrides discovery.

**Edit** updates the device's stable ID, so renaming never overwrites another
device. Duplicate display names are rejected. Older separate SSH/data settings
are preserved. Removing a device deletes its saved connection and stops any local
forwarding session for it. The Devices
page merges processed statistics and never starts, stops or restarts remote services.

Both machines need this version of Coport. The destination must have existing
GUI configuration and identity files created by starting its desktop daemon.
At startup the daemon atomically records its executable in the owner-only
`summary-executable` file in its application data directory. SSH reads that record
locally, checks ownership, permissions and file type, and executes `--summary-stream`
(or `--summary` for older helpers) for statistics or `--forward` when request forwarding is explicitly enabled.
The path stays on the destination. A stale or unsafe record fails closed with an
instruction to restart the destination app. When no record exists, discovery checks
`PATH`, standard user/system binary locations and macOS Applications bundles.
No directory scanning, configuration export, service restart or registration write
occurs during a statistics request.
Statistics connections return processed statistics without an HTTP listener. Raw logs and remote configurations are not returned.

Set up key authentication (or an SSH agent) and connect once from a terminal to
verify and trust the destination's host key. SSH ports, keys and jump hosts come
from local SSH config; Coport stores no SSH passwords or private keys. Use a
restricted read-only key as described above.

SSH agent/X11 forwarding, configured port forwards and local commands are disabled.
The destination requires a POSIX shell (Linux/macOS). Paths containing spaces are
supported; `~` is not expanded in executable paths. Batch authentication and strict
host-key checking reject missing keys and untrusted hosts without prompting.
Connection attempts have a five-second connection timeout and a 25-second overall
limit. A failed source does not prevent other devices' statistics from merging.

### Forward requests through a remote device

Configure an SSH device in **Settings → Devices**, then enable its switch
in the following **Request forwarding** block. The local proxy must be running.
Coport checks SSH access and the destination daemon before switching the existing
local listener to remote forwarding. Client URLs, service paths, port and local
TLS trust stay unchanged. Both HTTP and HTTPS continue to work; local TLS is
terminated before request bytes travel over the encrypted SSH connection.

Only one remote device can be active. The remote daemon applies its own routing
and credential-selection rules; incoming authorization headers retain their normal
meaning. No remote credentials or configuration files are exported. Home highlights
the selected remote device and collapses inactive local routing and historical
statistics. Devices remains a statistics-only page.

Switching devices or returning to local mode disconnects existing connections.
A remote connection failure returns **502 Bad Gateway** without falling back to
local routes, and the selected remote mode stays active for subsequent retries.
**Stop forwarding** restores local handling on the same port. Editing/removing the
selected device also restores local handling. The daemon owns forwarding, so closing
the GUI preserves the selected mode if **Keep proxy running after quit** is enabled.
Startup is local by default. Enable **Restore on startup** to
restore the selected remote before accepting any requests. An offline destination
stays selected and returns 502 until it recovers; it never falls back to local.
The option and destination are stored privately in `forwarding.json`. Turning
forwarding off or deleting the selected device clears the saved destination.
Malformed saved preferences prevent daemon startup rather than silently choosing local routes.

The local daemon must support unified forwarding; update and restart older local
daemons before enabling it. The destination must support `coportd --forward` and
already be running. A summary-only restricted SSH key cannot forward requests.
The helper only connects to the running daemon's loopback proxy. A destination
already forwarding to another device rejects this helper to prevent forwarding
chains and loops. No remote network listener needs to be exposed.

#### Connection health, traffic and remote versions

Home distinguishes a selected remote from a verified connection: **Checking**,
**Connected**, or **Disconnected · retrying**. Connection time includes SSH
negotiation, authentication and remote helper startup; it is not ICMP latency.
The last verified connection and recent recovery are shown separately. The daemon
checks the active destination again 15 seconds after each health check completes,
including while the GUI is closed. Request failures keep the same destination,
and a successful background check clears the connection error without needing
another client request. Errors include bounded SSH/helper diagnostics.

**This forwarding session** counts actual client connections, active connections,
failed connections, uploaded/downloaded bytes and sampled transfer rates. These are
transport counters, not model request counts; health and metadata checks are excluded.
Counters reset on destination changes or daemon restart. Local client disconnects
can increase failed connections without marking the remote server offline.

**Remote device · all requests · 30 min** uses the existing read-only summary to
show the remote device's request total and error rate, including its other clients.
Metadata is refreshed 30 seconds after each retrieval completes, independently of
health checks. Unavailable or stale statistics are marked explicitly and do not
make a working forwarding connection unhealthy.

Settings shows a compact version and capability status. The daemon automatically
checks each configured SSH connection once per configuration, with at most four
checks in flight. Successes and failures are retained; neither page visits nor
timers retry a denied or unavailable device. A changed connection or daemon restart
allows another check. Saving or removing devices reloads local configuration;
opening Settings only reads cached results. No manual version-check
button is needed. Unverified or unsupported destinations have a disabled forwarding
switch; an already active destination can always be switched off. Errors are shown
as a short status with details on hover. Older peers without `--capabilities` must
be updated before enabling a new forwarding session from the GUI.

This mode uses remote routing. Using the remote device solely as a network exit
while retaining local service/account routing is not yet supported.

## Development

```sh
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
cargo build --locked
cargo clippy --locked -p coport-gui --all-targets -- -D warnings
cargo test --locked -p coport-gui
```

Tests use synthetic credentials and loopback sockets. Rust tests cover TLS and
HTTP/HTTPS CONNECT, early SSE delivery, proxy selection, request framing, and
MCP credential isolation. Rust process integration tests cover account/API Key routing,
credential refresh, configuration snapshots, fallback refusal and
HTTP/SOCKS5 authentication. They do not call a real model. CI runs these checks
and builds release binaries for all three operating systems.

On Linux with a working systemd user session, also run:

```sh
cargo test --locked --test systemd -- --ignored
```

This test uses a temporary runtime directory, a unique service name, and an
ephemeral loopback port. It verifies installation, configuration-preserving
updates, restart, stop, and uninstall without changing the normal service.

Cargo builds the proxy binary used by the process integration tests. Set
`COPORT_BINARY` to test another build, such as the release executable. The
systemd lifecycle test is ignored by default because it requires a Linux user
service manager; all other tests run in the normal Cargo test suite.

Development, service management, and macOS packaging require no Python installation.

| File | Responsibility |
| --- | --- |
| `src/main.rs` | CLI, configuration overrides, startup and shutdown |
| `src/config.rs` | YAML parsing and validation |
| `src/identity.rs` | Account metadata, native TOML parsing, environment and shell credentials |
| `src/routing.rs` | Credential matching, upstream URL mapping |
| `src/server.rs` | Bounded HTTP listener, proxy selection, TLS and streaming |
| `src/logger.rs` | Redacted structured logs and rotation |
| `src/service.rs` | Built-in per-user platform service management (`coport service`) |
| `gui/src/main.rs` | Desktop app startup, Tauri commands registration |
| `gui/src/core.rs` | Shared state and the snapshot sent to the panel |
| `gui/src/panel.rs`, `gui/src/tray.rs` | Panel placement and dismissal, status icon and menu |
| `gui/src/proxy.rs`, `gui/src/logs.rs` | In-process proxy lifecycle, log tailing and statistics |
| `gui/ui/` | Panel UI (HTML, CSS, vanilla JS) |

## Acknowledgments

Special thanks to **[Copool](https://github.com/AlickH/Copool)** and its contributors. Copool's local proxy implementation informed this project's design. This implementation uses Tokio, Hyper, reqwest and rustls. This project's HTTP parsing and account routing are implemented separately; it does not include Copool's account pool, account rotation, quota management, model mapping, or protocol conversion features.

Thanks also to [mihomo](https://github.com/MetaCubeX/mihomo) for the proxy core used in the multi-listener setup.
