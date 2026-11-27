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
  if (ui.page !== "activity" || document.hidden || openSelect || activityLog.loadingMore) return;
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
  if (openSelect) { selectRenderPending = true; return; }
  // The shown page stays until the next one has its data (act "page").
  if (ui.pageChange) return;
  const focus = focusPlace(document.activeElement);
  renderTop();
  renderPage();
  restoreFocus(focus);
  queueFit();
}

/// What Tab moves through in the panel.
const FOCUSABLE = "button, select, input, textarea, summary, a[href], [tabindex]";
/// Attributes that change with what a control shows rather than which one it
/// is, as a toggle's state or a tooltip's text.
const FOCUS_VOLATILE = new Set(["data-tip", "data-text", "data-enabled", "data-options", "data-menu", "data-seq", "data-morph"]);

/// Names a control by what it does rather than by node, so the same one can be
/// found again after a render rebuilds the markup around it.
function focusName(el) {
  const named = [...el.attributes].filter((a) =>
    a.name === "id" || a.name === "name" || a.name === "type" || a.name === "role" || (a.name.startsWith("data-") && !FOCUS_VOLATILE.has(a.name)));
  return [el.tagName, ...named.map((a) => `${a.name}=${a.value}`).sort()].join(" ");
}

/// The controls in `root` Tab stops on, in order: not those inside a hidden
/// element or a closed <details> other than its summary, which the panel
/// does not render.
function tabStops(root) {
  return [...(root?.querySelectorAll(FOCUSABLE) ?? [])].filter((c) => !c.disabled && c.getAttribute("tabindex") !== "-1" && rendered(c));
}

function rendered(c) {
  if (c.closest("[hidden]")) return false;
  for (let details = c.closest("details"); details; details = details.parentElement?.closest("details")) {
    if (!details.open && !(c.tagName === "SUMMARY" && c.parentElement === details)) return false;
  }
  return true;
}

/// Where focus is among the panel's rebuilt parts: the page, the container,
/// the control's name and which of the controls so named it is, and its
/// position among all of them. Null outside them.
function focusPlace(el) {
  const root = el?.closest?.("#top, #content");
  if (!root || el === root) return null;
  const all = tabStops(root);
  const name = focusName(el);
  return { page: ui.builtPage, root: root.id, name, nth: all.filter((c) => focusName(c) === name).indexOf(el), index: all.indexOf(el) };
}

/// Puts focus back on the control it was on before a render of the same page
/// replaced it, so Tab goes on from there; else on the control now in its
/// place. Focus kept by the render itself (a field being typed in) stays, and
/// another page starts afresh.
function restoreFocus(place) {
  const active = document.activeElement;
  if (!place || place.page !== ui.page || (active && active !== document.body && active.isConnected)) return;
  const all = tabStops($(place.root));
  if (!all.length) return;
  const same = all.filter((c) => focusName(c) === place.name);
  const target = same[Math.max(0, place.nth)] ?? same[0] ?? all[Math.min(Math.max(0, place.index), all.length - 1)];
  target.focus({ preventScroll: true });
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
  if (ui.builtPage !== ui.page) hideRouteTooltip();
  const content = $("content");
  for (const details of content.querySelectorAll("details[data-device-traffic]")) (ui.deviceTrafficOpen ||= Object.create(null))[details.dataset.deviceTraffic] = details.open;
  if (ui.page === "activity") {
    if (ui.builtPage !== "activity") {
      closeSelect();
      content.innerHTML = activityShell();
      ui.builtPage = "activity";
      renderActivityTraffic();
    }
    renderActivityForwarding();
    renderActivityList();
    return;
  }
  closeSelect();
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

// One renderer for data-tip, HTML titles and SVG titles on every platform.
// Native titles are held while open to prevent a second desktop tooltip.
// Keyboard focus shows one as well; a click does not.
let routeTooltipTimer;
let routeTooltipOwner;
let routeTooltipMode;
let routeTooltipPointer;
let routeTooltipIdentity;
let routeTooltipAnchor;
const heldTooltipTitles = new Map();

function tooltipText(owner) {
  return owner.dataset?.tip ?? owner.getAttribute?.("title") ?? heldTooltipTitles.get(owner)?.text
    ?? owner.querySelector?.(":scope > title")?.textContent;
}

function tooltipOwner(node) {
  if (node?.nodeType === 3) node = node.parentElement;
  for (; node; node = node.parentElement) {
    if (tooltipText(node) != null) return tooltipText(node) ? node : null;
  }
  return null;
}

// Use stable controls/scopes where present, then a structural path within
// that scope. Tooltip text and status classes may change on every refresh.
function tooltipIdentity(owner) {
  if (owner.dataset.tipKey) return JSON.stringify(["key", owner.dataset.tipKey]);
  const parts = [];
  for (let node = owner; node; node = node.parentElement) {
    const anchor = node.id ? ["id", node.id]
      : node.dataset.tipScope ? ["scope", node.dataset.tipScope]
      : node.dataset.morph ? ["morph", node.dataset.morph] : null;
    if (anchor) { parts.push(anchor); break; }
    if (node.dataset.tipSlot || node.dataset.slice) {
      parts.push([node.tagName, node.dataset.tipSlot ?? node.dataset.slice]);
    } else if (node.dataset.action) {
      parts.push([node.tagName, ...["action", "name", "target", "id", "key", "service"].map(key => node.dataset[key] ?? "")]);
    } else if (node.matches?.(".stat")) {
      parts.push(["stat", node.querySelector(".stat-label")?.textContent]);
    } else {
      parts.push([node.tagName, node.parentElement ? [...node.parentElement.children].indexOf(node) : 0]);
    }
  }
  return JSON.stringify(parts);
}

function holdTooltipTitles(owner) {
  for (let node = owner; node; node = node.parentElement) {
    const title = node.getAttribute?.("title");
    if (title != null) {
      heldTooltipTitles.set(node, { ...heldTooltipTitles.get(node), text: title, title });
      node.removeAttribute("title");
    }
    const shape = node.querySelector?.(":scope > title");
    if (shape) {
      const held = heldTooltipTitles.get(node);
      heldTooltipTitles.set(node, { ...held, text: held?.title ?? shape.textContent, shape });
      shape.remove();
    }
  }
}

function releaseTooltipTitles() {
  for (const [node, { title, shape }] of heldTooltipTitles) {
    if (shape) {
      if (!node.querySelector(":scope > title")) node.prepend(shape);
    }
    if (title != null && !node.hasAttribute("title")) node.setAttribute("title", title);
  }
  heldTooltipTitles.clear();
}

function hideRouteTooltip() {
  clearTimeout(routeTooltipTimer);
  routeTooltipOwner?.removeAttribute("aria-describedby");
  routeTooltipOwner = null;
  $("route-tooltip").hidden = true;
  routeTooltipIdentity = null;
  routeTooltipAnchor = null;
  releaseTooltipTitles();
}

function routeTooltipPosition(anchor, size, width, height) {
  const margin = 12;
  const left = Math.max(margin, Math.min(anchor.left, width - size.width - margin));
  const above = anchor.top - size.height - 6;
  const top = Math.max(margin, Math.min(above >= margin ? above : anchor.bottom + 6, height - size.height - margin));
  return { left, top };
}

function paintRouteTooltip() {
  const owner = routeTooltipOwner;
  if (!owner?.isConnected) return hideRouteTooltip();
  holdTooltipTitles(owner);
  const tip = $("route-tooltip");
  const text = tooltipText(owner);
  if (!text) return hideRouteTooltip();
  if (tip.textContent !== text) tip.textContent = text;
  tip.hidden = false;
  tip.style.visibility = "hidden";
  const anchor = owner.getBoundingClientRect();
  routeTooltipAnchor = anchor;
  const height = document.documentElement.clientHeight;
  // Long notes scroll on the roomier side instead of covering their trigger
  // and causing a pointerout/show loop.
  tip.style.maxHeight = `${Math.max(0, anchor.top - 18, height - anchor.bottom - 18)}px`;
  const { left, top } = routeTooltipPosition(anchor, tip.getBoundingClientRect(), document.documentElement.clientWidth, height);
  tip.style.left = `${left}px`;
  tip.style.top = `${top}px`;
  tip.style.visibility = "visible";
  owner.setAttribute("aria-describedby", "route-tooltip");
}

function transferRouteTooltip(owner) {
  routeTooltipOwner?.removeAttribute("aria-describedby");
  releaseTooltipTitles();
  routeTooltipOwner = owner;
  holdTooltipTitles(owner);
  if (!$("route-tooltip").hidden) paintRouteTooltip();
}

// Let the pointer cross the six-pixel gap to read or scroll a long note.
function tooltipBridge(point) {
  const tip = $("route-tooltip");
  if (!routeTooltipOwner || tip.hidden || !point) return false;
  const anchor = routeTooltipOwner.isConnected ? routeTooltipOwner.getBoundingClientRect() : routeTooltipAnchor;
  if (!anchor) return false;
  const box = tip.getBoundingClientRect();
  return point.x >= Math.min(anchor.left, box.left) && point.x <= Math.max(anchor.right, box.right)
    && ((box.bottom <= anchor.top && point.y >= box.bottom && point.y <= anchor.top)
      || (box.top >= anchor.bottom && point.y >= anchor.bottom && point.y <= box.top));
}

// Keep both visible and pending tooltips across replacement of any panel
// region, including the top bar, native titles and SVG chart titles.
function refreshRouteTooltip() {
  const owner = routeTooltipOwner;
  if (!owner) return;
  if (owner.isConnected) {
    holdTooltipTitles(owner);
    if (!$("route-tooltip").hidden) paintRouteTooltip();
    return;
  }
  const hit = routeTooltipPointer && document.elementFromPoint(routeTooltipPointer.x, routeTooltipPointer.y);
  const overTip = (hit && $("route-tooltip").contains(hit)) || tooltipBridge(routeTooltipPointer);
  const next = routeTooltipMode === "focus" || overTip
    ? [$("top"), $("content")].filter(Boolean).flatMap(root => [...root.querySelectorAll("[data-tip], [title], svg title")])
        .map(el => el.tagName.toLowerCase() === "title" ? el.parentElement : el)
        .find(el => tooltipIdentity(el) === routeTooltipIdentity)
    : tooltipOwner(hit);
  if (!next || tooltipIdentity(next) !== routeTooltipIdentity
      || next.getBoundingClientRect().width === 0) return hideRouteTooltip();
  transferRouteTooltip(next);
  if (routeTooltipMode === "focus") next.focus({ preventScroll: true });
}

function showRouteTooltip(owner, mode = "pointer") {
  routeTooltipMode = mode;
  if (routeTooltipOwner === owner) return;
  if (routeTooltipOwner && !routeTooltipOwner.isConnected
      && tooltipIdentity(owner) === routeTooltipIdentity) {
    transferRouteTooltip(owner);
    return;
  }
  hideRouteTooltip();
  routeTooltipOwner = owner;
  routeTooltipIdentity = tooltipIdentity(owner);
  holdTooltipTitles(owner);
  routeTooltipTimer = setTimeout(paintRouteTooltip, owner.matches(".chart-bar, [data-slice]") ? 100 : 500);
}

document.addEventListener("pointermove", (event) => {
  routeTooltipPointer = { x: event.clientX, y: event.clientY };
  // WebKit may not send pointerout for a newly replaced hovered node.
  if (routeTooltipOwner?.isConnected && routeTooltipMode === "pointer"
      && !routeTooltipOwner.contains(event.target) && !$("route-tooltip").contains(event.target)
      && !tooltipBridge(routeTooltipPointer)) {
    if (routeTooltipOwner.matches(":focus-visible")) routeTooltipMode = "focus";
    else hideRouteTooltip();
  }
});
for (const type of ["pointerover", "focusin"]) {
  document.addEventListener(type, (event) => {
    const owner = tooltipOwner(event.target);
    if (type === "focusin" && !owner?.matches(":focus-visible")) return;
    if (type === "pointerover") routeTooltipPointer = { x: event.clientX, y: event.clientY };
    if (owner) showRouteTooltip(owner, type === "focusin" ? "focus" : "pointer");
  });
}
for (const type of ["pointerout", "focusout"]) {
  document.addEventListener(type, (event) => {
    if ($("route-tooltip").contains(event.target)) {
      if (!$("route-tooltip").contains(event.relatedTarget) && !routeTooltipOwner?.contains(event.relatedTarget)) hideRouteTooltip();
      return;
    }
    const owner = tooltipOwner(event.target);
    if (!owner || owner !== routeTooltipOwner || owner.contains(event.relatedTarget)) return;
    if (!owner.isConnected) return;
    if (type === "pointerout" && $("route-tooltip").contains(event.relatedTarget)) return;
    if (type === "pointerout" && tooltipBridge({ x: event.clientX, y: event.clientY })) return;
    // Some engines dispatch focusout just before innerHTML detaches the
    // focused node. Let that replacement finish before deciding to close.
    if (type === "focusout") {
      queueMicrotask(() => {
        if (owner !== routeTooltipOwner) return;
        if (!owner.isConnected) return refreshRouteTooltip();
        if (owner.matches(":hover")) routeTooltipMode = "pointer";
        else hideRouteTooltip();
      });
      return;
    }
    if (type === "pointerout" && owner.matches(":focus-visible")) { routeTooltipMode = "focus"; return; }
    if (type === "focusout" && owner.matches(":hover")) { routeTooltipMode = "pointer"; return; }
    hideRouteTooltip();
  });
}
document.addEventListener("scroll", (event) => {
  if (event.target !== $("route-tooltip")) hideRouteTooltip();
}, true);
window.addEventListener("resize", hideRouteTooltip);
window.addEventListener("pointerdown", event => {
  if (!$("route-tooltip").contains(event.target)) hideRouteTooltip();
}, true);
window.addEventListener("blur", hideRouteTooltip);
window.addEventListener("keydown", event => {
  if (event.key === "Escape") hideRouteTooltip();
});

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

const TRAFFIC_RANGES = [[30, "30 minutes"], [360, "6 hours"], [720, "12 hours"], [1440, "1 day"], [10080, "7 days"], [43200, "30 days"]];

// Native select popups can take OS focus away from the tray window, which
// dismisses the panel. Keep every selector's options inside the WebView.
function panelSelect(id, options, selected, label, cls = "", attrs = "") {
  const value = String(selected);
  const text = options.find(([v]) => String(v) === value)?.[1] ?? "";
  return `<button type="button" id="${id}" class="select ${cls}" value="${esc(value)}"
    data-options="${esc(JSON.stringify(options))}" aria-label="${esc(label)}"
    aria-haspopup="listbox" aria-expanded="false" ${attrs}>${esc(text)}</button>`;
}

let openSelect = null;
let selectRenderPending = false;

function closeSelect(restoreFocus = false) {
  if (!openSelect) return;
  const { trigger, menu } = openSelect;
  openSelect = null;
  trigger.setAttribute("aria-expanded", "false");
  trigger.removeAttribute("aria-controls");
  menu.remove();
  if (restoreFocus && trigger.isConnected) trigger.focus();
  // Live traffic can refresh while a choice is being made. Replace controls
  // only after the menu closes, so its trigger and options stay mounted.
  if (selectRenderPending) {
    selectRenderPending = false;
    queueMicrotask(() => {
      render();
      if (ui.page === "activity") renderActivityTraffic();
    });
  }
}

function showSelect(trigger, last = false) {
  const wasOpen = openSelect?.trigger === trigger;
  closeSelect();
  if (wasOpen) return;
  const options = JSON.parse(trigger.dataset.options);
  const menu = document.createElement("div");
  menu.id = "panel-select-options";
  menu.className = "select-menu";
  menu.setAttribute("role", "listbox");
  menu.setAttribute("aria-label", trigger.getAttribute("aria-label"));
  menu.innerHTML = options.map(([value, label]) => `<button type="button" role="option"
    tabindex="-1" value="${esc(value)}" aria-selected="${String(value) === trigger.value}">${esc(label)}</button>`).join("");
  placeMenu(trigger, menu);
  menu.style.minWidth = `${trigger.getBoundingClientRect().width}px`;
  const items = [...menu.children];
  (last ? items.at(-1) : items.find((el) => el.value === trigger.value) ?? items[0]).focus();
  menu.addEventListener("click", (event) => {
    const option = event.target.closest('[role="option"]');
    if (!option) return;
    const changed = trigger.value !== option.value;
    trigger.value = option.value;
    trigger.textContent = option.textContent;
    closeSelect(true);
    if (changed) trigger.dispatchEvent(new Event("change", { bubbles: true }));
  });
}

/// Opens `menu` below `trigger`, right-aligned and kept inside the panel.
function placeMenu(trigger, menu) {
  openSelect = { trigger, menu };
  trigger.setAttribute("aria-expanded", "true");
  trigger.setAttribute("aria-controls", menu.id);
  document.body.append(menu);
  // One highlight, as in a native menu: the item under the pointer takes
  // focus, so the one focused on opening or by keys does not stay lit.
  menu.addEventListener("pointermove", (event) => {
    const item = event.target.closest?.("button");
    if (item && item !== document.activeElement) item.focus({ preventScroll: true });
  });
  const rect = trigger.getBoundingClientRect();
  menu.style.maxHeight = `${Math.max(0, window.innerHeight - 8)}px`;
  const bounds = menu.getBoundingClientRect();
  menu.style.left = `${Math.max(4, Math.min(rect.right - bounds.width, window.innerWidth - bounds.width - 4))}px`;
  menu.style.top = `${Math.max(4, Math.min(rect.bottom + 3, window.innerHeight - bounds.height - 4))}px`;
}

/// An action menu: placed and dismissed like a select, but each item runs its
/// own `data-action`. `null` entries become separators.
function showMenu(trigger, last = false) {
  const wasOpen = openSelect?.trigger === trigger;
  closeSelect();
  if (wasOpen) return;
  const menu = document.createElement("div");
  menu.id = "panel-select-options";
  menu.className = "select-menu action-menu";
  menu.setAttribute("role", "menu");
  menu.setAttribute("aria-label", trigger.getAttribute("aria-label"));
  menu.innerHTML = JSON.parse(trigger.dataset.menu).map((item) => item?.heading
    ? `<div class="menu-heading" role="presentation"><strong>${esc(item.heading)}</strong>${item.detail ? `<span>${esc(item.detail)}</span>` : ""}</div>`
    : item
    ? `<button type="button" role="${item.checked == null ? "menuitem" : "menuitemradio"}" tabindex="-1" data-action="${esc(item.action)}"${
      item.target ? ` data-target="${esc(item.target)}"` : ""}${item.checked == null ? "" : ` aria-checked="${item.checked}"`}>${esc(item.label)}</button>`
    : '<div class="menu-separator" role="separator"></div>').join("");
  placeMenu(trigger, menu);
  const items = menuItems(menu);
  (last ? items.at(-1) : items.find((el) => el.getAttribute("aria-checked") === "true") ?? items[0])?.focus();
  // Close before the document's click handler runs the item's action.
  menu.addEventListener("click", (event) => {
    if (event.target.closest("[data-action]")) closeSelect(true);
  });
}

function menuItems(menu) {
  return [...menu.querySelectorAll("button")];
}

document.addEventListener("pointerdown", (event) => {
  if (openSelect && !openSelect.menu.contains(event.target) && !openSelect.trigger.contains(event.target)) closeSelect();
  const editor = $("range-editor");
  if (editor && !editor.hidden && !editor.contains(event.target) && !event.target.closest?.(".select-menu")) showRangeEditor(false);
});
document.addEventListener("scroll", (event) => {
  if (openSelect && !openSelect.menu.contains(event.target)) closeSelect();
}, true);
window.addEventListener("resize", () => closeSelect());

function trafficRangeSelect(id, selected, label) {
  return panelSelect(id, TRAFFIC_RANGES.map(([minutes, text]) => [minutes, `Last ${text}`]), selected, label, "traffic-range");
}

function trafficControls(prefix, minutes, label) {
  return `<span class="traffic-controls" data-morph="traffic-controls">${panelSelect(`${prefix}-scope`, [["all", "All"], ["model", "Models"]], trafficViews.scope,
    "Traffic request category", "traffic-scope", 'title="Model calls: each HTTP generation/compaction request and each generation within a WebSocket counts once, including active, failed and cancelled calls."')}${trafficRangeSelect(`${prefix}-range`, minutes, label)}</span>`;
}

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

const MATCH_REASONS = {
  renamed: "Same base URL, different name",
  baseChanged: "Same name, different base URL",
  legacy: "Logged before base URLs were recorded",
  ambiguous: "Several configurations use this base URL or name",
  unmatched: "No configuration has this name or base URL",
};

/// Logged configurations counted here without an exact match. Each can be
/// assigned to a current configuration or to Unidentified; the logs stay as they are.
function trafficReview(c, targets = [], scope = trafficViews.activity?.scope) {
  if (!c.sources?.length) return "";
  const review = c.sources.some((s) => s.reason);
  const unit = scope === "model" ? "call" : "request";
  const items = c.sources.flatMap((s, i) => {
    const choose = (target, automatic = false) => JSON.stringify({ service: c.service, name: s.name, base: s.base ?? null, target, automatic });
    const where = [s.name || "No name", s.base?.replace(/^https?:\/\//, "") ?? "No base URL"].join(" · ");
    return [
      i ? null : undefined,
      { heading: where, detail: `${s.reason ? MATCH_REASONS[s.reason] : "Assigned by you"} · ${s.requests} ${unit}${s.requests === 1 ? "" : "s"}` },
      ...targets.filter((t) => t.service === c.service).map((t) => ({
        label: t.label, action: "traffic-assign", target: choose({ name: t.name, base: t.base }), checked: c.credential === t.label,
      })),
      { label: "Unidentified", action: "traffic-assign", target: choose(null), checked: c.credential === "Unidentified" },
      !s.reason && { label: "Match Automatically", action: "traffic-assign", target: choose(null, true) },
    ];
  }).filter((item) => item !== undefined && item !== false);
  const label = review
    ? "Some requests here were matched to a renamed, changed or unknown configuration. Choose where they belong."
    : "Requests here were assigned by you.";
  return `<button type="button" class="icon-btn more traffic-review${review ? " review" : ""}" data-menu="${esc(JSON.stringify(items))}"
    aria-label="${esc(label)}" title="${esc(label)}" aria-haspopup="menu" aria-expanded="false">${review ? ICON.warning : ICON.more}</button>`;
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
  if (openSelect) { selectRenderPending = true; return; }
  closeSelect();
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
  queueFit();
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
  queueFit();
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

function deviceTrafficContent(traffic, scope, detailsKey = "merged", window = traffic) {
  let body = `<div class="traffic-content" aria-busy="${deviceList.loading}">${trafficSummary(traffic, scope, window, detailsKey)}</div>`;
  const pendingReviews = traffic.credentials.reduce((n, c) => n + (c.sources ?? []).filter(s => s.reason).length, 0);
  if (pendingReviews) body += `<div class="placeholder">${ICON.warning} Renamed or changed local configurations need review. Open Upstream accounts and choose where their traffic belongs.</div>`;
  if (traffic.credentials.length) body += `<details class="disclosure" data-device-traffic="${esc(detailsKey)}" ${ui.deviceTrafficOpen?.[detailsKey] ? "open" : ""}><summary>${ICON.chevron}Upstream accounts</summary>${traffic.credentials.map(c => accountRow(c, scope, window, trafficReview(c, traffic.targets, scope), detailsKey)).join("")}</details>`;
  return body;
}
document.addEventListener("toggle", event => {
  const details = event.target;
  if (!details.isConnected || !details.matches?.("details[data-device-traffic]")) return;
  (ui.deviceTrafficOpen ||= Object.create(null))[details.dataset.deviceTraffic] = details.open;
}, true);
function mergedDataBlock() {
  const data = deviceList.merged;
  const range = trafficControls("devices-traffic", ui.deviceTrafficMinutes, "Device traffic time range");
  let body = `<div class="devices-traffic-range">${range}</div>`;
  if (deviceList.updating) body += `<div class="placeholder">Updating traffic…</div>`;
  if (deviceList.error) body += message("bad", esc(deviceList.error));
  // The backend adds every device once on the newest one's time axis; peers
  // still loading are added once they arrive, and the sum so far stays visible.
  const { merged, localError, scope } = deviceCardViews();
  if (localError) body += message("warn", `This device: ${esc(localError)}`);
  else if (merged) body += deviceTrafficContent(merged, scope, "merged", merged);
  // A read that failed says so above; it is not shown as loading.
  else if (!deviceList.error) body += `<div class="placeholder">Loading traffic…</div>`;
  if (data) {
    for (const source of data.sources) {
      if (source.error === "TRAFFIC_UPDATING") body += `<div class="placeholder">${esc(source.name)}: Loading traffic…</div>`;
      else if (source.error) body += message("warn", `${esc(source.name)}: ${esc(source.error)}`);
      else if (source.exclusion === "duplicate") body += `<div class="placeholder">${esc(source.name)}: Already counted via another entry.</div>`;
    }
  }
  return block("Merged Traffic", `<button class="text-link" data-action="merged-refresh" ${deviceList.loading && deviceList.remote ? "disabled" : ""}>Refresh</button>`, body, "traffic");
}
function deviceCapabilities(device) {
  if (!device.ssh) return "";
  const entry = deviceList.capability(device);
  const caps = entry?.capabilities;
  const error = entry?.error || deviceList.capabilitiesError;
  return error ? `Version unavailable: ${error}` : entry?.pending ? "Checking…" : caps ? `Coport ${caps.version}${caps.runningVersion && caps.runningVersion !== caps.version ? ` · Running ${caps.runningVersion}` : ""} · Statistics ${caps.statistics ? "✓" : "✕"} · Forwarding ${caps.forwarding ? "✓" : "✕"}${caps.running ? caps.forwardingAvailable ? "" : " · Unavailable" : " · Offline"}` : "Checking…";
}
async function refreshForwardingStatus() {
  try {
    deviceList.setForwarders(await invoke("get_device_forwarders"));
    await deviceList.loadCapabilities();
    if (ui.snap) ui.snap.forwarding = deviceList.forwarders[0] || null;
    if (ui.page === "settings") render();
  } catch (error) { toast(String(error)); }
}
function deviceForwardingSettings(device) {
  if (!device.ssh) return "";
  const status = (deviceList.forwarders || []).find(item => item.deviceId === device.id);
  const entry = deviceList.capability(device);
  const caps = entry?.capabilities;
  const available = !entry?.error && !entry?.pending && caps?.forwarding && caps.forwardingAvailable
    && deviceList.status(device).state !== "failed";
  // Keep an active forwarding switch reachable even after the peer goes
  // offline, so the user can stop it without waiting for another connection.
  if (!status && !available) return "";
  const busy = ui.deviceForwardingBusy === device.id;
  return `<div class="row device-forwarding-row"><span class="row-label">SSH forwarding</span><button class="switch" role="switch" aria-checked="${!!status}" aria-busy="${busy}" aria-label="Forward requests through ${esc(device.name)}" data-action="device-forward" data-id="${esc(device.id)}" data-enabled="${!status}" ${ui.deviceForwardingBusy ? "disabled" : ""}></button></div>` + (status ? forwardingRestoreSettings() : "") + (status?.error ? message("warn", esc(status.error)) : "");
}
function forwardingRestoreSettings() {
  const tip = !ui.snap.forwardingRestoreSupported ? ' data-tip="Update the local proxy to enable startup restore."' : "";
  return `<div class="row device-forwarding-row"><span class="row-label"${tip}>Restore on startup</span><button class="switch" role="switch" aria-checked="${!!ui.snap.forwardingRestore}" aria-label="Restore forwarding on startup"${tip} data-action="forwarding-restore" ${ui.forwardingRestoreBusy || !ui.snap.forwardingRestoreSupported ? "disabled" : ""}></button></div>` + (ui.snap.forwardingRestoreError ? message("warn", esc(ui.snap.forwardingRestoreError)) : "");
}
/// The views the Devices cards show for the selected range and category: this
/// device's own, the forwarded device's live view, other peers' from the last
/// refresh. Until the backend completes the range's frame with this device
/// (ADR-0001), its own and the forwarded device's shared views fill their
/// cards and Merged Traffic waits.
function deviceCardViews() {
  const minutes = ui.deviceTrafficMinutes || 30;
  const scope = trafficViews.scope || "model";
  const data = deviceList.merged?.minutes === minutes && deviceList.merged.scope === scope ? deviceList.merged : null;
  // A frame completed for this range carries this device and the forwarded
  // device's live view beside the peers, and Merged Traffic is their sum.
  // A frame whose read of this device failed is complete too: it says why.
  const complete = !!(data?.local || data?.localError);
  const forwarded = complete ? data.forwarded?.device : ui.snap?.forwarding?.deviceId;
  const cards = new Map();
  for (const device of deviceList.devices) {
    const source = data?.sources.find((source) => source.name === device.name);
    // A device already counted via another entry gets no view, live or not.
    if (source?.exclusion === "duplicate") continue;
    const live = device.id !== forwarded ? null : complete ? data.forwarded.traffic
      : trafficViews.forwardedView(device.id, minutes, scope);
    if (live && (complete || live.device === device.id)) { cards.set(device.id, { view: live, window: live }); continue; }
    if (source?.traffic) cards.set(device.id, { view: source.traffic, window: source.traffic.end != null ? source.traffic : data });
  }
  return {
    local: data?.local || trafficViews.localView(minutes, scope), localError: complete ? data.localError : null,
    cards, merged: complete ? data.traffic : null, scope,
  };
}
function devicesPage() {
  const local = ui.snap.phase;
  const data = deviceList.merged;
  const { local: localTraffic, localError, cards: views, scope } = deviceCardViews();
  let cards = block("This Device", "",
    `<div class="row"><span class="row-label" data-morph="address">127.0.0.1:${local.port}</span><span class="state" data-morph="proxy-state"><span class="dot ${esc(local.state)}"></span>${local.busy ? "Updating…" : { running: "Running", stopped: "Stopped", failed: "Status unavailable" }[local.state] ?? "Unknown"}</span></div>` +
    (local.error ? message("warn", esc(local.error)) : "") +
    (localTraffic ? `<div class="block-head"><span class="block-title">Traffic</span></div>${deviceTrafficContent(localTraffic, scope, "local")}` : localError ? message("warn", esc(localError)) : `<div class="placeholder">Loading traffic…</div>`), "this-device");
  for (const device of deviceList.devices) {
    const source = (data || deviceList.latest)?.sources.find(source => source.name === device.name);
    const card = views.get(device.id);
    const ssh = device.data?.transport === "ssh" || !device.data;
    const protocol = ssh ? "SSH" : device.data.url.startsWith("https:") ? "HTTPS" : "HTTP";
    const status = deviceList.status(device);
    cards += block(esc(device.name), "",
      `<div class="row"><span class="row-label selectable">${esc(device.data?.transport === "ssh" || !device.data ? device.ssh?.host : device.data.url)}</span><span class="state"><span class="dot ${status.state}" role="img" aria-label="${esc(status.label)}" data-tip="${esc(status.label)}"></span>${protocol}</span></div>` +
      (source?.error && source.error !== "TRAFFIC_UPDATING" ? message("warn", esc(source.error)) : "") +
      (card ? `<div class="block-head"><span class="block-title">Traffic</span></div>${deviceTrafficContent(card.view, scope, `device/${device.id}`, card.window)}<div class="placeholder">Data through ${esc(new Date(card.window.end ?? card.window.windowEnd).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" }))}</div>` : `<div class="placeholder">${source?.error && source.error !== "TRAFFIC_UPDATING" ? "Traffic unavailable" : source?.exclusion === "duplicate" ? "Already counted via another entry." : deviceList.loading && (deviceList.remote || !data) ? "Loading traffic…" : "Not refreshed"}</div>`), `device/${device.id}`);
  }
  return mergedDataBlock() + cards;
}
function deviceSettings() {
  const d = deviceForm.draft || { transport: "ssh" };
  const rows = deviceList.devices.map(device => {
    const status = deviceList.status(device);
    const forwarding = deviceForwardingSettings(device);
    const capabilities = deviceCapabilities(device);
    const tip = capabilities ? ` data-tip="${esc(capabilities)}" tabindex="0"` : "";
    return `<div class="device-settings-item" data-tip-scope="device-settings:${esc(device.id)}"><div class="row device-setting-row"><span class="device-setting-name row-label"${tip}><span class="dot ${status.state}" role="img" aria-label="${esc(status.label)}" data-tip="${esc(status.label)}"></span>${esc(device.name)}<span class="device-setting-transport">${device.data?.transport === "ssh" || !device.data ? "SSH" : "HTTP"}</span></span><span class="row-value device-setting-actions"><button class="text-link" data-action="device-edit" data-id="${esc(device.id)}">Edit</button><button class="icon-btn" data-action="device-remove" data-id="${esc(device.id)}" data-tip="Remove device" aria-label="Remove ${esc(device.name)}">${ICON.trash}</button></span></div>${forwarding}</div>`;
  }).join("");
  const form = deviceForm.open ? `<div class="device-form">
    <label>Connection${panelSelect("device-transport", [["ssh", "SSH"], ["http", "HTTP / HTTPS"]], d.transport, "Connection", "field")}</label>
    <label>Name (optional)<input class="field" id="device-name" value="${esc(d.name || "")}" maxlength="128" data-tip="Defaults to the SSH config name or HTTP origin."></label>
    ${d.transport === "ssh" ? `
      <label>SSH config name or host<input class="field" id="device-host" value="${esc(d.host || "")}" maxlength="255" placeholder="mbp16"></label>
      <label>Coport path (optional)<input class="field" id="device-binary" value="${esc(d.binary === "coportd" ? "" : d.binary || "")}" maxlength="4096" data-tip="Path to the Coport executable on the remote device. Leave empty to find it automatically."></label>
    ` : `
      <label>Statistics URL<input class="field" id="device-url" value="${esc(d.url || "")}" placeholder="http://LAN-IP:8788 or https://host:8788"></label>
      <label>Access key file<input class="field" id="device-key-file" value="${esc(d.key || "")}" placeholder="/absolute/path/to/data.key"></label>
      <label>Access key variable<input class="field" id="device-key-env" value="${esc(d.env || "")}" data-tip="Name of the environment variable containing the access key. Use this or an access key file."></label>
      <label>CA certificate (optional)<input class="field" id="device-ca-file" value="${esc(d.ca || "")}" placeholder="/absolute/path/to/ca.pem"></label>
    `}
    <div class="message-actions"><button class="btn primary" data-action="device-save" aria-label="Save device">Save</button><button class="btn" data-action="device-cancel">Cancel</button></div>
  </div>` : "";
  return block("Devices", `<button class="icon-btn" data-action="device-new" data-tip="Add device" aria-label="Add device">${ICON.plus}</button>`, rows + form);
}
function captureDeviceDraft() {
  deviceForm.capture((id) => $(id)?.value);
}
document.addEventListener("input", event => { if (event.target.id?.startsWith("device-")) captureDeviceDraft(); });
document.addEventListener("change", event => {
  if (!event.target.id?.startsWith("device-")) return;
  captureDeviceDraft();
  if (event.target.id === "device-transport") render();
});

// ---------------------------------------------------------------- settings

function settings() {
  const s = ui.snap;
  const set = s.settings;
  return `
    ${block(
      "General",
      "",
      `<div class="row"><span class="row-label">Launch at login</span>
         <button class="switch" role="switch" aria-checked="${set.launchAtLogin !== "off"}" data-action="launch-at-login" aria-label="Launch at login"></button></div>
       ${set.launchAtLogin === "needsApproval" ? message("warn", "macOS blocks Coport from opening at login. Allow it in Login Items.", `<button class="btn" data-action="open-login-items">Open Login Items</button>`) : ""}
       <div class="row"><span class="row-label">Start proxy on startup</span>
         <button class="switch" role="switch" aria-checked="${set.startProxyOnLaunch}" data-action="auto-start" aria-label="Start proxy on startup"></button></div>
       <div class="row"><span class="row-label" data-tip="Reopen the app to manage or stop the background proxy.">Keep proxy running after quit</span>
         <button class="switch" role="switch" aria-checked="${set.keepProxyRunningOnQuit}" data-action="keep-running" aria-label="Keep proxy running after quit" data-tip="Reopen the app to manage or stop the background proxy."></button></div>
       <div class="row"><span class="row-label" data-tip="Applies to traffic and routing views.">Mask account names</span>
         <button class="switch" role="switch" aria-checked="${set.maskAccountNames}" data-action="mask-account-names" aria-label="Mask account names" data-tip="Applies to traffic and routing views."></button></div>
       <div class="row"><span class="row-label">Appearance</span>
         ${panelSelect("appearance", [["System", "System"], ["Light", "Light"], ["Dark", "Dark"]], set.appearance, "Appearance", "", 'data-setting="appearance"')}</div>`
    )}
    ${configSettings()}
    ${filesBlock()}
    ${deviceSettings()}`;
}

/// The fixed configuration and log files: a status per file, actions in a ⋯ menu.
function filesBlock() {
  const { config, settings } = ui.snap;
  const status = (dot, text) => `<span class="file-status">${dot ? `<span class="dot ${dot}"></span>` : ""}${text}</span>`;
  const configStatus = !config.exists ? status("", "Not created")
    : config.error ? status("failed", "Invalid")
    : config.changedSinceStart ? status("warn", "Changed")
    : status("running", "Valid");
  const configMenu = [
    config.exists ? { label: "Edit", action: "open", target: "config" } : { label: "Create from Example", action: "create-config" },
    { label: "Import…", action: "import-config" },
    config.changedSinceStart && { label: "Restart Proxy to Apply", action: "restart" },
    null,
    { label: REVEAL_LABEL, action: "open", target: "config-reveal" },
  ].filter((item) => item !== false);
  const logMenu = [
    { label: "Open", action: "open", target: "log" },
    null,
    { label: REVEAL_LABEL, action: "open", target: "log-reveal" },
  ];
  const compat = settings.trafficCompatibility;
  const compatStatus = compat.error ? status("failed", "Invalid")
    : compat.choices ? status("", `${compat.choices} ${compat.choices === 1 ? "choice" : "choices"}`)
    : status("", "No choices");
  const compatMenu = [
    compat.exists && { label: "Open", action: "open", target: "traffic-compatibility" },
    compat.exists && { label: "Clear Choices…", action: "clear-traffic-compatibility" },
    compat.exists && null,
    { label: REVEAL_LABEL, action: "open", target: "traffic-compatibility-reveal" },
  ].filter((item) => item !== false);
  const row = (name, value, items) => `<div class="row"><span class="row-label row-name">${name}</span>${value}
    <button type="button" class="icon-btn more" data-menu="${esc(JSON.stringify(items))}" aria-label="${name} actions"
      aria-haspopup="menu" aria-expanded="false">${ICON.more}</button></div>`;
  return block(
    "Files",
    "",
    `${config.error ? message("bad", esc(config.error)) : ""}
     ${row("Configuration", configStatus, configMenu)}
     ${row("Request Log", status("", settings.logBytes == null ? "Empty" : fmtBytes(settings.logBytes)), logMenu)}
     ${row("Traffic Compatibility", compatStatus, compatMenu)}`
  );
}

/// Network and Subscription Logins: values written to the YAML configuration, applied on restart.
function configSettings() {
  const v = ui.snap.config.details?.editable;
  if (!v) return "";
  const field = (key, label, unit = "", description = "") => {
    const id = `config-${key.replaceAll(".", "-")}`;
    const tip = description ? ` data-tip="${esc(description)}"` : "";
    return `<div class="row" data-tip-scope="config:${key}"><span class="row-label"${tip}>${label}</span>
      <span class="number-field"><input class="field" id="${id}" data-config="${key}" inputmode="decimal" spellcheck="false"
        value="${esc(String(v[key]))}" aria-label="${label}"${tip} /><span class="unit">${unit}</span></span></div>`;
  };
  // An inverted switch is on while its configuration value is false.
  const flag = (key, label, description, inverted = false) => {
    const tip = ` data-tip="${esc(description)}"`;
    const invert = inverted ? ' data-inverted="true"' : "";
    return `<div class="row" data-tip-scope="config:${key}"><span class="row-label"${tip}>${label}</span>
      <button class="switch" role="switch" aria-checked="${v[key] !== inverted}" data-action="config-flag" data-key="${key}"${invert} aria-label="${label}"${tip}></button></div>`;
  };
  return `${block(
    "Network",
    "",
    `${field("listen_port", "Listen port")}
     ${flag("allow_external_access", "Share statistics", "Requires external_data in the configuration file. Shares read-only statistics. Restart to apply.")}
     ${flag("connect_credential_routing", "Enable HTTPS proxy", "Other apps can use Coport via HTTPS_PROXY. Routes by account or API key. Requires trusting Coport's CA. Restart to apply.")}
     ${v.connect_credential_routing ? flag("connect_passthrough", "Pass through other hosts", "Non-API hosts follow the system proxy settings, unread and unlogged. Restart to apply.") : ""}
     ${field("request_timeout_seconds", "Request timeout", "s")}
     <details class="disclosure" id="websocket-timeouts" ${ui.websocketOpen ? "open" : ""}><summary>${ICON.chevron}WebSocket timeouts</summary>
       ${field("websocket.first_message_seconds", "First message", "s")}
       ${field("websocket.first_output_seconds", "First output", "s")}
       ${field("websocket.read_seconds", "Read", "s")}
       ${field("websocket.write_seconds", "Write", "s")}
       ${field("websocket.inter_turn_idle_seconds", "Between turns", "s", "0 disables this timer.")}
     </details>`
  )}
  ${block(
    "Subscription Logins",
    "",
    `${flag("auto_refresh_access_token", "Auto-refresh logins", "If a Codex, Claude or Antigravity login expires, retries the request once and saves the updated login. Restart the proxy to apply.")}
     ${otherLogins(v, flag)}`
  )}`;
}

/// Providers that can accept logins other than the ones saved in Coport.
const OTHER_LOGIN_PROVIDERS = [
  ["codex", "Codex", "Accepts any ChatGPT login and routes it by the account its token names."],
  ["claude", "Claude", "Accepts any Claude login and looks up its account to route it."],
  ["antigravity", "Antigravity", "Accepts any Google login and looks up its email to route it."],
  ["grok", "Grok", "Accepts any xAI login and routes it by the account its token names."],
];

/// One "Accept other logins" disclosure: its summary names the providers that
/// accept them, its rows switch `account_auth_file_only` off per provider.
function otherLogins(v, flag) {
  const on = OTHER_LOGIN_PROVIDERS.filter(([id]) => !v[`${id}.account_auth_file_only`]);
  const status = on.length === 0 ? "None"
    : on.length === OTHER_LOGIN_PROVIDERS.length ? "All"
    : on.map(([, name]) => name).join(", ");
  return `<details class="disclosure" id="other-logins" ${ui.otherLoginsOpen ? "open" : ""}>
    <summary data-tip="By default, Coport only accepts the logins saved in it and rejects others, such as a login from another device or account.">${ICON.chevron}Accept other logins<span class="disclosure-value">${status}</span></summary>
    ${OTHER_LOGIN_PROVIDERS.map(([id, name, tip]) => flag(`${id}.account_auth_file_only`, name, tip, true)).join("")}
  </details>`;
}

/// Saves a number field when it differs from the configuration. Called on
/// focus loss rather than `change`: a re-render recreates the focused field,
/// which resets the baseline `change` compares against.
async function commitConfigField(input) {
  const key = input.dataset.config;
  const text = input.value.trim();
  const value = Number(text);
  if (text === "" || !Number.isFinite(value)) {
    toast("Enter a number");
    render();
  } else if (value !== ui.snap.config.details?.editable[key]) {
    await setConfigValue(key, value);
  }
}

async function setConfigValue(key, value) {
  try {
    await invoke("set_config_value", { key, value });
  } catch (e) {
    toast(String(e));
  }
  await refresh();
}

// ---------------------------------------------------------------- sizing

let fitQueued = false;
let lastHeight = 0;
/// The page change in motion (changePage), if any.
let pageMotion = null;
function panelHeight(page) {
  const style = getComputedStyle($("content"));
  const padding = parseFloat(style.paddingTop) + parseFloat(style.paddingBottom);
  return Math.ceil($("top").offsetHeight + page.offsetHeight + padding);
}
function fitPanel(height) {
  if (Math.abs(height - lastHeight) <= 1) return;
  lastHeight = height;
  invoke("fit_panel", { height });
}
function queueFit() {
  // A page change resizes the window only before or after its motion.
  if (pageMotion) { pageMotion.fit = true; return; }
  if (fitQueued) return;
  fitQueued = true;
  requestAnimationFrame(() => {
    fitQueued = false;
    const page = $("content").firstElementChild;
    if (!page) return;
    if (pageMotion) pageMotion.fit = true;
    else fitPanel(panelHeight(page));
  });
}

// #content fills the window, so in-page growth (e.g. an opened disclosure)
// only shows up on the page element, which each render replaces.
const fitObserver = new ResizeObserver(queueFit);
fitObserver.observe($("content"));
let observedPage = null;
new MutationObserver(() => {
  refreshRouteTooltip();
  const page = $("content").firstElementChild;
  if (page === observedPage) return;
  if (observedPage) fitObserver.unobserve(observedPage);
  if (page) fitObserver.observe(page);
  observedPage = page;
}).observe($("content"), { childList: true, subtree: true, characterData: true, attributes: true, attributeFilter: ["title", "data-tip"] });
new MutationObserver(refreshRouteTooltip).observe($("top"), {
  childList: true, subtree: true, characterData: true, attributes: true, attributeFilter: ["title", "data-tip"],
});

// ---------------------------------------------------------------- share rings

// Every render rebuilds the rings. One whose numbers changed (a refresh, or
// All and Models) moves its slices from what the ring of the same source last
// showed: each grows or shrinks to its new part, new ones grow in and gone ones
// shrink away. A render in the middle carries the motion on in the new ring.
const ringShown = new Map();
const RING_SVG = "http://www.w3.org/2000/svg";
/// The `d` each slice ends with, as trafficShare drew it; also marks the rings
/// already taken up.
const ringFinal = new WeakMap();
const ringEase = cubicBezier(0.32, 0.72, 0, 1);

function cubicBezier(x1, y1, x2, y2) {
  const at = (a, b, t) => 3 * a * t * (1 - t) ** 2 + 3 * b * t * t * (1 - t) + t ** 3;
  return (x) => {
    let lo = 0;
    let hi = 1;
    for (let i = 0; i < 24; i++) {
      const mid = (lo + hi) / 2;
      if (at(x1, x2, mid) < x) lo = mid;
      else hi = mid;
    }
    return at(y1, y2, (lo + hi) / 2);
  };
}

function ringLayout(ring) {
  return [...ring.querySelectorAll("path[data-slice]:not([data-ghost])")].map((p) => ({
    key: p.dataset.slice, slice: p.dataset.slice, width: Number(p.dataset.width), color: p.style.getPropertyValue("--slice").trim(),
  }));
}

function sameRing(a, b) {
  return a.length === b.length && a.every((s, i) => s.key === b[i].key && s.color === b[i].color && Math.abs(s.width - b[i].width) < 1e-6);
}

function ringProgress(motion, now) {
  return Math.min(1, Math.max(0, (now - motion.start) / MORPH_MS));
}

/// The layout a ring shows at `now`, by slice: mid-motion, each slice where it
/// is drawn, in the color it is heading to.
function ringAt(shown, now) {
  if (!shown.motion) return shown.layout;
  const progress = ringProgress(shown.motion, now);
  if (progress >= 1) return shown.layout;
  const parts = blendRing(shown.motion.from, shown.motion.to, ringEase(progress));
  const staying = new Set(parts.filter((s) => !s.gone).map((s) => s.slice));
  return parts.filter((s) => s.to - s.from > 1e-6).map((s) => {
    const key = s.gone && staying.has(s.slice) ? `${s.slice}|gone` : s.slice;
    return { key, slice: key, width: s.to - s.from, color: s.toColor ?? s.fromColor };
  });
}

/// Where each source's ring sits below the top of its row, and its move there:
/// a longer or shorter legend recenters the ring, which then slides from where
/// it was drawn rather than jumping.
const ringPlace = new Map();
const RING_EASING = "cubic-bezier(0.32, 0.72, 0, 1)";

/// How far below its row's top a ring of `place` is drawn at `now`.
function ringOffset(place, now) {
  const progress = Math.min(1, Math.max(0, (now - place.start) / MORPH_MS));
  return place.top + (place.from - place.top) * (1 - ringEase(progress));
}

function placeRing(ring, source, now, still) {
  const row = ring.closest(".traffic-share");
  const box = ring.getBoundingClientRect();
  if (!row || !box.height) return;
  const top = box.top - row.getBoundingClientRect().top;
  const place = ringPlace.get(source);
  const at = place ? ringOffset(place, now) : top;
  // A ring redrawn mid-move heading the same place carries on with it.
  const going = !still && place && place.from !== place.top && Math.abs(place.top - top) < 0.5 && now - place.start < MORPH_MS;
  if (!going && (still || Math.abs(at - top) < 0.5)) {
    ringPlace.set(source, { top, from: top, start: now });
    return;
  }
  const next = going ? place : { top, from: at, start: now };
  ringPlace.set(source, next);
  ring.animate([{ transform: `translateY(${next.from - top}px)` }, { transform: "none" }],
    { duration: MORPH_MS, delay: next.start - now, easing: RING_EASING });
}

function showRing(ring, now) {
  const source = ring.dataset.ring;
  if (ringFinal.has(ring)) return;
  ringFinal.set(ring, true);
  placeRing(ring, source, now, pageMotion || REDUCED_MOTION.matches || document.hidden);
  const layout = ringLayout(ring);
  for (const path of ring.querySelectorAll("path[data-slice]")) ringFinal.set(path, path.getAttribute("d"));
  const shown = ringShown.get(source);
  if (shown?.motion && ringProgress(shown.motion, now) < 1 && sameRing(shown.layout, layout)) {
    shown.motion.rings.push(ring);
    drawRing(shown.motion, ring, now);
    runRing(source, shown.motion);
    return;
  }
  const from = shown && ringAt(shown, now);
  // The first ring of a source, a page change (its copies show the result)
  // and reduced motion show the numbers at once.
  if (!from || sameRing(from, layout) || pageMotion || REDUCED_MOTION.matches || document.hidden) {
    if (shown?.motion) cancelAnimationFrame(shown.motion.frame);
    ringShown.set(source, { layout, motion: null });
    return;
  }
  if (shown.motion) cancelAnimationFrame(shown.motion.frame);
  const [start, end] = matchRing(from, layout);
  const motion = { from: start, to: end, start: now, rings: [ring], frame: 0 };
  ringShown.set(source, { layout, motion });
  drawRing(motion, ring, now);
  runRing(source, motion);
}

/// Draws `motion`'s rings each frame until it ends or none is shown.
function runRing(source, motion) {
  if (motion.frame) return;
  const step = (time) => {
    motion.frame = 0;
    motion.rings = motion.rings.filter((r) => r.isConnected);
    for (const r of motion.rings) drawRing(motion, r, time);
    if (ringProgress(motion, time) >= 1) {
      if (ringShown.get(source)?.motion === motion) ringShown.get(source).motion = null;
    } else if (motion.rings.length) motion.frame = requestAnimationFrame(step);
  };
  motion.frame = requestAnimationFrame(step);
}

/// Draws `ring` as `motion` shows it at `now`; at the end, as it was rendered.
function drawRing(motion, ring, now) {
  const svg = ring.querySelector("svg");
  if (!svg) return;
  const progress = ringProgress(motion, now);
  const all = [...svg.querySelectorAll("path[data-slice]")];
  if (progress >= 1) {
    for (const path of all) {
      if (path.hasAttribute("data-ghost")) path.remove();
      else {
        delete path.dataset.blend;
        path.style.removeProperty("fill");
        if (ringFinal.has(path)) path.setAttribute("d", ringFinal.get(path));
      }
    }
    return;
  }
  // Paths already moving go by their part of the motion, the rest by slice.
  const moving = new Map(all.filter((p) => p.dataset.blend).map((p) => [p.dataset.blend, p]));
  const drawn = new Map(all.filter((p) => !p.dataset.blend && !p.hasAttribute("data-ghost")).map((p) => [p.dataset.slice, p]));
  const eased = ringEase(progress);
  const outer = SHARE_RING_SIZE / 2;
  for (const s of blendRing(motion.from, motion.to, eased)) {
    let path = moving.get(s.key) ?? (s.gone ? null : drawn.get(s.slice));
    if (!path) {
      // A slice the numbers no longer have.
      path = document.createElementNS(RING_SVG, "path");
      path.dataset.slice = s.slice;
      path.setAttribute("data-ghost", "");
    }
    path.dataset.blend = s.key;
    svg.append(path);
    path.setAttribute("d", ringSector(s.from, s.to, outer, outer * SHARE_RING_INNER));
    const fill = !s.fromColor || !s.toColor || s.fromColor === s.toColor
      ? s.toColor ?? s.fromColor
      : `color-mix(in srgb, ${s.toColor} ${(eased * 100).toFixed(1)}%, ${s.fromColor})`;
    path.style.setProperty("fill", fill || "var(--share-unidentified)");
  }
}

new MutationObserver((records) => {
  const now = performance.now();
  for (const record of records) {
    for (const node of record.addedNodes) {
      if (node.nodeType !== Node.ELEMENT_NODE) continue;
      const rings = node.matches(".share-ring[data-ring]") ? [node] : node.querySelectorAll(".share-ring[data-ring]");
      for (const ring of rings) if (ring.isConnected) showRing(ring, now);
    }
  }
}).observe($("content"), { childList: true, subtree: true });

// ---------------------------------------------------------------- page changes

// Settings turns in like the next page and turns back out. Between the other
// pages, what both show keeps its identity: an element with the same
// `data-morph` name on both moves to its new place (a block also reshapes),
// and the rest crossfades. A layer over the panel draws both pages meanwhile;
// the live page underneath is already the new one, hidden until the end.
const REDUCED_MOTION = matchMedia("(prefers-reduced-motion: reduce)");
// Keep the Linux panel responsive to direct clicks; its full-width turns and
// shared-element morphs use a shorter cadence than the macOS presentation.
const TURN_MS = navigator.userAgent.includes("Linux") ? 170 : 340;
const MORPH_MS = navigator.userAgent.includes("Linux") ? 210 : 420;
const MOTION_EASE = "cubic-bezier(0.32, 0.72, 0, 1)";
// Inherited text styles a moving element loses outside its page.
const MORPH_TEXT = ["color", "font-family", "font-size", "font-weight", "line-height", "letter-spacing", "white-space", "font-variant-numeric"];

function endPageMotion() {
  const motion = pageMotion;
  if (!motion) return;
  pageMotion = null;
  motion.layer.remove();
  $("top").style.visibility = $("content").style.visibility = "";
  if (motion.fit) queueFit();
}

/// The named elements in view or beside it, with their place on screen.
function morphTargets(region, view) {
  const found = new Map();
  for (const el of region.querySelectorAll("[data-morph]")) {
    const rect = el.getBoundingClientRect();
    if (!el.dataset.morph || !rect.width || !rect.height) continue;
    // A closed disclosure still lays out its content in WebKit, unseen.
    if (el.closest("details:not([open])") && !el.closest("summary")) continue;
    const near = rect.bottom > view.top - view.height && rect.top < view.bottom + view.height;
    if (!near) continue;
    const block = el.classList.contains("block");
    let text = null;
    let size = 1;
    if (!block) {
      const style = getComputedStyle(el);
      text = MORPH_TEXT.map((name) => [name, style.getPropertyValue(name)]);
      size = parseFloat(style.fontSize) || 1;
    }
    found.set(el.dataset.morph, { el, rect, block, text, size });
  }
  return found;
}

function motionLayer(className, rect, origin) {
  const el = document.createElement("div");
  el.className = className;
  if (rect) place(el, rect, origin);
  return el;
}

function place(el, rect, origin, height = false) {
  Object.assign(el.style, {
    position: "absolute", margin: "0",
    left: `${rect.left - origin.left}px`, top: `${rect.top - origin.top}px`, width: `${rect.width}px`,
    ...(height ? { height: `${rect.height}px` } : {}),
  });
}

// Snapshot row geometry while the original page is laid out.
function motionRows(region, view) {
  // The native window may grow during the transition. Keep enough rows for a
  // full monitor, even when the old page used a short viewport.
  const extent = Math.max(view.height, globalThis.screen?.height || 0);
  return [...region.querySelectorAll(".req")].map(el => {
    const rect = el.getBoundingClientRect();
    return { height: rect.height, keep: rect.bottom > view.top - extent && rect.top < view.top + extent * 2 };
  });
}

// Activity can contain 500 log rows. The animation copies need their geometry,
// but not the thousands of text nodes outside the moving viewport. Only trim
// copies (or the detached old page); the live page keeps every row and control.
function trimMotionRows(copy, rows) {
  let spacer = null;
  let height = 0;
  [...copy.querySelectorAll(".req")].forEach((el, i) => {
    const row = rows[i];
    if (!row || row.keep || !row.height || el.parentElement.id !== "activity-list") {
      spacer = null;
      return;
    }
    // The log list is normal block flow without row margins. Adjacent omitted
    // rows can share one exact-height spacer; expanded details break the run.
    if (spacer && spacer.nextSibling === el) {
      height += row.height;
      spacer.style.height = `${height}px`;
      el.remove();
    } else {
      spacer = document.createElement("div");
      height = row.height;
      spacer.style.height = `${height}px`;
      el.replaceWith(spacer);
    }
  });
}

/// A copy of a named element to move, without its name; named elements inside
/// it move on their own.
function morphCopy(target) {
  // Shared elements can travel vertically across the viewport; keep their
  // complete contents. Only the fixed-height page copies are trimmed below.
  const copy = target.el.cloneNode(true);
  copy.removeAttribute("data-morph");
  copy.removeAttribute("data-morph-paired");
  return copy;
}

/// Shows `update`'s page change in motion. `update` renders the next page
/// and scrolls it; what it shows afterwards is where everything lands.
function changePage(update) {
  endPageMotion();
  const top = $("top");
  const content = $("content");
  const from = ui.builtPage;
  const oldPage = content.firstElementChild;
  if (!oldPage || REDUCED_MOTION.matches || document.hidden) return update();
  const topRect = top.getBoundingClientRect();
  const contentRect = content.getBoundingClientRect();
  const oldHead = [...top.childNodes];
  const oldPageRect = oldPage.getBoundingClientRect();
  const oldRows = motionRows(oldPage, contentRect);
  const before = { head: morphTargets(top, topRect), body: morphTargets(content, contentRect) };
  update();
  const page = content.firstElementChild;
  // A render held back (an open menu) changed nothing.
  if (!page || page === oldPage || ui.builtPage === from) return;
  const turn = from === "settings" || ui.builtPage === "settings";
  const panelRect = $("panel").getBoundingClientRect();
  const after = { head: morphTargets(top, topRect), body: morphTargets(content, contentRect) };
  // Measure the live page while layout is already current. Reading its height
  // after mounting the animated copies forces WebKitGTK to lay them all out
  // synchronously, blocking the first animation frame on long Activity pages.
  const newRows = motionRows(page, contentRect);
  const pageRect = page.getBoundingClientRect();
  const height = panelHeight(page);

  const layer = motionLayer("page-motion");
  const head = motionLayer("page-motion-head", { left: panelRect.left, top: topRect.top, width: panelRect.width }, panelRect);
  head.style.height = `${topRect.height}px`;
  const body = motionLayer("page-motion-body", { left: panelRect.left, top: contentRect.top, width: panelRect.width }, panelRect);
  layer.append(head, body);

  // Elements named on both pages, at least one of them in view, move; their
  // places in the page copies stay empty.
  const pairs = [];
  if (!turn) {
    for (const [region, origin, into] of [["head", topRect, head], ["body", contentRect, body]]) {
      const inView = (rect) => rect.bottom > origin.top && rect.top < origin.bottom;
      for (const [name, next] of after[region]) {
        const prev = before[region].get(name);
        if (!prev || !(inView(prev.rect) || inView(next.rect))) continue;
        prev.el.setAttribute("data-morph-paired", "");
        next.el.setAttribute("data-morph-paired", "");
        pairs.push({ prev, next, origin: { left: panelRect.left, top: origin.top }, into });
      }
    }
  }

  const oldTop = motionLayer("top page-motion-page");
  oldTop.append(...oldHead);
  const newTop = top.cloneNode(true);
  newTop.removeAttribute("id");
  newTop.className = "top page-motion-page";
  const newPage = page.cloneNode(true);
  newPage.classList.add("page-motion-page");
  oldPage.classList.add("page-motion-page");
  for (const [el, rect] of [[oldTop, topRect], [newTop, topRect]]) place(el, rect, { left: panelRect.left, top: topRect.top }, true);
  place(oldPage, oldPageRect, { left: panelRect.left, top: contentRect.top });
  place(newPage, pageRect, { left: panelRect.left, top: contentRect.top });
  head.append(oldTop, newTop);
  body.append(oldPage, newPage);

  const duration = turn ? TURN_MS : MORPH_MS;
  const timing = { duration, easing: MOTION_EASE, fill: "both" };
  const pending = [];
  const animate = (el, frames, options = timing) => pending.push({ el, frames, options });
  if (turn) {
    // Into Settings the next page comes from the right; back out it leaves there.
    const shift = (ui.builtPage === "settings" ? 1 : -1) * panelRect.width;
    for (const el of [oldTop, oldPage]) animate(el, [{ transform: "none", opacity: 1 }, { transform: `translateX(${-shift}px)`, opacity: 0.4 }]);
    for (const el of [newTop, newPage]) animate(el, [{ transform: `translateX(${shift}px)` }, { transform: "none" }]);
  } else {
    for (const el of [oldTop, oldPage]) animate(el, [{ opacity: 1 }, { opacity: 0 }], { ...timing, duration: duration * 0.6 });
    for (const el of [newTop, newPage]) animate(el, [{ opacity: 0 }, { opacity: 1 }], { ...timing, delay: duration * 0.2, duration: duration * 0.8 });
    // Blocks first, so a moving ring or row is never covered by a block passing it.
    pairs.sort((a, b) => b.next.block - a.next.block
      || (a.next.el.compareDocumentPosition(b.next.el) & Node.DOCUMENT_POSITION_FOLLOWING ? -1 : 1));
    for (const pair of pairs) morph(pair, animate, timing);
  }
  for (const { next } of pairs) next.el.removeAttribute("data-morph-paired");
  trimMotionRows(oldPage, oldRows);
  trimMotionRows(newPage, newRows);

  top.style.visibility = content.style.visibility = "hidden";
  $("panel").append(layer);
  const motion = { layer, fit: true };
  pageMotion = motion;
  // A taller page gets its room before moving in; a shorter one gives it back after.
  if (height > lastHeight) fitPanel(height);
  // Mount every copy before starting animations, so WebKit starts the effects
  // together on the next rendering update rather than on detached elements.
  const animations = pending.map(({ el, frames, options }) => el.animate(frames, options));
  Promise.all(animations.map((a) => a.finished)).then(() => {
    if (pageMotion === motion) endPageMotion();
  }, () => {});
}

/// Moves one named element from its old place to its new one. The same
/// markup moves as one piece; otherwise the old copy fades into the new one
/// on the way. A block's box reshapes around both.
function morph({ prev, next, origin, into }, animate, timing) {
  const a = morphCopy(prev);
  const b = morphCopy(next);
  const same = a.isEqualNode(b);
  const fade = motionLayer("morph-fade");
  if (prev.block) {
    const box = motionLayer("morph-box", prev.rect, origin);
    box.style.height = `${prev.rect.height}px`;
    const frame = (rect) => ({ left: `${rect.left - origin.left}px`, top: `${rect.top - origin.top}px`, width: `${rect.width}px`, height: `${rect.height}px` });
    animate(box, [frame(prev.rect), frame(next.rect)]);
    a.style.width = `${prev.rect.width}px`;
    b.style.width = `${next.rect.width}px`;
    box.append(fade);
    into.append(box);
  } else {
    // Text keeps its page's look; the copy is placed and sized like the original.
    for (const [copy, target] of [[a, prev], [b, next]]) {
      for (const [name, value] of target.text) copy.style.setProperty(name, value);
      place(copy, target.rect, origin, true);
      // Fractional widths must not wrap the text.
      copy.style.width = `${Math.ceil(target.rect.width)}px`;
    }
    const dx = next.rect.left - prev.rect.left;
    const dy = next.rect.top - prev.rect.top;
    // Text grows or shrinks to the next page's size; boxes may differ in width.
    const scale = next.size / prev.size;
    animate(a, [{ transform: "none" }, { transform: `translate(${dx}px, ${dy}px) scale(${scale})` }]);
    animate(b, [{ transform: `translate(${-dx}px, ${-dy}px) scale(${1 / scale})` }, { transform: "none" }]);
    into.append(fade);
  }
  a.classList.add("morph-copy");
  b.classList.add("morph-copy");
  if (same) {
    fade.append(b);
    return;
  }
  // Fading with the motion's easing settles with the place.
  fade.append(a, b);
  animate(a, [{ opacity: 1 }, { opacity: 0 }]);
  animate(b, [{ opacity: 0 }, { opacity: 1 }]);
}

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
      changePage(() => {
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
      if (first) setTimeout(() => showMenu(first), 250);
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
  if (select) { showSelect(select); return; }
  const menu = event.target.closest("[data-menu]");
  if (menu) { showMenu(menu); return; }
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
  if (openSelect) {
    const { menu } = openSelect;
    const items = menuItems(menu);
    const index = items.indexOf(document.activeElement);
    if (event.key === "Escape" || event.key === "Tab") {
      closeSelect(true);
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
    (event.target.dataset.menu ? showMenu : showSelect)(event.target, event.key === "ArrowUp");
    return;
  }
  if (event.target.matches("[data-config]") && (event.key === "Enter" || event.key === "Escape")) {
    if (event.key === "Escape") event.target.value = ui.snap.config.details?.editable[event.target.dataset.config] ?? "";
    event.target.blur();
    return;
  }
  const step = openSelect ? null : menuBarStep(event);
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
    closeSelect();
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
  hideRouteTooltip();
  // Repeated native hide events must not extend the previous page's lifetime.
  panelHiddenAt ??= Date.now();
  closeSelect();
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
