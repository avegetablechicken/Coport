// Loads the panel's scripts whole into a sandbox, as the WebView does: one
// shared global scope, in index.html order. Tests reach a script's functions
// as the sandbox's globals and its top-level bindings through `get`.
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const UI = path.join(__dirname, '../ui');

/// The panel's scripts in the order index.html loads them.
function scripts() {
  const html = fs.readFileSync(path.join(UI, 'index.html'), 'utf8');
  return [...html.matchAll(/<script src="([^"]+)"/g)].map((m) => m[1]);
}

/// A sandbox with `globals`, its own global object as `globalThis`.
function sandbox(globals = {}) {
  const context = { console, ...globals };
  context.globalThis = context;
  return vm.createContext(context);
}

const loaded = new WeakMap();

/// Runs `names` in `context`, in the order index.html loads them. A script
/// already loaded there is not run again, as the WebView runs each once.
function load(context, ...names) {
  const order = scripts();
  for (const name of names) assert(order.includes(name), `${name} is not a panel script`);
  const done = loaded.get(context) ?? loaded.set(context, new Set()).get(context);
  for (const name of order.filter((n) => names.includes(n) && !done.has(n))) {
    vm.runInContext(fs.readFileSync(path.join(UI, name), 'utf8'), context, { filename: name });
    done.add(name);
  }
  return context;
}

/// Loads `names` beneath what `context` already has: a test's stubs stay,
/// and the scripts give everything else.
function under(context, ...names) {
  const stubs = { ...context };
  load(context, ...names);
  return Object.assign(context, stubs);
}

/// A top-level `const` or `let` of a loaded script, which is not a property
/// of the sandbox.
function get(context, name) {
  return vm.runInContext(name, context);
}

/// A stand-in element: what the panel reads and writes on one it renders into.
function element(id) {
  return {
    id, innerHTML: '', textContent: '', hidden: false, value: '', dataset: {}, style: {}, children: [],
    scrollTop: 0, offsetHeight: 0, isConnected: true, firstElementChild: null,
    classList: { add() {}, remove() {}, toggle() {}, contains: () => false },
    setAttribute() {}, removeAttribute() {}, getAttribute: () => null, addEventListener() {},
    contains: () => false, closest: () => null, matches: () => false, focus() {},
    querySelector: () => null, querySelectorAll: () => [],
    getBoundingClientRect: () => ({ top: 0, left: 0, width: 0, height: 0, right: 0, bottom: 0 }),
  };
}

/// The whole panel, app.js included, loaded as the WebView loads it, with
/// what it reaches outside itself faked:
/// - `invoke(command, args)` answers the app; by default nothing answers.
/// - `elements` are what `$(id)` finds; any other id gets a stand-in.
/// - `document`, `window` and `globals` replace the fakes they name.
/// - `before(context)` runs after the modules load and before app.js, where
///   a test can stand in for a factory app.js calls.
/// Listeners, Tauri events, observers and timers are kept, not run: tests
/// call them as the WebView would.
function panel({ invoke = () => new Promise(() => {}), elements = {}, document, window, globals = {}, before } = {}) {
  const listeners = {};
  const windowListeners = {};
  const events = {};
  const observers = [];
  const timers = [];
  const add = (into) => (type, fn) => { (into[type] ||= []).push(fn); };
  const found = { ...elements };
  // A test's element keeps what it defines and takes the rest of a stand-in's.
  for (const [id, el] of Object.entries(found)) {
    for (const [key, value] of Object.entries(element(id))) if (!(key in el)) el[key] = value;
  }
  const body = element('body');
  const doc = document ?? {
    body, activeElement: body, hidden: false,
    documentElement: { clientWidth: 480, clientHeight: 640, classList: { add() {}, remove() {}, toggle() {} } },
    addEventListener: add(listeners),
    getElementById: (id) => (found[id] ??= element(id)),
    createElement: (tag) => element(tag), createElementNS: (_, tag) => element(tag),
    querySelector: () => null, querySelectorAll: () => [], elementFromPoint: () => null,
  };
  const media = () => ({ matches: false, addEventListener() {} });
  const win = window ?? { innerWidth: 480, innerHeight: 640, addEventListener: add(windowListeners), matchMedia: media };
  win.__TAURI__ ??= {
    core: { invoke: (command, args) => invoke(command, args), Channel: class { constructor() { this.onmessage = null; } } },
    event: { listen: (name, fn) => { (events[name] ||= []).push(fn); return Promise.resolve(() => {}); } },
  };
  const timer = (fn, ms) => timers.push({ fn, ms });
  const context = sandbox({
    window: win, document: doc, navigator: { userAgent: 'Linux' }, screen: { height: 900 },
    matchMedia: media,
    getComputedStyle: () => ({ paddingTop: '0', paddingBottom: '0', getPropertyValue: () => '', fontSize: '13px' }),
    MutationObserver: class {
      constructor(fn) { this.fn = fn; observers.push(this); }
      observe(target, options) { this.target = target; this.options = options; }
      disconnect() {}
    },
    ResizeObserver: class { observe() {} unobserve() {} disconnect() {} },
    Node: { ELEMENT_NODE: 1 },
    Event: class { constructor(type, init = {}) { Object.assign(this, { type }, init); } },
    performance: { now: () => 0 }, requestAnimationFrame: timer, cancelAnimationFrame() {},
    setTimeout: timer, clearTimeout() {}, setInterval: timer, clearInterval() {},
    queueMicrotask: (fn) => Promise.resolve().then(fn),
    ...globals,
  });
  load(context, ...scripts().filter((name) => name !== 'app.js'));
  before?.(context);
  load(context, 'app.js');
  return {
    context, document: doc, window: win, listeners, windowListeners, events, observers, timers,
    get: (name) => get(context, name),
    /// Runs the document's `type` listeners on `event`, in the order added.
    fire: (type, event) => Promise.all((listeners[type] || []).map((fn) => fn(event))),
    /// Delivers the Tauri event `name` with `payload`.
    emit: (name, payload) => Promise.all((events[name] || []).map((fn) => fn({ payload }))),
  };
}

function assert(ok, message) {
  if (!ok) throw new Error(message);
}

module.exports = { UI, scripts, sandbox, load, under, get, element, panel };
