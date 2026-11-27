"use strict";

/// Formatting every page shares: icons, escaping, sizes, durations, times
/// and service marks. Nothing here reads or keeps panel state.

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
// Original provider SVGs copied from OpenQuota; CSS masks apply the GUI theme color.
function serviceMark(service) {
  const name = { Claude: "claude", Codex: "codex", Antigravity: "antigravity", Grok: "grok" }[service];
  return name ? `<span class="service-mark ${name}-mark" aria-hidden="true"></span>` : "";
}

/// A block: titled box with an optional right-aligned headline. `morph` names
/// what it shows, so another page's block of that name turns into it.
function block(title, aside, body, morph = "") {
  return `<section class="block"${morph ? ` data-morph="${esc(morph)}"` : ""}>
    <div class="block-head"><span class="block-title">${title}</span>${aside ? `<span class="block-aside">${aside}</span>` : ""}</div>
    ${body}</section>`;
}
function stat(label, value, cls = "") {
  return `<div class="stat"><div class="stat-label">${label}</div><div class="stat-value ${cls}">${value}</div></div>`;
}
function message(kind, text, actions = "") {
  return `<div class="message ${kind}"><span class="message-dot"></span><div class="message-body">${text}${
    actions ? `<div class="message-actions">${actions}</div>` : ""
  }</div></div>`;
}
