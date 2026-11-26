const assert = require('node:assert/strict');
const { test } = require('node:test');
const { createActivityPager } = require('../ui/activity_pager.js');
function setup() {
  const pending = [];
  const log = createActivityPager({
    invoke: (_, query) => new Promise(resolve => pending.push({ query, resolve })),
    show: (update) => update(),
    failed() {},
    now: () => 3600000,
  });
  Object.assign(log, { search: 'old', from: 0, to: 3600000 });
  return { log, pending, load: log.load };
}
test('newer search result survives an older response', async () => {
  const { log, pending, load } = setup();
  const old = load();
  log.search = '200';
  log.searchMode = 'status';
  const latest = load();
  assert.equal(pending[1].query.searchMode, 'status');
  // The To minute is inclusive.
  assert.equal(JSON.stringify(pending[1].query.range), '{"from":0,"to":3660000}');
  pending[1].resolve({ rows: ['new'], next: null });
  await latest;
  pending[0].resolve({ rows: ['old'], next: null });
  await old;
  assert.deepEqual(log.rows, ['new']);
});
test('more appends the next page after the cursor', async () => {
  const { log, pending, load } = setup();
  const first = load();
  pending[0].resolve({ rows: ['a'], next: { time: 1, seq: 2 } });
  await first;
  const more = load('more');
  assert.equal(JSON.stringify(pending[1].query.after), '{"time":1,"seq":2}');
  pending[1].resolve({ rows: ['b'], next: null });
  await more;
  assert.equal(JSON.stringify(log.rows), '["a","b"]');
  assert.equal(log.next, null);
});
for (const [key, value] of [['search', 'new'], ['searchMode', 'proxy'], ['filter', 'errors'], ['upstream', true], ['from', 60000], ['to', 7200000]]) {
  test(`ignores a response after ${key} changes before the next query`, async () => {
    const { log, pending, load } = setup();
    const request = load();
    log[key] = value;
    pending[0].resolve({ rows: ['stale'], next: null });
    await request;
    assert.deepEqual(log.rows, []);
  });
}
const row = (time, seq) => ({ time, seq });
const seqs = (rows) => rows.map((e) => e.seq);
test('live re-reads the log and replaces a single page', async () => {
  const { log, pending, load } = setup();
  log.rows = [row(10, 1)];
  const live = load('live');
  assert.equal(pending[0].query.fresh, true);
  assert.equal(pending[0].query.after, null);
  pending[0].resolve({ rows: [row(20, 2), row(10, 1)], next: null });
  await live;
  assert.deepEqual(seqs(log.rows), [2, 1]);
});
test('only live reads ask for a fresh read', async () => {
  const { pending, load } = setup();
  load();
  load('more');
  assert.deepEqual(pending.map((p) => p.query.fresh), [false, false]);
});
test('live keeps older pages below the new first page', async () => {
  const { log, pending, load } = setup();
  const first = load();
  pending[0].resolve({ rows: [row(40, 4), row(30, 3)], next: { time: 30, seq: 3 } });
  await first;
  const more = load('more');
  pending[1].resolve({ rows: [row(20, 2), row(10, 1)], next: { time: 10, seq: 1 } });
  await more;
  // The refreshed page overlaps the previous first page.
  const live = load('live');
  pending[2].resolve({ rows: [row(60, 6), row(50, 5), row(40, 4)], next: { time: 40, seq: 4 } });
  await live;
  assert.deepEqual(seqs(log.rows), [6, 5, 4, 3, 2, 1]);
  assert.equal(JSON.stringify(log.next), '{"time":10,"seq":1}');
});
test('live drops loaded rows that left a rolling range', async () => {
  const { log, pending, load } = setup();
  log.rows = [row(40, 4), row(30, 3), row(20, 2), row(10, 1)];
  log.next = { time: 10, seq: 1 };
  log.paged = true;
  log.from = 15;
  const live = load('live');
  pending[0].resolve({ rows: [row(50, 5), row(40, 4)], next: { time: 40, seq: 4 } });
  await live;
  assert.deepEqual(seqs(log.rows), [5, 4, 3, 2]);
});
test('a failed live read keeps the list', async () => {
  const { log, pending, load } = setup();
  log.rows = [row(10, 1)];
  const live = load('live');
  pending[0].resolve(Promise.reject(new Error('busy')));
  await live;
  assert.deepEqual(seqs(log.rows), [1]);
  assert.equal(log.error, '');
});

test('live refresh reuses a pending scan and accepts its result', async () => {
  const { log, pending, load } = setup();
  const first = load('live');
  await load('live');
  assert.equal(pending.length, 1);
  pending[0].resolve({ rows: [row(20, 2)], next: null });
  await first;
  assert.deepEqual(seqs(log.rows), [2]);
  const next = load('live');
  assert.equal(pending.length, 2);
  pending[1].resolve(Promise.reject(new Error('busy')));
  await next;
  const retry = load('live');
  assert.equal(pending.length, 3);
  pending[2].resolve({ rows: [row(30, 3)], next: null });
  await retry;
  assert.deepEqual(seqs(log.rows), [3]);
});
test('live refresh waits for the same initial query but a changed query starts immediately', async () => {
  const { log, pending, load } = setup();
  const first = load();
  await load('live');
  assert.equal(pending.length, 1);
  log.search = 'changed';
  const changed = load('live');
  assert.equal(pending.length, 2);
  pending[0].resolve({ rows: [row(10, 1)], next: null });
  await first;
  await load('live');
  assert.equal(pending.length, 2, 'stale completion must not clear the current scan');
  pending[1].resolve({ rows: [row(20, 2)], next: null });
  await changed;
  assert.deepEqual(seqs(log.rows), [2]);
});

test('a live burst larger than a page resets pagination instead of skipping a gap', async () => {
  const { log, pending, load } = setup();
  const rows = (from, count) => Array.from({ length: count }, (_, i) => row(from - i, from - i));
  log.rows = rows(2000, 1000);
  log.paged = true;
  log.next = { time: 1001, seq: 1001 };
  const live = load('live');
  pending[0].resolve({ rows: rows(3000, 500), next: { time: 2501, seq: 2501 } });
  await live;
  assert.equal(log.rows.length, 500);
  assert.equal(log.paged, false);
  const more = load('more');
  assert.equal(pending[1].query.after.seq, 2501);
  pending[1].resolve({ rows: rows(2500, 500), next: { time: 2001, seq: 2001 } });
  await more;
  assert.deepEqual(seqs(log.rows), seqs(rows(3000, 1000)));
});
test('the side switch asks for upstream responses, except under All Events', () => {
  const { log, pending, load } = setup();
  const filters = [];
  for (const filter of ['requests', 'models', 'errors', 'all']) {
    for (const upstream of [false, true]) {
      Object.assign(log, { filter, upstream });
      load();
      filters.push(pending.at(-1).query.filter);
    }
  }
  assert.deepEqual(filters, ['requests', 'upstream_requests', 'models', 'upstream_models', 'errors', 'upstream_errors', 'all', 'all']);
});
