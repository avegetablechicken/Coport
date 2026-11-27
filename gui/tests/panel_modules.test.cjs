const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { test } = require('node:test');
const { createLatest } = require('../ui/latest.js');
const { createDeviceForm } = require('../ui/device_form.js');
const { createActivityPager } = require('../ui/activity_pager.js');
const scripts = require('./panel_harness.cjs');

const ui = path.join(__dirname, '../ui');

test('the panel loads its scripts in an order where each finds what it uses', () => {
  const order = scripts.scripts();
  // Every script in the folder is loaded, app.js last.
  assert.deepEqual([...order].sort(), fs.readdirSync(ui).filter((f) => f.endsWith('.js')).sort());
  assert.equal(order.at(-1), 'app.js');
  // Anything the page touches while loading answers with more of itself.
  const anything = new Proxy(function () {}, {
    get: (_, key) => (key === 'then' ? undefined : key === Symbol.toPrimitive ? () => '' : anything),
    apply: () => anything,
    construct: () => anything,
  });
  const never = () => new Promise(() => {});
  const context = scripts.sandbox({
    window: { __TAURI__: { core: { invoke: never, Channel: class {} }, event: { listen: never } }, addEventListener() {}, matchMedia: () => anything },
    navigator: { userAgent: 'Linux' }, document: anything, getComputedStyle: () => anything,
    MutationObserver: class { observe() {} }, ResizeObserver: class { observe() {} },
    matchMedia: () => anything, setTimeout: () => 0, clearTimeout() {}, setInterval: () => 0, requestAnimationFrame: () => 0, queueMicrotask() {},
  });
  for (const script of order) assert.doesNotThrow(() => scripts.load(context, script), script);
});

test('only the newest read is current, and a read in flight is found by its key', () => {
  const reads = createLatest();
  const first = reads.begin('a');
  assert.equal(reads.loading('a'), true);
  assert.equal(reads.loading('b'), false);
  const second = reads.begin('b');
  assert.deepEqual([first.current(), second.current()], [false, true]);
  // The older read ending leaves the newer one in flight.
  first.done();
  assert.equal(reads.loading('b'), true);
  second.done();
  assert.equal(reads.loading('b'), false);
  // Cancelling supersedes every read so far.
  const third = reads.begin('c');
  reads.cancel();
  assert.equal(third.current(), false);
  assert.equal(reads.loading('c'), false);
});

test('a result lands unless a newer one already did, whatever order they finish in', () => {
  const reads = createLatest();
  const [older, newer, newest] = [reads.begin(), reads.begin(), reads.begin()];
  assert.equal(newer.apply(), true);
  assert.equal(older.apply(), false);
  assert.equal(newest.apply(), true);
  assert.equal(newer.apply(), false);
});

/// A form showing `fields`, whose values `capture` reads.
function shown(form, fields) {
  form.capture((id) => fields[id]);
}

test('an SSH device saves its host, named after it by default, and no HTTP source', () => {
  const form = createDeviceForm();
  form.add();
  assert.deepEqual([form.open, form.draft.transport], [true, 'ssh']);
  shown(form, { 'device-name': ' ', 'device-transport': 'ssh', 'device-host': ' mbp16 ', 'device-binary': '' });
  assert.deepEqual(form.device(), {
    id: null, name: 'mbp16', ssh: { host: 'mbp16', binary: '' },
    data: { transport: 'ssh', url: '', tokenFile: null, tokenEnv: null, caCertificate: null },
  });
  form.close();
  assert.deepEqual([form.open, form.draft], [false, null]);
});

test('switching HTTP to SSH never saves the HTTP fields typed before', () => {
  const form = createDeviceForm();
  form.add();
  shown(form, { 'device-name': '', 'device-transport': 'http', 'device-url': 'https://old.example', 'device-key-file': '/old.key', 'device-key-env': '', 'device-ca-file': '' });
  // The SSH fields replace the HTTP ones on screen; the draft keeps what was typed.
  shown(form, { 'device-name': '', 'device-transport': 'ssh', 'device-host': 'MS', 'device-binary': '' });
  assert.equal(form.draft.url, 'https://old.example');
  const device = form.device();
  assert.deepEqual([device.name, device.ssh.host, device.data.url, device.data.tokenFile], ['MS', 'MS', '', null]);
});

test('an HTTP device saves its source and credentials and no SSH connection', () => {
  const form = createDeviceForm();
  form.edit({ id: 'peer', name: 'Peer', data: { transport: 'http', url: 'https://data.example', tokenFile: '/data.key', tokenEnv: null, caCertificate: '/ca.pem' } });
  assert.deepEqual([form.open, form.draft.transport, form.draft.url], [true, 'http', 'https://data.example']);
  // Nothing is shown yet: the draft stays as edited.
  form.capture(() => undefined);
  assert.equal(form.draft.key, '/data.key');
  shown(form, { 'device-name': 'Peer', 'device-transport': 'http', 'device-url': 'https://data.example', 'device-key-file': '/data.key', 'device-key-env': ' ', 'device-ca-file': '/ca.pem' });
  assert.deepEqual(form.device(), {
    id: 'peer', name: 'Peer', ssh: null,
    data: { transport: 'http', url: 'https://data.example', tokenFile: '/data.key', tokenEnv: null, caCertificate: '/ca.pem' },
  });
});

test('a preset range follows the clock and a past custom range is not read again', async () => {
  const minute = new Date(2026, 10, 26, 15, 30).getTime();
  let now = minute;
  const queries = [];
  const log = createActivityPager({ invoke: async (_, query) => { queries.push(query); return { rows: [], next: null }; }, show: (update) => update(), failed() {}, now: () => now });
  log.resolveRange();
  assert.deepEqual([log.from, log.to], [minute - 3600000, minute]);
  log.preset = 'yesterday';
  log.resolveRange();
  // `to` is the last minute before midnight, inclusive.
  assert.deepEqual([log.from, log.to], [new Date(2026, 10, 25).getTime(), new Date(2026, 10, 26).getTime() - 60000]);
  // A range in the past has nothing new to list.
  log.preset = 'custom';
  log.refresh();
  assert.equal(queries.length, 0);
  // A preset reaching the present is read live as the clock moves.
  log.preset = 'hour';
  log.resolveRange();
  now += 60000;
  await log.refresh();
  assert.deepEqual([queries.length, queries[0].fresh, queries[0].range.to], [1, true, now + 60000]);
});

test('an older page stays loading until its latest read ends', async () => {
  const pending = [];
  const log = createActivityPager({ invoke: () => new Promise((resolve) => pending.push(resolve)), show: (update) => update(), failed() {}, now: () => 0 });
  log.resolveRange();
  log.next = 'cursor';
  const first = log.load('more');
  // A filter changed while it loaded, then the next older page was asked for.
  log.filter = 'errors';
  const second = log.load('more');
  pending[0]({ rows: [], next: null });
  await first;
  assert.equal(log.loadingMore, true);
  pending[1]({ rows: [], next: null });
  await second;
  assert.equal(log.loadingMore, false);
});
