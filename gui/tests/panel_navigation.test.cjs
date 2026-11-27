const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const { test } = require("node:test");
const vm = require("node:vm");

const source = fs.readFileSync(path.join(__dirname, "../ui/app.js"), "utf8");
const { createTrafficViews } = require("../ui/traffic_views.js");
const { createDeviceList } = require("../ui/device_list.js");
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
  const loginReads = [];
  vm.runInNewContext(handlers, {
    ui,
    Date: { now: () => now },
    listen: (name, handler) => { events[name] = handler; },
    scheduleRefresh() {},
    menus: { current: () => null, holdRender: () => false, close() {} },
    hideRouteTooltip() {},
    render: () => rendered.push(ui.page),
    $: () => content,
    refresh: async () => {},
    probeStale: () => probes.push("stale"),
    deviceList: { loaded: false, checkSsh: () => sshChecks.push("startup") },
    refreshLaunchAtLogin: async () => loginReads.push(ui.page),
    invoke: command => probes.push(command),
    setInterval() {},
  });
  return { ui, content, rendered, events, probes, sshChecks, loginReads, advance: (ms) => { now += ms; } };
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

test("reopening Settings rereads the login item that System Settings may have changed", async () => {
  const p = panel();
  await p.events["panel-shown"]();
  p.events["panel-hidden"]();
  p.advance(timeout);
  await p.events["panel-shown"]();
  const other = panel("activity");
  await other.events["panel-shown"]();
  assert.deepEqual(p.loginReads, ["settings"]);
  assert.deepEqual(other.loginReads, []);
});

// Page changes: the shown page stays while the next one reads its first data,
// then one motion (changePage) shows it.
async function navigation(page = "main") {
  const traffic = createTrafficViews({ invoke: async (_, { minutes, scope }) => ({ minutes, credentials: [] }) });
  await traffic.local(30, "model");
  const ui = { page, snap: { settings: {} } };
  const deviceList = createDeviceList({
    invoke: async () => [], Channel: class {}, trafficViews: traffic, page: () => ui.page,
    minutes: () => ui.deviceTrafficMinutes, snap: () => ui.snap, render() {}, toast() {},
  });
  deviceList.load = () => {};
  const rendered = [];
  const changes = [];
  const loads = [];
  const context = {
    ui, trafficViews: traffic, deviceList, menus: { current: () => null, holdRender: () => false, close() {} }, renderTop() {}, renderPage: () => rendered.push(ui.page), queueFit() {},
    changePage: (update) => { changes.push({ page: ui.page, traffic: traffic.activity, homeTraffic: traffic.home, mergedData: deviceList.merged }); update(); },
    loadTraffic() {}, loadHomeTraffic() {}, refreshLaunchAtLogin() {}, homeTrafficDevice: () => null,
    activityLog: { resolveRange() {}, load: () => new Promise((resolve) => loads.push(resolve)) },
    $: () => ({ scrollTop: 0 }), scrollToLog() {}, toast() {},
    // Nothing in the panel has focus to keep across renders.
    document: { activeElement: null },
  };
  vm.createContext(context);
  vm.runInContext(source.slice(source.indexOf("function render()"), source.indexOf("\nfunction renderTop()")), context);
  vm.runInContext(source.slice(source.indexOf("async function act("), source.indexOf("// Any scroll or input")), context);
  const go = (target) => context.act("page", { dataset: { page: target } });
  return { ui, traffic, rendered, changes, loads, render: context.render, go, context };
}

test("the shown page stays while the next one loads, then changes once", async () => {
  const p = await navigation();
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
  const p = await navigation();
  const activity = p.go("activity");
  await Promise.resolve();
  await p.go("main");
  p.loads[0]();
  await activity;
  assert.equal(p.ui.page, "main");
  assert.deepEqual(p.changes.map((c) => c.page), ["main"]);
});

test("Home and Activity open with the ring the other already read", async () => {
  const p = await navigation();
  const going = p.go("activity");
  p.loads[0]();
  await going;
  assert.equal(p.changes[0].traffic, p.traffic.localView(30, "model"));
  const other = await p.traffic.local(360, "model");
  p.traffic.minutes = 360;
  await p.go("main");
  assert.equal(p.changes[1].homeTraffic, other);
});

test("Devices opens with the merged traffic of the range Home shows", async () => {
  const p = await navigation();
  const day = { minutes: 1440, scope: "model", sources: [] };
  p.context.deviceList.merged = { minutes: 30, scope: "model", sources: [] };
  p.context.deviceList.views = { "30:model": p.context.deviceList.merged, "1440:model": day };
  p.traffic.minutes = p.ui.deviceTrafficMinutes = 1440;
  await p.go("devices");
  assert.equal(p.changes[0].mergedData, day);
});

test("a range chosen on any traffic page applies to Home, Activity and Devices", async () => {
  const pending = [];
  const traffic = createTrafficViews({ invoke: () => new Promise((resolve) => pending.push(resolve)) });
  const ui = { deviceTrafficMinutes: 30 };
  const context = { ui, trafficViews: traffic };
  vm.createContext(context);
  vm.runInContext(source.slice(source.indexOf("function setTrafficMinutes("), source.indexOf("\nasync function loadTraffic(")), context);
  const reads = [traffic.loadActivity(), traffic.loadHome(null)];
  Object.assign(traffic, { activityError: "old", homeError: "old", homeFetchedAt: 5 });
  context.setTrafficMinutes(360);
  assert.deepEqual([traffic.minutes, ui.deviceTrafficMinutes], [360, 360]);
  assert.deepEqual([traffic.activityLoading, traffic.homeLoading, traffic.activityError, traffic.homeError, traffic.homeFetchedAt], [false, false, "", "", 0]);
  // Reads still running for the previous range are superseded.
  for (const resolve of pending) resolve({ credentials: [] });
  assert.deepEqual(await Promise.all(reads), [false, false]);
  // Every range control on the three pages calls it.
  const handlers = source.slice(source.indexOf('document.addEventListener("change"'));
  for (const id of ["devices-traffic-range", "home-traffic-range", "traffic-range"]) assert.match(handlers, new RegExp(`"${id}"[^]*?setTrafficMinutes\\(`));
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
