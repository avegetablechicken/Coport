"use strict";

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const IS_MAC = navigator.userAgent.includes("Mac");
if (IS_MAC) document.documentElement.classList.add("macos");

// ---------------------------------------------------------------- icons

const svg = (body, extra = "") =>
  `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" ${extra}>${body}</svg>`;

const ICON = {
  copy: svg('<rect x="9" y="9" width="11" height="11" rx="2.5"/><path d="M5 15V6.5A2.5 2.5 0 0 1 7.5 4H15"/>'),
  check: svg('<path d="m5 12.5 4.5 4.5L19 7.5"/>'),
  restart: svg('<path d="M20 11.5A8 8 0 1 1 17.7 6"/><path d="M20 4v5h-5"/>'),
  back: svg('<path d="m15 18-6-6 6-6"/>', 'stroke-width="2"'),
  chevron: svg('<path d="m9 6 6 6-6 6"/>', 'stroke-width="2.4"'),
  search: svg('<circle cx="11" cy="11" r="7"/><path d="m20 20-3.5-3.5"/>'),
  trash: svg('<path d="M4 7h16M9 7V4.5h6V7M6.5 7l1 13h9l1-13"/>'),
  info: svg('<circle cx="12" cy="12" r="8.5"/><path d="M12 11v5.5M12 7.75v.01"/>'),
  warning: svg('<circle cx="12" cy="12" r="8.5"/><path d="M12 7.5v5M12 16.25v.01"/>'),
  folder: svg('<path d="M3 7.5V18a2 2 0 0 0 2 2h14a2 2 0 0 0 2-2V9.5a2 2 0 0 0-2-2h-7l-2-2.5H5a2 2 0 0 0-2 2.5z"/>'),
};

// ---------------------------------------------------------------- helpers

const $ = (id) => document.getElementById(id);

function esc(value) {
  return String(value ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);
}

function fmtMs(ms) {
  if (ms == null) return "—";
  return ms < 1000 ? `${ms} ms` : `${(ms / 1000).toFixed(1)} s`;
}

function fmtBytes(bytes) {
  const units = ["B", "KB", "MB", "GB", "TB"];
  let value = bytes ?? 0;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return unit === 0 ? `${value} B` : `${value.toFixed(1)} ${units[unit]}`;
}

function fmtUptime(secs) {
  const d = Math.floor(secs / 86400);
  const h = Math.floor((secs % 86400) / 3600);
  const m = Math.floor((secs % 3600) / 60);
  if (d) return `${d}d ${h}h`;
  if (h) return `${h}h ${m}m`;
  if (m) return `${m}m ${secs % 60}s`;
  return `${secs}s`;
}

function fmtTime(ms) {
  return ms == null ? "" : new Date(ms).toLocaleTimeString("en-GB", { hour12: false });
}

function statusClass(status) {
  if (status == null) return "none";
  return `s${Math.min(5, Math.max(2, Math.floor(status / 100)))}`;
}

/// Regional-indicator flag for an ISO country or region code.
function flag(code) {
  return /^[A-Z]{2}$/.test(code ?? "") ? String.fromCodePoint(...[...code].map((c) => 0x1f1a5 + c.charCodeAt(0))) : "";
}

const TAG_COLORS = 8;

/// Gives each proxy a color that stays put as others are added or removed:
/// the name's hash picks a slot, and collisions move to the next free one.
function tagColors(names) {
  const taken = new Set();
  const colors = {};
  for (const name of [...names].sort()) {
    let hash = 0;
    for (const ch of name) hash = (hash * 31 + ch.codePointAt(0)) >>> 0;
    let slot = hash % TAG_COLORS;
    while (taken.has(slot) && taken.size < TAG_COLORS) slot = (slot + 1) % TAG_COLORS;
    taken.add(slot);
    colors[name] = slot;
  }
  return colors;
}

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

// Original provider SVGs copied from OpenQuota; CSS masks apply the GUI theme color.
function serviceMark(service) {
  const name = { Claude: "claude", Codex: "codex" }[service];
  return name ? `<span class="service-mark ${name}-mark" aria-hidden="true"></span>` : "";
}

// ---------------------------------------------------------------- state

const ui = {
  snap: null,
  fetchedAt: 0,
  refreshRequest: 0,
  page: "main",
  filter: "requests",
  search: "",
  rows: [],
  recent: [],
  trafficMinutes: 30,
  trafficScope: "all",
  homeTraffic: null,
  homeTrafficMinutes: 30,
  homeTrafficRequest: 0,
  homeTrafficError: "",
  homeTrafficFetchedAt: 0,
  homeTrafficLoading: false,
  traffic: null,
  trafficError: "",
  trafficLoading: false,
  trafficRequest: 0,
  expanded: new Set(),
  builtPage: null,
  choosePath: false,
  websocketOpen: false,
  snippetsOpen: false,
  rendering: false,
  proxies: {},
  tagColors: {},
};

async function refresh() {
  const request = ++ui.refreshRequest;
  const [snap, recent] = await Promise.all([
    invoke("get_state"),
    invoke("get_activity", { filter: "requests", search: "" }),
  ]);
  if (request !== ui.refreshRequest) return;
  ui.snap = snap;
  ui.recent = recent.slice(0, 5);
  const proxies = snap.config.details?.proxies ?? [];
  ui.proxies = Object.fromEntries(proxies.map((p) => [p.name, p]));
  ui.tagColors = tagColors(proxies.map((p) => p.name));
  ui.fetchedAt = Date.now();
  if (ui.page === "activity") await loadActivity(false);
  else if (ui.page === "main" && Date.now() - ui.homeTrafficFetchedAt >= 15000) loadHomeTraffic();
  render();
}

let refreshTimer;
function scheduleRefresh() {
  clearTimeout(refreshTimer);
  refreshTimer = setTimeout(refresh, 120);
}

async function loadActivity(rerender = true) {
  ui.rows = await invoke("get_activity", { filter: ui.filter, search: ui.search });
  if (rerender) renderActivityList();
}

function uptime() {
  return ui.snap.phase.uptimeSecs + Math.floor((Date.now() - ui.fetchedAt) / 1000);
}

// ---------------------------------------------------------------- render

const BACK_KEYS = IS_MAC ? "Meta+[" : "Alt+ArrowLeft";

function render() {
  if (!ui.snap) return;
  if (openSelect) { selectRenderPending = true; return; }
  renderTop();
  renderPage();
  queueFit();
}

function renderTop() {
  if (ui.page === "main") {
    $("top").innerHTML = `
      <span class="app-name">Coport</span><span class="version">v${esc(ui.snap.version)}</span>
      <nav class="links">
        <button class="text-link" data-action="page" data-page="activity">Activity</button>
        <button class="text-link" data-action="page" data-page="settings">Settings</button>
        <button class="text-link" data-action="quit">Quit</button>
      </nav>`;
    return;
  }
  const title = ui.page === "activity" ? "Activity" : "Settings";
  const tools =
    ui.page === "activity"
      ? `<nav class="links">
          <button class="icon-btn" data-action="clear-activity" data-tip="Clear list" aria-label="Clear list">${ICON.trash}</button>
          <button class="icon-btn" data-action="open" data-target="log" data-tip="Open log file" aria-label="Open log file">${ICON.folder}</button>
        </nav>`
      : "";
  $("top").innerHTML = `
    <button class="text-link back" data-action="page" data-page="main" aria-keyshortcuts="${BACK_KEYS}">${ICON.back}Back</button>
    <span class="page-title">${title}</span>${tools}`;
}

function renderPage() {
  hideRouteTooltip();
  const content = $("content");
  if (ui.page === "activity") {
    if (ui.builtPage !== "activity") {
      closeSelect();
      content.innerHTML = activityShell();
      ui.builtPage = "activity";
      renderActivityTraffic();
    }
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
  content.innerHTML = `<div class="page">${ui.page === "settings" ? settings() : main()}</div>`;
  const field = typing && $(typing.id);
  if (field) {
    field.value = typing.value;
    field.focus();
    field.setSelectionRange(typing.start, typing.end);
  }
  ui.rendering = false;
}

/// A block: titled box with an optional right-aligned headline.
function block(title, aside, body) {
  return `<section class="block">
    <div class="block-head"><span class="block-title">${title}</span>${aside ? `<span class="block-aside">${aside}</span>` : ""}</div>
    ${body}</section>`;
}

function stat(label, value, cls = "") {
  return `<div class="stat"><div class="stat-label">${label}</div><div class="stat-value ${cls}">${value}</div></div>`;
}

// ---------------------------------------------------------------- main

function main() {
  return [proxyBlock(), trafficBlock(), connectBlock(), recentBlock(), proxiesBlock(), routingBlock()].join("");
}

function message(kind, text, actions = "") {
  return `<div class="message ${kind}"><span class="message-dot"></span><div class="message-body">${text}${
    actions ? `<div class="message-actions">${actions}</div>` : ""
  }</div></div>`;
}

function proxyBlock() {
  const s = ui.snap;
  const state = s.phase.state;
  const label = { running: "Running", stopped: "Stopped", failed: "Failed to start" }[state];
  const head = `<span class="state"><span class="dot ${state}"></span>${label}</span>
    <button class="switch" role="switch" aria-checked="${state === "running"}" data-action="power" aria-label="Start or stop the proxy"></button>`;
  let messages = "";
  if (!s.config.exists) {
    messages += message(
      "warn",
      `No configuration file at <span class="selectable">${esc(s.config.path)}</span>.`,
      `<button class="btn" data-action="create-config">Create from Example</button>
       <button class="btn" data-action="choose-config">Choose File…</button>`
    );
  } else if (s.config.error) {
    messages += message("bad", esc(s.config.error), `<button class="btn" data-action="open" data-target="config">Edit Configuration</button>`);
  } else if (state === "failed") {
    messages += message("bad", esc(s.phase.error));
  }
  if (s.config.changedSinceStart) {
    messages += message("warn", "The configuration changed since the proxy started.", `<button class="btn" data-action="restart">Restart to Apply</button>`);
  }
  const strip = `<div class="strip">
    ${stat("Address", `127.0.0.1:${s.phase.port}`)}
    ${stat("Uptime", state === "running" ? `<span id="uptime">${fmtUptime(uptime())}</span>` : "—")}
    ${stat("Config", s.config.error ? "Invalid" : s.config.exists ? "Valid" : "Missing", s.config.error ? "bad" : "")}
  </div>`;
  return block("Proxy", head, strip + messages);
}

function chart(values, errors, showPeak = false) {
  const n = values.length;
  if (!n) return "";
  const max = Math.max(1, ...values);
  const gap = 2;
  const w = 300;
  const h = 32;
  const bar = (w - gap * (n - 1)) / n;
  let bars = "";
  values.forEach((v, i) => {
    const x = (i * (bar + gap)).toFixed(2);
    const bh = v ? Math.max(2.5, (v / max) * h) : 1;
    bars += `<rect class="${v ? "" : "idle"}" x="${x}" y="${(h - bh).toFixed(2)}" width="${bar.toFixed(2)}" height="${bh.toFixed(2)}" rx="1"><title>${v} requests</title></rect>`;
    if (errors[i]) {
      const eh = (bh * errors[i]) / v;
      bars += `<rect class="err" x="${x}" y="${(h - bh).toFixed(2)}" width="${bar.toFixed(2)}" height="${eh.toFixed(2)}" rx="1"></rect>`;
    }
  });
  const plot = `<svg class="chart" viewBox="0 0 ${w} ${h}" preserveAspectRatio="none">${bars}</svg>`;
  if (!showPeak) return plot;
  const peak = Math.max(0, ...values);
  const index = values.indexOf(peak);
  const position = ((index * (bar + gap) + bar / 2) / w) * 100;
  // Align edge labels inward so the first/last bucket cannot clip the value.
  const shift = position < 20 ? "0" : position > 80 ? "-100%" : "-50%";
  return `<div class="traffic-chart">
    <span class="chart-peak" style="left:${position}%;transform:translateX(${shift})" title="Peak: ${peak.toLocaleString("en-US")} requests per bar">${peak.toLocaleString("en-US")}</span>
    ${peak ? `<span class="chart-peak-stem" style="left:${position}%" aria-hidden="true"></span>` : ""}
    ${plot}
  </div>`;
}

async function loadHomeTraffic(force = false) {
  if (ui.homeTrafficLoading && !force) return;
  const request = ++ui.homeTrafficRequest;
  ui.homeTrafficLoading = true;
  try {
    const traffic = await invoke("get_traffic", { minutes: ui.homeTrafficMinutes, scope: ui.trafficScope });
    if (request !== ui.homeTrafficRequest) return;
    ui.homeTraffic = traffic.summary;
    ui.homeTrafficError = "";
    ui.homeTrafficFetchedAt = Date.now();
  } catch (error) {
    if (request !== ui.homeTrafficRequest) return;
    ui.homeTrafficError = String(error);
  } finally {
    if (request === ui.homeTrafficRequest) ui.homeTrafficLoading = false;
  }
  if (ui.page === "main") render();
}

function trafficBlock() {
  const st = ui.homeTraffic;
  const range = trafficControls("home-traffic", ui.homeTrafficMinutes, "Home traffic time range");
  if (!st) return block("Traffic", range, `<div class="placeholder">${esc(ui.homeTrafficError || "Loading traffic…")}</div>`);
  let rate = "—";
  let rateClass = "";
  if (st.requests) {
    const r = (st.errors / st.requests) * 100;
    rate = `${r.toFixed(r > 0 && r < 10 ? 1 : 0)}%`;
    rateClass = r >= 10 ? "bad" : "";
  }
  return block(
    "Traffic",
    range,
    `<div class="traffic-content" aria-busy="${ui.homeTrafficLoading}">
     ${ui.homeTrafficError ? `<span class="traffic-update-error" title="${esc(ui.homeTrafficError)}">Update failed</span>` : ""}
     ${chart(st.counts, st.errorCounts)}
     <div class="strip">
       ${stat(ui.trafficScope === "model" ? "Calls" : "Requests", st.requests)}
       ${stat("Error Rate", rate, rateClass)}
       ${stat("Avg. Time", fmtMs(st.avgMs))}
       ${stat("Received", fmtBytes(st.bytes))}
     </div>${modelTokenStats(st)}</div>`
  );
}

function connectBlock() {
  const s = ui.snap;
  const row = (name, url) => `<div class="row">
      <span class="row-label">${name}</span>
      <span class="row-value selectable">${esc(url)}</span>
      <button class="icon-btn" data-action="copy" data-text="${esc(url)}" data-tip="Copy" aria-label="Copy ${name} base URL">${ICON.copy}</button>
    </div>`;
  return block(
    "Connect",
    "",
    `${row("Claude Code", s.urls.claude)}${row("Codex", s.urls.codex)}
     <details class="disclosure" id="setup-snippets" ${ui.snippetsOpen ? "open" : ""}><summary>${ICON.chevron}Setup snippets</summary>
       ${snippet("~/.claude/settings.json, merged into existing settings", `{\n  "env": {\n    "ANTHROPIC_BASE_URL": "${s.urls.claude}"\n  }\n}`)}
       ${snippet("Top of ~/.codex/config.toml, then restart Codex", `openai_base_url = "${s.urls.base}/v1"\nchatgpt_base_url = "${s.urls.chatgpt}"`)}
       ${snippet("CA for HTTPS: add to the system trust store, or set SSL_CERT_FILE in ~/.codex/.env", s.urls.caCertificate)}
     </details>`
  );
}

function snippet(label, code) {
  return `<div class="snippet"><div class="snippet-label">${label}</div><pre>${esc(code)}</pre>
    <button class="icon-btn" data-action="copy" data-text="${esc(code)}" aria-label="Copy snippet">${ICON.copy}</button></div>`;
}

function recentBlock() {
  const rows = ui.recent;
  const aside = rows.length ? `<button class="text-link" data-action="page" data-page="activity">View All</button>` : "";
  const body = rows.length
    ? rows.map((e) => requestRow(e, false)).join("")
    : `<div class="placeholder">${ui.snap.phase.state === "running" ? "Waiting for the first request…" : "Start the proxy to see requests."}</div>`;
  return block("Recent Requests", aside, body);
}

function proxiesBlock() {
  const d = ui.snap.config.details;
  if (!d) return "";
  const aside = d.proxies.length ? `<button class="text-link" data-action="probe">Test All</button>` : "";
  const body = d.proxies.length
    ? d.proxies
        .map((p) => {
          const probe = p.probe;
          const state = probe?.state ?? "idle";
          const dot = { ok: "running", error: "failed" }[state] ?? "";
          let value = `<span class="faint">—</span>`;
          if (state === "pending") value = `<span class="spinner"></span>`;
          else if (state === "ok") value = `${probe.ms} ms`;
          else if (state === "error") value = `<span class="bad">Unreachable</span>`;
          // Plain HTTP is the common case; other schemes stay visible.
          const shown = p.endpoint.replace(/^http:\/\//, "");
          const mark = flag(probe?.country);
          const exit =
            p.local && probe?.exitIp
              ? ` → ${mark ? `<span class="flag" data-tip="${esc(probe.country)}">${mark}</span> ` : ""}${esc(probe.exitIp)}`
              : "";
          return `<div class="row proxy">
            <span class="dot ${dot}" ${probe?.error ? `data-tip="${esc(probe.error)}"` : ""}></span>
            <span class="row-label">${tag(p.name)}<span class="row-sub">${esc(shown)}${exit}</span></span>
            <span class="row-value">${value}</span>
            <button class="icon-btn" data-action="probe" data-name="${esc(p.name)}" data-tip="Test" aria-label="Test ${esc(p.name)}">${ICON.restart}</button>
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
  apiKeyFallback: "API key fallback",
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

// Presentation only: full selectors remain intact in configuration and tooltips.
function routeIdentity(selector, source) {
  if (source === "account") {
    const uuid = /^[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}$/i.test(selector);
    return { name: uuid ? `${selector.slice(0, 8)}…${selector.slice(-4)}` : selector, kind: "account", type: "Account" };
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

function routingRow(route, account, fileOnly = false) {
  const identity = routeIdentity(route.selector, route.kind || (account ? "account" : "unknown"));
  const warning = route.activation === "remote"
    ? "No match in the local OAuth account cache. The account probe confirmed this account is available."
    : "No match in the local OAuth account cache. The account probe could not confirm availability. Check the credential and account probe connection.";
  const needsWarning = ["unknown", "remote", "probe_failed"].includes(route.activation);
  const activation = !account || !fileOnly ? ""
    : (route.activation === "inactive"
      ? '<span class="route-activation bad" title="No configured local credential currently selects this account route. Sign in or configure its credential source.">Inactive</span>' : "")
      + (route.activation === "probe_failed" ? '<span class="route-activation bad">Probe failed</span>' : "")
      + (needsWarning ? `<span class="route-activation route-warning tip-wide" tabindex="0" role="img" aria-label="${esc(warning)}" data-tip="${esc(warning)}">${ICON.warning}</span>` : "");
  if (account || route.kind === "api_key") {
    return `<div class="route-summary route-static">
      <span class="route-identity"><span class="route-icon" aria-hidden="true">${ROUTE_ICON[identity.kind]}</span><span class="route-name" data-full-name="${esc(route.selector)}">${esc(identity.name)}</span>${activation}</span>
      <span class="route-destination">${routingChain(route.proxies)}</span>
    </div>`;
  }
  const extra = route.detail && route.detail !== route.selector ? route.detail : "";
  const title = `${identity.type}: ${route.selector}${extra ? `\n${extra}` : ""}`;
  return `<div class="route-summary" title="${esc(title)}">
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

// Render account warnings outside the scrolling panel, with viewport-aware
// placement instead of extending a pseudo-element left of the icon.
let routeTooltipTimer;
let routeTooltipOwner;

function hideRouteTooltip() {
  clearTimeout(routeTooltipTimer);
  routeTooltipOwner?.removeAttribute("aria-describedby");
  routeTooltipOwner = null;
  $("route-tooltip").hidden = true;
}

function routeTooltipPosition(anchor, size, width, height) {
  const margin = 12;
  const left = Math.max(margin, Math.min(anchor.left, width - size.width - margin));
  const above = anchor.top - size.height - 6;
  const top = Math.max(margin, Math.min(above >= margin ? above : anchor.bottom + 6, height - size.height - margin));
  return { left, top };
}

function showRouteTooltip(owner) {
  if (routeTooltipOwner === owner) return;
  hideRouteTooltip();
  routeTooltipOwner = owner;
  routeTooltipTimer = setTimeout(() => {
    if (!owner.isConnected) return hideRouteTooltip();
    const tip = $("route-tooltip");
    tip.textContent = owner.dataset.tip;
    tip.hidden = false;
    tip.style.visibility = "hidden";
    const { left, top } = routeTooltipPosition(owner.getBoundingClientRect(), tip.getBoundingClientRect(), document.documentElement.clientWidth, document.documentElement.clientHeight);
    tip.style.left = `${left}px`;
    tip.style.top = `${top}px`;
    tip.style.visibility = "visible";
    owner.setAttribute("aria-describedby", "route-tooltip");
  }, 500);
}

for (const type of ["pointerover", "focusin"]) {
  document.addEventListener(type, (event) => {
    const owner = event.target.closest?.(".route-warning");
    if (owner) showRouteTooltip(owner);
  });
}
for (const type of ["pointerout", "focusout"]) {
  document.addEventListener(type, (event) => {
    const owner = event.target.closest?.(".route-warning");
    if (!owner || owner.contains(event.relatedTarget)) return;
    if (type === "pointerout" && document.activeElement === owner) return;
    if (type === "focusout" && owner.matches(":hover")) return;
    hideRouteTooltip();
  });
}
document.addEventListener("scroll", (event) => {
  if (event.target !== $("route-tooltip")) hideRouteTooltip();
}, true);
window.addEventListener("resize", hideRouteTooltip);

function routingBlock() {
  const s = ui.snap;
  const d = s.config.details;
  if (!d) return "";
  const report = Object.fromEntries(s.routes.map((r) => [r.name.toLowerCase(), r]));
  const section = (name, svc) => {
    const r = report[name.toLowerCase()];
    // Startup credential checks do not establish whole-service availability.
    // Older daemons also reported a missing local identity as a route failure
    // even when the configured account probe can resolve it.
    const profileWarning = name === "Claude" && r?.reason === "Claude account has no proxy route."
      && svc.accountRoutes.some((route) => ["unknown", "remote", "probe_failed"].includes(route.activation));
    const badge = r && !r.ok && !profileWarning
      ? `<span class="bad" data-tip="${esc(r.reason ?? "")}">Credential warning</span>` : "";
    const configured = svc.configured || svc.fallbacks.some((f) => f.proxies);
    if (!configured) {
      return `<div class="subhead"><span>${name}</span><span class="faint">Not configured</span></div>`;
    }
    const rows = svc.accountRoutes.map((route) => routingRow(route, true, svc.fileOnly)).join("")
      + svc.apiKeyRoutes.map((route) => routingRow(route, false)).join("");
    const fallbacks = svc.fallbacks.filter((f) => f.proxies)
      .sort((a, b) => MINOR_FALLBACKS.has(a.key) - MINOR_FALLBACKS.has(b.key))
      .map((f) => `<div class="route-default"><span class="route-default-name">${FALLBACK_LABEL[f.key]}</span><span class="route-destination">${routingChain(f.proxies)}</span></div>`).join("");
    return `<div class="routing-service"><div class="subhead"><span>${name}</span>${badge}</div>${rows}
      ${fallbacks ? `<div class="route-defaults"><div class="route-defaults-title">Defaults &amp; helpers</div>${fallbacks}</div>` : ""}</div>`;
  };
  return block("Routing", "", section("Codex", d.codex) + section("Claude", d.claude));
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
  openSelect = { trigger, menu };
  trigger.setAttribute("aria-expanded", "true");
  trigger.setAttribute("aria-controls", menu.id);
  document.body.append(menu);
  const rect = trigger.getBoundingClientRect();
  menu.style.minWidth = `${rect.width}px`;
  menu.style.maxHeight = `${Math.max(0, window.innerHeight - 8)}px`;
  const bounds = menu.getBoundingClientRect();
  menu.style.left = `${Math.max(4, Math.min(rect.right - bounds.width, window.innerWidth - bounds.width - 4))}px`;
  menu.style.top = `${Math.max(4, Math.min(rect.bottom + 3, window.innerHeight - bounds.height - 4))}px`;
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

document.addEventListener("pointerdown", (event) => {
  if (openSelect && !openSelect.menu.contains(event.target) && !openSelect.trigger.contains(event.target)) closeSelect();
});
document.addEventListener("scroll", (event) => {
  if (openSelect && !openSelect.menu.contains(event.target)) closeSelect();
}, true);
window.addEventListener("resize", () => closeSelect());

function trafficRangeSelect(id, selected, label) {
  return panelSelect(id, TRAFFIC_RANGES.map(([minutes, text]) => [minutes, `Last ${text}`]), selected, label, "traffic-range");
}

function trafficControls(prefix, minutes, label) {
  return `<span class="traffic-controls">${panelSelect(`${prefix}-scope`, [["all", "All"], ["model", "Models"]], ui.trafficScope,
    "Traffic request category", "traffic-scope", 'title="Model calls: each HTTP generation/compaction request and each generation within a WebSocket counts once, including active, failed and cancelled calls. CONNECT contents cannot be classified."')}${trafficRangeSelect(`${prefix}-range`, minutes, label)}</span>`;
}

async function loadTraffic() {
  const request = ++ui.trafficRequest;
  ui.trafficLoading = true;
  $("activity-traffic")?.setAttribute("aria-busy", "true");
  try {
    const traffic = await invoke("get_traffic", { minutes: ui.trafficMinutes, scope: ui.trafficScope });
    if (request !== ui.trafficRequest) return;
    ui.traffic = traffic;
    ui.trafficError = "";
  } catch (error) {
    if (request !== ui.trafficRequest) return;
    ui.trafficError = String(error);
  } finally {
    if (request === ui.trafficRequest) ui.trafficLoading = false;
  }
  if (ui.page === "activity") renderActivityTraffic();
}

function modelTokenStats(stats) {
  if (ui.trafficScope !== "model") return "";
  const tokens = (n) => n == null ? "—" : n.toLocaleString();
  return `<div class="strip">${stat("Input Tokens", tokens(stats.inputTokens))}${stat("Output Tokens", tokens(stats.outputTokens))}${stat("Cached Input", tokens(stats.cachedInputTokens))}${stat("Hit Rate", stats.cacheHitRate == null ? "—" : (100 * stats.cacheHitRate).toFixed(1) + "%")}</div>`;
}

function renderActivityTraffic() {
  if (openSelect) { selectRenderPending = true; return; }
  closeSelect();
  const el = $("activity-traffic");
  if (!el) return;
  const traffic = ui.traffic;
  el.setAttribute("aria-busy", String(ui.trafficLoading));
  const date = (ms) => new Date(ms).toLocaleString([], { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" });
  const rows = traffic?.credentials.map((c) => {
    const rate = c.requests ? (100 * c.errors / c.requests).toFixed(1) + "%" : "—";
    return `<div class="credential-traffic">
      <div class="traffic-identity"><span class="traffic-service">${serviceMark(c.service)}${esc(c.service)}</span><strong>${esc(c.credential)}</strong></div>
      ${chart(c.counts, c.errorCounts, true)}
      <div class="strip">${stat(ui.trafficScope === "model" ? "Calls" : "Requests", c.requests)}${stat("Error Rate", rate, c.errors ? "bad" : "")}${stat("Avg. Time", fmtMs(c.avgMs))}${stat("Received", fmtBytes(c.bytes))}</div>${modelTokenStats(c)}
    </div>`;
  }).join("");
  const notes = [
    "Sorted by received traffic, largest first. Red indicates errors; each chart uses its own scale.",
    `${ui.trafficScope === "model" ? "Each HTTP model request or WebSocket generation counts once, including active calls. Tokens are reported usage, not billing totals; missing usage is not treated as zero. Hit rate is cached input over all input tokens." : "Each HTTP request or tunnel connection counts once, including active connections."} Bytes and duration update when the call or connection ends.`,
    "Logs are retained for at least 30 days, including rotated history. Earlier records may be unavailable. Unidentified requests have no logged credential.",
  ].join("\n\n");
  el.innerHTML = `<div class="block-head"><span class="block-title">Traffic</span>
    <span class="info-tip" data-tip="${esc(notes)}" aria-label="${esc(notes)}">${ICON.info}</span>
    ${trafficControls("traffic", ui.trafficMinutes, "Traffic time range")}</div>
    ${ui.trafficError && traffic ? `<span class="traffic-update-error" title="${esc(ui.trafficError)}">Update failed</span>` : ""}
    ${!traffic ? `<div class="placeholder">${esc(ui.trafficError || "Loading traffic…")}</div>` : `${rows || '<div class="placeholder">No requests in this time range.</div>'}<div class="traffic-axis"><span>${esc(date(traffic.start))}</span><span>${esc(date(traffic.end))}</span></div><p class="traffic-note">${traffic.bucketMinutes} min per bar</p>`}`;
  queueFit();
}

function activityShell() {
  return `<div class="page">
    <section class="block" id="activity-traffic"></section>
    <section class="block">
      <div class="block-head"><span class="block-title">Log</span></div>
      <label class="search">${ICON.search}
        <input class="field" id="search" type="search" spellcheck="false" placeholder="Filter by path, proxy or status" value="${esc(ui.search)}" /></label>
      <div class="segmented" id="filters"></div>
      <div id="activity-list"></div>
    </section>
  </div>`;
}

function renderActivityList() {
  const filters = $("filters");
  if (!filters) return;
  filters.innerHTML = [
    ["requests", "Requests"],
    ["models", "Models"],
    ["errors", "Errors"],
    ["all", "All Events"],
  ]
    .map(([f, label]) => `<button aria-pressed="${ui.filter === f}" data-action="filter" data-filter="${f}">${label}</button>`)
    .join("");
  $("activity-list").innerHTML = ui.rows.length
    ? ui.rows.map((e) => requestRow(e, true) + (ui.expanded.has(e.seq) ? detail(e) : "")).join("")
    : `<div class="placeholder">${ui.snap.phase.state === "running" ? "No matching entries." : "The proxy is stopped."}</div>`;
  queueFit();
}

function requestRow(e, expandable) {
  const cancelled = e.event === "request_cancelled" || e.event === "model_call_cancelled";
  const modelCall = e.event.startsWith("model_call_");
  const unknown = e.event === "model_call_unknown";
  const incomplete = e.event === "model_call_incomplete";
  const code = cancelled ? "CXL" : modelCall ? (unknown ? "?" : e.error ? "ERR" : incomplete ? "INC" : e.event === "model_call_finished" ? "OK" : "RUN") : e.status ?? (e.error ? "ERR" : "—");
  const tip = cancelled ? "Cancelled" : unknown ? "Outcome unavailable" : incomplete ? `Incomplete${e.fields?.incomplete_reason ? `: ${e.fields.incomplete_reason}` : ""}` : "";
  const meta = [fmtTime(e.time), modelCall && e.fields?.model, e.service, e.proxy && (e.proxy === "none" ? "direct" : e.proxy), e.bytes != null && fmtBytes(e.bytes)]
    .filter(Boolean)
    .join(" · ");
  const tag = expandable ? "button" : "div";
  const attrs = expandable ? `data-action="expand" data-seq="${e.seq}" aria-expanded="${ui.expanded.has(e.seq)}"` : "";
  return `<${tag} class="req" ${attrs}>
    <span class="status ${cancelled || unknown ? "cancelled" : e.error ? "s5" : modelCall ? (incomplete ? "s4" : e.event === "model_call_finished" ? "s2" : "s3") : statusClass(e.status)}"${tip ? ` title="${esc(tip)}" aria-label="${esc(tip)}"` : ""}>${esc(code)}</span>
    <span class="req-main">
      <span class="req-path">${e.method ? `<span class="method">${esc(e.method)}</span>` : ""}${esc(e.path ?? e.event)}</span>
      ${expandable && meta ? `<span class="req-meta">${esc(meta)}</span>` : ""}
    </span>
    <span class="req-dur">${e.durationMs != null ? fmtMs(e.durationMs) : ""}</span>
  </${tag}>`;
}

function detail(e) {
  const rows = Object.entries(e.fields)
    .map(([k, v]) => {
      let value = typeof v === "string" ? v : JSON.stringify(v);
      if (k === "duration_ms" || k === "headers_ms") value = fmtMs(Number(value));
      if (k === "received_bytes") value = fmtBytes(Number(value));
      return `<dt>${esc(k)}</dt><dd>${esc(value)}</dd>`;
    })
    .join("");
  const json = JSON.stringify({ event: e.event, timestamp: e.time ? new Date(e.time).toISOString() : undefined, ...e.fields }, null, 2);
  return `<div class="detail">
    <dl class="kv"><dt>event</dt><dd>${esc(e.event)}</dd>${e.time ? `<dt>time</dt><dd>${esc(new Date(e.time).toLocaleString("en-GB"))}</dd>` : ""}${rows}</dl>
    <button class="icon-btn" data-action="copy" data-text="${esc(json)}" data-tip="Copy as JSON" aria-label="Copy as JSON">${ICON.copy}</button>
  </div>`;
}

// ---------------------------------------------------------------- settings

function settings() {
  const s = ui.snap;
  const set = s.settings;
  let status = "";
  if (!s.config.exists) status = message("warn", "The file does not exist yet.");
  else if (s.config.error) status = message("bad", esc(s.config.error));
  else if (s.config.changedSinceStart)
    status = message("warn", "Changed since the proxy started.", `<button class="btn" data-action="restart">Restart to Apply</button>`);
  let check = "";
  if (s.check.running) check = message("info", "Checking credentials…");
  else if (s.check.message != null) check = message(s.check.ok ? "good" : "bad", esc(s.check.message));
  return `
    ${block(
      "General",
      "",
      `<div class="row"><span class="row-label">Launch at login</span>
         <button class="switch" role="switch" aria-checked="${set.launchAtLogin}" data-action="launch-at-login" aria-label="Launch at login"></button></div>
       <div class="row"><span class="row-label">Start proxy when the app opens</span>
         <button class="switch" role="switch" aria-checked="${set.startProxyOnLaunch}" data-action="auto-start" aria-label="Start proxy when the app opens"></button></div>
       <div class="row"><span class="row-label">Keep proxy running after quit<span class="setting-description" id="keep-running-description">Quit exits the app; the proxy keeps running.<br>Reopen to manage or stop the proxy.</span></span>
         <button class="switch" role="switch" aria-checked="${set.keepProxyRunningOnQuit}" data-action="keep-running" aria-label="Keep proxy running after quit" aria-describedby="keep-running-description"></button></div>
       <div class="row"><span class="row-label">Appearance</span>
         ${panelSelect("appearance", [["System", "System"], ["Light", "Light"], ["Dark", "Dark"]], set.appearance, "Appearance", "", 'data-setting="appearance"')}</div>`
    )}
    ${configSettings()}
    ${block(
      "Configuration",
      "",
      `<div class="path selectable">${esc(s.config.path)}</div>
       ${status}${check}
       <div class="actions">
         <button class="btn" data-action="open" data-target="config">Edit…</button>
         <button class="btn" data-action="open" data-target="config-folder">Show in Folder</button>
         <button class="btn" data-action="check" ${s.config.exists && !s.check.running ? "" : "disabled"}
           data-tip="Validates credential sources like --check">Check Credentials</button>
       </div>
       <details class="disclosure" id="config-path-disclosure" ${ui.choosePath ? "open" : ""}><summary>${ICON.chevron}Use another file</summary>
         <div class="inline-form"><input class="field" id="config-path" spellcheck="false" aria-label="YAML configuration path" placeholder="Path to config.yaml or config.yml" value="${esc(s.config.path)}" />
           <button class="btn" data-action="apply-path">Apply</button></div>
       </details>`
    )}
    ${block(
      "Request Log",
      "",
      `<div class="row"><span class="row-label path selectable">${esc(set.logPath)}</span>
         <button class="icon-btn" data-action="open" data-target="log-folder" data-tip="Open folder" aria-label="Open log folder">${ICON.folder}</button></div>`
    )}`;
}

/// Network and Subscription Logins: values written to the YAML configuration, applied on restart.
function configSettings() {
  const v = ui.snap.config.details?.editable;
  if (!v) return "";
  const field = (key, label, unit = "", description = "") => {
    const id = `config-${key.replaceAll(".", "-")}`;
    const note = description ? `<span class="setting-description" id="${id}-description">${description}</span>` : "";
    return `<div class="row"><span class="row-label">${label}${note}</span>
      <span class="number-field"><input class="field" id="${id}" data-config="${key}" inputmode="decimal" spellcheck="false"
        value="${esc(String(v[key]))}" aria-label="${label}"${description ? ` aria-describedby="${id}-description"` : ""} /><span class="unit">${unit}</span></span></div>`;
  };
  const flag = (key, label, description) => {
    const id = `config-${key.replaceAll(".", "-")}`;
    return `<div class="row"><span class="row-label">${label}<span class="setting-description" id="${id}-description">${description}</span></span>
      <button class="switch" role="switch" aria-checked="${v[key]}" data-action="config-flag" data-key="${key}" aria-label="${label}" aria-describedby="${id}-description"></button></div>`;
  };
  return `${block(
    "Network",
    "",
    `${field("listen_port", "Listen port")}
     ${field("request_timeout_seconds", "Request timeout", "s")}
     <details class="disclosure" id="websocket-timeouts" ${ui.websocketOpen ? "open" : ""}><summary>${ICON.chevron}WebSocket timeouts</summary>
       ${field("websocket.first_message_seconds", "First message", "s")}
       ${field("websocket.first_output_seconds", "First output", "s")}
       ${field("websocket.read_seconds", "Read", "s")}
       ${field("websocket.write_seconds", "Write", "s")}
       ${field("websocket.inter_turn_idle_seconds", "Inter-turn idle", "s", "0 disables this timer.")}
     </details>`
  )}
  ${block(
    "Subscription Logins",
    "",
    `${flag("codex.account_auth_file_only", "Codex: saved logins only", "Off also accepts other ChatGPT logins, routed by their token claims.")}
     ${flag("claude.account_auth_file_only", "Claude: saved logins only", "Off also accepts other Claude logins after a profile lookup.")}`
  )}`;
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
function queueFit() {
  if (fitQueued) return;
  fitQueued = true;
  requestAnimationFrame(() => {
    fitQueued = false;
    const page = $("content").firstElementChild;
    if (!page) return;
    const style = getComputedStyle($("content"));
    const padding = parseFloat(style.paddingTop) + parseFloat(style.paddingBottom);
    const height = Math.ceil($("top").offsetHeight + page.offsetHeight + padding);
    if (Math.abs(height - lastHeight) > 1) {
      lastHeight = height;
      invoke("fit_panel", { height });
    }
  });
}

// #content fills the window, so in-page growth (e.g. an opened disclosure)
// only shows up on the page element, which each render replaces.
const fitObserver = new ResizeObserver(queueFit);
fitObserver.observe($("content"));
let observedPage = null;
new MutationObserver(() => {
  const page = $("content").firstElementChild;
  if (page === observedPage) return;
  if (observedPage) fitObserver.unobserve(observedPage);
  if (page) fitObserver.observe(page);
  observedPage = page;
}).observe($("content"), { childList: true });

// ---------------------------------------------------------------- actions

async function act(action, el) {
  const s = ui.snap;
  switch (action) {
    case "page":
      ui.page = el.dataset.page;
      if (ui.page === "activity") {
        loadTraffic();
        await loadActivity(false);
      }
      if (ui.page === "main") loadHomeTraffic();
      if (ui.page !== "settings") ui.choosePath = false;
      render();
      $("content").scrollTop = 0;
      break;
    case "power":
      try {
        await invoke("set_running", { running: s.phase.state !== "running" });
      } catch (e) {
        toast(String(e));
      }
      await refresh();
      break;
    case "restart":
      await invoke("restart_proxy");
      toast("Proxy restarted");
      await refresh();
      break;
    case "copy": {
      await invoke("copy_text", { text: el.dataset.text });
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
      ui.filter = el.dataset.filter;
      await loadActivity();
      break;
    case "clear-activity":
      await invoke("clear_activity");
      ui.expanded.clear();
      await loadActivity();
      break;
    case "open":
      await invoke("open_path", { target: el.dataset.target });
      break;
    case "probe":
      await invoke("probe_proxy", { name: el.dataset.name ?? null });
      await refresh();
      break;
    case "check":
      await invoke("check_credentials");
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
    case "auto-start":
      await invoke("update_settings", { patch: { startProxyOnLaunch: el.getAttribute("aria-checked") !== "true" } });
      await refresh();
      break;
    case "keep-running":
      await invoke("update_settings", { patch: { keepProxyRunningOnQuit: el.getAttribute("aria-checked") !== "true" } });
      await refresh();
      break;
    case "config-flag":
      await setConfigValue(el.dataset.key, el.getAttribute("aria-checked") !== "true");
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
    case "choose-config":
      ui.page = "settings";
      ui.choosePath = true;
      render();
      $("config-path")?.focus();
      break;
    case "apply-path": {
      const path = $("config-path").value.trim();
      if (path) {
        try {
          await invoke("set_config_path", { path });
          ui.choosePath = false;
          toast("Configuration file changed");
        } catch (error) {
          toast(String(error));
        }
        await refresh();
      }
      break;
    }
    case "quit":
      await invoke("quit_app");
      break;
  }
}

document.addEventListener("click", (event) => {
  const select = event.target.closest("[data-options]");
  if (select) { showSelect(select); return; }
  const el = event.target.closest("[data-action]");
  if (el && !el.disabled) act(el.dataset.action, el);
});

document.addEventListener("change", async (event) => {
  if (event.target.id === "home-traffic-scope" || event.target.id === "traffic-scope") {
    ui.trafficScope = event.target.value;
    // Both views share the category. In-flight results from the old category
    // must not repopulate either cache after a switch.
    ++ui.trafficRequest;
    ++ui.homeTrafficRequest;
    // Keep the previous charts mounted until the replacement data is ready.
    ui.trafficError = ui.homeTrafficError = "";
    ui.homeTrafficFetchedAt = 0;
    ui.homeTrafficLoading = false;
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
    ui.homeTrafficMinutes = Number(event.target.value);
    ui.homeTrafficError = "";
    // Launch immediately so concurrent refreshes cannot start an older range.
    const loading = loadHomeTraffic(true);
    render();
    await loading;
    return;
  }
  if (event.target.id === "traffic-range") {
    ui.trafficMinutes = Number(event.target.value);
    ui.trafficError = "";
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
  ui.search = event.target.value;
  clearTimeout(searchTimer);
  searchTimer = setTimeout(() => loadActivity(), 120);
});

document.addEventListener("keydown", (event) => {
  if (openSelect) {
    const { menu } = openSelect;
    const items = [...menu.children];
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
  } else if (event.target.matches("[data-options]") && ["ArrowDown", "ArrowUp"].includes(event.key)) {
    event.preventDefault();
    showSelect(event.target, event.key === "ArrowUp");
    return;
  }
  if (event.target.matches("[data-config]") && (event.key === "Enter" || event.key === "Escape")) {
    if (event.key === "Escape") event.target.value = ui.snap.config.details?.editable[event.target.dataset.config] ?? "";
    event.target.blur();
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
  } else if (event.key === "Enter" && event.target.id === "config-path") {
    act("apply-path");
  }
});

document.addEventListener("focusout", (event) => {
  if (!ui.rendering && event.target.matches?.("[data-config]")) commitConfigField(event.target);
});

// Re-renders rebuild disclosures, so their open state lives in `ui`.
document.addEventListener("toggle", (event) => {
  const { id, open } = event.target;
  if (id === "websocket-timeouts") ui.websocketOpen = open;
  else if (id === "config-path-disclosure") ui.choosePath = open;
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

// Exit addresses and reachability go stale; recheck old results on open.
const probeStale = () => invoke("probe_proxy", { name: null, staleOnly: true });

listen("state-changed", scheduleRefresh);
listen("panel-shown", () => refresh().then(probeStale));
listen("panel-hidden", () => closeSelect());
refresh().then(probeStale);

// Refresh even when no new requests arrive, so the rolling window advances.
setInterval(() => {
  if (document.hidden) return;
  if (ui.page === "activity") loadTraffic();
  else if (ui.page === "main") refresh();
}, 15000);
