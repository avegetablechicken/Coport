"use strict";

const { invoke, Channel } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const IS_MAC = navigator.userAgent.includes("Mac");
const REVEAL_LABEL = IS_MAC ? "Show in Finder" : navigator.userAgent.includes("Windows") ? "Show in Explorer" : "Show in Folder";
if (IS_MAC) document.documentElement.classList.add("macos");

// ---------------------------------------------------------------- icons

const svg = (body, extra = "") =>
  `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" ${extra}>${body}</svg>`;

const ICON = {
  plus: svg('<path d="M12 5v14M5 12h14"/>'),
  copy: svg('<rect x="9" y="9" width="11" height="11" rx="2.5"/><path d="M5 15V6.5A2.5 2.5 0 0 1 7.5 4H15"/>'),
  check: svg('<path d="m5 12.5 4.5 4.5L19 7.5"/>'),
  restart: svg('<path d="M20 11.5A8 8 0 1 1 17.7 6"/><path d="M20 4v5h-5"/>'),
  back: svg('<path d="m15 18-6-6 6-6"/>', 'stroke-width="2"'),
  settings: svg('<path d="m9 3-.5 2-2 1-2-.5-1 2 1.5 1.5v3L3.5 14.5l1 2 2-.5 2 1 .5 2h3l.5-2 2-1 2 .5 1-2-1.5-1.5v-3L18 8.5l-1-2-2 .5-2-1L12.5 3z"/><circle cx="10.75" cy="11" r="3"/>'),
  chevron: svg('<path d="m9 6 6 6-6 6"/>', 'stroke-width="2.4"'),
  search: svg('<circle cx="11" cy="11" r="7"/><path d="m20 20-3.5-3.5"/>'),
  trash: svg('<path d="M4 7h16M9 7V4.5h6V7M6.5 7l1 13h9l1-13"/>'),
  info: svg('<circle cx="12" cy="12" r="8.5"/><path d="M12 11v5.5M12 7.75v.01"/>'),
  warning: svg('<circle cx="12" cy="12" r="8.5"/><path d="M12 7.5v5M12 16.25v.01"/>'),
  more: svg('<circle cx="6.5" cy="12" r="1.1"/><circle cx="12" cy="12" r="1.1"/><circle cx="17.5" cy="12" r="1.1"/>', 'fill="currentColor" stroke-width="0.8"'),
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
  if (ms == null) return "";
  const date = new Date(ms);
  const time = date.toLocaleTimeString("en-GB", { hour12: false });
  // Activity ranges can span days; other days carry their date.
  return date.toDateString() === new Date().toDateString()
    ? time
    : `${date.toLocaleDateString("en-US", { month: "short", day: "numeric" })} ${time}`;
}

/// Epoch milliseconds as a local `datetime-local` value, to the minute.
function localInput(ms) {
  const d = new Date(ms);
  const pad = (n) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

function currentMinute() {
  const d = new Date();
  d.setSeconds(0, 0);
  return d.getTime();
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
  devices: [],
  mergedData: null,
  mergedDataError: "",
  mergedDataLoading: false,
  mergedDataRequest: 0,
  deviceTrafficViews: Object.create(null),
  deviceTrafficFetchedAt: 0,
  mergedDataUpdating: false,
  deviceTrafficMinutes: 30,
  deviceTrafficOpen: Object.create(null),
  deviceStates: Object.create(null),
  devicesLoaded: false,
  deviceRefresh: false,
  fetchedAt: 0,
  refreshRequest: 0,
  page: "main",
  /// Opened from View All: keep the Log at the top until the user scrolls.
  pinLog: false,
  filter: "requests",
  search: "",
  searchMode: "keyword",
  activityRequest: 0,
  rows: [],
  // Activity list range: a preset re-resolved when chosen or the page opens,
  // or "custom". Epoch milliseconds; `to` is an inclusive minute.
  activityPreset: "hour",
  activityFrom: null,
  activityTo: null,
  activityNext: null,
  // Pages past the first have been loaded; a live update keeps them.
  activityPaged: false,
  activityLoadingMore: false,
  activityError: "",
  recent: [],
  trafficMinutes: 30,
  trafficScope: "model",
  localTrafficViews: Object.create(null),
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
  websocketOpen: false,
  snippetsOpen: false,
  rendering: false,
  proxies: {},
  tagColors: {},
};

async function refresh() {
  const request = ++ui.refreshRequest;
  const [snap, recent] = await Promise.all([
    invoke("get_state", { refreshAccounts: false }),
    invoke("get_activity", { filter: "requests", search: "" }),
  ]);
  if (request !== ui.refreshRequest) return;
  ui.snap = snap;
  ui.recent = recent.rows.slice(0, 5);
  const proxies = snap.config.details?.proxies ?? [];
  ui.proxies = Object.fromEntries(proxies.map((p) => [p.name, p]));
  ui.tagColors = tagColors(proxies.map((p) => p.name));
  ui.fetchedAt = Date.now();
  if (ui.page === "main" && Date.now() - ui.homeTrafficFetchedAt >= 15000) loadHomeTraffic();
  if (ui.page === "activity") scheduleActivityLog();
  render();
}

let refreshTimer;
function scheduleRefresh() {
  clearTimeout(refreshTimer);
  refreshTimer = setTimeout(refresh, 120);
}

/// Lists the chosen range newest first. `"more"` appends the next, older page; `"live"`
/// re-reads the log for lines written since and keeps the older pages already loaded.
/// Otherwise the backend reuses its last read of the range for filters, searches and pages.
async function loadActivity(mode) {
  if (ui.activityTo == null) resolveActivityRange();
  const query = {
    filter: ui.filter, search: ui.search, searchMode: ui.searchMode,
    range: { from: ui.activityFrom, to: ui.activityTo + 60 * 1000 },
    after: mode === "more" ? ui.activityNext : null,
    fresh: mode === "live",
  };
  // A live refresh must not invalidate a scan of the same query still running.
  const key = JSON.stringify([query.filter, query.search, query.searchMode, query.range, query.after]);
  if (mode === "live" && ui.activityLoadingKey === key && ui.activityLoadingRequest === ui.activityRequest) return;
  const request = ++ui.activityRequest;
  ui.activityLoadingKey = key;
  ui.activityLoadingRequest = request;
  const current = () => request === ui.activityRequest && query.filter === ui.filter && query.search === ui.search
    && query.searchMode === ui.searchMode && query.range.from === ui.activityFrom && query.range.to === ui.activityTo + 60 * 1000;
  if (mode === "more") ui.activityLoadingMore = true;
  let activity;
  try {
    activity = await invoke("get_activity", query);
  } catch (error) {
    // A failed live update keeps the list; the next one retries.
    if (!current() || mode === "live") return;
    // A failed older page keeps the pages already loaded, so it can be retried.
    if (mode === "more") {
      renderActivityList();
      toast(String(error));
      return;
    }
    ui.rows = [];
    ui.activityNext = null;
    ui.activityError = String(error);
    renderActivityList();
    return;
  } finally {
    if (ui.activityLoadingRequest === request) ui.activityLoadingKey = null;
    if (mode === "more") ui.activityLoadingMore = false;
  }
  if (!current()) return;
  ui.activityError = "";
  if (mode === "more") {
    ui.rows = ui.rows.concat(activity.rows);
    ui.activityNext = activity.next;
    ui.activityPaged = true;
    renderActivityList();
    return;
  }
  // Keep older pages only when the new page reaches the already loaded rows.
  // Otherwise a burst larger than one page would leave an unpageable gap.
  const last = activity.rows.at(-1);
  const overlaps = last && ui.rows.some((e) => e.time === last.time && e.seq === last.seq);
  const older = mode === "live" && ui.activityPaged && activity.next && overlaps
    ? ui.rows.filter((e) => e.time >= ui.activityFrom && (e.time < last.time || (e.time === last.time && e.seq < last.seq)))
    : [];
  const update = () => {
    ui.rows = activity.rows.concat(older);
    if (!older.length) ui.activityNext = activity.next;
    ui.activityPaged = older.length > 0;
    renderActivityList();
  };
  if (mode === "live") keepLogScroll(update);
  else update();
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
  if (ui.page !== "activity" || document.hidden || openSelect || ui.activityLoadingMore) return;
  if (!$("range-editor")?.hidden) return;
  const [from, to] = [ui.activityFrom, ui.activityTo];
  resolveActivityRange();
  if (from === ui.activityFrom && to === ui.activityTo && to < currentMinute()) return;
  loadActivity("live");
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
        ${(ui.devicesLoaded ? ui.devices.length : ui.snap.settings?.deviceCount || 0) > 0 ? `<button class="text-link" data-action="page" data-page="devices">Devices</button>` : ""}
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
    <button class="text-link back" data-action="page" data-page="main" aria-keyshortcuts="${BACK_KEYS}">${ICON.back}Back</button>
    <span class="page-title">${title}</span>${tools}`;
}

function renderPage() {
  hideRouteTooltip();
  const content = $("content");
  for (const details of content.querySelectorAll("details[data-device-traffic]")) (ui.deviceTrafficOpen ||= Object.create(null))[details.dataset.deviceTraffic] = details.open;
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
  content.innerHTML = `<div class="page${ui.page === "settings" ? " settings-page" : ""}">${ui.page === "settings" ? settings() : ui.page === "devices" ? devicesPage() : main()}</div>`;
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
  if (ui.snap.forwarding) {
    return remoteForwardingBlock() + connectBlock() + `<details class="disclosure"><summary>${ICON.chevron}Local proxy configuration and history (inactive)</summary>${[proxiesBlock(), routingBlock(), trafficBlock(), recentBlock()].join("")}</details>`;
  }
  return [proxyBlock(), trafficBlock(), connectBlock(), recentBlock(), proxiesBlock(), routingBlock()].join("");
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
  const remote = target.remoteTraffic;
  body += `<div class="block-head"><span class="block-title">Remote device · all requests · 30 min</span></div>`;
  if (remote) {
    const stale = Date.now() - remote.fetchedAt > 90000 || target.remoteTrafficError;
    body += `<div class="row"><span class="row-label">Requests / error rate</span><span class="row-value">${remote.requests} / ${remote.requests ? (remote.errors / remote.requests * 100).toFixed(1) : "0.0"}%</span></div><div class="placeholder">Includes other clients on ${esc(target.name)}. Updated ${esc(stamp(remote.fetchedAt))}${stale ? " · stale" : ""}.</div>`;
  } else body += `<div class="placeholder">${target.remoteTrafficError ? "Remote statistics unavailable" : "Loading remote statistics…"}</div>`;
  if (target.remoteTrafficError) body += message("warn", esc(target.remoteTrafficError));
  return block("SSH forwarding", "", body);
}

function message(kind, text, actions = "") {
  return `<div class="message ${kind}"><span class="message-dot"></span><div class="message-body">${text}${
    actions ? `<div class="message-actions">${actions}</div>` : ""
  }</div></div>`;
}

function proxyBlock() {
  const s = ui.snap;
  const state = s.phase.state;
  const label = s.phase.busy ? "Updating…" : { running: "Running", stopped: "Stopped", failed: "Failed to start" }[state];
  const head = `<span class="state"><span class="dot ${state}"></span>${label}</span>
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
  const strip = `<div class="strip">
    ${stat("Address", `127.0.0.1:${s.phase.port}`)}
    ${stat("Uptime", state === "running" ? `<span id="uptime">${fmtUptime(uptime())}</span>` : "—")}
    ${stat("Config", s.config.error ? "Invalid" : s.config.exists ? "Valid" : "Missing", s.config.error ? "bad" : "")}
  </div>`;
  return block("Proxy", head, strip + messages);
}

/// Bars of a stats object's buckets: requests, or with Models reported input
/// plus output tokens, in the ring's unit. Red marks the share of failed
/// requests or calls. Every empty bucket, idle or with calls that reported no
/// tokens, keeps the same one-pixel baseline in every chart.
function chart(stats, scope, showPeak = false, window = stats) {
  const model = scope === "model";
  const values = model ? stats.tokenCounts : stats.counts;
  const calls = stats.counts;
  const errors = stats.errorCounts;
  const { unit, format } = shareMeasure(scope);
  const n = values.length;
  if (!n) return "";
  const max = Math.max(1, ...values);
  const gap = 2;
  const w = 300;
  const h = 32;
  const bar = (w - gap * (n - 1)) / n;
  const start = window.start ?? window.windowStart;
  const end = window.end ?? window.windowEnd;
  const bucketMs = window.bucketMinutes * 60_000;
  const date = (ms) => new Date(ms).toLocaleString([], { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit", second: "2-digit" });
  let bars = "";
  values.forEach((v, i) => {
    const x = (i * (bar + gap)).toFixed(2);
    const bh = v ? Math.max(2.5, (v / max) * h) : 1;
    const title = model ? (v || !calls[i] ? `${v.toLocaleString("en-US")} tokens` : "No reported tokens") : `${v.toLocaleString("en-US")} requests`;
    const from = start + i * bucketMs;
    const to = Math.min(from + bucketMs, end);
    const range = Number.isFinite(from) && Number.isFinite(to) ? `${date(from)} – ${date(to)}\n` : "";
    bars += `<g class="chart-bar" data-tip="${esc(range + title)}" aria-label="${esc(range + title)}">`;
    bars += `<rect class="${v ? "" : "idle"}" x="${x}" y="${(h - bh).toFixed(2)}" width="${bar.toFixed(2)}" height="${bh.toFixed(2)}" rx="1"></rect>`;
    if (v && errors[i]) {
      const eh = (bh * errors[i]) / calls[i];
      bars += `<rect class="err" x="${x}" y="${(h - bh).toFixed(2)}" width="${bar.toFixed(2)}" height="${eh.toFixed(2)}" rx="1"></rect>`;
    }
    bars += `<rect class="chart-hit" x="${x}" y="0" width="${bar.toFixed(2)}" height="${h}"></rect></g>`;
  });
  const plot = `<svg class="chart" viewBox="0 0 ${w} ${h}" preserveAspectRatio="none">${bars}</svg>`;
  const peak = Math.max(0, ...values);
  // Without reported usage, there is no token peak to show.
  if (!showPeak || (model && !peak)) return plot;
  const index = values.indexOf(peak);
  const position = ((index * (bar + gap) + bar / 2) / w) * 100;
  // Align edge labels inward so the first/last bucket cannot clip the value.
  const shift = position < 20 ? "0" : position > 80 ? "-100%" : "-50%";
  return `<div class="traffic-chart">
    <span class="chart-peak" style="left:${position}%;transform:translateX(${shift})" title="Peak: ${peak.toLocaleString("en-US")} ${unit} per bar">${format(peak)}</span>
    ${peak ? `<span class="chart-peak-stem" style="left:${position}%" aria-hidden="true"></span>` : ""}
    ${plot}
  </div>`;
}

function rememberLocalTraffic(traffic, minutes, scope) {
  (ui.localTrafficViews ||= Object.create(null))[`${minutes}:${scope}`] = {
    ...traffic.summary, start: traffic.start, end: traffic.end, bucketMinutes: traffic.bucketMinutes, credentials: traffic.credentials, targets: traffic.targets, scope,
  };
}

async function loadHomeTraffic(force = false) {
  if (ui.homeTrafficLoading && !force) return;
  const request = ++ui.homeTrafficRequest;
  ui.homeTrafficLoading = true;
  // As on the Activity page, stats follow the category their data was read for.
  const scope = ui.trafficScope;
  const minutes = ui.homeTrafficMinutes;
  try {
    const traffic = await invoke("get_traffic", { minutes, scope });
    if (request !== ui.homeTrafficRequest) return;
    rememberLocalTraffic(traffic, minutes, scope);
    ui.homeTraffic = { ...traffic.summary, start: traffic.start, end: traffic.end, bucketMinutes: traffic.bucketMinutes, scope };
    ui.homeTrafficError = "";
    ui.homeTrafficFetchedAt = Date.now();
  } catch (error) {
    if (request !== ui.homeTrafficRequest) return;
    ui.homeTrafficError = String(error);
  } finally {
    if (request === ui.homeTrafficRequest) ui.homeTrafficLoading = false;
  }
  if (ui.page === "main" || ui.page === "devices") render();
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
     ${chart(st, st.scope)}
     <div class="strip">
       ${stat(st.scope === "model" ? "Calls" : "Requests", st.requests)}
       ${stat("Error Rate", rate, rateClass)}
       ${stat("Avg. Time", fmtMs(st.avgMs))}
       ${stat("Received", fmtBytes(st.bytes))}
     </div>${modelTokenStats(st, st.scope)}</div>`
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
    `${row("Claude Code", "Claude", s.urls.claude)}${row("Codex", "Codex", s.urls.codex)}
     <details class="disclosure" id="setup-snippets" ${ui.snippetsOpen ? "open" : ""}><summary>${ICON.chevron}Setup snippets</summary>
       ${snippet("~/.claude/settings.json, merged into existing settings", `{\n  "env": {\n    "ANTHROPIC_BASE_URL": "${s.urls.claude}"\n  }\n}`)}
       ${snippet("Top of ~/.codex/config.toml, then restart Codex. Codex 0.156+ requires an HTTPS chatgpt_base_url", `openai_base_url = "${s.urls.base}/v1"\nchatgpt_base_url = "${s.urls.chatgpt}"`)}
       ${snippet("CA for the HTTPS chatgpt_base_url: add to the system trust store, or set SSL_CERT_FILE in ~/.codex/.env", s.urls.caCertificate)}
     </details>`
  );
}

function snippet(label, code) {
  return `<div class="snippet"><div class="snippet-label">${label}</div><div class="snippet-code"><pre>${esc(code)}</pre>
    <button class="icon-btn" data-action="copy" data-text="${esc(code)}" aria-label="Copy snippet">${ICON.copy}</button></div></div>`;
}

function recentBlock() {
  const rows = ui.recent;
  const aside = rows.length ? `<button class="text-link" data-action="page" data-page="activity" data-target="log">View All</button>` : "";
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
          else if (state === "ok") value = probe.ms == null ? "Available" : `${probe.ms} ms`;
          else if (state === "error") value = `<span class="bad">Unreachable</span>`;
          else if (state === "unavailable") value = "Unverified";
          // Plain HTTP is the common case; other schemes stay visible.
          const shown = p.endpoint.replace(/^http:\/\//, "");
          const mark = flag(probe?.country);
          const exit =
            p.local && probe?.exitIp
              ? ` → ${mark ? `<span class="flag" data-tip="${esc(probe.country)}">${mark}</span> ` : ""}${esc(probe.exitIp)}`
              : "";
          return `<div class="row proxy">
            <span class="dot ${dot}" ${probe?.error ? `data-tip="${esc(probe.error)}"` : ""}></span>
            <span class="proxy-heading">${tag(p.name)}<span class="row-value">${value}</span></span>
            <button class="icon-btn" data-action="probe" data-name="${esc(p.name)}" data-tip="Test" aria-label="Test ${esc(p.name)}">${ICON.restart}</button>
            <span class="row-sub">${esc(shown)}${exit}</span>
            ${state === "ok" && p.routeFailures?.length ? `<span class="row-sub proxy-route-warning">Route unavailable: ${p.routeFailures.map(esc).join(", ")}</span>` : ""}
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

// Render account warnings and bar details outside the scrolling panel,
// keeping tooltips inside the viewport.
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
  }, owner.matches(".chart-bar") ? 100 : 500);
}

for (const type of ["pointerover", "focusin"]) {
  document.addEventListener(type, (event) => {
    const owner = event.target.closest?.(".route-warning, .credential-warning, .chart-bar");
    if (owner) showRouteTooltip(owner);
  });
}
for (const type of ["pointerout", "focusout"]) {
  document.addEventListener(type, (event) => {
    const owner = event.target.closest?.(".route-warning, .credential-warning, .chart-bar");
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
    const checked = r?.checkedAt ? `Last checked: ${new Date(r.checkedAt * 1000).toLocaleTimeString()}` : "";
    const badge = r && !r.ok
      ? `<span class="bad credential-warning" tabindex="0" data-tip="${esc(`${r.reason ?? ""}\n${checked}`)}">Credential warning</span>` : "";
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
  const accountError = s.accountStatesError ? `<div class="placeholder">Account status unavailable: ${esc(s.accountStatesError)}</div>` : "";
  const credentialError = s.credentialError ? `<div class="placeholder">Credential checks unavailable: ${esc(s.credentialError)}</div>` : "";
  return block("Routing", "", credentialError + accountError + section("Codex", d.codex) + section("Claude", d.claude));
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
  return `<span class="traffic-controls">${panelSelect(`${prefix}-scope`, [["all", "All"], ["model", "Models"]], ui.trafficScope,
    "Traffic request category", "traffic-scope", 'title="Model calls: each HTTP generation/compaction request and each generation within a WebSocket counts once, including active, failed and cancelled calls. CONNECT contents cannot be classified."')}${trafficRangeSelect(`${prefix}-range`, minutes, label)}</span>`;
}

async function loadTraffic() {
  // Long ranges can take longer to read than the refresh interval; a read of
  // the same range and category that is still current is not started twice.
  const key = `${ui.trafficMinutes}:${ui.trafficScope}`;
  if (ui.trafficLoading && ui.trafficLoadingKey === key && ui.trafficLoadingRequest === ui.trafficRequest) return;
  const request = ++ui.trafficRequest;
  ui.trafficLoading = true;
  ui.trafficLoadingKey = key;
  ui.trafficLoadingRequest = request;
  $("activity-traffic")?.setAttribute("aria-busy", "true");
  // Charts follow the category their data was read for, so a switch keeps
  // the previous charts consistent until the replacement arrives.
  const scope = ui.trafficScope;
  const minutes = ui.trafficMinutes;
  try {
    const traffic = await invoke("get_traffic", { minutes, scope });
    if (request !== ui.trafficRequest) return;
    rememberLocalTraffic(traffic, minutes, scope);
    ui.traffic = { ...traffic, scope };
    ui.trafficError = "";
  } catch (error) {
    if (request !== ui.trafficRequest) return;
    ui.trafficError = String(error);
  } finally {
    if (request === ui.trafficRequest) ui.trafficLoading = false;
  }
  if (ui.page === "activity") renderActivityTraffic();
  else if (ui.page === "devices") render();
}

function modelTokenStats(stats, scope) {
  if (scope !== "model") return "";
  const tokens = (n) => n == null ? "—" : n.toLocaleString();
  // Claude reports cache reads and writes apart from input; Input Tokens adds them.
  const parts = stats.service === "Claude" && stats.inputTokens != null
    ? [`Uncached input: ${tokens(stats.uncachedInputTokens)}`, `Cache writes: ${tokens(stats.cacheWriteTokens)}`, `Cache reads: ${tokens(stats.cachedInputTokens)} (Cached Input)`].join("\n")
    : "";
  const input = parts ? `Input Tokens<span class="info-tip stat-tip" data-tip="${esc(parts)}" aria-label="${esc(parts)}">${ICON.warning}</span>` : "Input Tokens";
  return `<div class="strip">${stat(input, tokens(stats.inputTokens))}${stat("Output Tokens", tokens(stats.outputTokens))}${stat("Cached Input", tokens(stats.cachedInputTokens))}${stat("Hit Rate", stats.cacheHitRate == null ? "—" : (100 * stats.cacheHitRate).toFixed(1) + "%")}</div>`;
}

const SHARE_SERVICES = ["Codex", "Claude"];
const SHARE_SHADES = 4;

/// Credentials grouped by service, each group in one hue. `measure` gives a
/// credential's amount, or null when it is unknown. Each credential takes a shade
/// while they fit; beyond that the largest keep theirs and at least two others
/// fold into "Others". Unidentified requests come last; Others and unidentified
/// are drawn without a hue.
function shareGroups(credentials, measure) {
  const sum = (list) => list.some((c) => measure(c) != null) ? list.reduce((total, c) => total + (measure(c) ?? 0), 0) : null;
  const used = credentials.filter((c) => c.requests > 0);
  const services = [...new Set(used.map((c) => c.service))];
  const rank = (service) => (SHARE_SERVICES.includes(service) ? SHARE_SERVICES.indexOf(service) : SHARE_SERVICES.length);
  services.sort((a, b) => rank(a) - rank(b) || a.localeCompare(b));
  return services.map((service) => {
    const own = used.filter((c) => c.service === service);
    const named = own.filter((c) => c.credential !== "Unidentified").sort((a, b) => (measure(b) ?? -1) - (measure(a) ?? -1));
    const hue = SHARE_SERVICES.includes(service) ? service.toLowerCase() : "other";
    const shown = named.length > SHARE_SHADES ? SHARE_SHADES - 1 : named.length;
    const slices = named.slice(0, shown).map((c, i) => ({ name: c.credential, value: measure(c), color: `--${hue}-${i + 1}` }));
    const rest = named.slice(shown);
    if (rest.length) {
      const members = rest.map((c) => ({ name: c.credential, value: measure(c) }));
      slices.push({ name: `Others (${rest.length})`, value: sum(rest), muted: true, members });
    }
    for (const c of own) if (c.credential === "Unidentified") slices.push({ name: c.credential, value: measure(c), muted: true, unidentified: true });
    return { service, value: sum(own), slices };
  });
}

/// A donut sector between fractions `from` and `to` of the turn, starting at
/// 12 o'clock, with rounded corners and a gap to its neighbors.
function ringSector(from, to, outer, inner, gap = 1.6, radius = 3) {
  const top = -Math.PI / 2;
  const halfGap = gap / outer / 2;
  const start = top + from * Math.PI * 2 + halfGap;
  const end = top + to * Math.PI * 2 - halfGap;
  const width = end - start;
  if (width <= 0.001) return "";
  // Shrink the corners so they fit narrow slices.
  const sine = Math.sin(Math.min(width / 2, Math.PI / 2));
  let corner = Math.min(radius, (outer - inner) / 2, (outer * sine) / (1 + sine));
  if (sine < 1) corner = Math.min(corner, (inner * sine) / (1 - sine));
  const at = (r, angle) => `${(outer + r * Math.cos(angle)).toFixed(3)} ${(outer + r * Math.sin(angle)).toFixed(3)}`;
  const large = width > Math.PI ? 1 : 0;
  if (corner < 0.25) {
    return `M ${at(outer, start)} A ${outer} ${outer} 0 ${large} 1 ${at(outer, end)} L ${at(inner, end)} A ${inner} ${inner} 0 ${large} 0 ${at(inner, start)} Z`;
  }
  const outerBeta = Math.asin(Math.min(1, corner / (outer - corner)));
  const innerBeta = Math.asin(Math.min(1, corner / (inner + corner)));
  return [
    `M ${at(outer, start + outerBeta)}`,
    `A ${outer} ${outer} 0 ${large} 1 ${at(outer, end - outerBeta)}`,
    `Q ${at(outer, end)} ${at(outer - corner, end)}`,
    `L ${at(inner + corner, end)}`,
    `Q ${at(inner, end)} ${at(inner, end - innerBeta)}`,
    `A ${inner} ${inner} 0 ${large} 0 ${at(inner, start + innerBeta)}`,
    `Q ${at(inner, start)} ${at(inner + corner, start)}`,
    `L ${at(outer - corner, start)}`,
    `Q ${at(outer, start)} ${at(outer, start + outerBeta)}`,
    "Z",
  ].join(" ");
}

/// Input plus output tokens, or null when neither was reported. Input already
/// counts the whole prompt, cached tokens included, for every service.
function shareTokens(c) {
  return c.inputTokens == null && c.outputTokens == null ? null : (c.inputTokens ?? 0) + (c.outputTokens ?? 0);
}

/// Model calls are measured in tokens, other traffic in requests.
function shareMeasure(scope) {
  if (scope !== "model") return { unit: "requests", measure: (c) => c.requests, format: (n) => n.toLocaleString("en-US") };
  const compact = new Intl.NumberFormat("en-US", { notation: "compact", maximumFractionDigits: 1 });
  return {
    unit: "tokens",
    measure: shareTokens,
    format: (n) => compact.format(n),
  };
}

/// Each credential's part of the traffic in the range, as a ring and a legend
/// grouped by service.
function trafficShare(credentials, scope) {
  const { unit, measure, format } = shareMeasure(scope);
  // Unidentified model calls were rejected before reaching an upstream, so they
  // report no tokens; Models leaves them out, with any service left empty.
  const groups = shareGroups(credentials, measure)
    .map((g) => (scope === "model" ? { ...g, slices: g.slices.filter((s) => !s.unidentified || s.value > 0) } : g))
    .filter((g) => g.slices.length);
  const total = groups.reduce((sum, g) => sum + (g.value ?? 0), 0);
  if (!total) return "";
  const size = 84;
  const value = (n) => (n == null ? "—" : format(n));
  // Unknown and zero amounts stay in the legend but take no slice.
  const slices = groups.flatMap((g) => g.slices.filter((s) => s.value > 0).map((s) => ({ ...s, service: g.service })));
  // Every slice keeps a visible minimum width, so tiny parts stay hoverable.
  const widths = slices.map((s) => Math.max(s.value / total, 0.025));
  const turn = widths.reduce((sum, w) => sum + w, 0);
  const color = (s) => (s.muted ? "var(--track)" : `var(${s.color})`);
  const amount = (n) => (n == null ? "no reported usage" : `${n.toLocaleString("en-US")} ${unit}`);
  // Others lists every configuration it groups, one per line.
  const label = (s) => [`${s.service} · ${s.name}: ${amount(s.value)}`, ...(s.members ?? []).map((m) => `${m.name}: ${amount(m.value)}`)].join("\n");
  let cursor = 0;
  const paths = slices.map((s, i) => {
    const from = cursor;
    cursor += widths[i] / turn;
    return `<path style="--slice:${color(s)}" d="${ringSector(from, cursor, size / 2, (size / 2) * 0.618)}"><title>${esc(label(s))}</title></path>`;
  }).join("");
  const legend = groups.map((g) => `<div class="share-group">
    <div class="share-service" title="${esc(label({ service: g.service, name: "Total", value: g.value }))}">${serviceMark(g.service)}<span>${esc(g.service)}</span><span>${value(g.value)}</span></div>
    ${g.slices.map((s) => `<div class="share-item${s.muted ? " muted" : ""}" title="${esc(label({ ...s, service: g.service }))}">
      <i style="--slice:${color(s)}"></i><span>${esc(s.name)}</span><span>${value(s.value)}</span></div>`).join("")}</div>`).join("");
  return `<div class="traffic-share">
    <div class="share-ring" title="${esc(`${total.toLocaleString("en-US")} ${unit}`)}"><svg viewBox="0 0 ${size} ${size}" aria-hidden="true">${paths}</svg>
      <div class="share-total"><strong>${format(total)}</strong><span>${unit}</span></div></div>
    <div class="share-legend">${legend}</div></div>`;
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
function trafficReview(c, targets = [], scope = ui.traffic?.scope) {
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
    ui.trafficError = String(error);
    ui.mergedDataError = String(error);
    if (ui.page === "devices") render();
    else renderActivityTraffic();
    return;
  }
  ui.localTrafficViews = Object.create(null);
  ui.deviceTrafficViews = Object.create(null);
  await Promise.all([loadTraffic(), loadMergedData(true), refresh()]);
}

function renderActivityTraffic() {
  if (openSelect) { selectRenderPending = true; return; }
  closeSelect();
  const el = $("activity-traffic");
  if (!el) return;
  // While the Log is at or above the top, Traffic changing height must not move it.
  const logBefore = logOffset();
  const keepLog = logBefore != null && logBefore <= 0.5 && $("content").scrollTop > 0;
  const traffic = ui.traffic;
  const scope = traffic?.scope ?? ui.trafficScope;
  el.setAttribute("aria-busy", String(ui.trafficLoading));
  const date = (ms) => new Date(ms).toLocaleString([], { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" });
  const rows = traffic?.credentials.map((c) => {
    const rate = c.requests ? (100 * c.errors / c.requests).toFixed(1) + "%" : "—";
    return `<div class="credential-traffic">
      <div class="traffic-identity"><span class="traffic-service">${serviceMark(c.service)}${esc(c.service)}</span><strong>${esc(c.credential)}</strong>${trafficReview(c, traffic.targets)}</div>
      ${chart(c, scope, true, traffic)}
      <div class="strip">${stat(scope === "model" ? "Calls" : "Requests", c.requests)}${stat("Error Rate", rate, c.errors ? "bad" : "")}${stat("Avg. Time", fmtMs(c.avgMs))}${stat("Received", fmtBytes(c.bytes))}</div>${modelTokenStats(c, scope)}
    </div>`;
  }).join("");
  const reviews = traffic?.credentials.reduce((n, c) => n + (c.sources ?? []).filter((s) => s.reason).length, 0) ?? 0;
  const notes = [
    "The ring shows each credential's requests, or with Models its input plus output tokens, grouped by service; a service with more than four shows its three largest and groups the rest as Others.",
    "Sorted by received traffic, largest first. Bars show requests, or with Models input plus output tokens; red indicates the share that failed, and each chart uses its own scale.",
    `${scope === "model" ? "Each HTTP model request or WebSocket generation counts once, including active calls. Tokens are reported usage, not billing totals; missing usage is not treated as zero. Input is the whole prompt, cached input included; for Claude it adds cache reads and writes to the reported input, as OpenAI reports it. Hit rate is cached input over all input tokens." : "Each HTTP request or tunnel connection counts once, including active connections."} Bytes and duration update when the call or connection ends.`,
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
    ${trafficControls("traffic", ui.trafficMinutes, "Traffic time range")}</div>
    ${ui.trafficError && traffic ? `<span class="traffic-update-error" title="${esc(ui.trafficError)}">Update failed</span>` : ""}
    ${!traffic ? `<div class="placeholder">${esc(ui.trafficError || "Loading traffic…")}</div>` : `${rows ? trafficShare(traffic.credentials, scope) + rows : '<div class="placeholder">No requests in this time range.</div>'}<div class="traffic-axis"><span>${esc(date(traffic.start))}</span><span>${bucketLabel} per bar</span><span>${esc(date(traffic.end))}</span></div>`}`;
  if (ui.pinLog) scrollToLog();
  else if (keepLog) $("content").scrollTop += logOffset() - logBefore;
  queueFit();
}

const SEARCH_MODES = [["keyword", "Keyword"], ["path", "Path"], ["proxy", "Proxy"], ["status", "Status"], ["credential", "Credential"]];
function searchPlaceholder() {
  return { keyword: "Search log fields", path: "Path contains…", proxy: "Exact proxy name or direct", status: "HTTP status, e.g. 429", credential: "Account, API Key or settings" }[ui.searchMode];
}

const RANGE_PRESETS = [["hour", "Last hour"], ["6h", "Last 6 hours"], ["today", "Today"], ["yesterday", "Yesterday"]];

/// Sets the Activity range from its preset, relative to the current time.
function resolveActivityRange() {
  if (ui.activityPreset === "custom" && ui.activityTo != null) return;
  const now = currentMinute();
  const midnight = new Date(now);
  midnight.setHours(0, 0, 0, 0);
  const yesterday = new Date(midnight);
  yesterday.setDate(yesterday.getDate() - 1);
  const hour = 60 * 60 * 1000;
  [ui.activityFrom, ui.activityTo] = {
    "6h": [now - 6 * hour, now],
    today: [midnight.getTime(), now],
    // `to` is an inclusive minute: the last minute before midnight.
    yesterday: [yesterday.getTime(), midnight.getTime() - 60 * 1000],
  }[ui.activityPreset] ?? [now - hour, now];
}

function rangeLabel() {
  const preset = RANGE_PRESETS.find(([key]) => key === ui.activityPreset);
  if (preset) return preset[1];
  const day = (ms) => new Date(ms).toDateString() === new Date().toDateString()
    ? "Today" : new Date(ms).toLocaleDateString("en-US", { month: "short", day: "numeric" });
  const time = (ms) => new Date(ms).toLocaleTimeString("en-GB", { hour: "2-digit", minute: "2-digit" });
  const from = `${day(ui.activityFrom)} ${time(ui.activityFrom)}`;
  return day(ui.activityFrom) === day(ui.activityTo)
    ? `${from} – ${time(ui.activityTo)}` : `${from} – ${day(ui.activityTo)} ${time(ui.activityTo)}`;
}

function renderActivityRange() {
  const el = $("activity-range");
  if (!el) return;
  const items = [
    ...RANGE_PRESETS.map(([key, label]) => ({ label, action: "activity-preset", target: key, checked: ui.activityPreset === key })),
    null,
    { label: "Custom Range…", action: "range-edit", checked: ui.activityPreset === "custom" },
  ];
  el.innerHTML = `<button type="button" class="select range-select" data-menu="${esc(JSON.stringify(items))}"
    aria-haspopup="menu" aria-expanded="false" aria-label="Log time range">${esc(rangeLabel())}</button>`;
}

function showRangeEditor(open) {
  const editor = $("range-editor");
  if (!editor) return;
  editor.hidden = !open;
  if (!open) return;
  $("activity-from").value = localInput(ui.activityFrom);
  $("activity-to").value = localInput(ui.activityTo);
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
  ui.activityPreset = "custom";
  ui.activityFrom = from;
  ui.activityTo = to;
  showRangeEditor(false);
  renderActivityRange();
  await loadActivity();
}

function activityShell() {
  return `<div class="page">
    <section class="block" id="activity-traffic"></section>
    <section class="block log-block">
      <div class="block-head"><span class="block-title">Log</span><span class="block-aside" id="activity-range"></span></div>
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
        ${panelSelect("search-mode", SEARCH_MODES, ui.searchMode, "Search condition")}
        <label class="search">${ICON.search}
          <input class="field" id="search" type="search" spellcheck="false" aria-label="Search logs" placeholder="${esc(searchPlaceholder())}" value="${esc(ui.search)}" /></label>
      </div>
      <div class="segmented" id="filters"></div>
      <div id="activity-list"></div>
    </section>
  </div>`;
}

function renderActivityList() {
  const filters = $("filters");
  if (!filters) return;
  renderActivityRange();
  filters.innerHTML = [
    ["requests", "Requests"],
    ["models", "Models"],
    ["errors", "Errors"],
    ["all", "All Events"],
  ]
    .map(([f, label]) => `<button aria-pressed="${ui.filter === f}" data-action="filter" data-filter="${f}">${label}</button>`)
    .join("");
  $("activity-list").innerHTML = ui.activityError
    ? `<div class="placeholder">${esc(ui.activityError)}</div>`
    : ui.rows.length
      ? ui.rows.map((e) => requestRow(e, true) + (ui.expanded.has(e.seq) ? detail(e) : "")).join("")
        + (ui.activityNext ? '<div class="list-more"><button type="button" class="btn" data-action="activity-more">Load More</button></div>' : "")
      : '<div class="placeholder">No matching entries in this range.</div>';
  queueFit();
}

function requestRow(e, expandable) {
  const deviceQuery = ["device_statistics_succeeded", "device_statistics_failed", "device_statistics_recovered"].includes(e.event);
  const queryDevice = deviceQuery && ui.devices.find(device => device.id === e.fields?.device_id);
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
  return `<${tag} class="req" ${attrs}>
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

// ---------------------------------------------------------------- devices

function selectDeviceTraffic() {
  const cached = ui.deviceTrafficViews?.[`${ui.deviceTrafficMinutes}:${ui.trafficScope}`];
  if (!cached) return false;
  ui.mergedData = cached;
  return true;
}
// This device's statistics are local, so a range switch can show them while
// a remote refresh is still collecting peers. Errors leave the placeholder.
async function loadLocalDeviceTraffic() {
  const minutes = ui.deviceTrafficMinutes || 30, scope = ui.trafficScope || "model";
  try { rememberLocalTraffic(await invoke("get_traffic", { minutes, scope }), minutes, scope); }
  catch { return; }
  if (ui.page === "devices") render();
}
async function loadMergedData(force = false, refreshRemote = false) {
  if (ui.mergedDataLoading && !force) return;
  const request = ++ui.mergedDataRequest;
  ui.mergedDataLoading = true;
  ui.mergedDataRemote = refreshRemote;
  let completed = false;
  const requestedDevices = ui.devices ? ui.devices.map(d => ({ id: d.id, name: d.name })) : [];
  try {
    const onUpdate = new Channel();
    onUpdate.onmessage = data => {
      if (completed || request !== ui.mergedDataRequest) return;
      relabelDeviceViews(data, requestedDevices, ui.devices || []);
      ui.deviceTrafficViews ||= Object.create(null);
      for (const view of data) ui.deviceTrafficViews[`${view.minutes}:${view.scope}`] = view;
      selectDeviceTraffic();
      // Statuses and errors stay visible while another range waits for peers.
      ui.mergedDataLatest = ui.mergedData || data[0];
      ui.mergedDataError = ""; ui.mergedDataUpdating = false;
      if (ui.page === "devices" || ui.page === "settings") render();
    };
    const data = await invoke("get_merged_data", {
      minutes: ui.deviceTrafficMinutes || 30, scope: ui.trafficScope || "model", onUpdate, refreshRemote,
    });
    if (request !== ui.mergedDataRequest) return;
    if (Array.isArray(data)) {
      relabelDeviceViews(data, requestedDevices, ui.devices || []);
      ui.deviceTrafficViews = Object.fromEntries(data.map(view => [`${view.minutes}:${view.scope}`, view]));
      selectDeviceTraffic();
    } else { ui.mergedData = data; }
    ui.mergedDataLatest = ui.mergedData || ui.deviceTrafficViews?.["30:model"] || ui.mergedDataLatest;
    if (refreshRemote) ui.deviceTrafficFetchedAt = Date.now();
    ui.mergedDataError = ""; ui.mergedDataUpdating = false;
  } catch (e) {
    if (request === ui.mergedDataRequest) {
      // A cache read retries on its own; a refresh needs the user to press Refresh again.
      const skew = String(e).includes("TRAFFIC_UPDATING");
      ui.mergedDataUpdating = skew && !refreshRemote;
      ui.mergedDataError = skew && refreshRemote ? "Devices reported different time boundaries. Press Refresh to try again." : ui.mergedDataUpdating ? "" : String(e);
    }
  }
  finally { completed = true; if (request === ui.mergedDataRequest) { ui.mergedDataLoading = ui.mergedDataRemote = false; if (ui.page === "devices" || ui.page === "settings") render(); } }
}
function deviceTrafficContent(traffic, scope, detailsKey = "merged", window = traffic) {
  const rate = traffic.requests ? (100 * traffic.errors / traffic.requests).toFixed(1) + "%" : "—";
  let body = `<div class="traffic-content" aria-busy="${ui.mergedDataLoading}">
    ${trafficShare(traffic.credentials, scope)}${chart(traffic, scope, false, window)}
    <div class="strip">${stat(scope === "model" ? "Calls" : "Requests", traffic.requests)}${stat("Error Rate", rate, traffic.errors ? "bad" : "")}${stat("Avg. Time", fmtMs(traffic.avgMs))}${stat("Received", fmtBytes(traffic.bytes))}</div>
    ${modelTokenStats(traffic, scope)}</div>`;
  const pendingReviews = traffic.credentials.reduce((n, c) => n + (c.sources ?? []).filter(s => s.reason).length, 0);
  if (pendingReviews) body += `<div class="placeholder">${ICON.warning} Renamed or changed local configurations need review. Open Upstream accounts and choose where their traffic belongs.</div>`;
  if (traffic.credentials.length) body += `<details class="disclosure" data-device-traffic="${esc(detailsKey)}" ${ui.deviceTrafficOpen?.[detailsKey] ? "open" : ""}><summary>${ICON.chevron}Upstream accounts</summary>${traffic.credentials.map(c => `<div class="credential-traffic">
    <div class="traffic-identity"><span class="traffic-service">${serviceMark(c.service)}${esc(c.service)}</span><strong>${esc(c.credential)}</strong>${trafficReview(c, traffic.targets, scope)}</div>
    ${chart(c, scope, true, window)}<div class="strip">${stat(scope === "model" ? "Calls" : "Requests", c.requests)}${stat("Error Rate", c.requests ? (100 * c.errors / c.requests).toFixed(1) + "%" : "—")}${stat("Avg. Time", fmtMs(c.avgMs))}${stat("Received", fmtBytes(c.bytes))}</div>${modelTokenStats(c, scope)}</div>`).join("")}</details>`;
  return body;
}
document.addEventListener("toggle", event => {
  const details = event.target;
  if (!details.isConnected || !details.matches?.("details[data-device-traffic]")) return;
  (ui.deviceTrafficOpen ||= Object.create(null))[details.dataset.deviceTraffic] = details.open;
}, true);
function mergedDataBlock() {
  const data = ui.mergedData;
  const range = trafficControls("devices-traffic", ui.deviceTrafficMinutes, "Device traffic time range");
  let body = `<div class="devices-traffic-range">${range}</div>`;
  if (ui.mergedDataUpdating) body += `<div class="placeholder">Updating traffic…</div>`;
  if (ui.mergedDataError) body += message("bad", esc(ui.mergedDataError));
  if (!data && ui.mergedDataLoading) body += `<div class="placeholder">Loading traffic…</div>`;
  if (data) {
    body += deviceTrafficContent(data.traffic, data.scope, "merged", data);
    for (const source of data.sources) {
      if (source.error === "TRAFFIC_UPDATING") body += `<div class="placeholder">${esc(source.name)}: Loading traffic…</div>`;
      else if (source.error) body += message("warn", `${esc(source.name)}: ${esc(source.error)}`);
      else if (source.exclusion === "duplicate") body += `<div class="placeholder">${esc(source.name)}: Already counted via another entry.</div>`;
    }
  }
  return block("Merged Traffic", `<button class="text-link" data-action="merged-refresh" ${ui.mergedDataLoading && ui.mergedDataRemote ? "disabled" : ""}>Refresh</button>`, body);
}
function deviceConnectionKey(devices) {
  return JSON.stringify(devices.map(d => {
    const transport = d.data?.transport || (d.ssh ? "ssh" : "http");
    return [d.id, transport, transport === "ssh"
      ? [d.ssh?.host, d.ssh?.binary === "coportd" ? "" : d.ssh?.binary || ""]
      : [d.data?.url, d.data?.tokenFile, d.data?.tokenEnv, d.data?.caCertificate]];
  }).sort((a,b) => String(a[0]).localeCompare(String(b[0]))));
}
function relabelDeviceViews(views, previous, current) {
  const renamed = new Map(previous.map(d => [d.name, current.find(c => c.id === d.id)?.name || d.name]));
  for (const view of new Set(views)) {
    for (const source of view?.sources || []) source.name = renamed.get(source.name) || source.name;
  }
}
async function loadDeviceDefinitions() {
  let changed = false;
  try {
    const devices = await invoke("get_devices");
    changed = ui.devicesLoaded && deviceConnectionKey(devices) !== deviceConnectionKey(ui.devices);
    if (changed) {
      ++ui.mergedDataRequest; ui.mergedDataLoading = false;
      ui.deviceTrafficViews = Object.create(null); ui.mergedData = ui.mergedDataLatest = null;
    } else {
      relabelDeviceViews([...Object.values(ui.deviceTrafficViews || {}), ui.mergedData], ui.devices, devices);
    }
    ui.devices = devices; ui.devicesLoaded = true;
  }
  catch (error) { toast(String(error)); }
  if (ui.page === "settings") render();
  return changed;
}
async function checkConfiguredSsh() {
  if (ui.sshStartupCheckStarted) return;
  ui.sshStartupCheckStarted = true;
  await loadDeviceDefinitions();
  const pending = ui.devices.filter(device => device.ssh);
  ui.deviceStates ||= Object.create(null);
  const update = () => { if (ui.snap && (ui.page === "settings" || ui.page === "devices")) render(); };
  for (const device of pending) {
    ui.deviceStates[device.id] = { host: device.ssh.host, state: "warn", label: "Checking SSH…" };
  }
  update();
  // Bound startup connections and check each configured device only once.
  await Promise.all(Array.from({ length: Math.min(4, pending.length) }, async () => {
    while (pending.length) {
      const device = pending.shift();
      const result = ui.deviceStates[device.id];
      try {
        await invoke("check_ssh_device", { id: device.id });
        result.state = "running"; result.label = "SSH reachable at startup";
      } catch (error) {
        result.state = "failed"; result.label = `Startup SSH check failed: ${String(error)}`;
      }
      update();
    }
  }));
}
async function loadDevices(refreshRemote = false) {
  // A visit arriving during another load (e.g. from Settings) runs afterwards,
  // so the first Devices visit still starts its refresh.
  if (ui.deviceRefresh) { ui.deviceRefreshQueued = true; return; }
  ui.deviceRefresh = true;
  try {
    await loadDeviceDefinitions();
    // Forwarding status comes only from the local daemon; read it alongside
    // statistics instead of delaying the first traffic frame behind it.
    const local = Promise.all([
      invoke("get_device_forwarders").then(forwarders => { ui.deviceForwarders = forwarders; }),
      loadDeviceCapabilities(),
    ]);
    // Remote statistics must not start when Settings is open or the user has left Devices.
    if (ui.page !== "devices") { await local; return; }
    if (!(ui.localTrafficViews?.[`${ui.deviceTrafficMinutes || 30}:${ui.trafficScope || "model"}`])
        && (ui.deviceTrafficMinutes || 30) === ui.homeTrafficMinutes) loadHomeTraffic();
    // The first Devices visit after start reads remote statistics once; later
    // visits and timers use the cache until the user presses Refresh.
    const remote = refreshRemote || !ui.deviceStartupRefreshDone;
    ui.deviceStartupRefreshDone = true;
    await Promise.all([local, loadMergedData(remote, remote)]);

  } catch (error) { toast(String(error)); }
  finally {
    ui.deviceRefresh = false;
    if (ui.page === "devices" || ui.page === "settings") render();
    if (ui.deviceRefreshQueued) { ui.deviceRefreshQueued = false; if (ui.page === "devices") loadDevices(); }
  }
}

async function loadDeviceCapabilities() {
  try {
    const values = await invoke("get_device_capabilities");
    ui.deviceCapabilities = Object.fromEntries(values.map(value => [value.deviceId, value]));
    ui.deviceCapabilitiesError = "";
  } catch (error) { ui.deviceCapabilities = {}; ui.deviceCapabilitiesError = String(error); }
}
function deviceCapabilities(device) {
  if (!device.ssh) return "";
  const entry = ui.deviceCapabilities?.[device.id];
  const caps = entry?.capabilities;
  const error = entry?.error || ui.deviceCapabilitiesError;
  const text = error ? "Version unavailable" : entry?.pending ? "Checking…" : caps ? `Coport ${esc(caps.version)}${caps.runningVersion && caps.runningVersion !== caps.version ? ` · Running ${esc(caps.runningVersion)}` : ""} · Statistics ${caps.statistics ? "✓" : "✕"} · Forwarding ${caps.forwarding ? "✓" : "✕"}${caps.running ? caps.forwardingAvailable ? "" : " · Unavailable" : " · Offline"}` : "Checking…";
  return `<div class="row"><span class="row-label setting-description"${error ? ` data-tip="${esc(error)}"` : ""}>${text}</span></div>`;
}
async function refreshForwardingStatus() {
  try {
    ui.deviceForwarders = await invoke("get_device_forwarders");
    await loadDeviceCapabilities();
    if (ui.snap) ui.snap.forwarding = ui.deviceForwarders[0] || null;
    if (ui.page === "settings") render();
  } catch (error) { toast(String(error)); }
}
function forwardingSettings() {
  const devices = ui.devices.filter(device => device.ssh);
  const rows = devices.map(device => {
    const status = (ui.deviceForwarders || []).find(item => item.deviceId === device.id);
    const busy = ui.deviceForwardingBusy === device.id;
    const caps = ui.deviceCapabilities?.[device.id]?.capabilities;
    const unavailable = !status && !(caps?.forwarding && caps.forwardingAvailable);
    return `<div class="row"><span class="row-label">${esc(device.name)}</span><button class="switch" role="switch" aria-checked="${!!status}" aria-busy="${busy}" aria-label="Forward requests through ${esc(device.name)}" data-action="device-forward" data-id="${esc(device.id)}" data-enabled="${!status}" ${ui.deviceForwardingBusy || unavailable ? "disabled" : ""}></button></div>` + (status?.error ? message("warn", esc(status.error)) : "");
  }).join("");
  const restore = `<div class="row"><span class="row-label">Restore on startup</span><button class="switch" role="switch" aria-checked="${!!ui.snap.forwardingRestore}" aria-label="Restore forwarding on startup" data-action="forwarding-restore" ${ui.forwardingRestoreBusy || !ui.snap.forwardingRestoreSupported ? "disabled" : ""}></button></div>`;
  const hint = !devices.length ? "Add an SSH device above." : !ui.snap.forwardingRestoreSupported ? "Update the local proxy to enable startup restore." : "";
  return block("Request forwarding", "", rows + restore + (ui.snap.forwardingRestoreError ? message("warn", esc(ui.snap.forwardingRestoreError)) : "") + (hint ? `<div class="placeholder">${hint}</div>` : ""));
}
function devicesPage() {
  const local = ui.snap.phase;
  const data = ui.mergedData;
  const localTraffic = data?.local || ui.localTrafficViews?.[`${ui.deviceTrafficMinutes || 30}:${ui.trafficScope || "model"}`];
  const localScope = data?.scope || ui.trafficScope || "model";
  let cards = block("This Device", "",
    `<div class="row"><span class="row-label">127.0.0.1:${local.port}</span><span class="state"><span class="dot ${esc(local.state)}"></span>${local.busy ? "Updating…" : { running: "Running", stopped: "Stopped", failed: "Status unavailable" }[local.state] ?? "Unknown"}</span></div>` +
    (local.error ? message("warn", esc(local.error)) : "") +
    (localTraffic ? `<div class="block-head"><span class="block-title">Traffic</span></div>${deviceTrafficContent(localTraffic, localScope, "local", data || localTraffic)}` : `<div class="placeholder">Loading traffic…</div>`));
  for (const device of ui.devices) {
    const source = (data || ui.mergedDataLatest)?.sources.find(source => source.name === device.name);
    const traffic = data && source?.traffic;
    const ssh = device.data?.transport === "ssh" || !device.data;
    const protocol = ssh ? "SSH" : device.data.url.startsWith("https:") ? "HTTPS" : "HTTP";
    const status = deviceConnectionStatus(device);
    cards += block(esc(device.name), "",
      `<div class="row"><span class="row-label selectable">${esc(device.data?.transport === "ssh" || !device.data ? device.ssh?.host : device.data.url)}</span><span class="state"><span class="dot ${status.state}" role="img" aria-label="${esc(status.label)}" data-tip="${esc(status.label)}"></span>${protocol}</span></div>` +
      (source?.error && source.error !== "TRAFFIC_UPDATING" ? message("warn", esc(source.error)) : "") +
      (traffic ? `<div class="block-head"><span class="block-title">Traffic</span></div>${deviceTrafficContent(traffic, data.scope, `device/${device.id}`, data)}` : `<div class="placeholder">${source?.error && source.error !== "TRAFFIC_UPDATING" ? "Traffic unavailable" : source?.exclusion === "duplicate" ? "Already counted via another entry." : ui.mergedDataLoading && (ui.mergedDataRemote || !data) ? "Loading traffic…" : "Not refreshed"}</div>`));
  }
  return mergedDataBlock() + cards;
}
function deviceConnectionStatus(device) {
  const age = Date.now() - (ui.deviceTrafficFetchedAt || 0);
  const source = (ui.mergedData || ui.mergedDataLatest || ui.deviceTrafficViews?.["30:model"])?.sources?.find(s => s.name === device.name);
  if (source?.error && source.error !== "TRAFFIC_UPDATING") return { state: "failed", label: String(source.error) };
  if (source?.error === "TRAFFIC_UPDATING") return { state: "warn", label: "Loading traffic…" };
  if (source?.exclusion === "duplicate") return { state: "warn", label: "Statistics available; already counted via another entry." };
  if (source?.included) return { state: "running", label: age < 0 || age > 45000 || ui.mergedDataError ? "Last statistics refresh succeeded (cached)" : "Read-only statistics available" };
  if (!source || source.exclusion === "not_refreshed") {
    // Any known failure outranks an older success.
    const startup = ui.deviceStates?.[device.id];
    const current = startup && startup.host === device.ssh?.host && startup.state !== "warn" ? startup : null;
    const cap = ui.deviceCapabilities?.[device.id];
    if (cap?.error) return { state: "failed", label: `SSH check failed: ${cap.error}` };
    if (current?.state === "failed") return current;
    if (cap?.capabilities) return { state: "running", label: "SSH capability check succeeded (cached)" };
    if (current) return current;
    return ui.mergedDataRemote ? { state: "warn", label: "Refreshing statistics…" } : { state: "", label: "Not checked" };
  }
  return { state: "warn", label: "Statistics not included" };
}
function deviceSettings() {
  const d = ui.deviceDraft || { transport: "ssh" };
  const rows = ui.devices.map(device => {
    const status = deviceConnectionStatus(device);
    return `<div class="row device-setting-row"><span class="device-setting-name row-label"><span class="dot ${status.state}" role="img" aria-label="${esc(status.label)}" data-tip="${esc(status.label)}"></span>${esc(device.name)}<span class="device-setting-transport">${device.data?.transport === "ssh" || !device.data ? "SSH" : "HTTP"}</span></span><span class="row-value device-setting-actions"><button class="text-link" data-action="device-edit" data-id="${esc(device.id)}">Edit</button><button class="icon-btn" data-action="device-remove" data-id="${esc(device.id)}" data-tip="Remove device" aria-label="Remove ${esc(device.name)}">${ICON.trash}</button></span></div>` + deviceCapabilities(device);
  }).join("");
  const form = ui.deviceFormOpen ? `<div class="device-form">
    <label>Connection<select class="field" id="device-transport"><option value="ssh" ${d.transport === "ssh" ? "selected" : ""}>SSH</option><option value="http" ${d.transport !== "ssh" ? "selected" : ""}>HTTP / HTTPS</option></select></label>
    <label>Name (optional)<input class="field" id="device-name" value="${esc(d.name || "")}" maxlength="128" placeholder="Defaults to the SSH config name or HTTP origin"></label>
    ${d.transport === "ssh" ? `
      <label>SSH config name or host<input class="field" id="device-host" value="${esc(d.host || "")}" maxlength="255" placeholder="mbp16"></label>
      <label>Remote executable (optional; discovered automatically)<input class="field" id="device-binary" value="${esc(d.binary === "coportd" ? "" : d.binary || "")}" maxlength="4096" placeholder="Automatic discovery"></label>
    ` : `
      <label>Data API origin<input class="field" id="device-url" value="${esc(d.url || "")}" placeholder="http://LAN-IP:8788 or https://host:8788"></label>
      <label>Local access key file<input class="field" id="device-key-file" value="${esc(d.key || "")}" placeholder="/absolute/path/to/data.key"></label>
      <label>Access key environment variable (alternative)<input class="field" id="device-key-env" value="${esc(d.env || "")}" placeholder="Use either a file or an environment variable"></label>
      <label>CA certificate (optional)<input class="field" id="device-ca-file" value="${esc(d.ca || "")}" placeholder="/absolute/path/to/ca.pem"></label>
    `}
    <div class="message-actions"><button class="btn primary" data-action="device-save" aria-label="Save device">Save</button><button class="btn" data-action="device-cancel">Cancel</button></div>
  </div>` : "";
  return block("Devices", `<button class="icon-btn" data-action="device-new" data-tip="Add device" aria-label="Add device">${ICON.plus}</button>`, rows + form);
}
function captureDeviceDraft() {
  if (!$("device-name")) return;
  const d = ui.deviceDraft || {};
  ui.deviceDraft = { ...d, name: $("device-name").value, transport: $("device-transport").value,
    host: $("device-host")?.value ?? d.host ?? "", binary: $("device-binary")?.value ?? d.binary ?? "",
    url: $("device-url")?.value ?? d.url ?? "", key: $("device-key-file")?.value ?? d.key ?? "",
    env: $("device-key-env")?.value ?? d.env ?? "", ca: $("device-ca-file")?.value ?? d.ca ?? "" };
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
         <button class="switch" role="switch" aria-checked="${set.launchAtLogin}" data-action="launch-at-login" aria-label="Launch at login"></button></div>
       <div class="row"><span class="row-label">Start proxy when the app opens</span>
         <button class="switch" role="switch" aria-checked="${set.startProxyOnLaunch}" data-action="auto-start" aria-label="Start proxy when the app opens"></button></div>
       <div class="row"><span class="row-label">Keep proxy running after quit<span class="setting-description" id="keep-running-description">Quit exits the app; the proxy keeps running.<br>Reopen to manage or stop the proxy.</span></span>
         <button class="switch" role="switch" aria-checked="${set.keepProxyRunningOnQuit}" data-action="keep-running" aria-label="Keep proxy running after quit" aria-describedby="keep-running-description"></button></div>
       <div class="row"><span class="row-label">Appearance</span>
         ${panelSelect("appearance", [["System", "System"], ["Light", "Light"], ["Dark", "Dark"]], set.appearance, "Appearance", "", 'data-setting="appearance"')}</div>`
    )}
    ${configSettings()}
    ${filesBlock()}
    ${deviceSettings()}
    ${forwardingSettings()}`;
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
    const note = description ? `<span class="setting-description" id="${id}-description">${description}</span>` : "";
    return `<div class="row"><span class="row-label">${label}${note}</span>
      <span class="number-field"><input class="field" id="${id}" data-config="${key}" inputmode="decimal" spellcheck="false"
        value="${esc(String(v[key]))}" aria-label="${label}"${description ? ` aria-describedby="${id}-description"` : ""} /><span class="unit">${unit}</span></span></div>`;
  };
  const flag = (key, label, description, separateNote = false) => {
    const id = `config-${key.replaceAll(".", "-")}`;
    const note = `<span class="setting-description" id="${id}-description">${description}</span>`;
    return `<div class="row${separateNote ? " config-action-row" : ""}"><span class="row-label">${label}${separateNote ? "" : note}</span>
      <button class="switch" role="switch" aria-checked="${v[key]}" data-action="config-flag" data-key="${key}" aria-label="${label}" aria-describedby="${id}-description"></button>${separateNote ? note : ""}</div>`;
  };
  return `${block(
    "Network",
    "",
    `${field("listen_port", "Listen port")}
     ${flag("allow_external_access", "Share statistics", "Enable the separate read-only data API. Configure external_data in YAML first; restart to apply.", true)}
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
  if (routeTooltipOwner && !routeTooltipOwner.isConnected) hideRouteTooltip();
  const page = $("content").firstElementChild;
  if (page === observedPage) return;
  if (observedPage) fitObserver.unobserve(observedPage);
  if (page) fitObserver.observe(page);
  observedPage = page;
}).observe($("content"), { childList: true, subtree: true });

// ---------------------------------------------------------------- actions

async function act(action, el) {
  const s = ui.snap;
  switch (action) {
    case "page":
      ui.page = el.dataset.page;
      if (ui.page === "activity") {
        if (s.settings?.deviceCount > 0) {
          try { await loadDeviceDefinitions(); } catch (error) { toast(String(error)); }
        }
        loadTraffic();
        // Presets follow the clock; a custom range stays as chosen.
        resolveActivityRange();
        await loadActivity();
      }
      if (ui.page === "main") loadHomeTraffic();
      if (ui.page === "devices") loadDevices();
      if (ui.page === "settings") await loadDevices();
      ui.pinLog = ui.page === "activity" && el.dataset.target === "log";
      render();
      $("content").scrollTop = 0;
      if (ui.pinLog) scrollToLog();
      break;
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
        await invoke("set_device_forwarding", { id: el.dataset.id, enabled: el.dataset.enabled === "true" });
        ui.deviceForwarders = await invoke("get_device_forwarders");
        ui.snap.forwarding = ui.deviceForwarders[0] || null;
      } catch (error) { toast(String(error)); }
      finally { ui.deviceForwardingBusy = null; render(); }
      break;
    }
    case "device-new":
      ui.deviceDraft = { id: null, transport: "ssh" };
      ui.deviceFormOpen = true; render(); $("device-name")?.focus();
      break;
    case "device-cancel":
      ui.deviceFormOpen = false; ui.deviceDraft = null; render();
      break;
    case "device-edit": {
      const d = ui.devices.find(device => device.id === el.dataset.id);
      if (d) {
        ui.deviceDraft = { id: d.id, name: d.name, sshEnabled: !!d.ssh, dataEnabled: !!d.data, host: d.ssh?.host, binary: d.ssh?.binary,
          url: d.data?.url, key: d.data?.tokenFile, env: d.data?.tokenEnv, ca: d.data?.caCertificate, transport: d.data?.transport || (d.ssh ? "ssh" : "http") };
        ui.deviceFormOpen = true; render(); $("device-name")?.focus();
      }
      break;
    }
    case "merged-refresh":
      if (ui.mergedDataLoading && ui.mergedDataRemote) break;
      await loadMergedData(true, true);
      break;
    case "device-save": {
      captureDeviceDraft(); const d = ui.deviceDraft;
      const ssh = d.transport === "ssh";
      const device = { id: d.id || null, name: d.name.trim() || (ssh ? d.host.trim() : d.url.trim()),
        ssh: ssh ? { host: d.host.trim(), binary: d.binary.trim() } : null,
        data: { transport: d.transport, url: ssh ? "" : d.url.trim(), tokenFile: ssh ? null : d.key.trim() || null, tokenEnv: ssh ? null : d.env.trim() || null, caCertificate: ssh ? null : d.ca.trim() || null } };
      try { await invoke("save_device", { device }); ui.deviceFormOpen = false; ui.deviceDraft = null; const changed = await loadDeviceDefinitions(); await loadDeviceCapabilities(); if (changed || (!ui.mergedData && !(ui.mergedDataLoading && ui.mergedDataRemote))) loadMergedData(true); }
      catch (e) { toast(String(e)); }
      break;
    }
    case "device-remove":
      try { await invoke("remove_device", { id: el.dataset.id }); delete ui.deviceStates[el.dataset.id]; await loadDeviceDefinitions(); if (ui.devices.length) loadMergedData(true); }
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
    case "restart":
      try {
        await invoke("restart_proxy");
        toast("Proxy restarted");
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
      ui.filter = el.dataset.filter;
      await loadActivity();
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
      await loadActivity("more");
      break;
    case "activity-preset":
      ui.activityPreset = el.dataset.target;
      resolveActivityRange();
      renderActivityRange();
      await loadActivity();
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
  if (event.target.id === "search-mode") {
    ui.searchMode = event.target.value;
    clearTimeout(searchTimer);
    $("search").placeholder = searchPlaceholder();
    await loadActivity();
    return;
  }
  if (event.target.id === "devices-traffic-scope" || event.target.id === "devices-traffic-range") {
    if (event.target.id === "devices-traffic-scope") {
      ui.trafficScope = event.target.value;
      ++ui.trafficRequest; ++ui.homeTrafficRequest;
      ui.homeTrafficFetchedAt = 0; ui.homeTrafficLoading = false;
    } else { ui.deviceTrafficMinutes = Number(event.target.value); }
    if (selectDeviceTraffic()) { render(); return; }
    // A remote refresh delivers every range when it completes. A cache read now
    // would supersede it and show peers as not refreshed, so wait for it instead.
    if (ui.mergedDataLoading && ui.mergedDataRemote) { ui.mergedData = null; render(); await loadLocalDeviceTraffic(); return; }
    const loading = loadMergedData(true); render(); await loading;
    return;
  }
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

// Check configured proxies once at startup; reopening the panel does not repeat it.
const probeStale = () => invoke("probe_proxy", { name: null, staleOnly: true });

listen("state-changed", scheduleRefresh);
const PANEL_PAGE_TIMEOUT_MS = 60 * 1000;
let panelHiddenAt = null;
listen("panel-shown", () => {
  const returnHome = panelHiddenAt !== null && Date.now() - panelHiddenAt >= PANEL_PAGE_TIMEOUT_MS;
  panelHiddenAt = null;
  if (returnHome) {
    ui.page = "main";
    closeSelect();
    render();
    $("content").scrollTop = 0;
  }
  return refresh();
});
listen("panel-hidden", () => {
  // Repeated native hide events must not extend the previous page's lifetime.
  panelHiddenAt ??= Date.now();
  closeSelect();
});
refresh().then(probeStale);
checkConfiguredSsh();

// Refresh even when no new requests arrive, so the rolling window advances.
setInterval(() => {
  if (document.hidden) return;
  if (ui.page === "activity") {
    loadTraffic();
    scheduleActivityLog();
  } else if (ui.page === "devices") { refresh(); loadDevices(); }
  else if (ui.page === "main") refresh();
  else if (ui.page === "settings") refreshForwardingStatus();
}, 15000);
