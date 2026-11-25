// A panel menu lights exactly one item, as a native menu does: the item
// focused on opening (the current choice) must not stay lit once the
// pointer moves to another one.
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const { test } = require("node:test");
const vm = require("node:vm");

const ui = path.join(__dirname, "../ui");
const source = fs.readFileSync(path.join(ui, "app.js"), "utf8");
const from = source.indexOf("/// Opens `menu` below `trigger`");
const to = source.indexOf("/// An action menu", from);
assert.ok(from >= 0 && to > from, "placeMenu");

function menuWith(names) {
  const listeners = {};
  const document = { activeElement: null, body: { append() {} } };
  const items = names.map((name) => {
    const item = {
      name,
      closest: (selector) => (selector === "button" ? item : null),
      focus(options) { item.options = options; document.activeElement = item; },
    };
    return item;
  });
  const box = { width: 120, height: 80, right: 200, bottom: 20 };
  const menu = {
    id: "panel-select-options",
    style: {},
    addEventListener: (type, handler) => { listeners[type] = handler; },
    getBoundingClientRect: () => box,
  };
  const trigger = { setAttribute() {}, getBoundingClientRect: () => box };
  const context = { document, window: { innerWidth: 300, innerHeight: 400 }, openSelect: null };
  vm.createContext(context);
  vm.runInContext(source.slice(from, to), context);
  context.placeMenu(trigger, menu);
  return { document, items, move: (target) => listeners.pointermove({ target }) };
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

test("hover alone does not light an item; focus does", () => {
  const css = fs.readFileSync(path.join(ui, "style.css"), "utf8");
  assert.doesNotMatch(css, /\.select-menu button:hover/);
  assert.match(css, /\.select-menu button:focus \{[^}]*background: var\(--accent\)/);
});
