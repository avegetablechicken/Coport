# Desktop app (menu bar / tray)

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
| Settings | Launch at Login, auto-start, keep running after quit, appearance, proxy port credential routing, config file status and `--check` equivalent, log folder |

The Activity log keeps its Requests, Models, Errors and All Events tabs. Use the
search condition dropdown for Keyword (all string fields), Path (contains; a CONNECT shows and matches its `host:port`),
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
uses cached account status, and does not poll remote device statistics. Devices
is read-only: the first visit after the app starts retrieves remote statistics
once, showing this device immediately and each peer as it answers. Later visits
and its periodic refresh read cached statistics; use the Devices **Refresh**
action to retrieve them again. These UI controls do not stop health checks
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
the same two alignment boundaries and privacy/size limits. Both SSH frames use
one fixed snapshot boundary, even if computing the complete snapshot crosses a
minute. The selected and full snapshots each parse every log file once for their
scopes and boundaries. A progressive refresh does not reconnect automatically
when peers cannot align: it keeps the boundary most devices share and reports
the others, which the next explicit refresh retries.
Once all twelve views are cached, range/category switches are immediate.
Background failures retain statistics already displayed. The first Devices visit
after the app starts refreshes once; later page entry, periodic refresh and
range/category switches read cached snapshots only, and the explicit **Refresh**
action retrieves remote statistics for the current selection. Local
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
Home's credential warnings use current diagnostic results rather than startup logs.
The daemon checks Codex and Claude credentials every 60 seconds after each check,
regardless of success or failure, and continues when the GUI is closed. The GUI
reads these results in the background and checks locally only when the daemon is
confirmed absent; hiding the panel does not stop the GUI timer. An unavailable or
older daemon is reported explicitly instead of checking under a different process
environment. Warnings include the check time. When `claude.routing.account_probe`
is explicitly set, the background worker also resolves missing saved Claude account
identities through that route. Successful identities are cached per credential;
failed lookups are retried on later check cycles. `account_fallback` alone does not
authorize this discovery. Statistics views read the cache without initiating probes,
and neither credentials nor Claude metadata files are rewritten. Credential warnings
validate local credential resolution and routing, not upstream service availability. Configuration changes
still require restarting a running daemon.

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

Settings shows a compact version and capability status. The daemon checks each
configured SSH connection once per configuration, with at most four checks in flight.
Page visits and status reads do not start additional SSH checks. Existing capability,
statistics, and active-forwarding health results carry observation times; Settings
and Devices use the newest result rather than letting a startup failure override a
later success. Once Devices has been visited, an observed connection recovery also
refreshes its statistics, even while another page is open. Recoveries during an
in-flight statistics request are queued and coalesced. Before the first Devices
visit, recovery updates status only. The GUI reads local cached status every 15 seconds
while visible and on reopening; it does not add a periodic SSH probe.
No manual version-check
button is needed. Unverified or unsupported destinations have a disabled forwarding
switch; an already active destination can always be switched off. Errors are shown
as a short status with details on hover. Older peers without `--capabilities` must
be updated before enabling a new forwarding session from the GUI.

This mode uses remote routing. Using the remote device solely as a network exit
while retaining local service/account routing is not yet supported.
