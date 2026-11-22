// Renders rebuild the panel's markup on every state refresh. Focus on a
// control must survive that, so Tab goes on from where it was instead of
// starting over from wherever the browser puts it.
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const { test } = require("node:test");
const vm = require("node:vm");

const source = fs.readFileSync(path.join(__dirname, "../ui/app.js"), "utf8");
const from = source.indexOf("function render() {");
const to = source.indexOf("function renderTop() {", from);
assert.ok(from >= 0 && to > from, "render … renderTop");
const helpers = source.slice(from, to);

let context;

/// A control: `attrs` as written in the markup.
function control(tagName, attrs = {}, { disabled = false } = {}) {
  const el = {
    tagName,
    disabled,
    isConnected: true,
    root: null,
    attributes: Object.entries(attrs).map(([name, value]) => ({ name, value: String(value) })),
    getAttribute: (name) => (name in attrs ? String(attrs[name]) : null),
    closest: (selector) => (selector === "#top, #content" ? el.root : null),
    focus() { context.document.activeElement = el; },
  };
  return el;
}

/// A rebuilt part of the panel holding `controls`; rebuilding detaches the
/// old nodes and leaves focus on the body, as setting innerHTML does.
function container(id) {
  const box = { id, controls: [], closest: () => box, querySelectorAll: () => box.controls };
  box.fill = (controls) => {
    for (const old of box.controls) old.isConnected = false;
    if (box.controls.includes(context.document.activeElement)) context.document.activeElement = context.document.body;
    for (const c of controls) c.root = box;
    box.controls = controls;
  };
  return box;
}

function panel(page, top, content) {
  const body = { closest: () => null };
  const parts = { top: container("top"), content: container("content") };
  context = {
    ui: { snap: {}, page, builtPage: page },
    openSelect: null,
    queueFit() {},
    document: { body, activeElement: body },
    $: (id) => parts[id] ?? null,
    // Each render builds new nodes, as the real one does from markup.
    renderTop: () => parts.top.fill(top()),
    renderPage: () => {
      context.ui.builtPage = context.ui.page;
      parts.content.fill(content());
      context.afterRender?.();
    },
  };
  vm.createContext(context);
  vm.runInContext(`${helpers}\nthis.render = render;`, context);
  context.render();
  return { parts, render: () => context.render(), focused: () => context.document.activeElement };
}

const settings = (enabled = true) => () => [
  control("BUTTON", { "data-action": "probe", "data-name": "a", "data-tip": "Checked now" }),
  control("BUTTON", { "data-action": "probe", "data-name": "b", "data-tip": "Checked now" }),
  control("INPUT", { type: "checkbox", "data-setting": "launch", "data-enabled": enabled }),
  control("BUTTON", { "data-action": "remove", "data-id": 1 }),
  control("BUTTON", { "data-action": "remove", "data-id": 1 }),
];
const back = () => [control("BUTTON", { "data-action": "page", "data-page": "main", "data-morph": "back" })];

test("a control keeps focus when a refresh rebuilds the page", () => {
  const p = panel("settings", back, settings());
  p.parts.content.controls[1].focus();
  p.render();
  const focused = p.focused();
  assert.equal(focused.isConnected, true);
  assert.equal(p.parts.content.controls.indexOf(focused), 1);
  assert.equal(focused.getAttribute("data-name"), "b");
  // The Back button in the rebuilt top bar too.
  p.parts.top.controls[0].focus();
  p.render();
  assert.equal(p.focused(), p.parts.top.controls[0]);
});

test("a control is found again though what it shows changed", () => {
  let enabled = true;
  const p = panel("settings", back, () => settings(enabled)());
  p.parts.content.controls[2].focus();
  enabled = false;
  p.render();
  assert.equal(p.focused(), p.parts.content.controls[2]);
  assert.equal(p.focused().getAttribute("data-enabled"), "false");
});

test("of controls alike, the same one keeps focus", () => {
  const p = panel("settings", back, settings());
  p.parts.content.controls[4].focus();
  p.render();
  assert.equal(p.parts.content.controls.indexOf(p.focused()), 4);
});

test("a control gone falls back to the tab stop in its place", () => {
  let rows = settings()();
  const p = panel("settings", back, () => rows);
  p.parts.content.controls[1].focus();
  // Rebuilt without it; its place now holds a disabled control Tab skips.
  rows = [
    control("BUTTON", { "data-action": "probe", "data-name": "a" }),
    control("BUTTON", { "data-action": "probe", "data-name": "c" }, { disabled: true }),
    control("BUTTON", { "data-action": "skip", tabindex: -1 }),
    control("INPUT", { type: "checkbox", "data-setting": "launch" }),
  ];
  p.render();
  assert.equal(p.focused().getAttribute("data-setting"), "launch");
});

test("a menu button is found again though its items changed", () => {
  let counts = [1, 2];
  const menus = () => counts.map((n) => control("BUTTON", { type: "button", "data-menu": JSON.stringify([{ label: `${n} requests` }]) }));
  let extra = [];
  const p = panel("activity", back, () => [...extra, ...menus()]);
  p.parts.content.controls[1].focus();
  counts = [3, 4];
  extra = [control("BUTTON", { "data-action": "new" })];
  p.render();
  assert.equal(p.focused().getAttribute("data-menu"), JSON.stringify([{ label: "4 requests" }]));
});

test("a field the render refocused itself keeps focus", () => {
  const p = panel("settings", back, () => [
    control("BUTTON", { "data-action": "probe", "data-name": "a" }),
    control("INPUT", { id: "port", "data-config": "listen_port" }),
  ]);
  p.parts.content.controls[0].focus();
  context.afterRender = () => p.parts.content.controls[1].focus();
  p.render();
  assert.equal(p.focused().getAttribute("id"), "port");
});

test("another page starts afresh", () => {
  const p = panel("main", () => [control("BUTTON", { "data-action": "page", "data-page": "settings" })], settings());
  p.parts.top.controls[0].focus();
  context.ui.page = "settings";
  p.render();
  assert.equal(p.focused(), context.document.body);
});

test("focus elsewhere is left alone", () => {
  const p = panel("settings", back, settings());
  p.render();
  assert.equal(p.focused(), context.document.body);
});
