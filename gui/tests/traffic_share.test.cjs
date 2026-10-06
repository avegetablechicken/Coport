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
