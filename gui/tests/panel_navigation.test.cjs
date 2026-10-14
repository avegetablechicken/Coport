const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const { test } = require("node:test");
const vm = require("node:vm");

const source = fs.readFileSync(path.join(__dirname, "../ui/app.js"), "utf8");
const handlers = source.slice(source.indexOf('listen("state-changed", scheduleRefresh);'));
const timeout = 60 * 1000;

function panel(page = "settings") {
  let now = 0;
  const events = {};
  const content = { scrollTop: 120 };
  const ui = { page };
  const rendered = [];
  const probes = [];
  const sshChecks = [];
  vm.runInNewContext(handlers, {
    ui,
    Date: { now: () => now },
    listen: (name, handler) => { events[name] = handler; },
    scheduleRefresh() {},
    closeSelect() {},
    render: () => rendered.push(ui.page),
    $: () => content,
    refresh: async () => {},
    probeStale: () => probes.push("stale"),
    checkConfiguredSsh: () => sshChecks.push("startup"),
    invoke: command => probes.push(command),
    setInterval() {},
  });
  return { ui, content, rendered, events, probes, sshChecks, advance: (ms) => { now += ms; } };
}

test("first show and short reopening preserve the current page", async () => {
  const p = panel();
  p.advance(timeout);
  await p.events["panel-shown"]();
  assert.equal(p.ui.page, "settings");
  p.events["panel-hidden"]();
  p.advance(timeout - 1);
  await p.events["panel-shown"]();
  assert.equal(p.ui.page, "settings");
  assert.equal(p.content.scrollTop, 120);
});

for (const page of ["settings", "activity"]) {
  test(`${page} returns home after one minute hidden, including repeated hide events`, async () => {
    const p = panel(page);
    p.events["panel-hidden"]();
    p.advance(timeout - 1);
    p.events["panel-hidden"]();
    p.advance(1);
    await p.events["panel-shown"]();
    assert.equal(p.ui.page, "main");
    assert.equal(p.content.scrollTop, 0);
    assert.deepEqual(p.rendered, ["main"]);
  });
}

test("each reopen clears the old hidden timestamp", async () => {
  const p = panel();
  p.events["panel-hidden"]();
  p.advance(timeout - 1);
  await p.events["panel-shown"]();
  p.advance(timeout * 2);
  await p.events["panel-shown"]();
  assert.equal(p.ui.page, "settings");
  p.events["panel-hidden"]();
  p.advance(1);
  await p.events["panel-shown"]();
  assert.equal(p.ui.page, "settings");
  p.events["panel-hidden"]();
  p.advance(timeout * 12);
  await p.events["panel-shown"]();
  assert.equal(p.ui.page, "main");
});


test("startup tests proxies once and repeated panel reopening does not repeat them", async () => {
  const p = panel();
  await Promise.resolve();
  for (let i = 0; i < 5; i++) {
    p.events['panel-hidden']();
    p.advance(timeout * 3);
    await p.events['panel-shown']();
  }
  assert.deepEqual(p.probes, ["stale"]);
});


test("SSH availability is checked once at app startup, not on every panel reopen", async () => {
  const p = panel();
  assert.deepEqual(p.sshChecks, ['startup']);
  for (let i = 0; i < 3; i++) {
    p.events['panel-hidden']();
    await p.events['panel-shown']();
  }
  assert.deepEqual(p.sshChecks, ['startup']);
});

// Page changes: the shown page stays while the next one reads its first data,
// then one motion (changePage) shows it.
function navigation(page = "main") {
  const views = { "30:model": { scope: "model", credentials: [] } };
  const ui = { page, snap: { settings: {} }, trafficMinutes: 30, homeTrafficMinutes: 30, trafficScope: "model", localTrafficViews: views };
  const rendered = [];
  const changes = [];
  const loads = [];
  const context = {
    ui, openSelect: null, selectRenderPending: false, renderTop() {}, renderPage: () => rendered.push(ui.page), queueFit() {},
    changePage: (update) => { changes.push({ page: ui.page, traffic: ui.traffic, homeTraffic: ui.homeTraffic }); update(); },
    loadTraffic() {}, resolveActivityRange() {}, loadHomeTraffic() {}, loadDevices() {}, homeTrafficDevice: () => null,
    loadActivity: () => new Promise((resolve) => loads.push(resolve)),
    $: () => ({ scrollTop: 0 }), scrollToLog() {}, toast() {},
  };
  vm.createContext(context);
  vm.runInContext(source.slice(source.indexOf("function render()"), source.indexOf("\nfunction renderTop()")), context);
  vm.runInContext(source.slice(source.indexOf("function cachedTraffic("), source.indexOf("/// Home shows the forwarded device")), context);
  vm.runInContext(source.slice(source.indexOf("async function act("), source.indexOf("// Any scroll or input")), context);
  const go = (target) => context.act("page", { dataset: { page: target } });
  return { ui, views, rendered, changes, loads, render: context.render, go };
}

test("the shown page stays while the next one loads, then changes once", async () => {
  const p = navigation();
  const going = p.go("activity");
  await Promise.resolve();
  p.render();
  assert.deepEqual(p.rendered, []);
  p.loads[0]();
  await going;
  assert.deepEqual(p.changes.map((c) => c.page), ["activity"]);
  assert.deepEqual(p.rendered, ["activity"]);
  p.render();
  assert.deepEqual(p.rendered, ["activity", "activity"]);
});

test("a later page change supersedes one still loading", async () => {
  const p = navigation();
  const activity = p.go("activity");
  await Promise.resolve();
  await p.go("main");
  p.loads[0]();
  await activity;
  assert.equal(p.ui.page, "main");
  assert.deepEqual(p.changes.map((c) => c.page), ["main"]);
});

test("Home and Activity open with the ring the other already read", async () => {
  const p = navigation();
  const going = p.go("activity");
  p.loads[0]();
  await going;
  assert.equal(p.changes[0].traffic, p.views["30:model"]);
  const other = { scope: "model", credentials: [] };
  p.views["360:model"] = other;
  p.ui.trafficMinutes = p.ui.homeTrafficMinutes = 360;
  await p.go("main");
  assert.equal(p.changes[1].homeTraffic, other);
});

test("page changes move only what is shown, not content of a closed disclosure", () => {
  const context = { MORPH_TEXT: ["color"], getComputedStyle: () => ({ getPropertyValue: () => "", fontSize: "11px" }) };
  vm.createContext(context);
  vm.runInContext(source.slice(source.indexOf("function morphTargets("), source.indexOf("\nfunction motionLayer(")), context);
  // WebKit lays out a closed disclosure's content, so it has a place but is not seen.
  const element = (morph, { closed = false, summary = false } = {}) => ({
    dataset: { morph }, classList: { contains: () => false },
    getBoundingClientRect: () => ({ top: 100, bottom: 120, width: 300, height: 20 }),
    closest: (selector) => (selector === "summary" ? summary : selector === "details:not([open])" && closed) || null,
  });
  const region = { querySelectorAll: () => [element("log", { closed: true }), element("title:inactive", { closed: true, summary: true }), element("traffic")] };
  const found = context.morphTargets(region, { top: 0, bottom: 600, height: 600 });
  assert.deepEqual([...found.keys()], ["title:inactive", "traffic"]);
});
