// A panel menu lights exactly one item, as a native menu does: the item
// focused on opening (the current choice) must not stay lit once the
// pointer moves to another one.
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const { test } = require("node:test");

const ui = path.join(__dirname, "../ui");
const scripts = require("./panel_harness.cjs");

/// A select of `names` opened through the panel's menus, its first choice
/// current.
function menuWith(names) {
  const listeners = {};
  const document = { activeElement: null, body: { append() {} } };
  const items = names.map((name) => {
    const item = {
      name,
      value: name,
      closest: (selector) => (selector === "button" ? item : null),
      focus(options) { item.options = options; document.activeElement = item; },
    };
    return item;
  });
  const box = { width: 120, height: 80, right: 200, bottom: 20 };
  const menu = {
    style: {},
    children: items,
    set innerHTML(_) {},
    setAttribute() {},
    addEventListener: (type, handler) => { listeners[type] = handler; },
    getBoundingClientRect: () => box,
  };
  document.createElement = () => menu;
  const trigger = {
    value: names[0],
    dataset: { options: JSON.stringify(names.map((name) => [name, name])) },
    getAttribute: () => "Range",
    setAttribute() {},
    getBoundingClientRect: () => box,
  };
  const microtasks = [];
  const context = scripts.load(scripts.sandbox({ queueMicrotask: (fn) => microtasks.push(fn) }), "panel_format.js", "menus.js");
  const rerendered = [];
  const menus = context.createMenus({ document, window: { innerWidth: 300, innerHeight: 400 }, rerender: () => rerendered.push(true) });
  Object.assign(menu, { remove() {} });
  Object.assign(trigger, { removeAttribute() {}, isConnected: false });
  menus.showSelect(trigger);
  return { document, items, menus, microtasks, rerendered, move: (target) => listeners.pointermove({ target }) };
}

test("the item under the pointer takes focus from the current choice", () => {
  const m = menuWith(["30 minutes", "6 hours", "1 day"]);
  m.items[0].focus();
  m.move(m.items[2]);
  assert.equal(m.document.activeElement, m.items[2]);
  assert.equal(JSON.stringify(m.items[2].options), JSON.stringify({ preventScroll: true }));
});

test("moving over a separator or heading leaves focus where it is", () => {
  const m = menuWith(["Copy", "Open"]);
  m.items[1].focus();
  m.move({ closest: () => null });
  assert.equal(m.document.activeElement, m.items[1]);
});

test("a render held back while a menu is open runs once it closes", () => {
  const m = menuWith(["30 minutes", "6 hours"]);
  assert.equal(m.menus.holdRender(), true);
  assert.equal(m.menus.holdRender(), true);
  m.menus.close();
  assert.equal(m.menus.current(), null);
  // One redraw for every render held back, after the menu is gone.
  assert.equal(m.microtasks.length, 1);
  m.microtasks.shift()();
  assert.equal(m.rerendered.length, 1);
  assert.equal(m.menus.holdRender(), false);
  m.menus.close();
  assert.equal(m.microtasks.length, 0);
});

test("hover alone does not light an item; focus does", () => {
  const css = fs.readFileSync(path.join(ui, "style.css"), "utf8");
  assert.doesNotMatch(css, /\.select-menu button:hover/);
  assert.match(css, /\.select-menu button:focus \{[^}]*background: var\(--accent\)/);
});
