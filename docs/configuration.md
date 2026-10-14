# Configuration

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

## Service-specific matching

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

Codex API Key URL selectors match explicit upstream requests (see [Codex API URL routes](clients.md#codex-api-url-routes)).
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

## Upstreams, proxies and ordered candidates

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

## Command-line configuration overrides

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

## Older configurations

Earlier layouts are no longer read. Top-level Codex settings (`auth_file`,
`base_url`, `routing`, `accounts`, `api_key_providers`, `*_upstream_base_url`,
`*_fallback_proxy`), nested `accounts` sources and Claude inline routing fields
or split base URLs are rejected at startup. Move Codex settings under `codex:`,
Claude routing under `claude.routing`, and replace credential paths with
`homes`/`config_dirs`. `codex.auth_file` and `claude.auth_file` are accepted but
ignored.


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


## Limits and troubleshooting

- Default upstreams require HTTPS and a public service hostname. Explicit API URL routes and Codex custom provider upstreams also accept public IPv4 addresses; private addresses and local hostnames remain rejected.
- Proxy URLs require an explicit port and support `http`, `https`, or `socks5`. Optional username/password authentication uses `scheme://username:password@host:port`. Credentials are removed from logged proxy URLs.
- Inbound limits: 32 MiB request body, 64 KiB headers, 128 concurrent connections, and a 30-second read timeout. Content-Length and chunked uploads are supported; each connection handles one request.
- Repeated list-valued `Accept`, `Accept-Encoding`, `Accept-Language`, `Cache-Control`, `Connection`, `Pragma`, and `Via` headers are supported. Duplicate authentication, routing, Host, framing, and other unknown headers are rejected.
- `Expect: 100-continue` returns HTTP 417. Only WebSocket version 13 GET upgrades are supported; other Upgrade requests return HTTP 426.
- Upstream response chunks, including SSE, are forwarded as they arrive with backpressure. There is no whole-response buffering or automatic decompression. Content-Encoding is preserved when returned by an upstream.
- Bodyless GET requests (excluding WebSocket upgrades) retry connection, timeout, or request transport failures before response headers at most twice, with 200 ms and 400 ms backoff. Attempts use the same selected proxy and share one deadline with route selection, cold probes, backoff and response streaming. Claude profile lookups use the same sender with a 10-second maximum budget. HTTP error responses, response-body failures, and other methods are not retried. Proxy selection failures are not retried. Request logs include `upstream_attempts`; final transport failures also include a sanitized `transport_error` category.
- `request_timeout_seconds` accepts 1–3600 seconds and configures the HTTP request timeout and the idle timeout for established WebSocket/CONNECT tunnels. Tunnel activity in either direction resets the idle timer; active tunnels have no total lifetime limit. A failure after streaming starts closes the connection without inserting a JSON error into the stream.
- Local HTTP 401 means the Bearer token is missing/malformed, or no credential matches and OpenAI fallback is disabled. HTTP 409 means the account header does not match or the token matches multiple routes. HTTP 502 indicates a routing/configuration or upstream connection failure. Upstream HTTP errors retain their original status and body.
- If a mihomo listener refuses connections, confirm the effective profile contains it, its node name is valid, and the port is not occupied. If the exit changes unexpectedly, inspect the listener's node/group selection.
- After rebuilding a running service, restart that process to use the new executable.
