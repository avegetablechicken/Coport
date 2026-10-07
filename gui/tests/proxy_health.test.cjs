const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const { test } = require("node:test");
const vm = require("node:vm");

const source = fs.readFileSync(path.join(__dirname, "../ui/app.js"), "utf8");
const block = source.match(/function proxiesBlock\(\) \{[\s\S]*?\r?\n\}/)[0];

function render(probe) {
  return vm.runInNewContext(`${block}\nproxiesBlock()`, {
    ui: { snap: { config: { details: { proxies: [{ name: "used", endpoint: "http://127.0.0.1:12345", probe }] } } } },
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
