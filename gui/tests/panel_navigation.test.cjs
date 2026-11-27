const assert = require("node:assert/strict");
const { test } = require("node:test");

const { createTrafficViews } = require("../ui/traffic_views.js");
const { createDeviceList } = require("../ui/device_list.js");
const scripts = require("./panel_harness.cjs");
const timeout = 60 * 1000;

async function panel(page = "settings") {
  let now = 0;
  const content = { scrollTop: 120 };
  const rendered = [];
  const probes = [];
  const sshChecks = [];
  const loginReads = [];
  const p = scripts.panel({
    // The panel's first refresh is answered: the stale proxy probe follows it.
    answerStart: true, elements: { content }, globals: { Date: { now: () => now } },
    invoke: async (command) => {
      if (command === "probe_proxy") probes.push("stale");
      return command === "get_state" ? { config: {} } : command === "get_activity" ? { rows: [] } : [];
    },
    before(context) {
      const create = context.createDeviceList;
      context.createDeviceList = (deps) => Object.assign(create(deps), { checkSsh: () => sshChecks.push("startup") });
    },
  });
  const ui = Object.assign(p.get("ui"), { page });
  Object.assign(p.context, {
    render: () => rendered.push(ui.page), refreshLaunchAtLogin: async () => loginReads.push(ui.page),
    scheduleRefresh() {}, tagColors: () => ({}),
  });
  const events = Object.fromEntries(Object.entries(p.events).map(([name, [fn]]) => [name, (...args) => fn(...args)]));
  // Once the first refresh and its probe are done, later refreshes draw
  // nothing, so what is drawn is what the panel's own events draw.
  await new Promise(setImmediate);
  p.context.refresh = async () => {};
  rendered.length = 0;
  return { ui, content, rendered, events, probes, sshChecks, loginReads, advance: (ms) => { now += ms; } };
}

test("first show and short reopening preserve the current page", async () => {
  const p = await panel();
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
    const p = await panel(page);
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
  const p = await panel();
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
  const p = await panel();
  await Promise.resolve();
  for (let i = 0; i < 5; i++) {
    p.events['panel-hidden']();
    p.advance(timeout * 3);
    await p.events['panel-shown']();
  }
  assert.deepEqual(p.probes, ["stale"]);
});


test("SSH availability is checked once at app startup, not on every panel reopen", async () => {
  const p = await panel();
  assert.deepEqual(p.sshChecks, ['startup']);
  for (let i = 0; i < 3; i++) {
    p.events['panel-hidden']();
    await p.events['panel-shown']();
  }
  assert.deepEqual(p.sshChecks, ['startup']);
});

test("reopening Settings rereads the login item that System Settings may have changed", async () => {
  const p = await panel();
  await p.events["panel-shown"]();
  p.events["panel-hidden"]();
  p.advance(timeout);
  await p.events["panel-shown"]();
  const other = await panel("activity");
  await other.events["panel-shown"]();
  assert.deepEqual(p.loginReads, ["settings"]);
  assert.deepEqual(other.loginReads, []);
});

// Page changes: the shown page stays while the next one reads its first data,
// then one motion (changePage) shows it.
async function navigation(page = "main", invoke = undefined) {
  const traffic = createTrafficViews({ invoke: async (_, { minutes, scope }) => ({ minutes, credentials: [] }) });
  await traffic.local(30, "model");
  let ui;
  const deviceList = createDeviceList({
    invoke: async () => [], Channel: class {}, trafficViews: traffic, page: () => ui.page,
    minutes: () => ui.deviceTrafficMinutes, snap: () => ui.snap, render() {}, toast() {},
  });
  deviceList.load = () => {};
  const rendered = [];
  const changes = [];
  const loads = [];
  const p = scripts.panel({
    invoke,
    before(context) {
      Object.assign(context, {
        createTrafficViews: () => traffic,
        createDeviceList: () => deviceList,
        createActivityPager: () => ({ resolveRange() {}, load: () => new Promise((resolve) => loads.push(resolve)) }),
        createPageMotion: () => ({
          install() {}, watchPage() {}, moving: () => false, queueFit() {},
          changePage: (update) => { changes.push({ page: ui.page, traffic: traffic.activity, homeTraffic: traffic.home, mergedData: deviceList.merged }); update(); },
        }),
      });
    },
  });
  ui = Object.assign(p.get("ui"), { page, snap: { settings: {} } });
  const { context } = p;
  const original = { refreshLaunchAtLogin: context.refreshLaunchAtLogin };
  Object.assign(context, {
    renderTop() {}, renderPage: () => rendered.push(ui.page),
    loadTraffic() {}, loadHomeTraffic() {}, refreshLaunchAtLogin() {}, homeTrafficDevice: () => null, scrollToLog() {}, toast() {},
  });
  context.deviceList = deviceList;
  const go = (target) => context.act("page", { dataset: { page: target } });
  return { ui, traffic, rendered, changes, loads, render: context.render, go, context, original };
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

test("a superseded page change reads nothing for the page that replaced it", async () => {
  const p = await navigation();
  const homeReads = [];
  p.context.loadHomeTraffic = () => homeReads.push(p.ui.page);
  const activity = p.go("activity");
  await Promise.resolve();
  await p.go("main");
  p.loads[0]();
  await activity;
  assert.deepEqual(homeReads, ["main"]);
});

test("Settings opens even when Launch at login cannot be read", async () => {
  const p = await navigation("main", async (command) => {
    if (command === "refresh_launch_at_login") throw "Launch at login unavailable";
  });
  p.context.refreshLaunchAtLogin = p.original.refreshLaunchAtLogin;
  await p.go("settings");
  assert.deepEqual(p.changes.map((c) => c.page), ["settings"]);
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
  const p = scripts.panel({ before: (context) => { context.createTrafficViews = () => traffic; } });
  const ui = Object.assign(p.get("ui"), { deviceTrafficMinutes: 30 });
  const { context } = p;
  const reads = [traffic.loadActivity(), traffic.loadHome(null)];
  Object.assign(traffic, { activityError: "old", homeError: "old", homeFetchedAt: 5 });
  context.setTrafficMinutes(360);
  assert.deepEqual([traffic.minutes, ui.deviceTrafficMinutes], [360, 360]);
  assert.deepEqual([traffic.activityLoading, traffic.homeLoading, traffic.activityError, traffic.homeError, traffic.homeFetchedAt], [false, false, "", "", 0]);
  // Reads still running for the previous range are superseded.
  for (const resolve of pending) resolve({ credentials: [] });
  assert.deepEqual(await Promise.all(reads), [false, false]);
  // Every range control on the three pages calls it.
  for (const id of ["devices-traffic-range", "home-traffic-range", "traffic-range"]) {
    const chosen = [];
    context.setTrafficMinutes = (minutes) => chosen.push(minutes);
    // The handlers go on to read traffic, which nothing answers here.
    p.fire("change", { target: Object.assign(scripts.element(id), { value: "720" }) });
    assert.deepEqual(chosen, [720], id);
  }
});

test("a category change ends the Activity read it supersedes", async () => {
  const pending = [];
  const traffic = createTrafficViews({ invoke: () => new Promise((resolve) => pending.push(resolve)) });
  const read = traffic.loadActivity();
  traffic.setScope("all");
  assert.equal(traffic.activityLoading, false);
  pending[0]({ credentials: [] });
  assert.equal(await read, false);
  assert.equal(traffic.activityLoading, false);
});

test("Home rereads its traffic at once when forwarding changes its source", async () => {
  const p = scripts.panel({
    invoke: async (command) => command === "get_state" ? { config: {}, forwarding: { deviceId: "remote" } } : { rows: [] },
  });
  const ui = Object.assign(p.get("ui"), { page: "main" });
  const forces = [];
  Object.assign(p.context, { render() {}, tagColors: () => ({}), loadHomeTraffic: (force) => forces.push(force) });
  // This machine's traffic was read just now, and its read is still running.
  Object.assign(p.get("trafficViews"), { home: { device: null }, homeFetchedAt: Date.now(), homeLoading: true });
  await p.context.refresh();
  assert.deepEqual(forces, [true]);
});

test("Home's forced read of a source is not repeated while it runs", async () => {
  const pending = [];
  const traffic = createTrafficViews({ invoke: () => new Promise((resolve) => pending.push(resolve)) });
  const first = traffic.loadHome(null);
  const forced = traffic.loadHome("remote", true);
  assert.equal(await traffic.loadHome("remote", true), false);
  assert.equal(pending.length, 2);
  pending.forEach((resolve) => resolve({ credentials: [] }));
  assert.deepEqual([await first, await forced], [false, true]);
});

test("page changes move only what is shown, not content of a closed disclosure", () => {
  const context = scripts.load(scripts.sandbox({
    getComputedStyle: () => ({ getPropertyValue: () => "", fontSize: "11px" }),
    matchMedia: () => ({ matches: false }), navigator: { userAgent: "Linux" },
  }), "page_motion.js");
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
