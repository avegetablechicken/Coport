# Development

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

## README demos

`assets/coport-demo.gif` (Home while calls arrive) and `assets/coport-pages.gif`
(Home, Activity and Devices) show the real panel UI rendered with synthetic
data. After changing these pages, record them again (requires Google Chrome and
ffmpeg):

```sh
node scripts/readme_demo/record.mjs                 # assets/coport-demo.gif
node scripts/readme_demo/record.mjs --scene tour    # assets/coport-pages.gif
```

`scripts/readme_demo/mock.js` replaces the Tauri bridge with made-up accounts,
proxies, devices and traffic; the recorder serves `gui/ui` on a loopback port,
captures light and dark frames in headless Chrome with a temporary profile, and
joins them with ffmpeg.
`--still --at 3000,9000 --out frame.png` renders single frames at those
milliseconds for a quick check. It never starts the proxy or reads
configuration, credentials or logs.
