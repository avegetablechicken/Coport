const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const { test } = require("node:test");

const scripts = require("./panel_harness.cjs");

/// The whole panel showing `page`, answered by `answer`; drawing and proxy
/// colours are left out.
function panelFor(page, answer) {
  const p = scripts.panel({ invoke: answer });
  Object.assign(p.context, { render() {}, tagColors: () => ({}) });
  p.get("ui").page = page;
  return p;
}

function renderProxies(proxies) {
  return scripts.under(scripts.sandbox({
    ui: { snap: { config: { details: { proxies } } } },
    block: (_title, _aside, body) => body,
    flag: () => "", tag: (name) => name, esc: (value) => value,
    ICON: { restart: '<svg data-icon="restart"></svg>' },
  }), "home_page.js").proxiesBlock();
}

function render(probe, proxy = {}) {
  return renderProxies([{ name: "used", endpoint: "http://127.0.0.1:12345", probe, ...proxy }]);
}

test("home sorts proxy rows by host and port across schemes", () => {
  const proxies = Object.freeze([
    { name: "alpha", endpoint: "http://10.0.0.10:80" },
    { name: "beta", endpoint: "https://10.0.0.2:10000" },
    { name: "gamma", endpoint: "socks5://10.0.0.2:900" },
  ]);
  const html = renderProxies(proxies);
  const names = Array.from(html.matchAll(/data-name="([^"]+)"/g), (match) => match[1]);
  assert.deepEqual(names, ["gamma", "beta", "alpha"]);
});

test("runtime success renders as available without fabricated latency", () => {
  const html = render({ state: "ok", ms: null });
  assert.match(html, /Available/);
  assert.match(html, /dot running/);
  assert.doesNotMatch(html, /null ms|undefined ms|Unreachable/);
});

test("standalone probe results retain latency and failure display", () => {
  assert.match(render({ state: "ok", ms: 42 }), /42 ms/);
  assert.match(render({ state: "error", error: "timed out" }), /Unreachable/);
});

test("visible home refresh does not start another proxy probe", async () => {
  const calls = [];
  const p = panelFor("main", async (command) => { if (command === "probe_proxy") calls.push("probeStale"); });
  p.context.refresh = async () => { calls.push("refresh"); };
  p.get("deviceList").loadCapabilities = () => { calls.push("cached-device-status"); };
  // The rolling window's timer: the last one the panel starts, every 15 s.
  const tick = p.timers.findLast((t) => t.ms === 15000).fn;
  const { document } = p;
  tick();
  await Promise.resolve();
  assert.deepEqual(calls, ["refresh", "cached-device-status"]);
  document.hidden = true;
  tick();
  await Promise.resolve();
  assert.equal(calls.length, 2);
});

test("LAN proxy renders the measured exit IP", () => {
  const html = render(
    { state: "ok", ms: 42, exitIp: "203.0.113.5", country: "JP" },
    { name: "jp_lab", endpoint: "http://10.156.232.107:10810", local: true },
  );
  assert.match(html, /10\.156\.232\.107:10810 → 203\.0\.113\.5/);
  assert.match(html, /42 ms/);
});

test("route failures appear only after the proxy itself is reachable", () => {
  const proxy = { local: true, routeFailures: ["https://chatgpt.com/"] };
  const pending = render({ state: "pending" }, proxy);
  assert.match(pending, /class="icon-btn probing"/);
  assert.doesNotMatch(pending, /Route unavailable:/);
  assert.doesNotMatch(pending, /Unreachable|Available/);
  const measured = render({ state: "ok", ms: 42, exitIp: "203.0.113.5" }, proxy);
  assert.match(measured, /42 ms/);
  assert.match(measured, /203\.0\.113\.5/);
  assert.match(measured, /Route unavailable:/);
  assert.match(measured, /dot warn/);
  assert.doesNotMatch(measured, /dot running|Unreachable|Available/);
  const unreachable = render({ state: "error", error: "timed out" }, proxy);
  assert.match(unreachable, /Unreachable/);
  assert.match(unreachable, /timed out/);
  assert.match(unreachable, /dot failed/);
  assert.doesNotMatch(unreachable, /Route unavailable:|dot warn/);
  for (const probe of [null, { state: "idle" }, { state: "unavailable" }]) {
    assert.doesNotMatch(render(probe, proxy), /Route unavailable:|Unreachable|Available|dot warn/);
  }
  // Retain the destination failure so it returns after connectivity recovers.
  const recovered = render({ state: "ok", ms: 50 }, proxy);
  assert.match(recovered, /Route unavailable:/);
  assert.match(recovered, /dot warn/);
  // Destinations recovering restores the available indicator.
  assert.match(render({ state: "ok", ms: 50 }, { ...proxy, routeFailures: [] }), /dot running/);
});

test("proxy refresh spins the existing button icon only while pending", () => {
  for (const state of ["idle", "pending", "ok", "error", "unavailable"]) {
    const html = render({ state });
    const button = html.match(/<button\b[^>]*>[\s\S]*?<\/button>/)[0];
    assert.equal(button.includes('class="icon-btn probing"'), state === "pending");
    assert.match(button, new RegExp(`aria-busy="${state === "pending"}"`));
    assert.match(button, /<svg data-icon="restart"><\/svg>/);
    assert.equal((html.match(/<svg\b/g) || []).length, 1);
    assert.doesNotMatch(html, /class="spinner"/);
    if (state === "pending") assert.doesNotMatch(html, /—/);
    if (state === "idle") assert.match(html, /<span class="faint">—<\/span>/);
  }
  const css = fs.readFileSync(path.join(__dirname, "../ui/style.css"), "utf8");
  assert.match(css, /\.icon-btn\.probing svg\s*\{\s*animation: spin /);
});

test("unavailable daemon is not reported as an unreachable proxy", () => {
  const html = render({ state: "unavailable", error: "Start the proxy daemon to test proxies." });
  assert.match(html, /Unverified/);
  assert.match(html, /Start the proxy daemon/);
  assert.doesNotMatch(html, /Unreachable|dot failed/);
});

test('an exit IP service failure leaves proxy reachability unverified', () => {
  const html = render({ state: 'unavailable', error: 'Exit IP lookup failed: HTTP 503.' }, { routeFailures: ['https://example.invalid/'] });
  assert.match(html, /Unverified/);
  assert.match(html, /Exit IP lookup failed: HTTP 503/);
  assert.doesNotMatch(html, /Unreachable|dot failed|Route unavailable:/);
});


test("Settings state refresh requests no account work", async () => {
  const calls = [];
  const { context } = panelFor('settings', async (command, args) => {
    calls.push([command, args]);
    return command === 'get_state' ? { config: {} } : { rows: [] };
  });
  await context.refresh();
  assert.equal(calls[0][0], 'get_state');
  // Account states come from the backend's own schedule, never from a page read.
  assert.equal(calls[0][1], undefined);
});

test('every passive page refresh opts out of upstream account probes', async () => {
  for(const page of ['main','devices','settings','activity']) {
    const calls=[];
    const { context } = panelFor(page, async(command,args)=>{calls.push([command,args]);return command==='get_state'?{config:{details:{proxies:[]}}}:{rows:[]};});
    Object.assign(context, { scheduleActivityLog(){}, homeTrafficDevice:()=>null });
    await context.refresh();await context.refresh();
    assert.ok(calls.filter(([command])=>command==='get_state').every(([,args])=>args===undefined),page);
  }
});


test('account routes explain each activation state', () => {
  const row = (activation) => scripts.under(scripts.sandbox({
    routeIdentity: (name) => ({ kind: 'account', name }), routingChain: () => '',
    ROUTE_ICON: { account: '' }, ICON: { warning: '' }, esc: (value) => value,
  }), 'traffic_render.js', 'home_page.js').routingRow({ selector: 'you@example.com', proxies: [], activation }, true, true);
  // Not yet identified is not a failed probe.
  assert.match(row('unknown'), /not identified yet/);
  assert.doesNotMatch(row('unknown'), /Probe failed|could not confirm/);
  assert.match(row('probe_failed'), /Probe failed/);
  assert.match(row('probe_failed'), /could not confirm availability/);
  assert.match(row('remote'), /confirmed this account is available/);
  assert.match(row('inactive'), /Inactive/);
  assert.doesNotMatch(row('active'), /Inactive|Probe failed|route-warning/);
});
