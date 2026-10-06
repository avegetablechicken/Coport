const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const { test } = require('node:test');
const source = fs.readFileSync(path.join(__dirname, '../ui/app.js'), 'utf8');
function setup() {
  const pending = [];
  const ui = { activityRequest: 0, filter: 'requests', search: 'old', searchMode: 'keyword', activityFrom: 0, activityTo: 3600000, activityNext: null, rows: [] };
  const context = { ui, invoke: (_, query) => new Promise(resolve => pending.push({ query, resolve })), renderActivityList() {} };
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
  const more = load(true);
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
