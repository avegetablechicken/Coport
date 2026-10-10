const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const { test } = require("node:test");
const vm = require("node:vm");

const source = fs.readFileSync(path.join(__dirname, "../ui/app.js"), "utf8");
const block = source.match(/function proxiesBlock\(\) \{[\s\S]*?\r?\n\}/)[0];

function render(probe, proxy = {}) {
  return vm.runInNewContext(`${block}\nproxiesBlock()`, {
    ui: { snap: { config: { details: { proxies: [{ name: "used", endpoint: "http://127.0.0.1:12345", probe, ...proxy }] } } } },
    block: (_title, _aside, body) => body,
    flag: () => "", tag: (name) => name, esc: (value) => value,
    ICON: { restart: "" },
  });
}

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
  let tick;
  const ui = { page: "main" };
  const document = { hidden: false };
  vm.runInNewContext(source.slice(source.indexOf("// Refresh even when no new requests arrive")), {
    ui, document,
    setInterval: (callback) => { tick = callback; },
    refresh: async () => { calls.push("refresh"); },
    loadDeviceCapabilities: () => { calls.push("cached-device-status"); },
    probeStale: () => { calls.push("probeStale"); },
  });
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
  assert.match(pending, /class="spinner"/);
  assert.doesNotMatch(pending, /Route unavailable:/);
  assert.doesNotMatch(pending, /Unreachable|Available/);
  const measured = render({ state: "ok", ms: 42, exitIp: "203.0.113.5" }, proxy);
  assert.match(measured, /42 ms/);
  assert.match(measured, /203\.0\.113\.5/);
  assert.match(measured, /Route unavailable:/);
  assert.doesNotMatch(measured, /Unreachable|Available/);
  const unreachable = render({ state: "error", error: "timed out" }, proxy);
  assert.match(unreachable, /Unreachable/);
  assert.match(unreachable, /timed out/);
  assert.doesNotMatch(unreachable, /Route unavailable:/);
  for (const probe of [null, { state: "idle" }, { state: "unavailable" }]) {
    assert.doesNotMatch(render(probe, proxy), /Route unavailable:|Unreachable|Available/);
  }
  // Retain the destination failure so it returns after connectivity recovers.
  assert.match(render({ state: "ok", ms: 50 }, proxy), /Route unavailable:/);
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


test("Settings state refresh opts out of account profile network lookups", async () => {
  const calls = [];
  const refresh = source.slice(source.indexOf('async function refresh()'), source.indexOf('let refreshTimer;'));
  const context = {
    ui: { page: 'settings', refreshRequest: 0 },
    invoke: async (command, args) => {
      calls.push([command, args]);
      return command === 'get_state' ? { config: {} } : { rows: [] };
    },
    tagColors: () => ({}), render() {},
  };
  vm.runInNewContext(refresh, context);
  await context.refresh();
  assert.equal(calls[0][0], 'get_state');
  assert.equal(calls[0][1].refreshAccounts, false);
});

test('every passive page refresh opts out of upstream account probes', async () => {
  const code=source.slice(source.indexOf('async function refresh()'),source.indexOf('let refreshTimer;'));
  for(const page of ['main','devices','settings','activity']) {
    const calls=[];
    const context={ui:{page,refreshRequest:0,homeTrafficFetchedAt:Date.now()},invoke:async(command,args)=>{calls.push([command,args]);return command==='get_state'?{config:{details:{proxies:[]}}}:{rows:[]};},tagColors:()=>({}),render(){},scheduleActivityLog(){}};
    vm.runInNewContext(code,context);await context.refresh();await context.refresh();
    assert.ok(calls.filter(([command])=>command==='get_state').every(([,args])=>args.refreshAccounts===false),page);
  }
});


test('traffic IPC reads cached identities without starting account probes', () => {
  const commands = fs.readFileSync(path.join(__dirname, '../src/commands.rs'), 'utf8');
  for (const name of ['get_traffic', 'get_merged_data']) {
    const start = commands.indexOf(`pub async fn ${name}(`);
    const next = commands.indexOf('#[tauri::command]', start);
    const body = commands.slice(start, next < 0 ? undefined : next);
    assert.match(body, /cached_credential_labels/);
    assert.doesNotMatch(body, /\.credential_labels\(|account_states\(|credential_reports\(/);
  }
});
