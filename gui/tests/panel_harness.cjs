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

function assert(ok, message) {
  if (!ok) throw new Error(message);
}

module.exports = { UI, scripts, sandbox, load, under, get };
