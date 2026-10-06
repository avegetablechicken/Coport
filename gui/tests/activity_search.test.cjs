const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const { test } = require('node:test');
const source = fs.readFileSync(path.join(__dirname, '../ui/app.js'), 'utf8');
function setup() {
  const pending = [];
  const ui = { activityRequest: 0, filter: 'requests', search: 'old', searchMode: 'keyword', trafficMinutes: 30, rows: [] };
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
  pending[1].resolve(['new']);
  await latest;
  pending[0].resolve(['old']);
  await old;
  assert.deepEqual(ui.rows, ['new']);
});
for (const [key, value] of [['search', 'new'], ['searchMode', 'proxy'], ['filter', 'errors'], ['trafficMinutes', 360]]) {
  test(`ignores a response after ${key} changes before the next query`, async () => {
    const { ui, pending, load } = setup();
    const request = load();
    ui[key] = value;
    pending[0].resolve(['stale']);
    await request;
    assert.deepEqual(ui.rows, []);
  });
}
