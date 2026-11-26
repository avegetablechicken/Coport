const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const { test } = require('node:test');
const source = fs.readFileSync(path.join(__dirname, '../ui/app.js'), 'utf8');
const { createTrafficViews } = require('../ui/traffic_views.js');
function setup(scope = 'model') {
  const context = { ui: {}, trafficViews: { scope, activity: null }, esc: (v) => String(v), serviceMark: (s) => `[${s}]`, ICON: { warning: '!' } };
  vm.createContext(context);
  vm.runInContext(source.slice(source.indexOf('function tooltipScopeKey('), source.indexOf('function trafficSummary(')), context);
  vm.runInContext(source.slice(source.indexOf('const SHARE_SERVICES'), source.indexOf('\nfunction renderActivityTraffic(')), context);
  return context;
}
// Arrays built inside the VM context have another realm's prototype.
const plain = (value) => JSON.parse(JSON.stringify(value));
const credential = (credential, requests, service = 'Codex', tokens = {}) => ({ service, credential, requests, inputTokens: null, outputTokens: null, cachedInputTokens: null, ...tokens });
const requests = (c) => c.requests;
// Shared traffic views, markup and merging, as every page uses them.
function loadSharedViews(context) {
  vm.runInContext(source.slice(source.indexOf('const trafficMarkup = new WeakMap();'), source.indexOf('/// Home shows the forwarded device')), context);
}
function loadHome(context) {
  vm.runInContext(source.slice(source.indexOf('/// Home shows the forwarded device'), source.indexOf('\nfunction trafficBlock(')), context);
}
test('Account emails and IDs are shortened for display', () => {
  const { maskAccountName } = setup();
  for (const [name, masked] of [
    ['suzhi2333@gmail.com', 'su***3@gmail.com'],
    ['alice@example.com', 'al***e@example.com'],
    ['abc@example.com', 'a***c@example.com'],
    ['ab@example.com', 'a***@example.com'],
    ['a@example.com', '***@example.com'],
    ['12345678-1234-4234-8234-123456789abc', '12345678…9abc'],
  ]) {
    assert.equal(maskAccountName(name), masked);
    assert.notEqual(maskAccountName(name), name);
  }
  // Configuration names and aliases stay readable.
  for (const name of ['work', 'default', 'Unidentified']) assert.equal(maskAccountName(name), name);
});
test('turning the setting off shows full account names', () => {
  const context = setup('all');
  context.ui.snap = { settings: { maskAccountNames: false } };
  assert.equal(context.maskAccountName('suzhi2333@gmail.com'), 'suzhi2333@gmail.com');
  assert.match(context.trafficShare([credential('suzhi2333@gmail.com', 1)], 'all', 'local'), /suzhi2333@gmail\.com/);
});
test('changing the setting redraws cached traffic markup', () => {
  const context = setup('all');
  loadSharedViews(context);
  Object.assign(context, { chart: () => '<chart>', stat: () => '', fmtMs: String, fmtBytes: String, modelTokenStats: () => '' });
  const view = { requests: 1, errors: 0, credentials: [credential('suzhi2333@gmail.com', 1)], start: 0, end: 60000, bucketMinutes: 1 };
  context.ui.snap = { settings: { maskAccountNames: true } };
  assert.match(context.trafficSummary(view, 'all'), /su\*\*\*3@gmail\.com/);
  context.ui.snap.settings.maskAccountNames = false;
  assert.match(context.trafficSummary(view, 'all'), /suzhi2333@gmail\.com/);
  assert.doesNotMatch(context.trafficSummary(view, 'all'), /su\*\*\*3/);
});
test('Traffic rings shorten account emails on every page', () => {
  const { trafficShare } = setup();
  const credentials = [credential('suzhi2333@gmail.com', 3), credential('work', 2)];
  const html = trafficShare(credentials, 'all', 'local');
  assert.match(html, /<span>su\*\*\*3@gmail\.com<\/span>/);
  assert.match(html, /<span>work<\/span>/);
  assert.doesNotMatch(html, /suzhi2333@gmail\.com/);
});
test('Activity Traffic shortens account emails in its rows and ring', () => {
  const context = setup('all');
  const el = { innerHTML: '', setAttribute() {} };
  Object.assign(context, {
    ui: {}, trafficViews: { scope: 'all', activity: { scope: 'all', start: 0, end: 60000, bucketMinutes: 1,
      credentials: [{ service: 'Codex', credential: 'suzhi2333@gmail.com', requests: 1, errors: 0, bytes: 0, counts: [1], errorCounts: [0], tokenCounts: [0] }] } },
    openSelect: null, closeSelect() {}, $: (id) => (id === 'activity-traffic' ? el : null), logOffset: () => null,
    trafficControls: () => '', trafficReview: () => '', chart: () => '', stat: () => '', fmtMs: () => '',
    fmtBytes: () => '', modelTokenStats: () => '', serviceMark: () => '', ICON: { info: '', warning: '' }, queueFit() {}, scrollToLog() {},
  });
  vm.runInContext(source.slice(source.indexOf('/// One account\'s row'), source.indexOf('\n/// The share ring, chart and totals')), context);
  vm.runInContext(source.slice(source.indexOf('function renderActivityTraffic('), source.indexOf('\nconst SEARCH_MODES')), context);
  context.renderActivityTraffic();
  assert.match(el.innerHTML, /<strong>su\*\*\*3@gmail\.com<\/strong>/);
  assert.doesNotMatch(el.innerHTML, /suzhi2333@gmail\.com/);
});
test('Home Routing hides part of an account email', () => {
  const context = setup();
  vm.runInContext(source.slice(source.indexOf('function routeIdentity('), source.indexOf('function routingChain(')), context);
  assert.equal(context.routeIdentity('suzhi2333@gmail.com', 'account').name, 'su***3@gmail.com');
  assert.equal(context.routeIdentity('default', 'account').name, 'default');
  assert.equal(context.routeIdentity('suzhi2333@gmail.com', 'provider').name, 'suzhi2333@gmail.com');
});
test('groups by service with one hue each and keeps unidentified last', () => {
  const { shareGroups } = setup();
  const groups = shareGroups([credential('Unidentified', 50), credential('a', 10), credential('b', 30, 'Claude'), credential('c', 20), credential('idle', 0)], requests);
  assert.deepEqual(plain(groups.map((g) => [g.service, g.value])), [['Codex', 80], ['Claude', 30]]);
  assert.deepEqual(groups[0].slices.map((s) => [s.name, s.value, s.color ?? null]), [['c', 20, 'var(--codex-1)'], ['a', 10, 'var(--codex-2)'], ['Unidentified', 50, null]]);
  assert.deepEqual(groups[1].slices.map((s) => s.color), ['var(--claude-1)']);
});
test('Antigravity accounts form their own group in its hue after the other apps', () => {
  const { shareGroups } = setup();
  const groups = shareGroups([credential('g@example.com', 40, 'Antigravity'), credential('b', 30, 'Claude'), credential('c', 20)], requests);
  assert.deepEqual(plain(groups.map((g) => [g.service, g.value])), [['Codex', 20], ['Claude', 30], ['Antigravity', 40]]);
  assert.deepEqual(groups[2].slices.map((s) => s.color), ['var(--antigravity-1)']);
});
test('Connect always offers the Grok URL and both Grok Build endpoints on the listening port', () => {
  const context = setup();
  const grok = 'http://127.0.0.1:19427/grok/v1';
  context.ui.snap = { urls: { grok }, config: { details: null } };
  Object.assign(context, { ICON: { copy: '', chevron: '' }, block: (_, __, body) => body });
  vm.runInContext(source.slice(source.indexOf('function connectBlock('), source.indexOf('function recentBlock(')), context);
  const html = context.connectBlock();
  assert.ok(html.includes('[Grok]Grok'));
  assert.ok(html.includes(`data-text="${grok}" data-tip="Copy" aria-label="Copy Grok base URL"`));
  assert.ok(html.includes(`[endpoints]\ncli_chat_proxy_base_url = "${grok}"\nxai_api_base_url = "${grok}"`));
});
test('Connect gives agy its endpoint variables, and its own proxy and CA with the proxy port', () => {
  const context = setup();
  const base = 'http://127.0.0.1:19427';
  const ag = `${base}/antigravity`;
  const ca = '/Users/u/Library/Application Support/io.github.coport.gui/tls/proxy-ca.pem';
  Object.assign(context, { ICON: { copy: '', chevron: '' }, block: (_, __, body) => body });
  vm.runInContext(source.slice(source.indexOf('function connectBlock('), source.indexOf('function recentBlock(')), context);
  const endpoints = `CLOUD_CODE_URL=${ag}\nUSER_INFO_OVERRIDE_URL=${ag}/https://www.googleapis.com/oauth2/v2/userinfo\nAICODE_ENDPOINT_URL=${ag}/https://aicode.googleapis.com`;
  const snap = (routing) => ({ urls: { base, antigravity: ag, proxyCaCertificate: ca }, config: { details: { editable: { connect_credential_routing: routing } } } });
  assert.equal(context.agyEnvironment(snap(false)), endpoints);
  // The path keeps its spaces in one shell word; HTTPS_PROXY is not agy's.
  const routed = context.agyEnvironment(snap(true));
  assert.equal(routed, `${endpoints}\nAGY_LLM_GATEWAY_PROXY_URL=${base}\nAGY_CA_CERT="${ca}"`);
  assert.ok(!routed.includes('HTTPS_PROXY'));
  context.ui.snap = snap(true);
  const html = context.connectBlock();
  assert.ok(html.includes('AGY_LLM_GATEWAY_PROXY_URL=') && html.includes('AGY_CA_CERT='));
});
test('Connect points Grok Build at this issuer only when Grok logins are routed', () => {
  const context = setup();
  const base = 'http://127.0.0.1:19427';
  const grok = `${base}/grok/v1`;
  Object.assign(context, { ICON: { copy: '', chevron: '' }, block: (_, __, body) => body });
  vm.runInContext(source.slice(source.indexOf('function connectBlock('), source.indexOf('function recentBlock(')), context);
  const endpoints = `[endpoints]\ncli_chat_proxy_base_url = "${grok}"\nxai_api_base_url = "${grok}"`;
  const fallbacks = (proxies) => [{ key: 'accountFallback', proxies }, { key: 'fallback', proxies: null }, { key: 'accountProbe', proxies: ['none'] }];
  const snap = (accountRoutes, proxies) => ({ urls: { base, grok }, config: { details: { editable: {}, grok: { accountRoutes, fallbacks: fallbacks(proxies) } } } });
  // API keys alone, or an account probe, leave renewal where it was.
  assert.equal(context.grokConfig(snap([], null)), endpoints);
  assert.equal(context.grokConfig({ urls: { base, grok }, config: {} }), endpoints);
  const issuer = `${endpoints}\n\n[auth.oauth2]\nissuer = "${base}/grok/https://auth.x.ai"`;
  assert.equal(context.grokConfig(snap([{ selector: 'a@example.com' }], null)), issuer);
  assert.equal(context.grokConfig(snap([], ['us'])), issuer);
  context.ui.snap = snap([{ selector: 'a@example.com' }], null);
  const html = context.connectBlock();
  assert.ok(html.includes('[auth.oauth2]') && html.includes('grok login --device-auth'));
});
test('Grok accounts form their own group in its hue after Antigravity', () => {
  const { shareGroups } = setup();
  const groups = shareGroups([credential('x@example.com', 10, 'Grok'), credential('g@example.com', 40, 'Antigravity'), credential('c', 20)], requests);
  assert.deepEqual(plain(groups.map((g) => [g.service, g.value])), [['Codex', 20], ['Antigravity', 40], ['Grok', 10]]);
  assert.deepEqual(groups[2].slices.map((s) => s.color), ['var(--grok-1)']);
  const css = fs.readFileSync(path.join(__dirname, '../ui/style.css'), 'utf8');
  for (let shade = 1; shade <= 4; shade += 1) assert.match(css, new RegExp(`--grok-${shade}: #`));
  assert.match(css, /\.grok-mark \{ --service-icon: url\("icons\/grok\.svg"\); \}/);
  assert.ok(fs.existsSync(path.join(__dirname, '../ui/icons/grok.svg')));
});
test('keeps every sizable credential and folds only tiny ones into Others', () => {
  const { shareGroups } = setup();
  // Six large credentials all keep their slice; there is no fixed count.
  const named = Array.from({ length: 6 }, (_, i) => credential(`c${i}`, 100 - i, 'Claude'));
  const [all] = shareGroups([...named, credential('Unidentified', 5, 'Claude')], requests);
  assert.deepEqual(all.slices.map((s) => s.name), ['c0', 'c1', 'c2', 'c3', 'c4', 'c5', 'Unidentified']);
  // Under 2.5% of the whole ring, two or more fold, with any service.
  const tiny = [credential('t0', 2, 'Claude'), credential('t1', 1, 'Claude'), credential('x', 500)];
  const [codex, claude] = shareGroups([...named, ...tiny], requests);
  assert.deepEqual(codex.slices.map((s) => s.name), ['x']);
  assert.deepEqual(claude.slices.map((s) => s.name), ['c0', 'c1', 'c2', 'c3', 'c4', 'c5', 'Others (2)']);
  const others = claude.slices[6];
  assert.equal(others.value, 3);
  assert.equal(others.color, 'color-mix(in srgb, var(--claude-4) 70%, transparent)');
  assert.deepEqual(plain(others.members), [{ name: 't0', value: 2 }, { name: 't1', value: 1 }]);
  // Others never holds a single credential: a lone tiny one keeps its slice.
  const lone = shareGroups([...named, tiny[0], tiny[2]], requests)[1].slices;
  assert.deepEqual(lone.map((s) => s.name), ['c0', 'c1', 'c2', 'c3', 'c4', 'c5', 't0']);
  // The threshold is inclusive: exactly 2.5% stays.
  const edge = shareGroups([credential('big', 72), credential('s', 4), credential('e', 2), credential('f', 1), credential('g', 1)], requests)[0].slices;
  assert.deepEqual(plain(edge.map((s) => [s.name, s.value])), [['big', 72], ['s', 4], ['e', 2], ['Others (2)', 2]]);
});
test('the two largest of each service never fold into Others', () => {
  const { shareGroups } = setup();
  const tiny = (name, n) => credential(name, n, 'Claude');
  // Claude is tiny next to Codex: its two largest stay, the third and fourth fold.
  const [, claude] = shareGroups([credential('x', 1000), tiny('t0', 3), tiny('t1', 2), tiny('t2', 1), tiny('t3', 1)], requests);
  assert.deepEqual(claude.slices.map((s) => s.name), ['t0', 't1', 'Others (2)']);
  assert.deepEqual(plain(claude.slices[2].members.map((m) => m.name)), ['t2', 't3']);
  // A tiny third alone keeps its slice, and so do two or fewer.
  assert.deepEqual(shareGroups([credential('x', 1000), tiny('t0', 3), tiny('t1', 2), tiny('t2', 1)], requests)[1].slices.map((s) => s.name), ['t0', 't1', 't2']);
  assert.deepEqual(shareGroups([credential('x', 1000), tiny('t0', 3), tiny('t1', 2)], requests)[1].slices.map((s) => s.name), ['t0', 't1']);
});
test('unknown amounts keep their row instead of folding', () => {
  const { shareGroups } = setup();
  const measure = (c) => c.inputTokens;
  const tokens = (inputTokens) => ({ inputTokens });
  const [group] = shareGroups([credential('a', 1, 'Codex', tokens(1000)), credential('a2', 1, 'Codex', tokens(900)), credential('b', 1), credential('c', 1, 'Codex', tokens(1)), credential('d', 1, 'Codex', tokens(0))], measure);
  assert.deepEqual(plain(group.slices.map((s) => [s.name, s.value])), [['a', 1000], ['a2', 900], ['b', null], ['Others (2)', 1]]);
});
test('shades follow the defined ones and blend between them beyond four', () => {
  const { shareGroups, shareShade } = setup();
  const four = shareGroups(Array.from({ length: 4 }, (_, i) => credential(`c${i}`, 10 - i, 'Claude')), requests)[0].slices;
  assert.deepEqual(four.map((s) => s.color), ['var(--claude-1)', 'var(--claude-2)', 'var(--claude-3)', 'var(--claude-4)']);
  const seven = Array.from({ length: 7 }, (_, i) => shareShade('Codex', i, 7));
  assert.deepEqual(seven, [
    'var(--codex-1)',
    'color-mix(in srgb, var(--codex-2) 50%, var(--codex-1))',
    'var(--codex-2)',
    'color-mix(in srgb, var(--codex-3) 50%, var(--codex-2))',
    'var(--codex-3)',
    'color-mix(in srgb, var(--codex-4) 50%, var(--codex-3))',
    'var(--codex-4)',
  ]);
  assert.equal(new Set(Array.from({ length: 12 }, (_, i) => shareShade('Claude', i, 12))).size, 12);
});
test('Codex comes before Claude whatever the order or size', () => {
  const { shareGroups } = setup();
  const groups = shareGroups([credential('b', 500, 'Claude'), credential('a', 1)], requests);
  assert.deepEqual(plain(groups.map((g) => g.service)), ['Codex', 'Claude']);
  assert.deepEqual(plain(shareGroups([credential('b', 5, 'Claude')], requests).map((g) => g.service)), ['Claude']);
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
  const credentials = [
    credential('a', 3, 'Codex', { inputTokens: 1_200_000, outputTokens: 300_000 }),
    credential('b', 2, 'Claude', { inputTokens: 800, outputTokens: null }),
    credential('silent', 5, 'Claude'),
    credential('zero', 1, 'Claude', { inputTokens: 0, outputTokens: 0 }),
  ];
  const html = trafficShare(credentials, 'model');
  assert.match(html, /<strong>1\.5M<\/strong><span>tokens<\/span>/);
  assert.match(html, /<span>a<\/span><span>1\.5M<\/span>/);
  assert.match(html, /<span>b<\/span><span>800<\/span>/);
  assert.doesNotMatch(html, /silent|zero/);
  const all = trafficShare(credentials, 'all');
  assert.match(all, /<span>silent<\/span><span>5<\/span>/);
  assert.match(all, /<span>zero<\/span><span>1<\/span>/);
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
test('every sector arc bends the short or long way its ends call for', () => {
  // A growing or shrinking slice passes every width, including those just
  // over half a turn whose rounded corners leave arcs under half.
  const { ringSector } = setup();
  const arc = /(-?[\d.]+) (-?[\d.]+) A ([\d.]+) [\d.]+ 0 ([01]) ([01]) (-?[\d.]+) (-?[\d.]+)/g;
  for (const inner of [42 * 0.618, 30]) {
    for (let i = 1; i <= 2000; i++) {
      const from = 0.13;
      const d = ringSector(from, from + i / 2000, 42, inner);
      for (const m of d.matchAll(arc)) {
        const [x1, y1, , large, sweep, x2, y2] = m.slice(1).map(Number);
        const turn = sweep ? Math.atan2(y2 - 42, x2 - 42) - Math.atan2(y1 - 42, x1 - 42) : Math.atan2(y1 - 42, x1 - 42) - Math.atan2(y2 - 42, x2 - 42);
        const span = ((turn % (2 * Math.PI)) + 2 * Math.PI) % (2 * Math.PI);
        if (Math.abs(span - Math.PI) < 1e-3) continue;
        assert.equal(large, span > Math.PI ? 1 : 0, `width ${i / 2000}, inner ${inner}: ${m[0]}`);
      }
    }
  }
});
test('traffic data keeps the category it was read for', async () => {
  const pending = [];
  const views = createTrafficViews({ invoke: (_, query) => new Promise((resolve) => pending.push({ query, resolve })) });
  views.scope = 'all';
  const load = views.loadActivity();
  // The selector already shows the new category while the old data renders.
  views.scope = 'model';
  pending[0].resolve({ credentials: [] });
  assert.equal(await load, true);
  assert.equal(pending[0].query.scope, 'all');
  assert.equal(views.activity.scope, 'all');
});
test('a traffic read still running is not started again', async () => {
  const pending = [];
  let now = 1e6;
  const views = createTrafficViews({ invoke: (_, query) => new Promise((resolve) => pending.push({ query, resolve })), now: () => now });
  views.scope = 'all';
  views.minutes = 43200;
  const first = views.loadActivity();
  assert.equal(await views.loadActivity(), false);
  assert.equal(pending.length, 1);
  // Another range starts at once.
  views.minutes = 30;
  const second = views.loadActivity();
  assert.equal(pending.length, 2);
  // Switching back joins the read still running instead of repeating it.
  views.setMinutes(43200);
  const third = views.loadActivity();
  assert.equal(pending.length, 2);
  for (const { resolve } of pending) resolve({ credentials: [] });
  assert.deepEqual(await Promise.all([first, second, third]), [false, false, true]);
  // A view read moments ago is reused; an older one is read again.
  await views.loadActivity();
  assert.equal(pending.length, 2);
  now += 15000;
  views.loadActivity();
  assert.equal(pending.length, 3);
});
test('home traffic keeps the category it was read for', async () => {
  const pending = [];
  const views = createTrafficViews({ invoke: (_, query) => new Promise((resolve) => pending.push({ query, resolve })) });
  views.scope = 'all';
  const load = views.loadHome(null);
  views.scope = 'model';
  pending[0].resolve({ start: 1000, end: 181000, bucketMinutes: 1, requests: 3, credentials: [credential('a', 3)] });
  assert.equal(await load, true);
  assert.equal(pending[0].query.scope, 'all');
  assert.equal(views.home.scope, 'all');
  assert.equal(views.home.requests, 3);
  assert.deepEqual(plain(views.home.credentials.map((c) => [c.credential, c.requests])), [['a', 3]]);
  for (const stats of [views.home, views.localView(30, 'all')]) {
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
test('Models charts draw one baseline for every bucket without reported tokens', () => {
  const context = setup('model');
  vm.runInContext(source.slice(source.indexOf('function chart('), source.indexOf('\nasync function loadHomeTraffic(')), context);
  const failed = { tokenCounts: [0, 0, 0], counts: [1, 0, 2], errorCounts: [1, 0, 2] };
  const html = context.chart(failed, 'model', true);
  assert.doesNotMatch(html, /chart-peak/);
  assert.match(html, /No reported tokens/);
  assert.doesNotMatch(html, /calls|requests/);
  const groups = [...html.matchAll(/<g class="chart-bar"[^>]*>(.*?)<\/g>/g)].map(m => m[1]);
  // Idle buckets and calls without reported tokens look the same, as on This Device.
  for (const group of groups) {
    assert.match(group, /<rect class="idle"[^>]* y="31.00"[^>]* height="1.00"/);
    assert.doesNotMatch(group, /class="err"/);
  }
  assert.equal((html.match(/class="chart-hit"/g) || []).length, 3);
  const cancelled = context.chart({ ...failed, errorCounts: [0, 0, 0] }, 'model', true);
  assert.equal((cancelled.match(/class="idle"/g) || []).length, 3);
  assert.doesNotMatch(cancelled, /<rect class="(?:err|)"/);
  const requests = context.chart(failed, 'all', true);
  assert.equal((requests.match(/class="err"/g) || []).length, 2);
  assert.match(context.chart({ ...failed, tokenCounts: [5, 0, 0] }, 'model', true), /class="chart-peak"/);
  assert.match(context.chart({ ...failed, counts: [0, 0, 0], errorCounts: [0, 0, 0] }, 'all', true), /class="chart-peak"/);
});
test('Models shows the average first token time before Avg. Time where this device logged it', () => {
  const context = setup('model');
  Object.assign(context, { stat: (label, value) => `<${label}=${value}>`, fmtMs: (v) => v == null ? '—' : `${v} ms`, fmtBytes: (v) => `${v} B`, modelTokenStats: () => '' });
  vm.runInContext(source.slice(source.indexOf('function chart('), source.indexOf('\nasync function loadHomeTraffic(')), context);
  const local = { service: 'Codex', credential: 'work', requests: 3, errors: 0, avgMs: 4000, firstTokenMs: 300, firstTokenTotalMs: 600, firstTokenSamples: 2, forwardedFirstTokenTotalMs: 600, forwardedFirstTokenSamples: 2, bytes: 9, tokenCounts: [1], counts: [3], errorCounts: [0], credentials: [], start: 0, end: 60000, bucketMinutes: 1 };
  for (const html of [context.accountRow(local, 'model', local), context.trafficSummary(local, 'model')]) {
    assert.match(html, /<Error Rate=[^>]*>\s*<First Token=300 ms>\s*<Avg\. Time=4000 ms>\s*<Output TPS<span[^>]*>!<\/span>=—>/);
    assert.doesNotMatch(html, /Received/);
  }
  // No streamed output is unknown, not zero.
  const none = { ...local, firstTokenMs: null, firstTokenSamples: 0, forwardedFirstTokenTotalMs: 0, forwardedFirstTokenSamples: 0 };
  assert.match(context.accountRow(none, 'model', none), /<First Token=—>/);
  // Requests have no first token, and peers' summaries do not report it.
  const remote = { ...local, firstTokenMs: undefined, firstTokenSamples: undefined, forwardedFirstTokenTotalMs: undefined, forwardedFirstTokenSamples: undefined };
  for (const [view, scope] of [[local, 'all'], [remote, 'model']]) {
    assert.doesNotMatch(context.accountRow(view, scope, view), /First Token/);
    assert.doesNotMatch(context.trafficSummary({ ...view }, scope), /First Token/);
  }
});
test('Models shows Unidentified rows whose calls all failed as one line of calls and errors', () => {
  const context = setup('model');
  Object.assign(context, { stat: (label, value, cls = '') => `<${label}=${value}${cls ? `:${cls}` : ''}>`, fmtMs: (v) => `${v} ms`, fmtBytes: (v) => `${v} B`, modelTokenStats: () => '<tokens>' });
  vm.runInContext(source.slice(source.indexOf('function chart('), source.indexOf('\nasync function loadHomeTraffic(')), context);
  const failed = { service: 'Grok', credential: 'Unidentified', requests: 16, errors: 16, errorRate: 1, avgMs: 0, bytes: 0, tokenCounts: [0, 0, 0], counts: [6, 0, 10], errorCounts: [6, 0, 10] };
  const brief = context.accountRow(failed, 'model', failed, '<review>');
  assert.equal(brief.replace(/ data-tip-scope="[^"]*"/, ''), '<div class="credential-traffic"><div class="traffic-identity brief"><span class="traffic-service">[Grok]Grok</span><strong>Unidentified</strong>'
    + '<span class="traffic-brief"><span>16 calls · <span class="bad">100% errors</span></span><review></span></div></div>');
  assert.match(context.accountRow({ ...failed, requests: 1, errors: 1, errorRate: 1 }, 'model', failed), />1 call · </);
  const full = (html) => /class="chart-bar"/.test(html) && /<Avg\. Time=/.test(html) && !/Received|brief/.test(html);
  // Requests keep the full row, and so does a named account that only failed.
  assert.ok(full(context.accountRow(failed, 'all', failed)));
  assert.ok(full(context.accountRow({ ...failed, credential: 'work' }, 'model', failed)));
  assert.match(context.accountRow({ ...failed, credential: 'work' }, 'model', failed), /<tokens>/);
  // One successful call keeps the full row.
  const mixed = { ...failed, errors: 15, errorRate: 15 / 16, errorCounts: [6, 0, 9], tokenCounts: [0, 0, 40] };
  assert.ok(full(context.accountRow(mixed, 'model', mixed)));
  // Activity and Devices account rows both go through it.
  for (const [from, to] of [['function renderActivityTraffic(', '\nconst SEARCH_MODES'], ['function deviceTrafficContent(', '\ndocument.addEventListener("toggle"']]) {
    const body = source.slice(source.indexOf(from), source.indexOf(to));
    assert.match(body, /accountRow\(c, scope, /);
    assert.doesNotMatch(body, /chart\(c,|Avg\. Time|traffic-identity/);
  }
  // The single line drops the gap that a chart would follow.
  const css = fs.readFileSync(path.join(__dirname, '../ui/style.css'), 'utf8');
  assert.match(css, /\.traffic-identity\.brief \{ margin-bottom: 0; \}/);
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
  const named = [credential('c0', 100, 'Claude'), credential('c1', 99, 'Claude'), credential('c2', 2, 'Claude'), credential('c3', 1, 'Claude')];
  const html = trafficShare(named, 'all');
  const tip = 'Claude · Others (2): 3 requests\nc2: 2 requests\nc3: 1 requests';
  // Both the legend row and its ring sector carry the list.
  assert.equal(html.split(`title="${tip}"`).length, 2);
  assert.equal(html.split(`<title>${tip}</title>`).length, 2);
});
test('Others takes the next shade of its service, fainter past the defined ones', () => {
  const { shareGroups, trafficShare } = setup();
  const others = (count) => {
    const kept = Array.from({ length: count }, (_, i) => credential(`k${i}`, 1000, 'Codex'));
    return shareGroups([...kept, credential('t0', 1), credential('t1', 1)], requests)[0].slices.at(-1);
  };
  assert.equal(others(2).name, 'Others (2)');
  assert.equal(others(2).color, 'var(--codex-3)');
  assert.equal(others(3).color, 'var(--codex-4)');
  assert.equal(others(4).color, 'color-mix(in srgb, var(--codex-4) 70%, transparent)');
  assert.equal(others(6).color, 'color-mix(in srgb, var(--codex-4) 70%, transparent)');
  // Credentials past the fourth still blend between the defined shades.
  assert.equal(shareGroups(Array.from({ length: 6 }, (_, i) => credential(`k${i}`, 100)), requests)[0].slices.at(-1).color, 'var(--codex-4)');
  const named = [credential('c0', 100, 'Claude'), credential('c1', 90, 'Claude'), credential('c2', 2, 'Claude'), credential('c3', 1, 'Claude'), credential('Unidentified', 20, 'Claude')];
  const html = trafficShare(named, 'all');
  const fill = (name) => html.match(new RegExp(`<path style="--slice:([^"]+)"[^>]*><title>Claude · ${name}`))[1];
  assert.equal(fill('Others \\(2\\)'), 'var(--claude-3)');
  assert.equal(fill('Unidentified'), 'var(--share-unidentified)');
  // Light keeps the faint fill; dark needs a stronger one to show on the block.
  const css = fs.readFileSync(path.join(__dirname, '../ui/style.css'), 'utf8');
  const alphas = [...css.matchAll(/--share-unidentified: rgba\((\d+), \d+, \d+, ([\d.]+)\);/g)].map((m) => [Number(m[1]), Number(m[2])]);
  assert.deepEqual(alphas, [[0, 0.08], [255, 0.48]]);
  // Only Unidentified dims its legend row.
  assert.match(html, /<div class="share-item" title="Claude · Others \(2\)/);
  assert.match(html, /<div class="share-item muted" title="Claude · Unidentified/);
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
    for (const i of [1, 2]) assert.match(groups[i], /<rect class="idle"[^>]* y="31.00"[^>]* height="1.00"/);
  }
  const old = context.chart(stats, 'model', false, { start, end, bucketMinutes: 1 });
  const refreshed = context.chart(stats, 'model', false, { start: start + 15_000, end: end + 15_000, bucketMinutes: 1 });
  const slots = html => [...html.matchAll(/data-tip-slot="([^"]*)"/g)].map(m => m[1]);
  assert.deepEqual(slots(old), slots(refreshed), 'live-window movement retains the hovered column');
  assert.notDeepEqual(tips(old), tips(refreshed), 'its displayed time range still updates');
});

test('hovering across bars updates the tooltip and leaving hides it', () => {
  const events = {};
  const timers = new Map();
  let timerId = 0;
  const tip = { hidden: true, style: {}, contains: () => false, getBoundingClientRect: () => ({ width: 180, height: 40 }) };
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
    const target = { owner, parentElement: owner, closest: selector => selector.includes('.chart-bar') ? owner : null };
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

test('device traffic exposes renamed local sources with assignment choices', () => {
  const context = setup();
  Object.assign(context, {
    ICON: { warning: '<warning>', more: '<more>', chevron: '' },
    esc: v => String(v).replace(/&/g, '&amp;').replace(/"/g, '&quot;'),
    trafficShare: () => '', chart: () => '', stat: () => '', fmtMs: () => '',
    fmtBytes: () => '', modelTokenStats: () => '',
  });
  vm.runInContext(source.slice(source.indexOf('/// One account\'s row'), source.indexOf('\n/// The share ring, chart and totals')), context);
  loadSharedViews(context);
  vm.runInContext(source.slice(source.indexOf('function deviceTrafficContent('), source.indexOf('document.addEventListener("toggle"')), context);
  const traffic = { requests: 2, errors: 0, credentials: [{ service: 'Claude', credential: 'myServer', requests: 2,
    sources: [{ name: 'api', base: 'https://example.invalid', reason: 'renamed', requests: 2 }] }],
    targets: [{ service: 'Claude', name: 'myServer', label: 'myServer', base: 'https://example.invalid' }] };
  const html = context.deviceTrafficContent(traffic, 'all');
  assert.match(html, /configurations need review/);
  assert.match(html, /traffic-review review/);
  const items = JSON.parse(html.match(/data-menu="([^"]*)"/)[1].replace(/&quot;/g, '"').replace(/&amp;/g, '&'));
  assert.equal(items[0].detail, 'Same base URL, different name · 2 requests');
  assert.equal(JSON.parse(items[1].target).name, 'api');
  assert.equal(JSON.parse(items[1].target).target.name, 'myServer');
  traffic.credentials[0].sources[0].reason = null;
  assert.doesNotMatch(context.deviceTrafficContent(traffic, 'model'), /traffic-review review|configurations need review/);
});

test('assigning traffic refreshes device views and removes stale cached mappings', async () => {
  const context = setup();
  const calls = [];
  Object.assign(context, { invoke: async () => calls.push('assign'),
    loadTraffic: async () => calls.push('activity'), loadMergedData: async force => calls.push(force),
    refresh: async () => calls.push('refresh'), trafficViews: { reset: () => calls.push('reset') } });
  loadSharedViews(context);
  context.ui.deviceTrafficViews = { old: {} };
  await context.assignTraffic({});
  assert.deepEqual(calls, ['assign', 'reset', 'activity', true, 'refresh']);
  assert.equal(Object.keys(context.ui.deviceTrafficViews).length, 0);
});
test('every bucket in an idle chart retains its one-pixel baseline in both scopes', () => {
  const context = setup();
  vm.runInContext(source.slice(source.indexOf('function chart('), source.indexOf('\nasync function loadHomeTraffic(')), context);
  const stats = { counts: [0, 0, 0], tokenCounts: [0, 0, 0], errorCounts: [0, 0, 0] };
  for (const scope of ['model', 'all']) {
    const html = context.chart(stats, scope, true);
    assert.equal((html.match(/<rect class="idle"[^>]* y="31.00"[^>]* height="1.00"/g) || []).length, 3);
    assert.equal((html.match(/class="chart-hit"/g) || []).length, 3);
    assert.doesNotMatch(html, /class="err"/);
  }
});
test('home traffic draws the share ring above its chart', () => {
  const context = setup('all');
  context.trafficViews.home = { requests: 4, errors: 0, avgMs: 10, bytes: 0, counts: [4], errorCounts: [0], start: 0, end: 60000, bucketMinutes: 1, scope: 'all', credentials: [credential('a', 3), credential('b', 1, 'Claude')] };
  Object.assign(context, { block: (title, aside, body) => title + body, stat: (label, value) => `${label}:${value}`, fmtMs: String, fmtBytes: String, modelTokenStats: () => '', trafficControls: () => '', homeTrafficDevice: () => null });
  vm.runInContext(source.slice(source.indexOf('function chart('), source.indexOf('/// Home shows the forwarded device')), context);
  vm.runInContext(source.slice(source.indexOf('function trafficBlock('), source.indexOf('\nfunction connectBlock(')), context);
  const html = context.trafficBlock();
  assert.match(html, /class="traffic-share"[\s\S]*class="chart"/);
  assert.match(html, /\[Codex\][\s\S]*\[Claude\]/);
  // Before credentials arrive, the block still renders without a ring.
  context.trafficViews.home = { ...context.trafficViews.home };
  delete context.trafficViews.home.credentials;
  assert.doesNotMatch(context.trafficBlock(), /traffic-share/);
});
test('Home Traffic hides part of an account email while configuration names stay readable', () => {
  const context = setup('all');
  const stats = (requests) => ({ requests, errors: 0, bytes: 0, counts: [requests], errorCounts: [0], tokenCounts: [0] });
  context.trafficViews.home = { ...stats(4), avgMs: 10, start: 0, end: 60000, bucketMinutes: 1, scope: 'all', credentials: [
    { service: 'Codex', credential: 'suzhi2333@gmail.com', ...stats(3) },
    { service: 'Codex', credential: 'work', ...stats(1) },
  ] };
  Object.assign(context, { block: (title, aside, body) => title + body, stat: (label, value) => `${label}:${value}`, fmtMs: String, fmtBytes: String, modelTokenStats: () => '', trafficControls: () => '', homeTrafficDevice: () => null });
  vm.runInContext(source.slice(source.indexOf('function chart('), source.indexOf('/// Home shows the forwarded device')), context);
  vm.runInContext(source.slice(source.indexOf('function trafficBlock('), source.indexOf('\nfunction connectBlock(')), context);
  const html = context.trafficBlock();
  assert.match(html, /<span>su\*\*\*3@gmail\.com<\/span>/);
  assert.match(html, /<span>work<\/span>/);
  assert.doesNotMatch(html, /suzhi2333@gmail\.com/);
});
test('forwarding Home reads the destination from the daemon cache, never local history', async () => {
  const calls = [];
  const remote = {
    deviceId: 'ms', name: 'MS', fetchedAt: Date.now(), error: null, scope: 'all', windowStart: 0, windowEnd: 21600000, bucketMinutes: 10,
    traffic: { requests: 5, errors: 1, avgMs: 20, bytes: 9, counts: [5], errorCounts: [1], tokenCounts: [0], credentials: [credential('remote-account', 5)] },
  };
  const views = createTrafficViews({ invoke: async (command, args) => { calls.push([command, args]); return remote; } });
  views.scope = 'all';
  views.minutes = 360;
  await views.loadHome('ms');
  assert.deepEqual(plain(calls), [['get_forwarding_traffic', { minutes: 360, scope: 'all' }]]);
  // A remote view must not enter this machine's cache used by Devices.
  assert.equal(views.localView(360, 'all'), undefined);
  assert.equal(views.home.device, 'ms');
  assert.deepEqual([views.home.start, views.home.end, views.home.bucketMinutes], [0, 21600000, 10]);
  // The Devices card of the forwarded device reuses Home's view instead of asking again.
  assert.equal(await views.forwarded('ms', 360, 'all'), views.home);
  assert.equal(calls.length, 1);
  // This device's own view is still read locally.
  await views.local(360, 'all');
  assert.equal(calls[1][0], 'get_traffic');
});
test('forwarding Home Traffic labels the destination and hides another source', () => {
  const context = setup('all');
  vm.runInContext(source.slice(source.indexOf('function homeTrafficDevice('), source.indexOf('async function loadHomeTraffic(')), context);
  Object.assign(context, { block: (title, aside, body) => title + body, stat: (label, value) => `${label}:${value}`, fmtMs: String, fmtBytes: String, modelTokenStats: () => '', trafficControls: () => '' });
  vm.runInContext(source.slice(source.indexOf('function chart('), source.indexOf('/// Home shows the forwarded device')), context);
  vm.runInContext(source.slice(source.indexOf('function trafficBlock('), source.indexOf('\nfunction connectBlock(')), context);
  const { ui } = context;
  ui.page = 'main';
  ui.snap = { forwarding: { deviceId: 'ms' } };
  const views = context.trafficViews;
  views.homeError = '';
  // Local history read before forwarding started is not shown as the destination's.
  views.home = { requests: 7, errors: 0, counts: [7], errorCounts: [0], start: 0, end: 60000, bucketMinutes: 1, scope: 'all', credentials: [credential('local', 7)], device: null };
  assert.match(context.trafficBlock(), /Loading traffic…/);
  views.home = { requests: 5, errors: 0, counts: [5], errorCounts: [0], start: 0, end: 60000, bucketMinutes: 1, scope: 'all', credentials: [credential('remote-account', 5)], device: 'ms', deviceName: 'MS', fetchedAt: Date.now(), remoteError: null };
  let html = context.trafficBlock();
  assert.match(html, /class="traffic-share"[\s\S]*remote-account/);
  assert.match(html, /Includes other clients on MS/);
  assert.doesNotMatch(html, /stale|Update failed/);
  // A failed later read keeps the last window, marked stale.
  views.home.remoteError = 'statistics offline';
  html = context.trafficBlock();
  assert.match(html, /stale/);
  assert.match(html, /Update failed/);
  // Before the daemon's first summary, the block waits instead of reporting failure.
  views.home = null;
  views.homeError = 'TRAFFIC_UPDATING';
  assert.match(context.trafficBlock(), /Loading traffic…/);
});
test('First Token shows where a device reports it, formatting the backend average', () => {
  const context = setup('model');
  loadSharedViews(context);
  Object.assign(context, { stat: (label, value) => `<${label}=${value}>`, fmtMs: v => v == null ? '—' : `${v} ms` });
  assert.equal(context.firstTokenStat({ firstTokenSamples: 2, firstTokenMs: 450 }, 'model'), '<First Token=450 ms>');
  // A measured zero is valid; reported without streamed output is unknown.
  assert.equal(context.firstTokenStat({ firstTokenSamples: 1, firstTokenMs: 0 }, 'model'), '<First Token=0 ms>');
  assert.equal(context.firstTokenStat({ firstTokenSamples: 0, firstTokenMs: null }, 'model'), '<First Token=—>');
  assert.equal(context.firstTokenStat({ forwardedFirstTokenSamples: 0, firstTokenMs: null }, 'model'), '<First Token=—>');
  // Older devices do not report it: not shown at all, nor with All.
  assert.equal(context.firstTokenStat({ firstTokenMs: null }, 'model'), '');
  assert.equal(context.firstTokenStat({ firstTokenSamples: 2, firstTokenMs: 450 }, 'all'), '');
  Object.assign(context, { chart: () => '', fmtBytes: String, modelTokenStats: () => '' });
  assert.doesNotMatch(context.trafficSummary({ requests: 3, credentials: [] }, 'model'), /First Token/);
  assert.match(context.trafficSummary({ requests: 3, firstTokenSamples: 2, firstTokenMs: 450, credentials: [] }, 'model'), /<First Token=450 ms>/);
});
test('Output TPS renders the backend rate and unknown measurements', () => {
  const context = setup('model');
  loadSharedViews(context);
  Object.assign(context, { ICON: { warning: '!' }, stat: (label, value) => `<stat>${label}=${value}</stat>` });
  vm.runInContext(source.slice(source.indexOf('function modelTokenStats('), source.indexOf('const SHARE_SERVICES')), context);
  const total = { outputTps: 40, credentials: [] };
  assert.match(context.outputTpsStat(total, 'model'), /Output TPS[^]*=40\.0 tok\/s/);
  assert.doesNotMatch(context.modelTokenStats(total, 'model'), /Output TPS/);
  for (const outputTps of [null, undefined, NaN]) assert.match(context.outputTpsStat({ outputTps }, 'model'), /Output TPS[^]*=—/);
  assert.equal(context.modelTokenStats(total, 'all'), '');
  assert.equal(context.outputTpsStat(total, 'all'), '');
});
test('Output TPS replaces Received in the summary and account row, leaving four token stats', () => {
  const context = setup('model');
  loadSharedViews(context);
  Object.assign(context, { ICON: { warning: '!' }, chart: () => '', fmtMs: String, fmtBytes: String });
  for (const [start, end] of [
    ['function stat(', '// ---------------------------------------------------------------- main'],
    ["/// One account's row", '/// The share ring, chart and totals'],
    ['function modelTokenStats(', 'const SHARE_SERVICES'],
  ]) vm.runInContext(source.slice(source.indexOf(start), source.indexOf(end)), context);
  const view = { service: 'Codex', credential: 'work', requests: 1, errors: 0,
    firstTokenSamples: 1, firstTokenMs: 500, avgMs: 2500, outputTps: 50,
    inputTokens: 10, outputTokens: 101, cachedInputTokens: 0, cacheHitRate: 0, credentials: [] };
  for (const html of [context.trafficSummary(view, 'model'), context.accountRow(view, 'model', view)]) {
    const strips = html.split('<div class="strip">').slice(1);
    assert.equal(strips.length, 2);
    assert.equal((strips[0].match(/class="stat"/g) || []).length, 5);
    assert.match(strips[0], /Avg\. Time[^]*Output TPS[^]*50\.0 tok\/s/);
    assert.equal((strips[1].match(/class="stat"/g) || []).length, 4);
    assert.doesNotMatch(strips[1], /Output TPS/);
    assert.doesNotMatch(html, /Received/);
  }
});
test('Output TPS warning tooltip adds the total-duration rate without changing the main value', () => {
  const context = setup('model');
  context.stat = (label, value) => `${label}<value>${value}</value>`;
  const stats = { outputTpsFromStart: 50, outputTps: 66.6 };
  const html = context.outputTpsStat(stats, 'model');
  assert.match(html, /^Output TPS<span class="info-tip stat-tip" tabindex="0" data-tip-key="tooltip-\d+" data-tip="From request start instead of the first token: 50\.0 tok\/s\."/);
  assert.match(html, /aria-label="From request start instead of the first token: 50\.0 tok\/s\."/);
  assert.match(html, />!<\/span><value>66\.6 tok\/s<\/value>$/);
  for (const fields of [{ outputTpsFromStart: null }, { outputTpsFromStart: undefined }, { outputTpsFromStart: NaN }, { outputTpsFromStart: Infinity }]) {
    const unknown = context.outputTpsStat({ ...stats, ...fields }, 'model');
    assert.match(unknown, /data-tip="From request start instead of the first token: —\."/);
    assert.match(unknown, /<value>66\.6 tok\/s<\/value>$/);
  }
  const merged = { outputTpsFromStart: 40, outputTps: 49.75 };
  assert.match(context.outputTpsStat(merged, 'model'), /data-tip="From request start instead of the first token: 40\.0 tok\/s\."/);
  const keyOf = html => html.match(/data-tip-key="([^"]+)"/)[1];
  const identity = JSON.stringify(['account', 'local', 'Codex', 'private@example.test']);
  const first = context.outputTpsStat(stats, 'model', identity);
  assert.doesNotMatch(first, /private@example/);
  assert.equal(keyOf(first), keyOf(context.outputTpsStat(merged, 'model', identity)));
  assert.notEqual(keyOf(first), keyOf(context.outputTpsStat(stats, 'model', JSON.stringify(['account', 'remote', 'Codex', 'private@example.test']))));
});
test('an unchanged view reuses its markup', () => {
  const context = setup('all');
  loadSharedViews(context);
  let charts = 0;
  Object.assign(context, { chart: () => { charts++; return '<chart>'; }, stat: () => '', fmtMs: String, fmtBytes: String, modelTokenStats: () => '' });
  const view = { requests: 1, errors: 0, credentials: [credential('a', 1)], start: 0, end: 60000, bucketMinutes: 1 };
  const html = context.trafficSummary(view, 'all');
  assert.equal(context.trafficSummary(view, 'all'), html);
  assert.equal(charts, 1);
  // Another window or category is drawn afresh.
  context.trafficSummary(view, 'model');
  context.trafficSummary(view, 'all', { start: 0, end: 120000, bucketMinutes: 1 });
  assert.equal(charts, 3);
});
test('Home, Activity and This Device share one read of the same traffic', async () => {
  const pending = [];
  const views = createTrafficViews({ invoke: (command, query) => new Promise((resolve) => pending.push({ command, query, resolve })) });
  views.scope = 'all';
  const reads = [views.loadHome(null), views.loadActivity(), views.local(30, 'all')];
  assert.equal(pending.length, 1);
  pending[0].resolve({ start: 0, end: 1800000, bucketMinutes: 1, requests: 4, credentials: [], targets: [] });
  const [, , card] = await Promise.all(reads);
  assert.equal(views.home, card);
  assert.equal(views.activity, card);
  assert.equal(card.requests, 4);
  // A different range is its own read.
  views.minutes = 360;
  views.loadHome(null);
  assert.equal(pending.length, 2);
  assert.equal(pending[1].query.minutes, 360);
});
test('a reset while reading discards the old result and reads again', async () => {
  const pending = [];
  const views = createTrafficViews({ invoke: (command, query) => new Promise((resolve) => pending.push({ command, query, resolve })) });
  const read = views.local(30, 'all');
  // Traffic is reassigned while the read runs.
  views.reset();
  pending[0].resolve({ requests: 1, credentials: [] });
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(pending.length, 2, 'the stale result is read again');
  assert.equal(views.localView(30, 'all'), undefined);
  pending[1].resolve({ requests: 2, credentials: [] });
  assert.equal((await read).requests, 2);
  assert.equal(views.localView(30, 'all').requests, 2);
});
test('Account names start after the longest service name, Antigravity', () => {
  const css = fs.readFileSync(path.join(__dirname, '../ui/style.css'), 'utf8');
  const rule = css.match(/\.traffic-identity \{[^}]*grid-template-columns: ([^;]+);/)[1];
  // The service column grows with its content and never starts narrower than
  // the 16px mark, 4px gap and "Antigravity" at 11px (about 57px on macOS).
  const first = rule.match(/^minmax\((\d+)px, max-content\)/);
  assert.ok(first, `service column must grow with its content: ${rule}`);
  assert.ok(Number(first[1]) >= 16 + 4 + 57, `service column too narrow: ${rule}`);
});
test('Ring slices carry their key and part of the turn for redraw motion', () => {
  const { trafficShare } = setup('all');
  const credentials = [credential('work', 3), credential('home', 1), credential('Unidentified', 1, 'Claude')];
  const html = trafficShare(credentials, 'all', 'local');
  assert.match(html, /class="share-ring" data-morph="ring:local" data-ring="local"/);
  const slices = [...html.matchAll(/<path style="--slice:([^"]+)" data-slice="([^"]+)" data-width="([^"]+)"/g)].map((m) => [m[2], Number(m[3])]);
  assert.deepEqual(slices.map((s) => s[0]), ['Codex|work', 'Codex|home', 'Claude|Unidentified']);
  assert.ok(Math.abs(slices.reduce((sum, s) => sum + s[1], 0) - 1) < 1e-9);
  assert.ok(Math.abs(slices[0][1] - 0.6) < 1e-9);
  // Without a source a ring has nothing to move from.
  assert.doesNotMatch(trafficShare(credentials, 'all'), /data-ring=/);
});
test('Others keeps one key while its count changes', () => {
  const { trafficShare } = setup('all');
  const tiny = (n) => Array.from({ length: n }, (_, i) => credential(`tiny${i}`, 1));
  const keys = (html) => [...html.matchAll(/data-slice="([^"]+)"/g)].map((m) => m[1]);
  const big = [credential('a', 1000), credential('b', 1000)];
  assert.ok(keys(trafficShare([...big, ...tiny(2)], 'all', 'local')).includes('Codex|Others'));
  assert.ok(keys(trafficShare([...big, ...tiny(3)], 'all', 'local')).includes('Codex|Others'));
});
test('A redrawn ring grows, shrinks, adds and removes slices without overlap', () => {
  const { blendRing } = setup();
  const from = [{ key: 'a', width: 0.5, color: 'A' }, { key: 'gone', width: 0.25, color: 'G' }, { key: 'b', width: 0.25, color: 'B' }];
  const to = [{ key: 'new', width: 0.2, color: 'N' }, { key: 'a', width: 0.4, color: 'A2' }, { key: 'b', width: 0.4, color: 'B' }];
  const parts = (progress) => plain(blendRing(from, to, progress));
  // Gone slices stay after the slice they followed; new ones are where they will be.
  assert.deepEqual(parts(0).map((s) => s.key), ['new', 'a', 'gone', 'b']);
  const close = (actual, expected) => actual.forEach((s, i) => {
    assert.ok(Math.abs(s.from - expected[i][0]) < 1e-9 && Math.abs(s.to - expected[i][1]) < 1e-9, JSON.stringify(actual));
  });
  close(parts(0), [[0, 0], [0, 0.5], [0.5, 0.75], [0.75, 1]]);
  close(parts(1), [[0, 0.2], [0.2, 0.6], [0.6, 0.6], [0.6, 1]]);
  close(parts(0.5), [[0, 0.1], [0.1, 0.55], [0.55, 0.675], [0.675, 1]]);
  for (const progress of [0, 0.25, 0.5, 0.75, 1]) {
    const p = parts(progress);
    p.forEach((s, i) => {
      assert.ok(s.to >= s.from);
      if (i) assert.ok(Math.abs(s.from - p[i - 1].to) < 1e-12, 'slices touch, never overlap');
    });
    assert.ok(Math.abs(p[p.length - 1].to - 1) < 1e-9, 'the ring stays whole');
  }
  const colors = Object.fromEntries(parts(0.5).map((s) => [s.key, [s.fromColor ?? null, s.toColor ?? null]]));
  assert.deepEqual(colors, { new: [null, 'N'], a: ['A', 'A2'], gone: ['G', null], b: ['B', 'B'] });
});
test('A slice removed first in the ring shrinks away at the start', () => {
  const { blendRing } = setup();
  const p = plain(blendRing([{ key: 'x', width: 0.5 }, { key: 'y', width: 0.5 }], [{ key: 'y', width: 1 }], 0));
  assert.deepEqual(p.map((s) => s.key), ['x', 'y']);
});
test('A removed slice shrinks away inside its own service', () => {
  const { blendRing } = setup();
  const from = [{ key: 'Codex|a', width: 0.4 }, { key: 'Codex|b', width: 0.3 }, { key: 'Claude|c', width: 0.3 }];
  const to = [{ key: 'Codex|a', width: 0.5 }, { key: 'Claude|c', width: 0.5 }];
  assert.deepEqual(plain(blendRing(from, to, 0.5)).map((s) => [s.key, s.gone]), [['Codex|a', false], ['Codex|b', true], ['Claude|c', false]]);
});
test('Slices that change places are matched by rank, so the ring does not jump', () => {
  const { matchRing, blendRing } = setup();
  const slice = (key, width) => ({ key, slice: key, width });
  const same = [[slice('Codex|a', 0.6), slice('Codex|b', 0.4)], [slice('Codex|a', 0.5), slice('Codex|c', 0.2), slice('Codex|b', 0.3)]];
  assert.deepEqual(plain(matchRing(...same)), plain(same), 'unchanged order keeps configurations');
  const from = [slice('Codex|a', 0.5), slice('Codex|b', 0.3), slice('Codex|Unidentified', 0.2)];
  const to = [slice('Codex|b', 0.6), slice('Codex|a', 0.2), slice('Codex|new', 0.1), slice('Codex|Unidentified', 0.1)];
  const [start, end] = plain(matchRing(from, to));
  assert.deepEqual(start.map((s) => s.key), ['Codex|#1', 'Codex|#2', 'Codex|Unidentified']);
  assert.deepEqual(end.map((s) => s.key), ['Codex|#1', 'Codex|#2', 'Codex|#3', 'Codex|Unidentified']);
  // The first frame is the old ring, and each part draws the new ring's slice.
  const first = plain(blendRing(start, end, 0));
  assert.deepEqual(first.map((s) => [s.slice, +(s.to - s.from).toFixed(6)]), [['Codex|b', 0.5], ['Codex|a', 0.3], ['Codex|new', 0], ['Codex|Unidentified', 0.2]]);
});
test('a recentered ring slides from where it was drawn to its new place', () => {
  const context = { MORPH_MS: 400 };
  vm.createContext(context);
  vm.runInContext(source.slice(source.indexOf('const ringShown = new Map();'), source.indexOf('\nfunction showRing(')), context);
  const moves = [];
  const ring = (top) => ({
    closest: () => ({ getBoundingClientRect: () => ({ top: 100 }) }),
    getBoundingClientRect: () => ({ top: 100 + top, height: 84 }),
    animate: (frames, options) => moves.push({ from: plain(frames)[0].transform, delay: options.delay, duration: options.duration }),
  });
  const offset = (s) => Number(/translateY\((-?[\d.]+)px\)/.exec(s)[1]);
  context.placeRing(ring(10), 'home', 0, false);
  assert.equal(moves.length, 0, 'a first ring shows where it is');
  context.placeRing(ring(10), 'home', 100, false);
  assert.equal(moves.length, 0, 'a ring that stays put does not move');
  // A longer legend moves the ring down 10px: it starts where it was.
  context.placeRing(ring(20), 'home', 1000, false);
  assert.deepEqual(moves.pop(), { from: 'translateY(-10px)', delay: 0, duration: 400 });
  // Redrawn halfway, it carries on with the same move.
  context.placeRing(ring(20), 'home', 1200, false);
  assert.deepEqual(moves.pop(), { from: 'translateY(-10px)', delay: -200, duration: 400 });
  // Sent elsewhere halfway, it leaves from where it is drawn then.
  const drawn = vm.runInContext("ringOffset(ringPlace.get('home'), 1200)", context);
  assert.ok(drawn > 10 && drawn < 20, `mid-move offset ${drawn}`);
  context.placeRing(ring(4), 'home', 1200, false);
  const turn = moves.pop();
  assert.equal(turn.delay, 0);
  assert.ok(Math.abs(offset(turn.from) - (drawn - 4)) < 1e-9, turn.from);
  // A page change or reduced motion puts it in place at once, and later moves start there.
  context.placeRing(ring(30), 'home', 1300, true);
  assert.equal(moves.length, 0);
  context.placeRing(ring(36), 'home', 5000, false);
  assert.equal(moves.pop().from, 'translateY(-6px)');
  // Each source keeps its own place.
  context.placeRing(ring(50), 'device', 5000, false);
  assert.equal(moves.length, 0);
});

test('Error rates say a little is not none and nearly all is not all', () => {
  const { fmtErrorRate } = setup();
  const rate = (errors, requests) => fmtErrorRate({ errors, requests, errorRate: requests ? errors / requests : null });
  for (const [errors, requests, shown] of [
    [0, 0, '—'], [0, 10, '0%'], [1, 2000, '<0.1%'], [1, 1000, '0.1%'], [23, 1000, '2.3%'],
    [995, 10000, '10%'], [37, 100, '37%'], [9996, 10000, '>99%'], [10, 10, '100%'], [1, 1, '100%'],
  ]) assert.equal(rate(errors, requests), shown, `${errors}/${requests}`);
});
