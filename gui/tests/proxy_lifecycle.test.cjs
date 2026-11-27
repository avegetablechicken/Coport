const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const { test } = require('node:test');
const source = fs.readFileSync(path.join(__dirname, '../ui/app.js'), 'utf8');
const { createLatest } = require('../ui/latest.js');

test('pending proxy operations remain visible and disable duplicate power requests', () => {
  const ui = { snap: { phase: { state: 'running', busy: true, port: 8787 }, config: { exists: true } } };
  const context = { ui, block: (_, head, body) => head + body, stat: () => '', fmtUptime: String, uptime: () => 0, message: (_, text) => text, esc: String };
  vm.createContext(context);
  vm.runInContext(source.slice(source.indexOf('function proxyBlock('), source.indexOf('/// Home shows the forwarded device')), context);
  assert.match(context.proxyBlock(), /Updating/);
  assert.match(context.proxyBlock(), /data-action="power" disabled/);
  ui.snap.phase.busy = false;
  assert.doesNotMatch(context.proxyBlock(), /disabled/);
  ui.snap.phase.error = 'Restart failed';
  assert.match(context.proxyBlock(), /Restart failed/);
});

test('a failed phase names no cause beyond its error', () => {
  const ui = { snap: { phase: { state: 'failed', port: 8787 }, config: { exists: true } } };
  const context = { ui, block: (_, head, body) => head + body, stat: () => '', fmtUptime: String, uptime: () => 0, message: (_, text) => text, esc: String };
  vm.createContext(context);
  vm.runInContext(source.slice(source.indexOf('function proxyBlock('), source.indexOf('/// Home shows the forwarded device')), context);
  // Lost contact with a running daemon is not a failed start.
  ui.snap.phase.error = 'Lost contact with the proxy daemon. Restart to reconnect.';
  const html = context.proxyBlock();
  assert.match(html, /dot failed/);
  assert.match(html, /Status unavailable/);
  assert.match(html, /Lost contact with the proxy daemon/);
  assert.doesNotMatch(html, /Failed to start/);
});

test('a proxy CA missing hosts warns with what still works, then shrinks once kept', () => {
  const ui = { snap: { phase: { state: 'running', port: 8787 }, config: { exists: true } } };
  const context = {
    ui, block: (_, head, body) => head + body, stat: () => '', fmtUptime: String, uptime: () => 0,
    message: (kind, text, actions = '') => `[${kind}] ${text} ${actions}`,
    esc: (v) => String(v).replace(/</g, '&lt;'),
  };
  vm.createContext(context);
  vm.runInContext(source.slice(source.indexOf('function proxyBlock('), source.indexOf('/// Home shows the forwarded device')), context);
  assert.doesNotMatch(context.proxyBlock(), /Proxy CA/);
  ui.snap.proxyCa = {
    decrypted: ['api.anthropic.com:443', 'chatgpt.com:443'],
    uncovered: ['sharecoder.cc:443', '182.92.106.196:6060'],
    unrestricted: false,
    kept: false,
  };
  const warning = context.proxyBlock();
  assert.match(warning, /\[warn\] Proxy CA doesn't cover <strong>sharecoder\.cc<\/strong>, <strong>182\.92\.106\.196:6060<\/strong>/);
  assert.match(warning, /Still working: base URLs and the proxy port for 2 other hosts\. Proxy-port clients can't reach these hosts\./);
  assert.match(warning, /data-action="proxy-ca-replace">Replace &amp; Trust…/);
  assert.match(warning, /data-action="proxy-ca-keep"/);
  assert.doesNotMatch(warning, /isn't limited/);
  ui.snap.proxyCa.kept = true;
  const kept = context.proxyBlock();
  assert.match(kept, /\[info\] Proxy port skips <strong>sharecoder\.cc<\/strong>, <strong>182\.92\.106\.196:6060<\/strong>; base URLs still work\./);
  assert.match(kept, /class="text-link" data-action="proxy-ca-replace">Replace CA…/);
  assert.doesNotMatch(kept, /\[warn\]|proxy-ca-keep/);
  ui.snap.proxyCa = { decrypted: ['chatgpt.com:443'], uncovered: [], unrestricted: true, kept: false };
  const unrestricted = context.proxyBlock();
  assert.match(unrestricted, /\[warn\] <strong>Proxy CA isn't limited to its hosts\.<\/strong>.*Everything still works/);
  ui.snap.proxyCa = { decrypted: [], uncovered: ['<x>.example:443'], unrestricted: true, kept: false };
  const escaped = context.proxyBlock();
  assert.match(escaped, /<strong>&lt;x>\.example<\/strong>/);
  assert.match(escaped, /Still working: base URLs\. Proxy-port clients can't reach this host\. The CA also isn't limited to its hosts\./);
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

test('Settings shows applied values until restart, rather than pending file values', () => {
  const ui = { snap: { config: {
    editable: { listen_port: 9999, request_timeout_seconds: 60 },
    details: { editable: { listen_port: 8787, request_timeout_seconds: 30 }, proxies: [{ name: 'running' }] },
  } } };
  const context = { ui, esc: String, block: (_, __, body) => body, ICON: { chevron: '' }, otherLogins: () => '' };
  vm.createContext(context);
  vm.runInContext(source.slice(source.indexOf('function configSettings()'), source.indexOf('const OTHER_LOGIN_PROVIDERS')), context);
  const html = context.configSettings();
  assert.match(html, /id="config-listen_port"[^>]*value="8787"/);
  assert.match(html, /id="config-request_timeout_seconds"[^>]*value="30"/);
  assert.equal(ui.snap.config.details.editable.listen_port, 8787);
  assert.deepEqual(ui.snap.config.details.proxies, [{ name: 'running' }]);
});

test('Restart publishes new proxies before announcing success, including concurrent refreshes', async () => {
  const pending = [];
  const messages = [];
  const rendered = [];
  const snap = name => ({ config: { details: { proxies: [{ name }] } } });
  const ui = { page: 'settings', snap: snap('old'), proxies: { old: { name: 'old' } } };
  const context = {
    ui, tagColors: () => ({}), refreshes: createLatest(), deviceList: { loaded: false }, trafficViews: { homeFetchedAt: Date.now(), home: null, cached: () => null },
    homeTrafficDevice: () => null, loadHomeTraffic() {},
    panelMotion: { changePage: update => update(), queueFit() {} }, $: () => ({ scrollTop: 0 }),
    invoke: async command => {
      if (command === 'get_state') return new Promise(resolve => pending.push(resolve));
      if (command === 'get_activity') return { rows: [] };
      assert.equal(command, 'restart_proxy');
    },
    toast: message => messages.push([message, Object.keys(ui.proxies)]),
    render: () => rendered.push(Object.keys(ui.proxies)),
  };
  vm.createContext(context);
  vm.runInContext(source.slice(source.indexOf('async function refresh()'), source.indexOf('let refreshTimer;')), context);
  vm.runInContext(source.slice(source.indexOf('async function act('), source.indexOf('// Any scroll or input')), context);
  const old = context.refresh();
  const restarting = context.act('restart');
  await new Promise(setImmediate);
  assert.equal(pending.length, 2);
  assert.deepEqual(messages, []);
  // A state-changed event starts a later read before Restart's read returns.
  const eventRefresh = context.refresh();
  pending[1](snap('new'));
  await restarting;
  assert.deepEqual(messages, [['Proxy restarted', ['new']]]);
  assert.deepEqual(rendered, [['new']]);
  // Returning Home immediately uses the new configuration, without a timer.
  await context.act('page', { dataset: { page: 'main' } });
  assert.equal(ui.page, 'main');
  assert.deepEqual(rendered.at(-1), ['new']);
  // An older response cannot put the pre-restart config back on screen.
  pending[0](snap('old'));
  await old;
  assert.deepEqual(Object.keys(ui.proxies), ['new']);
  pending[2](snap('new'));
  await eventRefresh;
  // A second restart in this same panel also publishes its new config first.
  const again = context.act('restart');
  await new Promise(setImmediate);
  pending[3](snap('newer'));
  await again;
  assert.deepEqual(messages[1], ['Proxy restarted', ['newer']]);
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
  // Turn frames are rendered on a worker, which only dispatches them and
  // never takes the view lock or calls native setters; the main thread shows
  // them under the view lock. The menu bar's appearance is read there too.
  for (const name of ['turn', 'play', 'menu_bar_is_dark']) {
    const worker = tray.match(new RegExp(`fn ${name}\\(app: &AppHandle[^)]*\\)[^{]*\\{[\\s\\S]*?\\r?\\n\\}`))[0];
    assert.doesNotMatch(worker, /\.view\.lock\(|\.set_(?:icon|text|tooltip|enabled)\w*\(/, name);
  }
  const play = tray.match(/fn play\(app: &AppHandle, service: Service\) \{[\s\S]*?\r?\n\}/)[0];
  assert.match(play, /run_on_main_thread\(move \|\| show_frame\(&h, Some\(frame\)\)\)/);
  const appearance = tray.match(/fn menu_bar_is_dark\(app: &AppHandle\) -> bool \{[\s\S]*?\r?\n\}/)[0];
  assert.match(appearance, /run_on_main_thread\(move \|\|[\s\S]*with_inner_tray_icon/);
  const frame = tray.match(/fn show_frame\(app: &AppHandle, frame: Option<Image<'static>>\) \{[\s\S]*?\r?\n\}/)[0];
  assert.match(frame, /tray\.view\.lock\(/);
  assert.match(frame, /tray\s*\.icon\s*\.set_icon_with_as_template\(/);
  assert.equal((tray.match(/show_frame\(/g) || []).length, 3);
});
