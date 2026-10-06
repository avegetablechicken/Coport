const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const { test } = require('node:test');
const source = fs.readFileSync(path.join(__dirname, '../ui/app.js'), 'utf8');
function setup(scope = 'model') {
  const context = { ui: { trafficScope: scope }, esc: (v) => String(v), serviceMark: (s) => `[${s}]` };
  vm.createContext(context);
  vm.runInContext(source.slice(source.indexOf('const SHARE_SERVICES'), source.indexOf('\nfunction renderActivityTraffic(')), context);
  return context;
}
// Arrays built inside the VM context have another realm's prototype.
const plain = (value) => JSON.parse(JSON.stringify(value));
const credential = (credential, requests, service = 'Codex', tokens = {}) => ({ service, credential, requests, inputTokens: null, outputTokens: null, cachedInputTokens: null, ...tokens });
const requests = (c) => c.requests;
test('groups by service with one hue each and keeps unidentified last', () => {
  const { shareGroups } = setup();
  const groups = shareGroups([credential('Unidentified', 50), credential('a', 10), credential('b', 30, 'Claude'), credential('c', 20), credential('idle', 0)], requests);
  assert.deepEqual(plain(groups.map((g) => [g.service, g.value])), [['Codex', 80], ['Claude', 30]]);
  assert.deepEqual(groups[0].slices.map((s) => [s.name, s.value, s.color ?? null]), [['c', 20, '--codex-1'], ['a', 10, '--codex-2'], ['Unidentified', 50, null]]);
  assert.deepEqual(groups[1].slices.map((s) => s.color), ['--claude-1']);
});
test('folds credentials beyond the shades into Others per service', () => {
  const { shareGroups } = setup();
  const named = Array.from({ length: 5 }, (_, i) => credential(`c${i}`, 100 - i, 'Claude'));
  const [group] = shareGroups([...named, credential('Unidentified', 5, 'Claude')], requests);
  assert.deepEqual(group.slices.map((s) => s.name), ['c0', 'c1', 'c2', 'Others (2)', 'Unidentified']);
  assert.equal(group.slices[3].value, 97 + 96);
  assert.equal(group.slices[3].muted, true);
  // Exactly at the limit, nothing is grouped.
  assert.equal(shareGroups(named.slice(0, 3), requests)[0].slices.some((s) => s.muted), false);
});
test('services other than Codex and Claude follow them in a neutral hue', () => {
  const { shareGroups } = setup();
  const groups = shareGroups([credential('x', 500, 'Gemini'), credential('b', 1, 'Claude'), credential('a', 1)], requests);
  assert.deepEqual(plain(groups.map((g) => g.service)), ['Codex', 'Claude', 'Gemini']);
  assert.equal(groups[2].slices[0].color, '--other-1');
});
test('All shows request counts without percentages', () => {
  const { trafficShare } = setup();
  const html = trafficShare([credential('a', 1200), credential('b', 34, 'Claude')], 'all');
  assert.match(html, /<strong>1,234<\/strong><span>requests<\/span>/);
  assert.equal((html.match(/<path /g) || []).length, 2);
  assert.match(html, /\[Codex\]<span>Codex<\/span><span>1,200<\/span>/);
  assert.match(html, /<span>b<\/span><span>34<\/span>/);
  assert.doesNotMatch(html, /%/);
  assert.equal(setup().trafficShare([credential('a', 0)], 'all'), '');
});
test('Models shows reported tokens and leaves unknown usage out of the ring', () => {
  const { trafficShare } = setup();
  const html = trafficShare([
    credential('a', 3, 'Codex', { inputTokens: 1_200_000, outputTokens: 300_000 }),
    credential('b', 2, 'Claude', { inputTokens: 800, outputTokens: null }),
    credential('silent', 5, 'Claude'),
  ], 'model');
  assert.match(html, /<strong>1\.5M<\/strong><span>tokens<\/span>/);
  assert.match(html, /<span>a<\/span><span>1\.5M<\/span>/);
  assert.match(html, /<span>b<\/span><span>800<\/span>/);
  assert.match(html, /<span>silent<\/span><span>—<\/span>/);
  assert.match(html, /Codex · a: 1,500,000 tokens/);
  assert.equal((html.match(/<path /g) || []).length, 2);
  assert.doesNotMatch(html, /%/);
  // Calls without any reported usage draw no ring.
  assert.equal(trafficShare([credential('silent', 5)], 'model'), '');
});
test('sectors are closed paths and vanish when too narrow', () => {
  const { ringSector } = setup();
  assert.match(ringSector(0, 0.5, 42, 26), /^M .* Z$/);
  assert.match(ringSector(0, 0.75, 42, 26), / A 42 42 0 1 1 /);
  assert.equal(ringSector(0, 0.0001, 42, 26), '');
});
test('traffic data keeps the category it was read for', async () => {
  const pending = [];
  const ui = { trafficScope: 'all', trafficRequest: 0, trafficMinutes: 30, page: 'main' };
  const context = { ui, $: () => null, renderActivityTraffic() {}, invoke: (_, query) => new Promise((resolve) => pending.push({ query, resolve })) };
  vm.createContext(context);
  vm.runInContext(source.slice(source.indexOf('async function loadTraffic('), source.indexOf('\nfunction modelTokenStats(')), context);
  const load = context.loadTraffic();
  // The selector already shows the new category while the old data renders.
  ui.trafficScope = 'model';
  pending[0].resolve({ credentials: [] });
  await load;
  assert.equal(pending[0].query.scope, 'all');
  assert.equal(ui.traffic.scope, 'all');
});
test('home traffic keeps the category it was read for', async () => {
  const pending = [];
  const ui = { trafficScope: 'all', homeTrafficRequest: 0, homeTrafficMinutes: 30, page: 'activity' };
  const context = { ui, render() {}, invoke: (_, query) => new Promise((resolve) => pending.push({ query, resolve })) };
  vm.createContext(context);
  vm.runInContext(source.slice(source.indexOf('async function loadHomeTraffic('), source.indexOf('\nfunction trafficBlock(')), context);
  const load = context.loadHomeTraffic();
  ui.trafficScope = 'model';
  pending[0].resolve({ summary: { requests: 3 } });
  await load;
  assert.equal(pending[0].query.scope, 'all');
  assert.equal(ui.homeTraffic.scope, 'all');
  assert.equal(ui.homeTraffic.requests, 3);
});
test('Models adds output to input, which already holds cached tokens', () => {
  const { shareTokens, trafficShare } = setup();
  // The backend counts cached input inside input for every service.
  for (const service of ['Claude', 'Codex']) {
    assert.equal(shareTokens(credential('c', 1, service, { inputTokens: 1000, outputTokens: 20, cachedInputTokens: 900 })), 1020);
  }
  assert.equal(shareTokens(credential('out', 1, 'Claude', { outputTokens: 50 })), 50);
  assert.equal(shareTokens(credential('silent', 1, 'Claude', { cachedInputTokens: 5 })), null);
  assert.match(trafficShare([credential('c', 1, 'Claude', { inputTokens: 1000, outputTokens: 20, cachedInputTokens: 900 })], 'model'), /Claude · c: 1,020 tokens/);
});
test('review menu offers current configurations and Unidentified for each changed source', () => {
  const context = setup('all');
  context.ICON = { warning: '!', more: '…' };
  context.esc = (v) => String(v).replace(/&/g, '&amp;').replace(/"/g, '&quot;');
  const targets = [{ service: 'Codex', name: 'main', base: 'https://api.example.com/v1', label: 'main' }, { service: 'Claude', name: 'x', base: 'https://x.test', label: 'x' }];
  const group = { ...credential('main', 3), sources: [
    { name: 'old', base: 'https://api.example.com/v1', reason: 'renamed', requests: 2 },
    { name: 'kept', base: null, reason: null, requests: 1 },
  ] };
  const html = context.trafficReview(group, targets);
  assert.match(html, /class="icon-btn more traffic-review review"/);
  const items = JSON.parse(html.match(/data-menu="([^"]*)"/)[1].replace(/&quot;/g, '"').replace(/&amp;/g, '&'));
  assert.deepEqual(items.map((i) => i && (i.heading ?? i.label)), [
    'old · api.example.com/v1', 'main', 'Unidentified', null, 'kept · No base URL', 'main', 'Unidentified', 'Match Automatically',
  ]);
  assert.equal(items[0].detail, 'Same base URL, different name · 2 requests');
  assert.equal(items[1].checked, true);
  assert.deepEqual(JSON.parse(items[2].target), { service: 'Codex', name: 'old', base: 'https://api.example.com/v1', target: null, automatic: false });
  assert.deepEqual(JSON.parse(items[5].target).target, { name: 'main', base: 'https://api.example.com/v1' });
  assert.equal(JSON.parse(items[7].target).automatic, true);
  // Choices already made show no warning; groups without sources show nothing.
  assert.doesNotMatch(context.trafficReview({ ...group, sources: [group.sources[1]] }, targets), /traffic-review review/);
  assert.equal(context.trafficReview(credential('main', 3), targets), '');
});
