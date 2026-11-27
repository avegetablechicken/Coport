"use strict";

/// The Devices page and the Devices part of Settings: every Device's card
/// with its Device traffic beside Merged Traffic, forwarding, capabilities and
/// the form that adds or edits a Device. Its state is the panel's: `ui`,
/// `trafficViews`, `deviceList` and `deviceForm`.

function deviceTrafficContent(traffic, scope, detailsKey = "merged", window = traffic) {
  let body = `<div class="traffic-content" aria-busy="${deviceList.loading}">${trafficSummary(traffic, scope, window, detailsKey)}</div>`;
  const pendingReviews = traffic.credentials.reduce((n, c) => n + (c.sources ?? []).filter(s => s.reason).length, 0);
  if (pendingReviews) body += `<div class="placeholder">${ICON.warning} Renamed or changed local configurations need review. Open Upstream accounts and choose where their traffic belongs.</div>`;
  if (traffic.credentials.length) body += `<details class="disclosure" data-device-traffic="${esc(detailsKey)}" ${ui.deviceTrafficOpen?.[detailsKey] ? "open" : ""}><summary>${ICON.chevron}Upstream accounts</summary>${traffic.credentials.map(c => accountRow(c, scope, window, trafficReview(c, traffic.targets, scope), detailsKey)).join("")}</details>`;
  return body;
}
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

/// Keeps each card's disclosure open across renders, and the device form's
/// draft as it is typed.
function installDevicesPage() {
  document.addEventListener("toggle", event => {
    const details = event.target;
    if (!details.isConnected || !details.matches?.("details[data-device-traffic]")) return;
    (ui.deviceTrafficOpen ||= Object.create(null))[details.dataset.deviceTraffic] = details.open;
  }, true);
  document.addEventListener("input", event => { if (event.target.id?.startsWith("device-")) captureDeviceDraft(); });
  document.addEventListener("change", event => {
    if (!event.target.id?.startsWith("device-")) return;
    captureDeviceDraft();
    if (event.target.id === "device-transport") render();
  });
}
