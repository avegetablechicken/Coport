"use strict";

/// The Activity page: its traffic with the review of changed
/// configurations, the search and time range of the request log, and the
/// log's rows with their details. Like the other pages it keeps no state of
/// its own: the log is `activityLog`'s, the traffic `trafficViews`'.

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
  // Refreshes must not replace the link while it is focused or hovered. The
  // markup is compared as written: read back, a quote in a name is not escaped.
  if (el.forwardingHtml !== html) { el.innerHTML = html; el.forwardingHtml = html; }
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

/// Closes the range editor on a press outside it and its menus.
function installActivityPage() {
  document.addEventListener("pointerdown", (event) => {
    const editor = $("range-editor");
    if (editor && !editor.hidden && !editor.contains(event.target) && !event.target.closest?.(".select-menu")) showRangeEditor(false);
  });
}
