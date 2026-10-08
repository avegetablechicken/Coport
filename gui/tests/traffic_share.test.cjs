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
  const named = Array.from({ length: 6 }, (_, i) => credential(`c${i}`, 100 - i, 'Claude'));
  const [group] = shareGroups([...named, credential('Unidentified', 5, 'Claude')], requests);
  assert.deepEqual(group.slices.map((s) => s.name), ['c0', 'c1', 'c2', 'Others (3)', 'Unidentified']);
  assert.equal(group.slices[3].value, 97 + 96 + 95);
  assert.equal(group.slices[3].muted, true);
  // Others never holds a single credential: four fit, five group two.
  const four = shareGroups(named.slice(0, 4), requests)[0].slices;
  assert.deepEqual(four.map((s) => s.color), ['--claude-1', '--claude-2', '--claude-3', '--claude-4']);
  assert.deepEqual(shareGroups(named.slice(0, 5), requests)[0].slices.map((s) => s.name), ['c0', 'c1', 'c2', 'Others (2)']);
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
  vm.runInContext(source.slice(source.indexOf('function rememberLocalTraffic('), source.indexOf('async function loadHomeTraffic(')), context);
  vm.runInContext(source.slice(source.indexOf('async function loadTraffic('), source.indexOf('\nfunction modelTokenStats(')), context);
  const load = context.loadTraffic();
  // The selector already shows the new category while the old data renders.
  ui.trafficScope = 'model';
  pending[0].resolve({ credentials: [] });
  await load;
  assert.equal(pending[0].query.scope, 'all');
  assert.equal(ui.traffic.scope, 'all');
});
test('a traffic read still running is not started again', async () => {
  const pending = [];
  const ui = { trafficScope: 'all', trafficRequest: 0, trafficMinutes: 43200, page: 'activity' };
  const context = { ui, $: () => null, renderActivityTraffic() {}, invoke: (_, query) => new Promise((resolve) => pending.push({ query, resolve })) };
  vm.createContext(context);
  vm.runInContext(source.slice(source.indexOf('function rememberLocalTraffic('), source.indexOf('async function loadHomeTraffic(')), context);
  vm.runInContext(source.slice(source.indexOf('async function loadTraffic('), source.indexOf('\nfunction modelTokenStats(')), context);
  const first = context.loadTraffic();
  context.loadTraffic();
  assert.equal(pending.length, 1);
  // Another range starts at once, and so does any read after a switch.
  ui.trafficMinutes = 30;
  const second = context.loadTraffic();
  assert.equal(pending.length, 2);
  ui.trafficMinutes = 43200;
  ++ui.trafficRequest;
  const third = context.loadTraffic();
  assert.equal(pending.length, 3);
  for (const { resolve } of pending) resolve({ credentials: [] });
  await Promise.all([first, second, third]);
  context.loadTraffic();
  assert.equal(pending.length, 4);
});
test('home traffic keeps the category it was read for', async () => {
  const pending = [];
  const ui = { trafficScope: 'all', homeTrafficRequest: 0, homeTrafficMinutes: 30, page: 'activity' };
  const context = { ui, render() {}, invoke: (_, query) => new Promise((resolve) => pending.push({ query, resolve })) };
  vm.createContext(context);
  vm.runInContext(source.slice(source.indexOf('function rememberLocalTraffic('), source.indexOf('async function loadHomeTraffic(')), context);
  vm.runInContext(source.slice(source.indexOf('async function loadHomeTraffic('), source.indexOf('\nfunction trafficBlock(')), context);
  const load = context.loadHomeTraffic();
  ui.trafficScope = 'model';
  pending[0].resolve({ start: 1000, end: 181000, bucketMinutes: 1, summary: { requests: 3 } });
  await load;
  assert.equal(pending[0].query.scope, 'all');
  assert.equal(ui.homeTraffic.scope, 'all');
  assert.equal(ui.homeTraffic.requests, 3);
  for (const stats of [ui.homeTraffic, ui.localTrafficViews['30:all']]) {
    assert.equal(stats.start, 1000);
    assert.equal(stats.end, 181000);
    assert.equal(stats.bucketMinutes, 1);
  }
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
test('Models charts omit empty and unreported token bars but retain hover targets', () => {
  const context = setup('model');
  vm.runInContext(source.slice(source.indexOf('function chart('), source.indexOf('\nasync function loadHomeTraffic(')), context);
  const failed = { tokenCounts: [0, 0, 0], counts: [1, 0, 2], errorCounts: [1, 0, 2] };
  const html = context.chart(failed, 'model', true);
  assert.doesNotMatch(html, /chart-peak/);
  assert.match(html, /No reported tokens/);
  assert.doesNotMatch(html, /calls|requests/);
  assert.doesNotMatch(html, /<rect class="(?!chart-hit)/);
  assert.equal((html.match(/class="chart-hit"/g) || []).length, 3);
  const cancelled = context.chart({ ...failed, errorCounts: [0, 0, 0] }, 'model', true);
  assert.doesNotMatch(cancelled, /<rect class="(?!chart-hit)/);
  const requests = context.chart(failed, 'all', true);
  assert.equal((requests.match(/class="err"/g) || []).length, 2);
  assert.match(context.chart({ ...failed, tokenCounts: [5, 0, 0] }, 'model', true), /class="chart-peak"/);
  assert.match(context.chart({ ...failed, counts: [0, 0, 0], errorCounts: [0, 0, 0] }, 'all', true), /class="chart-peak"/);
});
test('Models leaves out unidentified calls without tokens, All keeps them', () => {
  const { trafficShare } = setup();
  const credentials = [
    credential('a', 3, 'Codex', { inputTokens: 100, outputTokens: 20 }),
    credential('Unidentified', 4, 'Codex'),
    // Claude only has rejected calls, so the whole service is left out.
    credential('Unidentified', 2, 'Claude'),
  ];
  const model = trafficShare(credentials, 'model');
  assert.doesNotMatch(model, /Unidentified/);
  assert.doesNotMatch(model, /\[Claude\]/);
  assert.match(model, /<span>a<\/span><span>120<\/span>/);
  const all = trafficShare(credentials, 'all');
  assert.equal((all.match(/<span>Unidentified<\/span>/g) || []).length, 2);
  assert.match(all, /\[Claude\]/);
  // Should an unidentified call ever report tokens, it stays visible.
  assert.match(trafficShare([credential('Unidentified', 1, 'Codex', { outputTokens: 5 })], 'model'), /Unidentified/);
});
test('Others lists every configuration it groups on hover', () => {
  const { trafficShare } = setup();
  const named = Array.from({ length: 5 }, (_, i) => credential(`c${i}`, 100 - i, 'Claude'));
  const html = trafficShare(named, 'all');
  const tip = 'Claude · Others (2): 193 requests\nc3: 97 requests\nc4: 96 requests';
  // Both the legend row and its ring sector carry the list.
  assert.equal(html.split(`title="${tip}"`).length, 2);
  assert.equal(html.split(`<title>${tip}</title>`).length, 2);
});
test('Unidentified takes an extra row in All, so named rows match Models', () => {
  const { trafficShare } = setup();
  const tokens = { inputTokens: 100, outputTokens: 1 };
  const named = Array.from({ length: 4 }, (_, i) => credential(`c${i}`, 10 - i, 'Codex', tokens));
  const credentials = [...named, credential('Unidentified', 50, 'Codex')];
  const rows = (html) => [...html.matchAll(/<i style="[^"]*"><\/i><span>([^<]+)<\/span>/g)].map((m) => m[1]);
  assert.deepEqual(rows(trafficShare(credentials, 'model')), ['c0', 'c1', 'c2', 'c3']);
  assert.deepEqual(rows(trafficShare(credentials, 'all')), ['c0', 'c1', 'c2', 'c3', 'Unidentified']);
});
test('Claude Input Tokens explain uncached input and cache writes on hover', () => {
  const context = setup('model');
  context.ICON = { warning: '!' };
  context.stat = (label, value) => `<${label}=${value}>`;
  vm.runInContext(source.slice(source.indexOf('function modelTokenStats('), source.indexOf('const SHARE_SERVICES')), context);
  const claude = { service: 'Claude', inputTokens: 50, uncachedInputTokens: 10, cacheWriteTokens: 10, cachedInputTokens: 30, outputTokens: 5, cacheHitRate: 0.6 };
  const html = context.modelTokenStats(claude, 'model');
  assert.match(html, /data-tip="Uncached input: 10\nCache writes: 10\nCache reads: 30 \(Cached Input\)"/);
  assert.match(html, /=50>/);
  assert.doesNotMatch(context.modelTokenStats({ ...claude, service: 'Codex' }, 'model'), /data-tip/);
  assert.doesNotMatch(context.modelTokenStats({ ...claude, inputTokens: null }, 'model'), /data-tip/);
});

test('bar tooltips use their own time bucket and only the plotted metric', () => {
  const context = setup();
  vm.runInContext(source.slice(source.indexOf('function chart('), source.indexOf('\nasync function loadHomeTraffic(')), context);
  const start = new Date(2026, 9, 9, 23, 59, 0).getTime();
  const end = start + 150_000;
  const stats = { counts: [2, 3, 0], tokenCounts: [1234, 0, 0], errorCounts: [1, 3, 0] };
  const date = ms => new Date(ms).toLocaleString([], { month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit', second: '2-digit' });
  const ranges = [0, 1, 2].map(i => `${date(start + i * 60_000)} – ${date(Math.min(start + (i + 1) * 60_000, end))}\n`);
  const tips = html => [...html.matchAll(/data-tip="([^"]*)"/g)].map(m => m[1]);
  // Local and merged-device payloads use different names for window bounds.
  for (const window of [{ start, end, bucketMinutes: 1 }, { windowStart: start, windowEnd: end, bucketMinutes: 1 }]) {
    const requests = context.chart(stats, 'all', false, window);
    assert.deepEqual(tips(requests), ranges.map((range, i) => `${range}${stats.counts[i]} requests`));
    const tokens = context.chart(stats, 'model', false, window);
    assert.deepEqual(tips(tokens), [ranges[0] + '1,234 tokens', ranges[1] + 'No reported tokens', ranges[2] + '0 tokens']);
    // Even idle or failed bars have a full-height target over the error overlay.
    const groups = [...tokens.matchAll(/<g class="chart-bar"[^>]*>(.*?)<\/g>/g)].map(m => m[1]);
    assert.equal(groups.length, 3);
    for (const group of groups) assert.match(group, /<rect class="chart-hit"[^>]* y="0"[^>]* height="32"><\/rect>$/);
    assert.match(groups[0], /<rect class=""/);
    for (const group of groups.slice(1)) assert.doesNotMatch(group, /<rect class="(?!chart-hit)/);
  }
});

test('hovering across bars updates the tooltip and leaving hides it', () => {
  const events = {};
  const timers = new Map();
  let timerId = 0;
  const tip = { hidden: true, style: {}, getBoundingClientRect: () => ({ width: 180, height: 40 }) };
  const context = {
    $: () => tip,
    setTimeout(fn) { timers.set(++timerId, fn); return timerId; },
    clearTimeout(id) { timers.delete(id); },
    document: { documentElement: { clientWidth: 320, clientHeight: 480 }, addEventListener(type, fn) { events[type] = fn; } },
    window: { addEventListener() {} },
  };
  vm.createContext(context);
  vm.runInContext(source.slice(source.indexOf('let routeTooltipTimer;'), source.indexOf('\nfunction routingBlock(')), context);
  const bar = text => {
    const owner = {
      dataset: { tip: text }, isConnected: true,
      matches: selector => selector === '.chart-bar',
      contains: target => target?.owner === owner,
      getBoundingClientRect: () => ({ left: 290, top: 10, bottom: 42 }),
      setAttribute(name, value) { this[name] = value; },
      removeAttribute(name) { delete this[name]; },
    };
    const target = { owner, closest: selector => selector.includes('.chart-bar') ? owner : null };
    return { owner, target };
  };
  const flush = () => { for (const [id, fn] of timers) { timers.delete(id); fn(); } };
  const first = bar('10:00 – 10:01\n2 requests');
  const second = bar('10:01 – 10:02\n5 requests');
  events.pointerover({ target: first.target });
  flush();
  assert.equal(tip.hidden, false);
  assert.equal(tip.textContent, first.owner.dataset.tip);
  assert.equal(tip.style.left, '128px'); // Right-edge bars stay inside the viewport.
  events.pointerout({ target: first.target, relatedTarget: second.target });
  events.pointerover({ target: second.target });
  flush();
  assert.equal(tip.textContent, second.owner.dataset.tip);
  assert.equal(first.owner['aria-describedby'], undefined);
  events.pointerout({ target: second.target, relatedTarget: null });
  assert.equal(tip.hidden, true);
  // A quick pass over a bar must not leave a delayed tooltip behind.
  events.pointerover({ target: first.target });
  events.pointerout({ target: first.target, relatedTarget: null });
  flush();
  assert.equal(tip.hidden, true);
});
