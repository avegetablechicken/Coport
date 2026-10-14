# Multiple proxy services with mihomo and Clash

[mihomo](https://github.com/MetaCubeX/mihomo) is the proxy core used by compatible Clash clients. One core can expose several local proxy services at once, each pinned to a different outbound node. For example:

| Account mapping | Local endpoint | Fixed outbound node |
| --- | --- | --- |
| `us` | `127.0.0.1:8101` | `US-Node` |
| `jp` | `127.0.0.1:8102` | `JP-Node` |

Use a Clash client with a **mihomo-compatible core**. Older Clash cores may not support custom `listeners`. System Proxy and TUN mode are not required for these explicit local connections.

## 1. Add dedicated listeners

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

## 2. Configure the outbound nodes

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

## 3. Load the configuration

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

## 4. Verify each exit and connect the application

Test each listener independently:

```sh
curl --noproxy '' --proxy http://127.0.0.1:8101 --max-time 20 https://api.ipify.org
curl --noproxy '' --proxy socks5h://127.0.0.1:8102 --max-time 20 https://api.ipify.org
```

These commands contact an external IP-check service through the selected node. Compare the results with your provider's expected exits; an IP response alone does not establish a country or guarantee access to the Codex upstream. `socks5h` is curl's remote-DNS option; use `socks5://` in this application's YAML.

The `proxies` entries in `config.example.yaml` already match these listener ports. Add the real account mappings, run `--check`, start the application, and inspect `route_selected` events to confirm the account and local endpoint selected for each request.

For another region, add a node, give it a listener on a unique port, and add the corresponding application proxy label and account mapping.

Reference: [mihomo listener fields](https://wiki.metacubex.one/en/config/inbound/listeners/) and [official configuration examples](https://github.com/MetaCubeX/mihomo/blob/Meta/docs/config.yaml).
