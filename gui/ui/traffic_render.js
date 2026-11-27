"use strict";

/// How Device traffic is drawn wherever a page shows it: the chart, account
/// rows, the totals with their Traffic metrics, and the share ring of
/// accounts. Nothing here keeps state; the only setting read is whether
/// account names are masked (`ui.snap.settings.maskAccountNames`).

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
    // Live windows advance on refresh; keep the hovered column and update
    // its timestamp/value, while a different category or bucket width differs.
    bars += `<g class="chart-bar" data-tip-slot="${scope}:${i}:${bucketMs}" data-tip="${esc(range + title)}" aria-label="${esc(range + title)}">`;
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
/// One account's row, as Activity and Devices show it. With Models,
/// Unidentified calls that all failed reported nothing beyond their count, so
/// the row is one line: the account, its calls and its error rate. Any call
/// that succeeded keeps the full row.
function accountRow(c, scope, window, review = "", source = "local") {
  const identityKey = JSON.stringify(["account", source, c.service, c.credential]);
  const rate = fmtErrorRate(c);
  const identity = `<span class="traffic-service">${serviceMark(c.service)}${esc(c.service)}</span><strong>${esc(maskAccountName(c.credential))}</strong>`;
  if (scope === "model" && c.credential === "Unidentified" && c.requests > 0 && c.errors >= c.requests) {
    const calls = `${c.requests.toLocaleString("en-US")} ${c.requests === 1 ? "call" : "calls"}`;
    return `<div class="credential-traffic" data-tip-scope="${tooltipScopeKey(identityKey)}"><div class="traffic-identity brief">${identity}<span class="traffic-brief"><span>${calls} · <span class="bad">${rate} errors</span></span>${review}</span></div></div>`;
  }
  return `<div class="credential-traffic" data-tip-scope="${tooltipScopeKey(identityKey)}"><div class="traffic-identity">${identity}${review}</div>
    ${chart(c, scope, true, window)}<div class="strip">${stat(scope === "model" ? "Calls" : "Requests", c.requests)}${stat("Error Rate", rate, c.errors ? "bad" : "")}${firstTokenStat(c, scope)}${stat("Avg. Time", fmtMs(c.avgMs))}${outputTpsStat(c, scope, identityKey)}</div>${modelTokenStats(c, scope)}</div>`;
}
/// The share ring, chart and totals of one traffic view, as Home, each device
/// card and Merged Traffic show it. An unchanged view reuses its markup.
/// `source` names whose traffic it is ("local", "merged" or "device/<id>"), so
/// a page change carries it over to the same traffic on the next page.
const trafficMarkup = new WeakMap();
// Keep private identities in memory; DOM scopes expose only session-local IDs.
function tooltipScopeKey(key) {
  const keys = tooltipScopeKey.keys ||= new Map();
  if (!keys.has(key)) keys.set(key, `tooltip-${keys.size}`);
  return keys.get(key);
}
/// Traffic metrics come derived from the backend; the panel only formats them.
/// First Token shows wherever a device reports it, before any call streamed too.
function firstTokenStat(stats, scope) {
  if (scope !== "model" || (stats.firstTokenSamples == null && stats.forwardedFirstTokenSamples == null)) return "";
  return stat("First Token", fmtMs(Number.isFinite(stats.firstTokenMs) ? stats.firstTokenMs : null));
}
function outputTpsStat(stats, scope, key = "output-tps") {
  if (scope !== "model") return "";
  const average = stats.outputTpsFromStart;
  const averageText = Number.isFinite(average) ? average.toFixed(1) + " tok/s" : "—";
  const tip = `From request start instead of the first token: ${averageText}.`;
  const label = `Output TPS<span class="info-tip stat-tip" tabindex="0" data-tip-key="${tooltipScopeKey(key)}" data-tip="${esc(tip)}" aria-label="${esc(tip)}">${ICON.warning}</span>`;
  const value = stats.outputTps == null || !Number.isFinite(stats.outputTps) ? "—" : stats.outputTps.toFixed(1) + " tok/s";
  return stat(label, value);
}
/// An error rate as every view shows it: a little is never 0% and nearly all
/// is never 100%. Unknown without requests.
function fmtErrorRate(stats) {
  const rate = stats.errorRate;
  if (!Number.isFinite(rate)) return "—";
  if (stats.errors >= stats.requests) return "100%";
  const percent = 100 * rate;
  if (percent <= 0) return "0%";
  if (percent < 0.1) return "<0.1%";
  const tenth = Math.round(percent * 10) / 10;
  if (tenth < 10) return `${tenth.toFixed(1)}%`;
  const whole = Math.round(percent);
  return whole >= 100 ? ">99%" : `${whole}%`;
}
function trafficSummary(view, scope, window = view, source = "") {
  const key = `${scope}|${window.start ?? window.windowStart}|${window.end ?? window.windowEnd}|${window.bucketMinutes}|${source}|${accountNamesMasked()}`;
  const cached = trafficMarkup.get(view);
  if (cached?.key === key) return cached.html;
  const rate = fmtErrorRate(view);
  // Overall health turns red at a tenth of requests failing.
  const rateClass = view.errorRate >= 0.1 ? "bad" : "";
  // Devices older than First Token do not report it, and then it is not shown.
  const html = `<div${source ? ` data-morph="summary:${esc(source)}"` : ""}>${trafficShare(view.credentials ?? [], scope, source)}${chart(view, scope, false, window)}
     <div class="strip">
       ${stat(scope === "model" ? "Calls" : "Requests", view.requests)}
       ${stat("Error Rate", rate, rateClass)}
       ${firstTokenStat(view, scope)}
       ${stat("Avg. Time", fmtMs(view.avgMs))}
       ${outputTpsStat(view, scope, JSON.stringify(["summary", source]))}
     </div>${modelTokenStats(view, scope)}</div>`;
  trafficMarkup.set(view, { key, html });
  return html;
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
const SHARE_SERVICES = ["Codex", "Claude", "Antigravity", "Grok"];
const SHARE_SHADES = 4;
/// The smallest part of the ring a sector is drawn at, so it stays hoverable;
/// credentials below it fold into Others.
const SHARE_MIN = 0.025;
/// Settings may show account names in full; masking is the default.
function accountNamesMasked() {
  return ui.snap?.settings?.maskAccountNames !== false;
}
/// Traffic views hide part of an account name. An email keeps its domain, and
/// an account ID keeps its ends; configuration names stay readable.
function maskAccountName(name) {
  if (!accountNamesMasked()) return name;
  const at = name.indexOf("@");
  if (at > 0 && at < name.length - 1) {
    const local = Array.from(name.slice(0, at));
    if (local.length <= 1) return `***@${name.slice(at + 1)}`;
    const head = local.length >= 5 ? local.slice(0, 2).join("") : local[0];
    const tail = local.length >= 3 ? local[local.length - 1] : "";
    return `${head}***${tail}@${name.slice(at + 1)}`;
  }
  if (/^[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}$/i.test(name)) {
    return `${name.slice(0, 8)}…${name.slice(-4)}`;
  }
  return name;
}
/// Credentials grouped by app, Codex, Claude, Antigravity, then Grok, each group in its hue; traffic
/// has no other service. `measure` gives a credential's amount, or null when it
/// is unknown. The two largest of a service always keep their slice; past them
/// a credential under SHARE_MIN of the whole ring folds into "Others", when at
/// least two do. Unknown amounts keep their row. Others takes the next shade after
/// the credentials, and unidentified requests come last, drawn without a hue.
function shareGroups(credentials, measure) {
  const sum = (list) => list.some((c) => measure(c) != null) ? list.reduce((total, c) => total + (measure(c) ?? 0), 0) : null;
  const displayName = (c) => maskAccountName(c.credential);
  const used = credentials.filter((c) => c.requests > 0);
  const total = sum(used) ?? 0;
  const small = (c) => measure(c) != null && measure(c) < total * SHARE_MIN;
  return SHARE_SERVICES.filter((service) => used.some((c) => c.service === service)).map((service) => {
    const own = used.filter((c) => c.service === service);
    const named = own.filter((c) => c.credential !== "Unidentified").sort((a, b) => (measure(b) ?? -1) - (measure(a) ?? -1));
    const foldable = named.slice(2).filter(small);
    const rest = foldable.length >= 2 ? foldable : [];
    const kept = named.filter((c) => !rest.includes(c));
    const slices = kept.map((c, i) => ({ name: displayName(c), value: measure(c), color: shareShade(service, i, kept.length) }));
    if (rest.length) {
      const members = rest.map((c) => ({ name: displayName(c), value: measure(c) }));
      slices.push({ name: `Others (${rest.length})`, value: sum(rest), color: othersShade(service, kept.length), members });
    }
    for (const c of own) if (c.credential === "Unidentified") slices.push({ name: c.credential, value: measure(c), muted: true, unidentified: true });
    return { service, value: sum(own), slices };
  });
}
/// The `index`th of `count` shades of a service's hue, darkest first. Up to
/// SHARE_SHADES use the defined shades; more are blended between them.
function shareShade(service, index, count) {
  const shade = (n) => `var(--${service.toLowerCase()}-${n})`;
  if (count <= SHARE_SHADES) return shade(index + 1);
  const at = (index * (SHARE_SHADES - 1)) / (count - 1);
  const low = Math.floor(at);
  const mix = Math.round((at - low) * 100);
  return mix ? `color-mix(in srgb, ${shade(low + 2)} ${mix}%, ${shade(low + 1)})` : shade(low + 1);
}
/// Others follows `kept` credentials: the next defined shade while one is left,
/// otherwise fainter than the last.
function othersShade(service, kept) {
  if (kept < SHARE_SHADES) return shareShade(service, kept, kept + 1);
  return `color-mix(in srgb, var(--${service.toLowerCase()}-${SHARE_SHADES}) 70%, transparent)`;
}
/// A share ring's size in pixels, and its hole's radius as a part of the ring's.
const SHARE_RING_SIZE = 84;
const SHARE_RING_INNER = 0.618;
/// The ring `progress` of the way from layout `from` to `to`, each a list of
/// { key, slice, width, color } in turn order with widths summing to 1. A
/// slice in both grows or shrinks in place; one only in `to` grows from nothing
/// where it will be, and one only in `from` shrinks away where it was, before
/// the slice that followed it. Gives each slice's { key, from, to } part of the
/// turn, the slice it draws, whether it is going away, and its colors at both ends.
function blendRing(from, to, progress) {
  const old = new Map(from.map((s) => [s.key, s]));
  const next = new Map(to.map((s) => [s.key, s]));
  const order = to.map((s) => s.key);
  for (let i = from.length - 1; i >= 0; i--) {
    if (next.has(from[i].key)) continue;
    let at = order.length;
    for (let j = i + 1; j < from.length; j++) {
      const k = order.indexOf(from[j].key);
      if (k >= 0) { at = k; break; }
    }
    order.splice(at, 0, from[i].key);
  }
  const widths = order.map((key) => {
    const a = old.get(key)?.width ?? 0;
    return a + ((next.get(key)?.width ?? 0) - a) * progress;
  });
  const turn = widths.reduce((sum, w) => sum + w, 0) || 1;
  let cursor = 0;
  return order.map((key, i) => {
    const start = cursor;
    cursor += widths[i] / turn;
    const slice = (next.get(key) ?? old.get(key)).slice ?? key;
    return { key, slice, gone: !next.has(key), from: start, to: cursor, fromColor: old.get(key)?.color, toColor: next.get(key)?.color };
  });
}
/// Slices are matched by configuration. When two have changed places, each
/// service's slices are matched by rank instead, as their shades are, so the
/// ring changes in place rather than jumping.
function matchRing(from, to) {
  const next = new Set(to.map((s) => s.key));
  const both = from.filter((s) => next.has(s.key)).map((s) => s.key);
  const kept = new Set(both);
  const after = to.filter((s) => kept.has(s.key)).map((s) => s.key);
  if (both.every((key, i) => key === after[i])) return [from, to];
  const rank = (layout) => {
    const counts = {};
    return layout.map((s) => {
      const [service, name] = s.key.split("|");
      if (name === "Others" || name === "Unidentified") return s;
      counts[service] = (counts[service] ?? 0) + 1;
      return { ...s, key: `${service}|#${counts[service]}` };
    });
  };
  return [rank(from), rank(to)];
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
  // The corners shorten each arc, so a slice just over half a turn can have
  // arcs under half; flagging those large would draw them the other way round.
  const outerLarge = width - 2 * outerBeta > Math.PI ? 1 : 0;
  const innerLarge = width - 2 * innerBeta > Math.PI ? 1 : 0;
  return [
    `M ${at(outer, start + outerBeta)}`,
    `A ${outer} ${outer} 0 ${outerLarge} 1 ${at(outer, end - outerBeta)}`,
    `Q ${at(outer, end)} ${at(outer - corner, end)}`,
    `L ${at(inner + corner, end)}`,
    `Q ${at(inner, end)} ${at(inner, end - innerBeta)}`,
    `A ${inner} ${inner} 0 ${innerLarge} 0 ${at(inner, start + innerBeta)}`,
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
/// grouped by service. Rings of the same `source` are one ring across pages.
function trafficShare(credentials, scope, source = "") {
  const { unit, measure, format } = shareMeasure(scope);
  const visible = scope === "model" ? credentials.filter((c) => shareTokens(c) > 0) : credentials;
  const groups = shareGroups(visible, measure);
  const total = groups.reduce((sum, g) => sum + (g.value ?? 0), 0);
  if (!total) return "";
  const size = SHARE_RING_SIZE;
  const value = (n) => (n == null ? "—" : format(n));
  const slices = groups.flatMap((g) => g.slices.filter((s) => s.value > 0).map((s) => ({ ...s, service: g.service })));
  // Every slice keeps a visible minimum width, so tiny parts stay hoverable.
  const widths = slices.map((s) => Math.max(s.value / total, SHARE_MIN));
  const turn = widths.reduce((sum, w) => sum + w, 0);
  const color = (s) => s.color ?? "var(--share-unidentified)";
  const amount = (n) => (n == null ? "no reported usage" : `${n.toLocaleString("en-US")} ${unit}`);
  // Others lists every configuration it groups, one per line.
  const label = (s) => [`${s.service} · ${s.name}: ${amount(s.value)}`, ...(s.members ?? []).map((m) => `${m.name}: ${amount(m.value)}`)].join("\n");
  let cursor = 0;
  const paths = slices.map((s, i) => {
    const from = cursor;
    cursor += widths[i] / turn;
    // Its key and width let a redrawn ring move each slice from where it was (showRing).
    return `<path style="--slice:${color(s)}" data-slice="${esc(`${s.service}|${s.members ? "Others" : s.name}`)}" data-width="${widths[i] / turn}" d="${ringSector(from, cursor, size / 2, (size / 2) * SHARE_RING_INNER)}"><title>${esc(label(s))}</title></path>`;
  }).join("");
  const legend = groups.map((g) => `<div class="share-group" data-tip-slot="${g.service}">
    <div class="share-service" title="${esc(label({ service: g.service, name: "Total", value: g.value }))}">${serviceMark(g.service)}<span>${esc(g.service)}</span><span>${value(g.value)}</span></div>
    ${g.slices.map((s) => `<div class="share-item${s.muted ? " muted" : ""}" title="${esc(label({ ...s, service: g.service }))}" data-tip-slot="${tooltipScopeKey(JSON.stringify([g.service, s.members ? "Others" : s.name]))}">
      <i style="--slice:${color(s)}"></i><span>${esc(s.name)}</span><span>${value(s.value)}</span></div>`).join("")}</div>`).join("");
  return `<div class="traffic-share" data-tip-scope="${tooltipScopeKey(JSON.stringify(["share", source, scope]))}">
    <div class="share-ring"${source ? ` data-morph="ring:${esc(source)}" data-ring="${esc(source)}"` : ""} title="${esc(`${total.toLocaleString("en-US")} ${unit}`)}"><svg viewBox="0 0 ${size} ${size}" aria-hidden="true">${paths}</svg>
      <div class="share-total"><strong>${format(total)}</strong><span>${unit}</span></div></div>
    <div class="share-legend">${legend}</div></div>`;
}
