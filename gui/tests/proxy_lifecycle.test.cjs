const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const { test } = require('node:test');
const source = fs.readFileSync(path.join(__dirname, '../ui/app.js'), 'utf8');

test('pending proxy operations remain visible and disable duplicate power requests', () => {
  const ui = { snap: { phase: { state: 'running', busy: true, port: 8787 }, config: { exists: true } } };
  const context = { ui, block: (_, head, body) => head + body, stat: () => '', fmtUptime: String, uptime: () => 0, message: (_, text) => text, esc: String };
  vm.createContext(context);
  vm.runInContext(source.slice(source.indexOf('function proxyBlock('), source.indexOf('\nfunction chart(')), context);
  assert.match(context.proxyBlock(), /Updating/);
  assert.match(context.proxyBlock(), /data-action="power" disabled/);
  ui.snap.phase.busy = false;
  assert.doesNotMatch(context.proxyBlock(), /disabled/);
  ui.snap.phase.error = 'Restart failed';
  assert.match(context.proxyBlock(), /Restart failed/);
});

test('a failed asynchronous restart does not display success', async () => {
  const messages = [];
  let refreshed = false;
  const context = { ui: { snap: {} }, invoke: async () => { throw new Error('stop failed'); }, toast: text => messages.push(text), refresh: async () => { refreshed = true; } };
  vm.createContext(context);
  vm.runInContext(source.slice(source.indexOf('async function act('), source.indexOf('// Any scroll or input')), context);
  await context.act('restart');
  assert.deepEqual(messages, ['Error: stop failed']);
  assert.equal(refreshed, true);
});

test('tray updates dispatch before taking locks or calling native setters', () => {
  const tray = fs.readFileSync(path.join(__dirname, '../src/tray.rs'), 'utf8');
  const sync = tray.match(/pub fn sync\(app: &AppHandle\) \{[\s\S]*?\r?\n\}/)[0];
  // Stop/restart completion and state notifications can arrive simultaneously
  // on worker and UI threads. Neither may hold the view lock while dispatching.
  assert.match(sync, /run_on_main_thread\(move \|\| sync_on_main_thread\(&handle\)\)/);
  assert.doesNotMatch(sync, /\.lock\(|\.state::<|\.try_state::<|\.set_(?:icon|text|tooltip|enabled)\(/);
  const update = tray.match(/fn sync_on_main_thread\(app: &AppHandle\) \{[\s\S]*?\r?\n\}/)[0];
  assert.match(update, /tray\.view\.lock\(/);
  assert.match(update, /tray\.icon\.set_icon\(/);
  // The main-thread implementation must remain private, with its only call
  // originating inside the dispatcher above.
  assert.doesNotMatch(tray, /pub(?:\([^)]*\))? fn sync_on_main_thread/);
  assert.equal((tray.match(/sync_on_main_thread\(/g) || []).length, 2);
});
