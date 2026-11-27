"use strict";

const { invoke, Channel } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const IS_MAC = navigator.userAgent.includes("Mac");
const REVEAL_LABEL = IS_MAC ? "Show in Finder" : navigator.userAgent.includes("Windows") ? "Show in Explorer" : "Show in Folder";

// ---------------------------------------------------------------- helpers

/// A proxy's colored label, its identity wherever it is named; `none` is
/// direct. Exit countries are not identities (proxies can share one), so
/// flags only annotate exit addresses.
function tag(name) {
  if (name === "none") return `<span class="tag direct">direct</span>`;
  // A proxy whose last test failed is grey wherever it is named.
  const down = ui.proxies[name]?.probe?.state === "error";
  return `<span class="tag ${down ? "down" : `c${ui.tagColors[name] ?? 0}`}">${esc(name)}</span>`;
}

/// Proxy candidates in order, e.g. [jp_lab] → [jp].
function chain(names) {
  return `<span class="chain">${names.map(tag).join('<span class="arrow">→</span>')}</span>`;
}

let toastTimer;
function toast(text) {
  const el = $("toast");
  el.textContent = text;
  el.classList.add("show");
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => el.classList.remove("show"), 1600);
}

// ---------------------------------------------------------------- state

const ui = {
  snap: null,
  // Opened by keyboard navigation of the menu bar (macOS 27); see menuBarStep.
  menuBarNavigation: false,
  deviceTrafficMinutes: 30,
  deviceTrafficOpen: Object.create(null),
  fetchedAt: 0,
  page: "main",
  /// Opened from View All: keep the Log at the top until the user scrolls.
  pinLog: false,
  recent: [],
  expanded: new Set(),
  builtPage: null,
  websocketOpen: false,
  otherLoginsOpen: false,
  snippetsOpen: false,
  rendering: false,
  proxies: {},
  tagColors: {},
};

const trafficViews = createTrafficViews({ invoke });
const activityLog = createActivityPager({
  invoke,
  show(update, live) {
    const apply = () => { update(); renderActivityList(); };
    if (live) keepLogScroll(apply);
    else apply();
  },
  failed: (error) => toast(String(error)),
  now: currentMinute,
});
const deviceForm = createDeviceForm();
const deviceList = createDeviceList({
  invoke, Channel, trafficViews, page: () => ui.page, minutes: () => ui.deviceTrafficMinutes,
  snap: () => ui.snap, render: () => render(), toast: message => toast(message),
});
const refreshes = createLatest();

async function refresh() {
  const request = refreshes.begin();
  const [snap, recent] = await Promise.all([
    invoke("get_state"),
    invoke("get_activity", { filter: "requests", search: "" }),
  ]);
  // A later request still in flight must not discard this completed snapshot.
  // In particular, Restart waits until its fresh configuration is displayed.
  if (!request.apply()) return;
  ui.snap = snap;
  if (deviceList.loaded) deviceList.setForwarders(snap.forwarding ? [snap.forwarding] : []);
  ui.recent = recent.rows.slice(0, 5);
  const proxies = snap.config.details?.proxies ?? [];
  ui.proxies = Object.fromEntries(proxies.map((p) => [p.name, p]));
  ui.tagColors = tagColors(proxies.map((p) => p.name));
  ui.fetchedAt = Date.now();
  // Switching forwarding on or off changes which source Home reads at once.
  if (ui.page === "main" && (Date.now() - trafficViews.homeFetchedAt >= 15000 || (trafficViews.home?.device ?? null) !== homeTrafficDevice())) loadHomeTraffic();
  if (ui.page === "activity") scheduleActivityLog();
  render();
}

/// Rereads the login item, which System Settings can change while the app runs.
async function refreshLaunchAtLogin() {
  const state = await invoke("refresh_launch_at_login");
  if (ui.snap) ui.snap.settings.launchAtLogin = state;
}

let refreshTimer;
function scheduleRefresh() {
  clearTimeout(refreshTimer);
  refreshTimer = setTimeout(refresh, 120);
}

/// Offset of the Log block from where the first block of the page starts.
function logOffset() {
  const content = $("content");
  const log = content.querySelector(".log-block");
  if (!log) return null;
  return log.getBoundingClientRect().top - content.getBoundingClientRect().top - parseFloat(getComputedStyle(content).paddingTop);
}

/// Scrolls the Activity page so the Log block starts at the top. Until the
/// user scrolls, Traffic loading above it keeps it there.
function scrollToLog() {
  $("content").scrollTop += logOffset() ?? 0;
}


/// Applies `update` without moving the Log rows in view when the list is
/// scrolled, so entries added above do not push them down.
function keepLogScroll(update) {
  const list = $("activity-list");
  if (!list) return update();
  const content = $("content");
  const top = content.getBoundingClientRect().top;
  const anchor = list.getBoundingClientRect().top < top
    ? [...list.querySelectorAll(".req[data-seq]")].find((el) => el.getBoundingClientRect().bottom > top)
    : null;
  const before = anchor?.getBoundingClientRect().top;
  update();
  const after = anchor && $("activity-list")?.querySelector(`.req[data-seq="${anchor.dataset.seq}"]`);
  if (after) content.scrollTop += after.getBoundingClientRect().top - before;
}

/// Lists log lines written since the last read when the range reaches the
/// present (or a preset moved with the clock). Waits while a choice is open or
/// an older page is loading.
function refreshActivityLog() {
  if (ui.page !== "activity" || document.hidden || menus.current() || activityLog.loadingMore) return;
  if (!$("range-editor")?.hidden) return;
  activityLog.refresh();
}

// New log lines arrive in bursts; list them at most every few seconds.
const LOG_REFRESH_MS = 3000;
let logRefreshAt = 0;
let logRefreshTimer = null;
function scheduleActivityLog() {
  if (logRefreshTimer) return;
  logRefreshTimer = setTimeout(() => {
    logRefreshTimer = null;
    logRefreshAt = Date.now();
    refreshActivityLog();
  }, Math.max(0, logRefreshAt + LOG_REFRESH_MS - Date.now()));
}

function uptime() {
  return ui.snap.phase.uptimeSecs + Math.floor((Date.now() - ui.fetchedAt) / 1000);
}

// ---------------------------------------------------------------- render

const BACK_KEYS = IS_MAC ? "Meta+[" : "Alt+ArrowLeft";

function render() {
  if (!ui.snap) return;
  if (menus.holdRender()) return;
  // The shown page stays until the next one has its data (act "page").
  if (ui.pageChange) return;
  const focus = focusPlace(document.activeElement);
  renderTop();
  renderPage();
  restoreFocus(focus);
  panelMotion.queueFit();
}

function renderTop() {
  if (ui.page === "main") {
    $("top").innerHTML = `
      <span class="app-name">Coport</span><span class="version">v${esc(ui.snap.version)}</span>
      <nav class="links">
        <button class="text-link" data-action="page" data-page="activity" data-morph="title:activity">Activity</button>
        ${(deviceList.loaded ? deviceList.devices.length : ui.snap.settings?.deviceCount || 0) > 0 ? `<button class="text-link" data-action="page" data-page="devices" data-morph="title:devices">Devices</button>` : ""}
        <button class="text-link" data-action="page" data-page="settings">Settings</button>
        <button class="text-link" data-action="quit">Quit</button>
      </nav>`;
    return;
  }
  const title = ui.page === "activity" ? "Activity" : ui.page === "devices" ? "Devices" : "Settings";
  const tools =
    ui.page === "activity"
      ? `<nav class="links">
          <button class="icon-btn" data-action="open" data-target="log" data-tip="Open log file" aria-label="Open log file">${ICON.folder}</button>
        </nav>`
      : ui.page === "devices" ? `<nav class="links"><button class="icon-btn" data-action="page" data-page="settings" data-tip="Configure devices" aria-label="Configure devices in Settings">${ICON.settings}</button></nav>` : "";
  $("top").innerHTML = `
    <button class="text-link back" data-action="page" data-page="main" aria-keyshortcuts="${BACK_KEYS}" data-morph="back">${ICON.back}Back</button>
    <span class="page-title" data-morph="title:${ui.page}">${title}</span>${tools}`;
}

function renderPage() {
  if (ui.builtPage !== ui.page) routeTooltip.hide();
  const content = $("content");
  for (const details of content.querySelectorAll("details[data-device-traffic]")) (ui.deviceTrafficOpen ||= Object.create(null))[details.dataset.deviceTraffic] = details.open;
  if (ui.page === "activity") {
    if (ui.builtPage !== "activity") {
      menus.close();
      content.innerHTML = activityShell();
      ui.builtPage = "activity";
      renderActivityTraffic();
    }
    renderActivityForwarding();
    renderActivityList();
    return;
  }
  menus.close();
  ui.builtPage = ui.page;
  // State refreshes must not discard what is being typed.
  const focused = content.contains(document.activeElement) && document.activeElement.matches("input[id]")
    ? document.activeElement : null;
  const typing = focused && { id: focused.id, value: focused.value, start: focused.selectionStart, end: focused.selectionEnd };
  ui.rendering = true;
  content.innerHTML = `<div class="page${ui.page === "settings" ? " settings-page" : ""}">${ui.page === "settings" ? settings() : ui.page === "devices" ? devicesPage() : main()}</div>`;
  const field = typing && $(typing.id);
  if (field) {
    field.value = typing.value;
    field.focus();
    field.setSelectionRange(typing.start, typing.end);
  }
  ui.rendering = false;
}

// ---------------------------------------------------------------- main

function main() {
  if (ui.snap.forwarding) {
    return remoteForwardingBlock() + connectBlock() + trafficBlock() + `<details class="disclosure"><summary>${ICON.chevron}Local proxy configuration and history (inactive)</summary>${[recentBlock(), proxiesBlock(), routingBlock()].join("")}</details>`;
  }
  return [proxyBlock(), connectBlock(), trafficBlock(), recentBlock(), proxiesBlock(), routingBlock()].join("");
}

function remoteForwardingBlock() {
  const target = ui.snap.forwarding;
  const health = target.health || {};
  const state = health.state || "checking";
  const label = { connected: "Connected", disconnected: "Disconnected · retrying", checking: "Checking connection…" }[state] || "Status unavailable";
  const stamp = value => value ? new Date(value).toLocaleTimeString() : "Not yet";
  let body = `<div class="row"><span class="row-label">${esc(target.name)}</span><span class="state">${label}</span></div><div class="row"><span class="row-label selectable">127.0.0.1:${ui.snap.phase.port}</span><span class="state">Client address unchanged</span></div><div class="row"><span class="row-label" data-tip="Includes SSH authentication and remote helper startup; not network ping">SSH connection time</span><span class="row-value">${health.latencyMs == null ? "—" : `${health.latencyMs} ms`}</span></div><div class="row"><span class="row-label">Last verified connection</span><span class="row-value">${esc(stamp(health.lastConnectedAt))}</span></div>`;
  if (target.error) body += message("warn", `${esc(target.error)} Automatic retries keep remote routing selected; requests never fall back to local routes.`);
  else if (state === "checking") body += message("info", "Checking the saved remote destination. Requests stay in remote mode.");
  else if (health.recoveredAt && Date.now() - health.recoveredAt < 120000) body += message("info", `Connection restored at ${esc(stamp(health.recoveredAt))}.`);
  const traffic = target.traffic || {};
  body += `<div class="block-head"><span class="block-title">This forwarding session</span></div><div class="row"><span class="row-label">Connections / active / failed</span><span class="row-value">${traffic.connections || 0} / ${traffic.active || 0} / ${traffic.failures || 0}</span></div><div class="row"><span class="row-label">Upload / download</span><span class="row-value">${fmtBytes(traffic.uploadBytes || 0)} / ${fmtBytes(traffic.downloadBytes || 0)}</span></div><div class="row"><span class="row-label">Upload / download speed</span><span class="row-value">${fmtBytes(traffic.uploadBytesPerSecond || 0)}/s / ${fmtBytes(traffic.downloadBytesPerSecond || 0)}/s</span></div>`;
  return block("SSH forwarding", "", body, `device/${target.deviceId}`);
}

function proxyBlock() {
  const s = ui.snap;
  const state = s.phase.state;
  // Failed covers start and stop errors and lost daemon contact; the error says which.
  const label = s.phase.busy ? "Updating…" : { running: "Running", stopped: "Stopped", failed: "Status unavailable" }[state];
  const head = `<span class="state" data-morph="proxy-state"><span class="dot ${state}"></span>${label}</span>
    <button class="switch" role="switch" aria-checked="${state === "running"}" data-action="power" ${s.phase.busy ? "disabled" : ""} aria-label="Start or stop the proxy"></button>`;
  let messages = "";
  if (!s.config.exists) {
    messages += message(
      "warn",
      "No configuration file yet.",
      `<button class="btn" data-action="create-config">Create from Example</button>
       <button class="btn" data-action="import-config">Import…</button>`
    );
  } else if (s.config.error) {
    messages += message("bad", esc(s.config.error), `<button class="btn" data-action="open" data-target="config">Edit Configuration</button>`);
  } else if (s.phase.error) {
    messages += message("bad", esc(s.phase.error));
  }
  if (s.config.changedSinceStart) {
    messages += message("warn", "The configuration changed since the proxy started.", `<button class="btn" data-action="restart">Restart to Apply</button>`);
  }
  if (s.proxyCa) messages += proxyCaMessage(s.proxyCa);
  const strip = `<div class="strip">
    ${stat("Address", `<span data-morph="address">127.0.0.1:${s.phase.port}</span>`)}
    ${stat("Uptime", state === "running" ? `<span id="uptime">${fmtUptime(uptime())}</span>` : "—")}
    ${stat("Config", s.config.error ? "Invalid" : s.config.exists ? "Valid" : "Missing", s.config.error ? "bad" : "")}
  </div>`;
  return block("Proxy", head, strip + messages, "this-device");
}

/// Hosts the trusted proxy CA does not cover stay undecrypted until the user
/// replaces it. States what still works; once kept, only a short note remains.
function proxyCaMessage(ca) {
  const hosts = ca.uncovered.map((a) => `<strong>${esc(a.replace(/:443$/, ""))}</strong>`).join(", ");
  const replace = (label, kind) => `<button class="${kind}" data-action="proxy-ca-replace">${label}</button>`;
  if (ca.kept) {
    const text = ca.uncovered.length
      ? `Proxy port skips ${hosts}; base URLs still work.`
      : "The proxy CA isn't limited to its hosts.";
    return message("info", text, replace("Replace CA…", "text-link"));
  }
  const others = ca.decrypted.length;
  const text = ca.uncovered.length
    ? `Proxy CA doesn't cover ${hosts}.<span class="message-note">Still working: base URLs${others ? ` and the proxy port for ${others} other host${others === 1 ? "" : "s"}` : ""}. Proxy-port clients can't reach ${ca.uncovered.length === 1 ? "this host" : "these hosts"}.${ca.unrestricted ? " The CA also isn't limited to its hosts." : ""}</span>`
    : `<strong>Proxy CA isn't limited to its hosts.</strong><span class="message-note">Everything still works, but it could vouch for any IP address.</span>`;
  return message("warn", text, `${replace("Replace &amp; Trust…", "btn")}<button class="btn" data-action="proxy-ca-keep">Keep Current CA</button>`);
}

/// A Devices frame completed with this device's traffic, read within the
/// Devices cadence; a Devices visit reads it again only after that.

/// Home shows the forwarded device's traffic while SSH forwarding is on, else this machine's.
function homeTrafficDevice() {
  return ui.snap?.forwarding?.deviceId ?? null;
}

async function loadHomeTraffic(force = false) {
  if (await trafficViews.loadHome(homeTrafficDevice(), force) && ui.page === "main") render();
}

function trafficBlock() {
  // Traffic read for another source (this machine or a former destination) is not shown.
  const st = trafficViews.home && (trafficViews.home.device ?? null) === homeTrafficDevice() ? trafficViews.home : null;
  const range = trafficControls("home-traffic", trafficViews.minutes, "Home traffic time range");
  const error = trafficViews.homeError === "TRAFFIC_UPDATING" ? "" : trafficViews.homeError || st?.remoteError || "";
  if (!st) return block("Traffic", range, `<div class="placeholder">${esc(error || "Loading traffic…")}</div>`, "traffic");
  return block(
    "Traffic",
    range,
    `<div class="traffic-content" aria-busy="${trafficViews.homeLoading}">
     ${error ? `<span class="traffic-update-error" title="${esc(error)}">Update failed</span>` : ""}
     ${trafficSummary(st, st.scope, st, st.device ? `device/${st.device}` : "local")}
     ${st.device ? `<div class="placeholder">Includes other clients on ${esc(st.deviceName)}. Updated ${esc(new Date(st.fetchedAt).toLocaleTimeString())}${Date.now() - st.fetchedAt > 90000 || st.remoteError ? " · stale" : ""}.</div>` : ""}</div>`,
    "traffic"
  );
}

function connectBlock() {
  const s = ui.snap;
  const row = (name, service, url) => `<div class="row">
      <span class="row-label"><span class="connect-app">${serviceMark(service)}${name}</span></span>
      <span class="row-value selectable">${esc(url)}</span>
      <button class="icon-btn" data-action="copy" data-text="${esc(url)}" data-tip="Copy" aria-label="Copy ${name} base URL">${ICON.copy}</button>
    </div>`;
  return block(
    "Connect",
    "",
    `${row("Codex", "Codex", s.urls.codex)}${row("Claude", "Claude", s.urls.claude)}${row("Antigravity", "Antigravity", s.urls.antigravity)}${row("Grok", "Grok", s.urls.grok)}
     <details class="disclosure" id="setup-snippets" ${ui.snippetsOpen ? "open" : ""}><summary>${ICON.chevron}Setup snippets</summary>
       ${snippet("Top of ~/.codex/config.toml, then restart Codex. Codex 0.156+ requires an HTTPS chatgpt_base_url", `openai_base_url = "${s.urls.base}/v1"\nchatgpt_base_url = "${s.urls.chatgpt}"`)}
       ${snippet("CA for the HTTPS chatgpt_base_url: add to the system trust store, or set SSL_CERT_FILE in ~/.codex/.env", s.urls.caCertificate)}
       ${snippet("~/.claude/settings.json, merged into existing settings", `{\n  "env": {\n    "ANTHROPIC_BASE_URL": "${s.urls.claude}"\n  }\n}`)}
       ${snippet("Antigravity CLI (agy): environment, so its Google login requests come here", agyEnvironment(s))}
       ${snippet(grokAccountRouting(s) ? "~/.grok/config.toml, so Grok Build's requests, sign-in and token renewal come here; then sign in with grok login --device-auth" : "~/.grok/config.toml, so Grok Build's login and API key requests come here", grokConfig(s))}
       ${ui.snap.config.details?.editable?.connect_credential_routing ? `
       ${snippet("Proxy port for clients using their original API URLs; set HTTPS_PROXY to the listener", `HTTPS_PROXY=${s.urls.base}\nNODE_EXTRA_CA_CERTS=${s.urls.proxyCaCertificate}\nCODEX_CA_CERTIFICATE=${s.urls.proxyCaCertificate}`)}` : ""}
     </details>`
  );
}

function snippet(label, code) {
  return `<div class="snippet"><div class="snippet-label">${label}</div><div class="snippet-code"><pre>${esc(code)}</pre>
    <button class="icon-btn" data-action="copy" data-text="${esc(code)}" aria-label="Copy snippet">${ICON.copy}</button></div></div>`;
}

// agy reads these only from its environment. Its token refresh has no URL
// setting: with the proxy port, agy's own proxy and CA variables send it here
// without HTTPS_PROXY reaching the commands it runs.
function agyEnvironment(s) {
  const lines = [
    `CLOUD_CODE_URL=${s.urls.antigravity}`,
    `USER_INFO_OVERRIDE_URL=${s.urls.antigravity}/https://www.googleapis.com/oauth2/v2/userinfo`,
    `AICODE_ENDPOINT_URL=${s.urls.antigravity}/https://aicode.googleapis.com`,
  ];
  if (s.config.details?.editable?.connect_credential_routing) {
    lines.push(`AGY_LLM_GATEWAY_PROXY_URL=${s.urls.base}`, `AGY_CA_CERT="${s.urls.proxyCaCertificate}"`);
  }
  return lines.join("\n");
}

// With account routes, Grok Build's issuer points here too, so its token
// renewal takes the login's route without HTTPS_PROXY. The issuer names the
// key Grok Build files its login under, so a device sign-in follows.
function grokAccountRouting(s) {
  const grok = s.config.details?.grok;
  return !!grok && (grok.accountRoutes.length > 0 || grok.fallbacks.some((f) => (f.key === "accountFallback" || f.key === "fallback") && f.proxies));
}

function grokConfig(s) {
  const lines = ["[endpoints]", `cli_chat_proxy_base_url = "${s.urls.grok}"`, `xai_api_base_url = "${s.urls.grok}"`];
  if (grokAccountRouting(s)) lines.push("", "[auth.oauth2]", `issuer = "${s.urls.base}/grok/https://auth.x.ai"`);
  return lines.join("\n");
}

function recentBlock() {
  const rows = ui.recent;
  const aside = rows.length ? `<button class="text-link" data-action="page" data-page="activity" data-target="log">View All</button>` : "";
  const body = rows.length
    ? rows.map((e) => requestRow(e, false)).join("")
    : `<div class="placeholder">${ui.snap.phase.state === "running" ? "Waiting for the first request…" : "Start the proxy to see requests."}</div>`;
  return block("Recent Requests", aside, body, "log");
}

function proxiesBlock() {
  const d = ui.snap.config.details;
  if (!d) return "";
  const aside = d.proxies.length ? `<button class="text-link" data-action="probe">Test All</button>` : "";
  const address = (p) => p.endpoint.replace(/^\w+:\/\//, "");
  const order = new Intl.Collator("en", { numeric: true, sensitivity: "base" });
  const body = d.proxies.length
    ? [...d.proxies]
        .sort((a, b) => order.compare(address(a), address(b)))
        .map((p) => {
          const probe = p.probe;
          const state = probe?.state ?? "idle";
          // A reachable proxy with failing destinations is not fully available.
          const routeFailing = state === "ok" && p.routeFailures?.length > 0;
          const dot = routeFailing ? "warn" : ({ ok: "running", error: "failed" }[state] ?? "");
          let value = state === "pending" ? "" : `<span class="faint">—</span>`;
          if (state === "ok") value = probe.ms == null ? "Available" : `${probe.ms} ms`;
          else if (state === "error") value = `<span class="bad">Unreachable</span>`;
          else if (state === "unavailable") value = "Unverified";
          // Plain HTTP is the common case; other schemes stay visible.
          const shown = p.endpoint.replace(/^http:\/\//, "");
          const mark = flag(probe?.country);
          const exit =
            p.local && probe?.exitIp
              ? ` → ${mark ? `<span class="flag" data-tip="${esc(probe.country)}">${mark}</span> ` : ""}${esc(probe.exitIp)}`
              : "";
          return `<div class="row proxy" data-tip-scope="proxy:${esc(p.name)}">
            <span class="dot ${dot}" ${probe?.error ? `data-tip="${esc(probe.error)}"` : ""}></span>
            <span class="proxy-heading">${tag(p.name)}<span class="row-value">${value}</span></span>
            <button class="icon-btn${state === "pending" ? " probing" : ""}" data-action="probe" data-name="${esc(p.name)}" data-tip="Test" aria-label="Test ${esc(p.name)}" aria-busy="${state === "pending"}">${ICON.restart}</button>
            <span class="row-sub">${esc(shown)}${exit}</span>
            ${routeFailing ? `<span class="row-sub proxy-route-warning">Route unavailable: ${p.routeFailures.map(esc).join(", ")}</span>` : ""}
          </div>`;
        })
        .join("")
    : `<div class="placeholder">No proxies defined; every route connects directly.</div>`;
  return block("Outbound Proxies", aside, body);
}

/// Special-purpose rules, shown de-emphasized below the regular routes.
const MINOR_FALLBACKS = new Set(["mcpFallback", "accountProbe"]);

const FALLBACK_LABEL = {
  accountFallback: "Account fallback",
  fallback: "Default fallback",
  mcpFallback: "Docs MCP fallback",
  accountProbe: "Account probe",
};

const ROUTE_ICON = {
  account: svg('<circle cx="12" cy="8" r="3"/><path d="M5 20v-2a7 7 0 0 1 14 0v2"/>'),
  key: svg('<circle cx="8" cy="9" r="4"/><path d="m11 12 9 9m-3-3 3-3m-6 0 3-3"/>'),
  gateway: svg('<circle cx="12" cy="12" r="9"/><path d="M3 12h18M12 3a19 19 0 0 1 0 18 19 19 0 0 1 0-18"/>'),
  proxy: svg('<rect x="8" y="4" width="8" height="16" rx="2"/><path d="M2 9h9m-3-3 3 3-3 3M13 15h9m-3-3 3 3-3 3"/>'),
  file: svg('<path d="M14 3H6v18h12V7zM14 3v5h4M9 12h6M9 16h6"/>'),
};

// Presentation only: selectors remain intact in configuration. Account names
// are shown shortened so the panel does not reveal a full identity.
function routeIdentity(selector, source) {
  if (source === "account") {
    return { name: maskAccountName(selector), kind: "account", type: "Account" };
  }
  if (source === "provider" || source === "profile") {
    return { name: selector, kind: "proxy", type: source === "provider" ? "Provider configuration" : "Profile configuration" };
  }
  if (source === "gateway") {
    try {
      const url = new URL(selector.startsWith("//") ? `https:${selector}` : /^[a-z]+:\/\//i.test(selector) ? selector : `https://${selector}`);
      return { name: url.host, kind: "gateway", type: "Gateway" };
    } catch (_) { /* Preserve an unrecognized selector verbatim. */ }
  }
  return source === "api_key"
    ? { name: selector, kind: "key", type: "API key" }
    : { name: selector, kind: "file", type: "Configuration (unresolved)" };
}

function routingChain(names) {
  return `<span class="chain">${names.map((name) => `<span class="route-proxy${name.length > 14 ? " route-proxy-long" : ""}" title="${esc(name === "none" ? "direct" : name)}">${tag(name)}</span>`).join('<span class="arrow">→</span>')}</span>`;
}

function routingRow(route, account, fileOnly = false, service = "") {
  const identity = routeIdentity(route.selector, route.kind || (account ? "account" : "unknown"));
  const warning = {
    remote: "No match in the local OAuth account cache. The account probe confirmed this account is available.",
    unknown: "No match in the local OAuth account cache. Its account is not identified yet; background discovery or its first request identifies it.",
  }[route.activation] ?? "No match in the local OAuth account cache. The account probe could not confirm availability. Check the credential and account probe connection.";
  const needsWarning = ["unknown", "remote", "probe_failed"].includes(route.activation);
  const activation = !account || !fileOnly ? ""
    : (route.activation === "inactive"
      ? '<span class="route-activation bad" title="No configured local credential currently selects this account route. Sign in or configure its credential source.">Inactive</span>' : "")
      + (route.activation === "probe_failed" ? '<span class="route-activation bad">Probe failed</span>' : "")
      + (needsWarning ? `<span class="route-activation route-warning tip-wide" tabindex="0" role="img" aria-label="${esc(warning)}" data-tip="${esc(warning)}">${ICON.warning}</span>` : "");
  if (account || route.kind === "api_key") {
    return `<div class="route-summary route-static" data-tip-scope="${tooltipScopeKey(JSON.stringify(["route", service, account, route.kind, route.selector]))}">
      <span class="route-identity"><span class="route-icon" aria-hidden="true">${ROUTE_ICON[identity.kind]}</span><span class="route-name" data-full-name="${esc(identity.name)}">${esc(identity.name)}</span>${activation}</span>
      <span class="route-destination">${routingChain(route.proxies)}</span>
    </div>`;
  }
  const extra = route.detail && route.detail !== route.selector ? route.detail : "";
  const title = `${identity.type}: ${route.selector}${extra ? `\n${extra}` : ""}`;
  return `<div class="route-summary" data-tip-scope="${tooltipScopeKey(JSON.stringify(["route", service, account, route.kind, route.selector]))}" title="${esc(title)}">
    <span class="route-identity"><span class="route-icon" aria-hidden="true">${ROUTE_ICON[identity.kind]}</span><span class="route-name">${esc(identity.name)}</span></span>
    <span class="route-destination">${routingChain(route.proxies)}</span>
  </div>`;
}

document.addEventListener("pointerover", (event) => {
  const name = event.target.closest?.(".route-static .route-name");
  if (!name) return;
  if (name.scrollWidth > name.clientWidth || name.textContent !== name.dataset.fullName) {
    name.title = name.dataset.fullName;
  } else {
    name.removeAttribute("title");
  }
});

const routeTooltip = createRouteTooltip({ document, window });
routeTooltip.install();

function routingBlock() {
  const s = ui.snap;
  const d = s.config.details;
  if (!d) return "";
  const report = Object.fromEntries(s.routes.map((r) => [r.name.toLowerCase(), r]));
  const section = (name, svc) => {
    const r = report[name.toLowerCase()];
    const checked = r?.checkedAt ? `Last checked: ${new Date(r.checkedAt * 1000).toLocaleTimeString()}` : "";
    const badge = r && !r.ok
      ? `<span class="bad credential-warning" tabindex="0" data-tip="${esc(`${r.reason ?? ""}\n${checked}`)}">Credential warning</span>` : "";
    const configured = svc.configured || svc.fallbacks.some((f) => f.proxies);
    if (!configured) {
      return `<div class="subhead"><span>${name}</span><span class="faint">Not configured</span></div>`;
    }
    const rows = svc.accountRoutes.map((route) => routingRow(route, true, svc.fileOnly, name)).join("")
      + svc.apiKeyRoutes.map((route) => routingRow(route, false, false, name)).join("");
    const fallbacks = svc.fallbacks.filter((f) => f.proxies)
      .sort((a, b) => MINOR_FALLBACKS.has(a.key) - MINOR_FALLBACKS.has(b.key))
      .map((f) => `<div class="route-default" data-tip-slot="${f.key}"><span class="route-default-name">${FALLBACK_LABEL[f.key]}</span><span class="route-destination">${routingChain(f.proxies)}</span></div>`).join("");
    return `<div class="routing-service" data-tip-scope="routing:${name}"><div class="subhead"><span>${name}</span>${badge}</div>${rows}
      ${fallbacks ? `<div class="route-defaults"><div class="route-defaults-title">Defaults &amp; helpers</div>${fallbacks}</div>` : ""}</div>`;
  };
  const accountError = s.accountStatesError ? `<div class="placeholder">Account status unavailable: ${esc(s.accountStatesError)}</div>` : "";
  const credentialError = s.credentialError ? `<div class="placeholder">Credential checks unavailable: ${esc(s.credentialError)}</div>` : "";
  return block("Routing", "", credentialError + accountError + section("Codex", d.codex) + section("Claude", d.claude) + section("Antigravity", d.antigravity) + section("Grok", d.grok));
}

// ---------------------------------------------------------------- activity

const menus = createMenus({
  document,
  window,
  rerender() {
    render();
    if (ui.page === "activity") renderActivityTraffic();
  },
});
menus.install();
document.addEventListener("pointerdown", (event) => {
  const editor = $("range-editor");
  if (editor && !editor.hidden && !editor.contains(event.target) && !event.target.closest?.(".select-menu")) showRangeEditor(false);
});

/// Home, Activity and Devices show traffic for one range, so changing pages
/// keeps it. Reads started for the previous range must not land afterwards.
function setTrafficMinutes(minutes) {
  trafficViews.setMinutes(minutes);
  ui.deviceTrafficMinutes = minutes;
}

async function loadTraffic() {
  $("activity-traffic")?.setAttribute("aria-busy", "true");
  if (!(await trafficViews.loadActivity())) return;
  if (ui.page === "activity") renderActivityTraffic();
  else if (ui.page === "devices") render();
}

async function assignTraffic(choice) {
  try {
    await invoke("set_traffic_assignment", choice);
  } catch (error) {
    trafficViews.activityError = String(error);
    deviceList.error = String(error);
    if (ui.page === "devices") render();
    else renderActivityTraffic();
    return;
  }
  trafficViews.reset();
  deviceList.views = Object.create(null);
  await Promise.all([loadTraffic(), deviceList.loadMerged(true), refresh()]);
}

function renderActivityTraffic() {
  if (menus.holdRender()) return;
  menus.close();
  const el = $("activity-traffic");
  if (!el) return;
  // While the Log is at or above the top, Traffic changing height must not move it.
  const logBefore = logOffset();
  const keepLog = logBefore != null && logBefore <= 0.5 && $("content").scrollTop > 0;
  const traffic = trafficViews.activity;
  const scope = traffic?.scope ?? trafficViews.scope;
  el.setAttribute("aria-busy", String(trafficViews.activityLoading));
  const date = (ms) => new Date(ms).toLocaleString([], { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" });
  const rows = traffic?.credentials.map((c) => accountRow(c, scope, traffic, trafficReview(c, traffic.targets))).join("");
  const reviews = traffic?.credentials.reduce((n, c) => n + (c.sources ?? []).filter((s) => s.reason).length, 0) ?? 0;
  const notes = [
    "The ring shows each credential's requests, or with Models its input plus output tokens, grouped by service; past each service's two largest, credentials under 2.5% of the ring are grouped as Others when there are at least two.",
    "Sorted by received traffic, largest first. Bars show requests, or with Models input plus output tokens; red indicates the share that failed, and each chart uses its own scale.",
    `${scope === "model" ? "Each HTTP model request or WebSocket generation counts once, including active calls. Tokens are reported usage, not billing totals; missing usage is not treated as zero. Input is the whole prompt, cached input included; for Claude it adds cache reads and writes to the reported input, as OpenAI reports it. Hit rate is cached input over all input tokens. First Token averages the time from the proxy receiving a call to its first streamed output, thinking included, over the calls that streamed any." : "Each HTTP request or tunnel connection counts once, including active connections."} Bytes and duration update when the call or connection ends.`,
    "Logs are retained for at least 30 days, including rotated history. Earlier records may be unavailable. Requests from a renamed configuration, one with a changed base URL, or logged before base URLs were recorded are merged and marked for review. Unidentified requests have no logged credential, or match no single current configuration. Logs are never rewritten.",
  ].join("\n\n");
  const minutes = traffic?.bucketMinutes;
  const bucketLabel = minutes >= 1440 && minutes % 1440 === 0
    ? `${minutes / 1440} ${minutes === 1440 ? "day" : "days"}`
    : minutes >= 60 && minutes % 60 === 0
      ? `${minutes / 60} ${minutes === 60 ? "hour" : "hours"}`
      : `${minutes} min`;
  el.innerHTML = `<div class="block-head"><span class="block-title">Traffic</span>
    <span class="info-tip" data-tip="${esc(notes)}" aria-label="${esc(notes)}">${ICON.info}</span>
    ${reviews ? `<button type="button" class="traffic-review-all" data-action="traffic-review" title="Requests from ${reviews} renamed, changed or unknown ${reviews === 1 ? "configuration need" : "configurations need"} review">${ICON.warning}Review</button>` : ""}
    ${trafficControls("traffic", trafficViews.minutes, "Traffic time range")}</div>
    ${trafficViews.activityError && traffic ? `<span class="traffic-update-error" title="${esc(trafficViews.activityError)}">Update failed</span>` : ""}
    ${!traffic ? `<div class="placeholder">${esc(trafficViews.activityError || "Loading traffic…")}</div>` : `${rows ? trafficShare(traffic.credentials, scope, "local") + rows : '<div class="placeholder">No requests in this time range.</div>'}<div class="traffic-axis"><span>${esc(date(traffic.start))}</span><span>${bucketLabel} per bar</span><span>${esc(date(traffic.end))}</span></div>`}`;
  if (ui.pinLog) scrollToLog();
  else if (keepLog) $("content").scrollTop += logOffset() - logBefore;
  panelMotion.queueFit();
}

const SEARCH_MODES = [["keyword", "Keyword"], ["path", "Path"], ["proxy", "Proxy"], ["status", "Status"], ["credential", "Credential"]];
function searchPlaceholder() {
  return { keyword: "Search log fields", path: "Path contains…", proxy: "Exact proxy name or direct", status: "HTTP status, e.g. 429", credential: "Account, API Key or settings" }[activityLog.searchMode];
}

const RANGE_PRESETS = [["hour", "Last hour"], ["6h", "Last 6 hours"], ["today", "Today"], ["yesterday", "Yesterday"]];

function rangeLabel() {
  const preset = RANGE_PRESETS.find(([key]) => key === activityLog.preset);
  if (preset) return preset[1];
  const day = (ms) => new Date(ms).toDateString() === new Date().toDateString()
    ? "Today" : new Date(ms).toLocaleDateString("en-US", { month: "short", day: "numeric" });
  const time = (ms) => new Date(ms).toLocaleTimeString("en-GB", { hour: "2-digit", minute: "2-digit" });
  const from = `${day(activityLog.from)} ${time(activityLog.from)}`;
  return day(activityLog.from) === day(activityLog.to)
    ? `${from} – ${time(activityLog.to)}` : `${from} – ${day(activityLog.to)} ${time(activityLog.to)}`;
}

function renderActivityRange() {
  const el = $("activity-range");
  if (!el) return;
  const items = [
    ...RANGE_PRESETS.map(([key, label]) => ({ label, action: "activity-preset", target: key, checked: activityLog.preset === key })),
    null,
    { label: "Custom Range…", action: "range-edit", checked: activityLog.preset === "custom" },
  ];
  el.innerHTML = `<button type="button" class="select range-select" data-menu="${esc(JSON.stringify(items))}"
    aria-haspopup="menu" aria-expanded="false" aria-label="Log time range">${esc(rangeLabel())}</button>`;
}

function showRangeEditor(open) {
  const editor = $("range-editor");
  if (!editor) return;
  editor.hidden = !open;
  if (!open) return;
  $("activity-from").value = localInput(activityLog.from);
  $("activity-to").value = localInput(activityLog.to);
  $("range-error").hidden = true;
  $("activity-from").focus();
}

async function applyRangeEditor() {
  const from = new Date($("activity-from").value).getTime();
  const to = new Date($("activity-to").value).getTime();
  // `to` is an inclusive minute, so a single-minute range is allowed.
  if (Number.isNaN(from) || Number.isNaN(to) || from > to) {
    $("range-error").hidden = false;
    return;
  }
  activityLog.preset = "custom";
  activityLog.from = from;
  activityLog.to = to;
  showRangeEditor(false);
  renderActivityRange();
  await activityLog.load();
}

/// Forwarded requests are logged on the destination, not here; say so rather
/// than let this machine's log look idle.
function renderActivityForwarding() {
  const el = $("activity-forwarding");
  if (!el) return;
  const target = ui.snap?.forwarding;
  const html = target
    ? `<div class="block-head"><span class="block-title">Forwarding to ${esc(target.name)}</span><span class="block-aside"><button class="text-link" data-action="page" data-page="devices">View in Devices</button></span></div>
      <div class="placeholder">New requests are handled and logged on ${esc(target.name)}. This page shows this machine's history.</div>`
    : "";
  el.hidden = !target;
  el.dataset.morph = target ? `device/${target.deviceId}` : "";
  // Refreshes must not replace the link while it is focused or hovered.
  if (el.innerHTML !== html) el.innerHTML = html;
}

function activityShell() {
  return `<div class="page">
    <section class="block" id="activity-forwarding" hidden></section>
    <section class="block" id="activity-traffic" data-morph="traffic"></section>
    <section class="block log-block" data-morph="log">
      <div class="block-head"><span class="block-title">Log</span><span class="block-aside">
        ${panelSelect("activity-side", ACTIVITY_SIDES, activityLog.upstream ? "upstream" : "client", "Responses listed", "", `title="${esc(ACTIVITY_SIDE_TIP)}"`)}<span id="activity-range"></span></span></div>
      <div class="range-editor" id="range-editor" role="dialog" aria-label="Custom log range" hidden>
        <label class="range-row"><span class="range-label">From</span>
          <input class="field" id="activity-from" type="datetime-local" aria-label="Start of the log range" /></label>
        <label class="range-row"><span class="range-label">To</span>
          <input class="field" id="activity-to" type="datetime-local" aria-label="End of the log range" />
          <button type="button" class="text-link" data-action="range-now">Now</button></label>
        <div class="range-error" id="range-error" hidden>The start must be before the end.</div>
        <div class="range-actions">
          <button type="button" class="btn" data-action="range-cancel">Cancel</button>
          <button type="button" class="btn primary" data-action="range-apply">Apply</button>
        </div>
      </div>
      <div class="activity-search">
        ${panelSelect("search-mode", SEARCH_MODES, activityLog.searchMode, "Search condition")}
        <label class="search">${ICON.search}
          <input class="field" id="search" type="search" spellcheck="false" aria-label="Search logs" placeholder="${esc(searchPlaceholder())}" value="${esc(activityLog.search)}" /></label>
      </div>
      <div class="segmented" id="filters"></div>
      <div id="activity-list"></div>
    </section>
  </div>`;
}

const ACTIVITY_SIDES = [["client", "Client"], ["upstream", "Upstream"]];
const ACTIVITY_SIDE_TIP = "Client: responses the proxy sent to clients. Upstream: responses upstreams sent to the proxy.";

function renderActivitySide() {
  const el = $("activity-side");
  if (!el) return;
  // All Events lists both sides, so there is nothing to switch.
  el.hidden = activityLog.filter === "all";
}

function renderActivityList() {
  const filters = $("filters");
  if (!filters) return;
  renderActivityRange();
  renderActivitySide();
  filters.innerHTML = [
    ["requests", "Requests"],
    ["models", "Models"],
    ["errors", "Errors"],
    ["all", "All Events"],
  ]
    .map(([f, label]) => `<button aria-pressed="${activityLog.filter === f}" data-action="filter" data-filter="${f}">${label}</button>`)
    .join("");
  $("activity-list").innerHTML = activityLog.error
    ? `<div class="placeholder">${esc(activityLog.error)}</div>`
    : activityLog.rows.length
      ? activityLog.rows.map((e) => requestRow(e, true) + (ui.expanded.has(e.seq) ? detail(e) : "")).join("")
        + (activityLog.next ? '<div class="list-more"><button type="button" class="btn" data-action="activity-more">Load More</button></div>' : "")
      : '<div class="placeholder">No matching entries in this range.</div>';
  panelMotion.queueFit();
}

function requestRow(e, expandable) {
  const deviceQuery = ["device_statistics_succeeded", "device_statistics_failed", "device_statistics_recovered"].includes(e.event);
  const queryDevice = deviceQuery && deviceList.devices.find(device => device.id === e.fields?.device_id);
  const queryLabel = deviceQuery ? `Statistics query · ${queryDevice?.name || `Device ${String(e.fields?.device_id || "").slice(0, 8)}`}` : "";
  const cancelled = e.event === "request_cancelled" || e.event === "model_call_cancelled";
  const modelCall = e.event.startsWith("model_call_");
  const unknown = e.event === "model_call_unknown";
  const incomplete = e.event === "model_call_incomplete";
  const code = deviceQuery ? (e.error ? "ERR" : "OK") : cancelled ? "CXL" : modelCall ? (unknown ? "?" : e.error ? "ERR" : incomplete ? "INC" : e.event === "model_call_finished" ? "OK" : "RUN") : e.error && !(e.status >= 400) ? "ERR" : e.status ?? "—";
  const tip = deviceQuery ? (e.error ? "Statistics query failed" : e.event === "device_statistics_recovered" ? "Statistics connection recovered" : "Statistics query succeeded") : cancelled ? "Cancelled" : unknown ? "Outcome unavailable" : incomplete ? `Incomplete${e.fields?.incomplete_reason ? `: ${e.fields.incomplete_reason}` : ""}` : "";
  const meta = [fmtTime(e.time), deviceQuery && e.fields?.transport?.toUpperCase(), deviceQuery && e.fields?.query_count > 1 && `${e.fields.query_count} queries since previous event`, modelCall && e.fields?.model, e.service, e.proxy && (e.proxy === "none" ? "direct" : e.proxy), e.bytes != null && fmtBytes(e.bytes)]
    .filter(Boolean)
    .join(" · ");
  const tag = expandable ? "button" : "div";
  const attrs = expandable ? `data-action="expand" data-seq="${e.seq}" aria-expanded="${ui.expanded.has(e.seq)}"` : "";
  return `<${tag} class="req" ${attrs} data-morph="req:${esc(e.time)}:${esc(e.seq)}">
    <span class="status ${cancelled || unknown ? "cancelled" : e.error ? "s5" : deviceQuery ? "s2" : modelCall ? (incomplete ? "s4" : e.event === "model_call_finished" ? "s2" : "s3") : statusClass(e.status)}"${tip ? ` title="${esc(tip)}" aria-label="${esc(tip)}"` : ""}>${esc(code)}</span>
    <span class="req-main">
      <span class="req-path">${e.method ? `<span class="method">${esc(e.method)}</span>` : ""}${esc(deviceQuery ? queryLabel : e.path ?? e.event)}</span>
      ${expandable && meta ? `<span class="req-meta">${esc(meta)}</span>` : ""}
    </span>
    <span class="req-dur">${e.durationMs != null ? fmtMs(e.durationMs) : ""}</span>
  </${tag}>`;
}

function detail(e) {
  const rows = Object.entries(e.fields)
    .map(([k, v]) => {
      let value = typeof v === "string" ? v : JSON.stringify(v);
      if (["duration_ms", "headers_ms", "first_token_ms", "forwarded_first_token_ms", "location_cooldown_actual_ms", "location_cooldown_wait_ms", "upstream_started_ms"].includes(k)) value = fmtMs(Number(value));
      if (k === "received_bytes") value = fmtBytes(Number(value));
      return `<dt>${esc(k)}</dt><dd>${esc(value)}</dd>`;
    })
    .join("");
  const json = JSON.stringify({ event: e.event, timestamp: e.time ? new Date(e.time).toISOString() : undefined, ...e.fields }, null, 2);
  return `<div class="detail" data-tip-scope="detail:${esc(e.time)}:${esc(e.seq)}">
    <dl class="kv"><dt>event</dt><dd>${esc(e.event)}</dd>${e.time ? `<dt>time</dt><dd>${esc(new Date(e.time).toLocaleString("en-GB"))}</dd>` : ""}${rows}</dl>
    <button class="icon-btn" data-action="copy" data-text="${esc(json)}" data-tip="Copy as JSON" aria-label="Copy as JSON">${ICON.copy}</button>
  </div>`;
}

// ---------------------------------------------------------------- devices

installDevicesPage();

// ---------------------------------------------------------------- page motion

const panelMotion = createPageMotion({ document, ui, invoke });
panelMotion.install();
// #content fills the window, so in-page growth (e.g. an opened disclosure)
// only shows up on the page element, which each render replaces.
new MutationObserver(() => {
  routeTooltip.refresh();
  panelMotion.watchPage($("content").firstElementChild);
}).observe($("content"), { childList: true, subtree: true, characterData: true, attributes: true, attributeFilter: ["title", "data-tip"] });
new MutationObserver(() => routeTooltip.refresh()).observe($("top"), {
  childList: true, subtree: true, characterData: true, attributes: true, attributeFilter: ["title", "data-tip"],
});

// ---------------------------------------------------------------- share rings

const ringMotion = createRingMotion({
  document,
  morphMs: MORPH_MS,
  still: () => panelMotion.moving() || REDUCED_MOTION.matches || document.hidden,
});
ringMotion.install($("content"));

// ---------------------------------------------------------------- actions

async function act(action, el) {
  const s = ui.snap;
  switch (action) {
    case "page": {
      ui.page = el.dataset.page;
      // The shown page stays while the next one reads what it shows first, so
      // the change happens in one motion. A later change supersedes this one.
      const change = ui.pageChange = (ui.pageChanges || 0) + 1;
      ui.pageChanges = change;
      try {
        if (ui.page === "activity") {
          if (s.settings?.deviceCount > 0) {
            try { await deviceList.loadDefinitions(); } catch (error) { toast(String(error)); }
          }
          // Home's ring, already read, opens Activity unchanged.
          trafficViews.activity = trafficViews.cached(null) ?? trafficViews.activity;
          loadTraffic();
          // Presets follow the clock; a custom range stays as chosen.
          activityLog.resolveRange();
          await activityLog.load();
        }
        if (ui.page === "main") {
          trafficViews.home = trafficViews.cached(homeTrafficDevice()) ?? trafficViews.home;
          loadHomeTraffic();
        }
        if (ui.page === "devices") {
          // Peers' views for the shared range, from the last refresh, open with the page.
          deviceList.select();
          deviceList.load();
        }
        if (ui.page === "settings") await Promise.all([deviceList.load(), refreshLaunchAtLogin()]);
      } finally {
        if (ui.pageChange === change) ui.pageChange = null;
      }
      if (ui.pageChanges !== change) break;
      ui.pinLog = ui.page === "activity" && el.dataset.target === "log";
      panelMotion.changePage(() => {
        render();
        $("content").scrollTop = 0;
        if (ui.pinLog) scrollToLog();
      });
      break;
    }
    case "forwarding-restore": {
      if (ui.forwardingRestoreBusy) break;
      const enabled = !ui.snap.forwardingRestore;
      ui.forwardingRestoreBusy = true; render();
      try { await invoke("set_forwarding_restore", { enabled }); ui.snap.forwardingRestore = enabled; }
      catch (error) { toast(String(error)); }
      finally { ui.forwardingRestoreBusy = false; render(); }
      break;
    }
    case "device-forward": {
      if (ui.deviceForwardingBusy) break;
      ui.deviceForwardingBusy = el.dataset.id; render();
      try {
        ui.snap.forwarding = await deviceList.forward(el.dataset.id, el.dataset.enabled === "true");
      } catch (error) { toast(String(error)); }
      finally { ui.deviceForwardingBusy = null; render(); }
      break;
    }
    case "device-new":
      deviceForm.add(); render(); $("device-name")?.focus();
      break;
    case "device-cancel":
      deviceForm.close(); render();
      break;
    case "device-edit": {
      const d = deviceList.devices.find(device => device.id === el.dataset.id);
      if (d) {
        deviceForm.edit(d); render(); $("device-name")?.focus();
      }
      break;
    }
    case "merged-refresh":
      await deviceList.refresh();
      break;
    case "device-save": {
      captureDeviceDraft(); const device = deviceForm.device();
      try { await deviceList.save(device, () => deviceForm.close()); }
      catch (e) { toast(String(e)); }
      break;
    }
    case "device-remove":
      try { await deviceList.remove(el.dataset.id); }
      catch (e) { toast(String(e)); }
      break;
    case "power":
      try {
        await invoke("set_running", { running: s.phase.state !== "running" });
      } catch (e) {
        toast(String(e));
      }
      await refresh();
      break;
    case "restart": {
      let restarted = false;
      try {
        await invoke("restart_proxy");
        restarted = true;
      } catch (e) {
        toast(String(e));
      }
      await refresh();
      if (restarted) toast("Proxy restarted");
      break;
    }
    case "proxy-ca-replace":
      try {
        const trusted = await invoke("replace_proxy_ca");
        if (trusted === true) toast("Proxy CA replaced and trusted");
        else if (trusted === false) toast("Proxy CA replaced. Add it to your trust store");
      } catch (e) {
        toast(String(e));
      }
      await refresh();
      break;
    case "proxy-ca-keep":
      try {
        await invoke("keep_proxy_ca");
      } catch (e) {
        toast(String(e));
      }
      await refresh();
      break;
    case "copy": {
      try {
        await invoke("copy_text", { text: el.dataset.text });
      } catch (e) {
        toast(String(e));
        break;
      }
      const before = el.innerHTML;
      el.innerHTML = ICON.check;
      el.classList.add("done");
      setTimeout(() => {
        el.innerHTML = before;
        el.classList.remove("done");
      }, 1200);
      break;
    }
    case "expand": {
      const seq = Number(el.dataset.seq);
      if (ui.expanded.has(seq)) ui.expanded.delete(seq);
      else ui.expanded.add(seq);
      renderActivityList();
      break;
    }
    case "filter":
      activityLog.filter = el.dataset.filter;
      await activityLog.load();
      break;
    case "traffic-assign":
      await assignTraffic(JSON.parse(el.dataset.target));
      break;
    case "traffic-review": {
      const first = document.querySelector(".traffic-review.review");
      first?.scrollIntoView({ block: "center", behavior: "smooth" });
      if (first) setTimeout(() => menus.showMenu(first), 250);
      break;
    }
    case "clear-traffic-compatibility":
      try {
        if (await invoke("clear_traffic_compatibility")) {
          toast("Traffic compatibility choices cleared");
        }
      } catch (e) {
        toast(String(e));
      }
      await refresh();
      break;
    case "activity-more":
      el.disabled = true;
      await activityLog.load("more");
      break;
    case "activity-preset":
      activityLog.preset = el.dataset.target;
      activityLog.resolveRange();
      renderActivityRange();
      await activityLog.load();
      break;
    case "range-edit":
      showRangeEditor(true);
      break;
    case "range-now":
      $("activity-to").value = localInput(currentMinute());
      break;
    case "range-cancel":
      showRangeEditor(false);
      break;
    case "range-apply":
      await applyRangeEditor();
      break;
    case "open":
      await invoke("open_path", { target: el.dataset.target });
      break;
    case "probe":
      await invoke("probe_proxy", { name: el.dataset.name ?? null });
      await refresh();
      break;
    case "launch-at-login":
      try {
        await invoke("set_launch_at_login", { enabled: el.getAttribute("aria-checked") !== "true" });
      } catch (e) {
        toast(String(e));
      }
      await refresh();
      break;
    case "open-login-items":
      await invoke("open_login_items");
      break;
    case "auto-start":
      await invoke("update_settings", { patch: { startProxyOnLaunch: el.getAttribute("aria-checked") !== "true" } });
      await refresh();
      break;
    case "keep-running":
      await invoke("update_settings", { patch: { keepProxyRunningOnQuit: el.getAttribute("aria-checked") !== "true" } });
      await refresh();
      break;
    case "mask-account-names":
      await invoke("update_settings", { patch: { maskAccountNames: el.getAttribute("aria-checked") !== "true" } });
      await refresh();
      break;
    case "config-flag":
      // The new configuration value: the switch's new state, unless inverted.
      await setConfigValue(el.dataset.key, (el.getAttribute("aria-checked") !== "true") !== (el.dataset.inverted === "true"));
      break;
    case "create-config":
      try {
        await invoke("create_example_config");
        toast("Example configuration created");
      } catch (e) {
        toast(String(e));
      }
      await refresh();
      break;
    case "import-config":
      try {
        if (await invoke("import_config")) toast("Configuration imported");
      } catch (e) {
        toast(String(e));
      }
      await refresh();
      break;
    case "quit":
      await invoke("quit_app");
      break;
  }
}

// Any scroll or input on the page ends a View All pin of the Log.
for (const type of ["wheel", "touchstart", "keydown", "pointerdown"]) {
  $("content").addEventListener(type, () => { ui.pinLog = false; }, { passive: true });
}

document.addEventListener("click", (event) => {
  const select = event.target.closest("[data-options]");
  if (select) { menus.showSelect(select); return; }
  const menu = event.target.closest("[data-menu]");
  if (menu) { menus.showMenu(menu); return; }
  const el = event.target.closest("[data-action]");
  if (el && !el.disabled) act(el.dataset.action, el);
});

document.addEventListener("change", async (event) => {
  if (event.target.id === "activity-side") {
    activityLog.upstream = event.target.value === "upstream";
    await activityLog.load();
    return;
  }
  if (event.target.id === "search-mode") {
    activityLog.searchMode = event.target.value;
    clearTimeout(searchTimer);
    $("search").placeholder = searchPlaceholder();
    await activityLog.load();
    return;
  }
  if (event.target.id === "devices-traffic-scope" || event.target.id === "devices-traffic-range") {
    if (event.target.id === "devices-traffic-scope") {
      trafficViews.setScope(event.target.value);
    } else { setTrafficMinutes(Number(event.target.value)); }
    const cards = deviceList.loadCards();
    const shown = deviceList.select();
    // A remote refresh delivers every range when it completes. A cache read now
    // would supersede it and show peers as not refreshed, so wait for it instead.
    if (deviceList.loading && deviceList.remote) { if (!shown) deviceList.merged = null; render(); await cards; return; }
    if (shown && deviceList.frameFresh(deviceList.merged)) { render(); await cards; return; }
    // A cached frame shows at once while a read completes it with this device.
    const loading = deviceList.loadMerged(true); render(); await Promise.all([loading, cards]);
    return;
  }
  if (event.target.id === "home-traffic-scope" || event.target.id === "traffic-scope") {
    // Both views share the category. In-flight results from the old category
    // must not repopulate either cache after a switch; the previous charts
    // stay mounted until the replacement data is ready.
    trafficViews.setScope(event.target.value);
    trafficViews.clearErrors();
    if (ui.page === "main") {
      const loading = loadHomeTraffic(true);
      render();
      await loading;
    } else {
      renderActivityTraffic();
      await loadTraffic();
    }
    return;
  }

  if (event.target.id === "home-traffic-range") {
    setTrafficMinutes(Number(event.target.value));
    // Launch immediately so concurrent refreshes cannot start an older range.
    const loading = loadHomeTraffic(true);
    render();
    await loading;
    return;
  }
  if (event.target.id === "traffic-range") {
    setTrafficMinutes(Number(event.target.value));
    renderActivityTraffic();
    await loadTraffic();
    return;
  }
  const key = event.target.dataset?.setting;
  if (!key) return;
  await invoke("update_settings", { patch: { [key]: event.target.value } });
  await refresh();
});

let searchTimer;
document.addEventListener("input", (event) => {
  if (event.target.id !== "search") return;
  activityLog.search = event.target.value;
  clearTimeout(searchTimer);
  searchTimer = setTimeout(() => activityLog.load(), 120);
});

/// Whether keys on `el` edit text, where ←/→ move the caret.
function textEntry(el) {
  return Boolean(el?.closest?.("textarea, [contenteditable], input:not([type=checkbox]):not([type=radio]):not([type=button])"));
}

/// While keyboard navigation of the menu bar holds the panel open (macOS 27),
/// the focused panel receives ←/→; those it does not use move the navigation
/// on to the neighbouring item, as from a menu. Every other key keeps its own
/// meaning. Returns "left", "right" or null.
function menuBarStep(event) {
  if (!ui.menuBarNavigation || event.defaultPrevented || event.isComposing) return null;
  if (event.metaKey || event.ctrlKey || event.altKey || event.shiftKey || textEntry(event.target)) return null;
  return event.key === "ArrowLeft" ? "left" : event.key === "ArrowRight" ? "right" : null;
}

document.addEventListener("keydown", (event) => {
  const editor = $("range-editor");
  if (editor && !editor.hidden && editor.contains(event.target) && (event.key === "Escape" || event.key === "Enter")) {
    event.preventDefault();
    if (event.key === "Escape") showRangeEditor(false);
    else applyRangeEditor();
    return;
  }
  const open = menus.current();
  if (open) {
    const { menu } = open;
    const items = menus.items(menu);
    const index = items.indexOf(document.activeElement);
    if (event.key === "Escape" || event.key === "Tab") {
      menus.close(true);
      if (event.key === "Escape") event.preventDefault();
      return;
    }
    if (["ArrowDown", "ArrowUp", "Home", "End"].includes(event.key)) {
      event.preventDefault();
      const next = event.key === "Home" ? 0 : event.key === "End" ? items.length - 1
        : (index + (event.key === "ArrowDown" ? 1 : -1) + items.length) % items.length;
      items[next].focus();
      return;
    }
  } else if (event.target.matches("[data-options], [data-menu]") && ["ArrowDown", "ArrowUp"].includes(event.key)) {
    event.preventDefault();
    (event.target.dataset.menu ? menus.showMenu : menus.showSelect)(event.target, event.key === "ArrowUp");
    return;
  }
  if (event.target.matches("[data-config]") && (event.key === "Enter" || event.key === "Escape")) {
    if (event.key === "Escape") event.target.value = ui.snap.config.details?.editable[event.target.dataset.config] ?? "";
    event.target.blur();
    return;
  }
  const step = menus.current() ? null : menuBarStep(event);
  if (step) {
    event.preventDefault();
    invoke("navigate_menu_bar", { right: step === "right" });
    return;
  }
  const mod = event.metaKey || event.ctrlKey;
  const back = !event.shiftKey && (IS_MAC
    ? event.metaKey && !event.ctrlKey && !event.altKey && event.key === "["
    : event.altKey && !event.ctrlKey && !event.metaKey && event.key === "ArrowLeft");
  if (event.key === "Escape") {
    if (ui.page !== "main") act("page", { dataset: { page: "main" } });
    else invoke("hide_panel");
  } else if (back) {
    // Same as the Back button, on every page that has one.
    event.preventDefault();
    if (ui.page !== "main") act("page", { dataset: { page: "main" } });
  } else if (mod && event.key === "q") {
    invoke("quit_app");
  } else if (mod && event.key === ",") {
    act("page", { dataset: { page: "settings" } });
  }
});

document.addEventListener("focusout", (event) => {
  if (!ui.rendering && event.target.matches?.("[data-config]")) commitConfigField(event.target);
});

// Re-renders rebuild disclosures, so their open state lives in `ui`.
document.addEventListener("toggle", (event) => {
  const { id, open } = event.target;
  if (id === "websocket-timeouts") ui.websocketOpen = open;
  else if (id === "other-logins") ui.otherLoginsOpen = open;
  else if (id === "setup-snippets") ui.snippetsOpen = open;
}, true);

document.addEventListener("contextmenu", (event) => {
  if (!event.target.closest(".selectable, input, pre, dd")) event.preventDefault();
});

// Live uptime without refetching state.
setInterval(() => {
  const el = $("uptime");
  if (el && ui.snap?.phase.state === "running") el.textContent = fmtUptime(uptime());
}, 1000);

// Check configured proxies at startup; the backend retests aged results on its own
// schedule, so opening the panel does not trigger tests.
const probeStale = () => invoke("probe_proxy", { name: null, staleOnly: true });

// Linux has no system material behind the panel: the backend sends the screen
// under it, blurred, and where the window lies on it. `image` is null when
// unchanged and empty without a backdrop.
window.__TAURI__.event
  .listen("panel-backdrop", ({ payload }) => {
    const root = document.documentElement;
    root.classList.toggle("composited", payload.composited);
    root.style.setProperty("--backdrop-size", `${payload.width}px ${payload.height}px`);
    root.style.setProperty("--backdrop-position", `${-payload.x}px ${-payload.y}px`);
    if (payload.image === null) return;
    root.style.setProperty("--backdrop-image", payload.image ? `url("${payload.image}")` : "none");
    root.classList.toggle("frosted", Boolean(payload.image));
  })
  .catch((error) => toast(error));

listen("state-changed", scheduleRefresh);
const PANEL_PAGE_TIMEOUT_MS = 60 * 1000;
let panelHiddenAt = null;
listen("panel-shown", async ({ payload } = {}) => {
  ui.menuBarNavigation = Boolean(payload?.keyboard);
  // Keys start on the panel, not in a text field left focused, where ←/→
  // would not move along the menu bar.
  if (ui.menuBarNavigation && textEntry(document.activeElement)) document.activeElement.blur();
  const returnHome = panelHiddenAt !== null && Date.now() - panelHiddenAt >= PANEL_PAGE_TIMEOUT_MS;
  panelHiddenAt = null;
  if (returnHome) {
    ui.page = "main";
    menus.close();
    render();
    $("content").scrollTop = 0;
  }
  if (deviceList.loaded) deviceList.loadCapabilities();
  // Returning from System Settings may have allowed or blocked the login item.
  if (ui.page === "settings") await refreshLaunchAtLogin();
  return refresh();
});
listen("panel-hidden", () => {
  ui.menuBarNavigation = false;
  routeTooltip.hide();
  // Repeated native hide events must not extend the previous page's lifetime.
  panelHiddenAt ??= Date.now();
  menus.close();
});
refresh().then(probeStale);
deviceList.checkSsh();

// Refresh even when no new requests arrive, so the rolling window advances.
setInterval(() => {
  if (document.hidden) return;
  if (ui.page === "activity") {
    deviceList.loadCapabilities();
    loadTraffic();
    scheduleActivityLog();
  } else if (ui.page === "devices") { refresh(); deviceList.load(); }
  else if (ui.page === "main") { refresh(); deviceList.loadCapabilities(); }
  else if (ui.page === "settings") refreshForwardingStatus();
}, 15000);
