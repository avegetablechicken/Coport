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
    invoke: command => probes.push(command),
    setInterval() {},
  });
  return { ui, content, rendered, events, probes, advance: (ms) => { now += ms; } };
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


test("startup and repeated panel reopening do not automatically test proxies", async () => {
  const p = panel();
  await Promise.resolve();
  for (let i = 0; i < 5; i++) {
    p.events['panel-hidden']();
    p.advance(timeout * 3);
    await p.events['panel-shown']();
  }
  assert.deepEqual(p.probes, []);
});
