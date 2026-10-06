const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const { test } = require('node:test');
const source = fs.readFileSync(path.join(__dirname, '../ui/app.js'), 'utf8');
function setup() {
  const pending = [];
  const ui = { activityRequest: 0, filter: 'requests', search: 'old', searchMode: 'keyword', activityFrom: 0, activityTo: 3600000, activityNext: null, rows: [] };
  const context = { ui, invoke: (_, query) => new Promise(resolve => pending.push({ query, resolve })), renderActivityList() {}, $: () => null };
  vm.createContext(context);
  vm.runInContext(source.slice(source.indexOf('async function loadActivity('), source.indexOf('\nfunction uptime()')), context);
  return { ui, pending, load: context.loadActivity };
}
test('newer search result survives an older response', async () => {
  const { ui, pending, load } = setup();
  const old = load();
  ui.search = '200';
  ui.searchMode = 'status';
  const latest = load();
  assert.equal(pending[1].query.searchMode, 'status');
  // The To minute is inclusive.
  assert.equal(JSON.stringify(pending[1].query.range), '{"from":0,"to":3660000}');
  pending[1].resolve({ rows: ['new'], next: null });
  await latest;
  pending[0].resolve({ rows: ['old'], next: null });
  await old;
  assert.deepEqual(ui.rows, ['new']);
});
test('more appends the next page after the cursor', async () => {
  const { ui, pending, load } = setup();
  const first = load();
  pending[0].resolve({ rows: ['a'], next: { time: 1, seq: 2 } });
  await first;
  const more = load('more');
  assert.equal(JSON.stringify(pending[1].query.after), '{"time":1,"seq":2}');
  pending[1].resolve({ rows: ['b'], next: null });
  await more;
  assert.equal(JSON.stringify(ui.rows), '["a","b"]');
  assert.equal(ui.activityNext, null);
});
for (const [key, value] of [['search', 'new'], ['searchMode', 'proxy'], ['filter', 'errors'], ['activityFrom', 60000], ['activityTo', 7200000]]) {
  test(`ignores a response after ${key} changes before the next query`, async () => {
    const { ui, pending, load } = setup();
    const request = load();
    ui[key] = value;
    pending[0].resolve({ rows: ['stale'], next: null });
    await request;
    assert.deepEqual(ui.rows, []);
  });
}
const row = (time, seq) => ({ time, seq });
const seqs = (rows) => rows.map((e) => e.seq);
test('live re-reads the log and replaces a single page', async () => {
  const { ui, pending, load } = setup();
  ui.rows = [row(10, 1)];
  const live = load('live');
  assert.equal(pending[0].query.fresh, true);
  assert.equal(pending[0].query.after, null);
  pending[0].resolve({ rows: [row(20, 2), row(10, 1)], next: null });
  await live;
  assert.deepEqual(seqs(ui.rows), [2, 1]);
});
test('only live reads ask for a fresh read', async () => {
  const { pending, load } = setup();
  load();
  load('more');
  assert.deepEqual(pending.map((p) => p.query.fresh), [false, false]);
});
test('live keeps older pages below the new first page', async () => {
  const { ui, pending, load } = setup();
  const first = load();
  pending[0].resolve({ rows: [row(40, 4), row(30, 3)], next: { time: 30, seq: 3 } });
  await first;
  const more = load('more');
  pending[1].resolve({ rows: [row(20, 2), row(10, 1)], next: { time: 10, seq: 1 } });
  await more;
  // Two lines were written; the first page now ends one row newer.
  const live = load('live');
  pending[2].resolve({ rows: [row(60, 6), row(50, 5)], next: { time: 50, seq: 5 } });
  await live;
  assert.deepEqual(seqs(ui.rows), [6, 5, 4, 3, 2, 1]);
  assert.equal(JSON.stringify(ui.activityNext), '{"time":10,"seq":1}');
});
test('live drops loaded rows that left a rolling range', async () => {
  const { ui, pending, load } = setup();
  ui.rows = [row(40, 4), row(30, 3), row(20, 2), row(10, 1)];
  ui.activityNext = { time: 10, seq: 1 };
  ui.activityPaged = true;
  ui.activityFrom = 15;
  const live = load('live');
  pending[0].resolve({ rows: [row(50, 5), row(40, 4)], next: { time: 40, seq: 4 } });
  await live;
  assert.deepEqual(seqs(ui.rows), [5, 4, 3, 2]);
});
test('a failed live read keeps the list', async () => {
  const { ui, pending, load } = setup();
  ui.rows = [row(10, 1)];
  const live = load('live');
  pending[0].resolve(Promise.reject(new Error('busy')));
  await live;
  assert.deepEqual(seqs(ui.rows), [1]);
  assert.equal(ui.activityError, undefined);
});
