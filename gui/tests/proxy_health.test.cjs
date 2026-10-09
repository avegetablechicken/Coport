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
    probeStale: () => { calls.push("probeStale"); },
  });
  tick();
  await Promise.resolve();
  assert.deepEqual(calls, ["refresh"]);
  document.hidden = true;
  tick();
  await Promise.resolve();
  assert.equal(calls.length, 1);
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
  assert.match(html, /Not tested/);
  assert.match(html, /Start the proxy daemon/);
  assert.doesNotMatch(html, /Unreachable|dot failed/);
});
