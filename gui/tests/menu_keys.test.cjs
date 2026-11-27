// Keys on a panel opened by keyboard navigation of the menu bar (macOS 27):
// ←/→ it does not use move the navigation to the neighbouring item; every
// other key, ↑/↓ included, keeps the meaning it has after a click.
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const { test } = require("node:test");
const vm = require("node:vm");

const source = fs.readFileSync(path.join(__dirname, "../ui/app.js"), "utf8");
const between = (start, end) => {
  const from = source.indexOf(start);
  const to = source.indexOf(end, from);
  assert.ok(from >= 0 && to > from, `${start} … ${end}`);
  return source.slice(from, to);
};
const helpers = between("/// Whether keys on `el` edit text", 'document.addEventListener("keydown"');
const keydown = between('document.addEventListener("keydown"', 'document.addEventListener("focusout"');
const shown = between("let panelHiddenAt", "refresh().then(probeStale);");

/// An element; `text` makes it a text field, whose `closest` matches only the
/// text-entry selector.
function element(name, { text = false, select = false } = {}) {
  const el = {
    name, blurred: 0, dataset: {},
    closest: (selector) => (text && selector.startsWith("textarea") ? el : null),
    matches: () => select,
    focus() { context.document.activeElement = el; },
    blur() { el.blurred += 1; context.document.activeElement = context.document.body; },
  };
  return el;
}

let context;
function panel({ navigating = true } = {}) {
  const listeners = {};
  const events = {};
  const invoked = [];
  const opened = [];
  const body = element("body");
  context = {
    IS_MAC: true,
    ui: { page: "main", menuBarNavigation: navigating },
    deviceList: { loaded: false },
    hideRouteTooltip() {},
    menus: {
      open: null, current() { return this.open; }, close() {}, items: () => [],
      showSelect: (trigger, last) => opened.push([trigger.name, last]),
    },
    PANEL_PAGE_TIMEOUT_MS: 60 * 1000,
    invoke: (command, args) => { invoked.push([command, args]); return Promise.resolve(); },
    act() {},
    listen: (name, handler) => { events[name] = handler; },
    refresh: async () => {},
    $: () => null,
    document: {
      body,
      activeElement: body,
      addEventListener: (type, handler) => { listeners[type] = handler; },
    },
    Date,
  };
  vm.createContext(context);
  vm.runInContext(`${helpers}\n${keydown}\n${shown}`, context);
  const press = (key, target = context.document.activeElement, modifiers = {}) => {
    const event = { key, target, defaultPrevented: false, preventDefault() { this.defaultPrevented = true; }, ...modifiers };
    listeners.keydown(event);
    return event;
  };
  // Calls made inside the context compare as plain data.
  return {
    context, events, press,
    get invoked() { return JSON.parse(JSON.stringify(invoked)); },
    get opened() { return JSON.parse(JSON.stringify(opened)); },
  };
}

test("left and right move menu bar navigation to the neighbouring item", () => {
  const p = panel();
  assert.ok(p.press("ArrowRight").defaultPrevented);
  assert.ok(p.press("ArrowLeft", element("button")).defaultPrevented);
  assert.deepEqual(p.invoked, [
    ["navigate_menu_bar", { right: true }],
    ["navigate_menu_bar", { right: false }],
  ]);
});

test("up and down keep their meaning, as after a click", () => {
  const p = panel();
  for (const key of ["ArrowUp", "ArrowDown"]) {
    assert.equal(p.press(key).defaultPrevented, false, key);
  }
  // ↓ on a select still opens it.
  assert.ok(p.press("ArrowDown", element("trigger", { select: true })).defaultPrevented);
  assert.deepEqual(p.opened, [["trigger", false]]);
  assert.deepEqual(p.invoked, []);
});

test("left and right stay in a text field, an open select and with modifiers", () => {
  const p = panel();
  const search = element("search", { text: true });
  for (const key of ["ArrowLeft", "ArrowRight"]) {
    assert.equal(p.press(key, search).defaultPrevented, false, key);
  }
  for (const modifier of ["metaKey", "altKey", "shiftKey", "ctrlKey"]) {
    p.press("ArrowRight", undefined, { [modifier]: true });
  }
  p.press("ArrowRight", undefined, { isComposing: true });
  p.context.menus.open = { trigger: element("trigger"), menu: {} };
  p.press("ArrowRight");
  assert.deepEqual(p.invoked, []);
});

test("a clicked-open panel keeps left and right", () => {
  const p = panel({ navigating: false });
  for (const key of ["ArrowLeft", "ArrowRight"]) {
    assert.equal(p.press(key).defaultPrevented, false, key);
  }
  assert.deepEqual(p.invoked, []);
});

test("navigation follows the panel's show and hide notices", async () => {
  const p = panel({ navigating: false });
  await p.events["panel-shown"]({ payload: { keyboard: true } });
  assert.equal(p.context.ui.menuBarNavigation, true);
  p.events["panel-hidden"]();
  assert.equal(p.context.ui.menuBarNavigation, false);
  // Clicked open, or shown some other way (a dialog's return, other systems).
  await p.events["panel-shown"]({ payload: { keyboard: false } });
  assert.equal(p.context.ui.menuBarNavigation, false);
  await p.events["panel-shown"]();
  assert.equal(p.context.ui.menuBarNavigation, false);
});

test("opening by keyboard leaves a focused text field", async () => {
  const p = panel({ navigating: false });
  const search = element("search", { text: true });
  search.focus();
  await p.events["panel-shown"]({ payload: { keyboard: false } });
  assert.equal(search.blurred, 0);
  await p.events["panel-shown"]({ payload: { keyboard: true } });
  assert.equal(search.blurred, 1);
  const button = element("button");
  button.focus();
  await p.events["panel-shown"]({ payload: { keyboard: true } });
  assert.equal(p.context.document.activeElement, button);
});
