const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const { test } = require('node:test');
const source = fs.readFileSync(require('node:path').join(__dirname, '../ui/app.js'), 'utf8');
const { createTrafficViews } = require('../ui/traffic_views.js');
const { createDeviceForm } = require('../ui/device_form.js');
const { createDeviceList } = require('../ui/device_list.js');
const scripts = require('./panel_harness.cjs');
/// The Device list of the panel `context`, through its invoke, Channel,
/// traffic views, page, range and redraw as they are when used.
function deviceListOf(context) {
  return createDeviceList({
    invoke: (...args) => context.invoke(...args),
    Channel: function (...args) { return new context.Channel(...args); },
    trafficViews: new Proxy({}, { get: (_, key) => context.trafficViews[key], set: (_, key, value) => { context.trafficViews[key] = value; return true; } }),
    page: () => context.ui.page, minutes: () => context.ui.deviceTrafficMinutes, snap: () => context.ui.snap,
    render: () => context.render?.(), toast: message => context.toast?.(message),
  });
}
/// Leaves `views` in the harness's shared traffic cache as reads just made
/// them; one without `loadedAt` is as old as a view that must be read again.
async function seedTraffic(h, { local = {}, forwarded = {} }) {
  for (const [kind, views] of [['local', local], ['forwarded', forwarded]]) {
    for (const [key, view] of Object.entries(views)) {
      const parts = key.split(':');
      const [minutes, scope] = parts.slice(-2);
      h.seeding = async () => kind === 'local' ? view : { traffic: view, windowStart: view.start, windowEnd: view.end, bucketMinutes: view.bucketMinutes, deviceId: view.device };
      h.clock = () => view.loadedAt ?? 0;
      await (kind === 'forwarded' ? h.trafficViews.forwarded(parts[0], Number(minutes), scope) : h.trafficViews.local(Number(minutes), scope));
    }
  }
  h.seeding = null;
  h.clock = () => Date.now();
}
function harness(invoke) {
  const ui = { page: 'devices', snap: { phase: { state: 'running', port: 8787 }, settings: {} } };
  const fields = {}, listeners = {};
  const context = { ui, invoke, Promise, deviceForm: createDeviceForm(), seeding: null, clock: () => Date.now(), ICON: { plus: '<plus>', trash: '<trash>' }, mergedDataBlock: () => '', toast() {}, panelMotion: { changePage: update => update(), queueFit() {} },
    message: (_, text) => text, esc: v => String(v).replace(/</g, '&lt;'), block: (title, aside, body) => `${title}${aside}${body}`,
    $: id => fields[id], document: { addEventListener: (event, callback) => { listeners[event] = callback; } } };
  context.trafficViews = createTrafficViews({ invoke: (command, args) => (context.seeding || context.invoke)(command, args), now: () => context.clock() });
  context.render = () => {
    for (const key of Object.keys(fields)) delete fields[key];
    if (!context.deviceForm.open) return;
    const html = context.deviceSettings();
    for (const match of html.matchAll(/<input[^>]*id="([^"]+)"[^>]*value="([^"]*)"/g)) fields[match[1]] = { value: match[2], id: match[1], focus() {} };
    fields['device-transport'] = { value: context.deviceForm.draft.transport, id: 'device-transport' };
  };
  context.deviceList = deviceListOf(context);
  // Peers' statistics are read only where a test reads them, through this.
  context.loadMerged = context.deviceList.loadMerged;
  context.deviceList.loadMerged = () => {};
  vm.createContext(context);
  scripts.load(context, 'menus.js');
  context.menus = { current: () => null, holdRender: () => false, close() {} };
  // Shared traffic views, markup and merging, as every page uses them.
  scripts.load(context, 'traffic_render.js');
  vm.runInContext(source.slice(source.indexOf('async function assignTraffic('), source.indexOf('\nfunction renderActivityTraffic(')), context);
  // One range for Home, Activity and Devices.
  vm.runInContext(source.slice(source.indexOf('function setTrafficMinutes('), source.indexOf('\nasync function loadTraffic(')), context);
  scripts.load(context, 'devices_page.js');
  // The real ones, for the tests of them; the rest of the page sees stubs.
  context.real = { mergedDataBlock: context.mergedDataBlock, deviceTrafficContent: context.deviceTrafficContent };
  context.mergedDataBlock = () => '';
  context.installDevicesPage();
  vm.runInContext(source.slice(source.indexOf('async function refreshLaunchAtLogin('), source.indexOf('let refreshTimer;')), context);
  context.fields = fields; context.listeners = listeners;
  vm.runInContext(source.slice(source.indexOf('async function act('), source.indexOf('// Any scroll or input')), context);
  context.action = async (name, el = context.el || { dataset: {} }) => {
    context.el = el;
    await context.act(name, el);
  };
  return context;
}

test('adding a device with only an SSH config name sends an SSH data source, not HTTP', async () => {
  let saved;
  const h = harness(async (command, args) => { if (command === 'save_device') saved = args.device; return []; });
  await h.action('device-new');
  assert.equal(h.fields['device-transport'].value, 'ssh');
  assert.equal(h.fields['device-url'], undefined);
  h.fields['device-host'].value = 'mbp16';
  h.listeners.input({ target: h.fields['device-host'] });
  await h.action('device-save');
  assert.equal(saved.name, 'mbp16');
  assert.equal(saved.ssh.host, 'mbp16');
  assert.equal(saved.ssh.binary, '');
  assert.equal(saved.data.transport, 'ssh');
  assert.equal(saved.data.url, '');
  assert.equal(saved.data.tokenFile, null);
  assert.equal(saved.data.tokenEnv, null);
  assert.equal(saved.data.caCertificate, null);
});

test('switching HTTP to SSH uses one selector and never submits previous HTTP credentials', async () => {
  let saved;
  const h = harness(async (command, args) => { if (command === 'save_device') saved = args.device; return []; });
  await h.action('device-new');
  h.fields['device-transport'].value = 'http';
  h.listeners.change({ target: h.fields['device-transport'] });
  assert.ok(h.fields['device-url']);
  assert.equal(h.fields['device-host'], undefined);
  h.fields['device-url'].value = 'https://old.example';
  h.fields['device-key-file'].value = '/old.key';
  h.fields['device-transport'].value = 'ssh';
  h.listeners.change({ target: h.fields['device-transport'] });
  assert.equal(h.fields['device-url'], undefined);
  h.fields['device-host'].value = 'MS';
  await h.action('device-save');
  assert.equal(saved.data.transport, 'ssh');
  assert.equal(saved.data.url, '');
  assert.equal(saved.data.tokenFile, null);
  assert.equal(saved.ssh.host, 'MS');
});

test('HTTP save retains URL and credentials and creates no SSH connection', async () => {
  let saved;
  const h = harness(async (command, args) => { if (command === 'save_device') saved = args.device; return []; });
  await h.action('device-new');
  h.fields['device-transport'].value = 'http'; h.listeners.change({ target: h.fields['device-transport'] });
  h.fields['device-url'].value = 'https://data.example'; h.fields['device-key-file'].value = '/data.key';
  await h.action('device-save');
  assert.equal(saved.ssh, null); assert.equal(saved.data.transport, 'http');
  assert.equal(saved.data.url, 'https://data.example'); assert.equal(saved.data.tokenFile, '/data.key');
});

test('refresh loads definitions and statistics without invoking remote lifecycle commands', async () => {
  const calls = [];
  const h = harness(async command => { calls.push(command); return [{ id: 'remote', name: 'Remote', ssh: { host: '<remote>', management: true } }]; });
  h.deviceTrafficContent = () => '';
  await h.deviceList.load();
  // This device's card is read locally; no remote lifecycle command is involved.
  assert.deepEqual(calls, ['get_devices', 'get_device_forwarders', 'get_device_capabilities', 'get_traffic']);
  const html = h.devicesPage();
  assert.match(html, /&lt;remote>/); assert.doesNotMatch(html, /Read-only statistics/);
  assert.doesNotMatch(html, /device-control|Remote Port|PID|Uptime/);
  assert.doesNotMatch(html, /data-action="power"/);
  assert.doesNotMatch(source, /control_device|device-management|device-ssh-enabled|device-data-enabled/);
});

test('configuration lives in Settings and Settings loading makes no remote requests', async () => {
  const calls = [];
  const h = harness(async command => { calls.push(command); return []; });
  h.ui.page = 'settings'; await h.deviceList.loadDefinitions(); assert.deepEqual(calls, ['get_devices']);
  const html = h.devicesPage(); assert.doesNotMatch(html, /device-save|Configure in Settings|Updates automatically/);
});

test('Settings includes one Devices block without a separate forwarding block', () => {
  const code = source.slice(source.indexOf('function settings()'), source.indexOf('/// The fixed configuration'));
  const rendered = vm.runInNewContext(code + '\nsettings()', {
    ui: { snap: { settings: { maskAccountNames: false } } }, ICON: {}, panelSelect: () => '',
    block: (title, aside, body) => (title === 'General' ? body : `[${title}]`), configSettings: () => '[Network]', filesBlock: () => '[Files]', deviceSettings: () => '[Devices]',
  });
  assert.ok(rendered.includes('[Files]\n    [Devices]'));
  assert.doesNotMatch(rendered, /Request forwarding/);
  assert.ok(rendered.trim().endsWith('[Devices]')); assert.equal((rendered.match(/\[Devices\]/g) || []).length, 1);
  assert.match(rendered, /Mask account names/);
  assert.match(rendered, /aria-checked="false" data-action="mask-account-names"/);
});

test('each device renders full Traffic metrics and an offline peer leaves other devices visible', async () => {
  const h = harness(async () => []);
  Object.assign(h, { chart: () => '<svg class="chart"></svg>', stat: (name, value) => `${name}:${value};`, fmtMs: v => `${v} ms`, fmtBytes: v => `${v} B`, modelTokenStats: t => `Input Tokens:${t.inputTokens};Hit Rate:${t.cacheHitRate};`, trafficShare: () => '<div class="traffic-share">Account usage</div>', serviceMark: () => '', message: (kind, value) => value, ICON: { chevron: '' } });
  h.deviceTrafficContent = h.real.deviceTrafficContent;
  h.installDevicesPage();
  const traffic = { requests: 5, errors: 1, errorRate: 0.2, avgMs: 200, bytes: 1234, outputTps: 50, inputTokens: 100, cacheHitRate: 0.3, credentials: [] };
  h.deviceList.devices = [{ id: 'online', name: 'Online', ssh: { host: 'online' }, data: { transport: 'ssh' } }, { id: 'offline', name: 'Offline', ssh: { host: 'offline' }, data: { transport: 'ssh' } }];
  await seedTraffic(h, { local: { '30:model': { ...traffic, start: 0, end: 1800000, bucketMinutes: 1 } } });
  h.deviceList.merged = { minutes: 30, scope: 'model', windowStart: 0, windowEnd: 1800000, bucketMinutes: 1, sources: [{ name: 'Online', traffic }, { name: 'Offline', error: 'Unavailable' }] };
  const html = h.devicesPage();
  assert.equal((html.match(/Calls:5/g) || []).length, 2);
  // A peer's card says how recent its data is; this device's is live.
  assert.equal((html.match(/Data through/g) || []).length, 1);
  for (const metric of ['Error Rate:20%', 'Avg. Time:200 ms', 'Output TPS<span', '</span>:50.0 tok/s', 'Input Tokens:100', 'Hit Rate:0.3', 'traffic-share', 'class="chart"']) assert.ok(html.includes(metric), metric);
  assert.doesNotMatch(html, /Received/);
  assert.match(html, /Offline/); assert.match(html, /Traffic unavailable/);
});

test('traffic range changes reject late responses from the previous selection', async () => {
  const pending = [];
  const context = { Channel: class {}, trafficViews: { scope: 'model' }, ui: { page: 'devices', deviceTrafficMinutes: 30 }, invoke: (command, args) => new Promise(resolve => pending.push({ command, args, resolve })), render() {} };
  context.deviceList = deviceListOf(context);
  const first = context.deviceList.loadMerged();
  context.ui.deviceTrafficMinutes = 1440; context.trafficViews.scope = 'all';
  const next = context.deviceList.loadMerged(true);
  assert.equal(pending[0].command, 'get_merged_data');
  assert.equal(pending[1].command, 'get_merged_data');
  pending[1].resolve({ selected: 'new' }); await next;
  pending[0].resolve({ selected: 'old' }); await first;
  assert.equal(context.deviceList.merged.selected, 'new');
  assert.equal(context.deviceList.loading, false);
});

test('Devices button is visible only with configured peers and the redundant Devices block is removed', () => {
  const top = source.slice(source.indexOf('function renderTop()'), source.indexOf('function renderPage(', source.indexOf('function renderTop()')));
  let html;
  const context = { ui: { page: 'main', snap: { version: 'test', settings: { deviceCount: 0 } } }, deviceList: { loaded: false }, esc: v => v, $: () => ({ set innerHTML(value) { html = value; } }) };
  vm.createContext(context); vm.runInContext(top, context);
  context.renderTop(); assert.doesNotMatch(html, /data-page="devices"/);
  context.ui.snap.settings.deviceCount = 1; context.renderTop(); assert.match(html, /data-page="devices"/);
  context.deviceList.loaded = true; context.deviceList.devices = []; context.renderTop(); assert.doesNotMatch(html, /data-page="devices"/);
  const page = fs.readFileSync(require('node:path').join(__dirname, '../ui/devices_page.js'), 'utf8');
  const devices = page.slice(page.indexOf('function devicesPage()'), page.indexOf('function deviceSettings()'));
  assert.doesNotMatch(devices, /block\("Devices"/);
});

test('automatic alignment retains previous traffic while refreshing without an error message', async () => {
  const context = { Channel: class {}, trafficViews: { scope: 'model' }, ui: { page: 'devices', deviceTrafficMinutes: 30 }, invoke: async () => { throw 'TRAFFIC_UPDATING'; }, render() {} };
  context.deviceList = deviceListOf(context);
  context.deviceList.merged = { selected: 'last-good' };
  await context.deviceList.loadMerged();
  assert.equal(context.deviceList.merged.selected, 'last-good');
  assert.equal(context.deviceList.error, '');
  assert.equal(context.deviceList.updating, true);
});

test('upstream account disclosures retain per-device expansion and place rings before charts', () => {
  const h = harness(async () => []);
  const events = {};
  Object.assign(h, { trafficReview: () => '', chart: () => '<chart>', trafficShare: () => '<ring>', modelTokenStats: () => '', stat: () => '', fmtMs: () => '', fmtBytes: () => '', serviceMark: () => '', ICON: { chevron: '' }, document: { addEventListener: (name, callback) => { events[name] = callback; } } });
  h.deviceTrafficContent = h.real.deviceTrafficContent;
  h.installDevicesPage();
  const traffic = { requests: 2, errors: 0, credentials: [
    { credential: 'suzhi2333@gmail.com', service: 'Claude', requests: 1, errors: 0 },
    { credential: 'work', service: 'Claude', requests: 1, errors: 0 },
  ] };
  events.toggle({ target: { isConnected: true, matches: () => true, dataset: { deviceTraffic: 'device/a' }, open: true } });
  let html = h.deviceTrafficContent(traffic, 'model', 'device/a');
  assert.match(html, /data-device-traffic="device\/a" open/);
  assert.ok(html.indexOf('<ring>') < html.indexOf('<chart>'));
  // Account emails are shortened; configuration names stay readable.
  assert.match(html, /<strong>su\*\*\*3@gmail\.com<\/strong>/);
  assert.match(html, /<strong>work<\/strong>/);
  assert.doesNotMatch(html, /suzhi2333@gmail\.com/);
  assert.doesNotMatch(h.deviceTrafficContent(traffic, 'model', 'device/b'), /data-device-traffic="device\/b" open/);
  events.toggle({ target: { isConnected: false, matches: () => true, dataset: { deviceTraffic: 'device/a' }, open: false } });
  assert.match(h.deviceTrafficContent(traffic, 'model', 'device/a'), /data-device-traffic="device\/a" open/);
  events.toggle({ target: { isConnected: true, matches: () => true, dataset: { deviceTraffic: 'device/a' }, open: false } });
  assert.doesNotMatch(h.deviceTrafficContent(traffic, 'model', 'device/a'), /data-device-traffic="device\/a" open/);
});

test('Devices title contains only a settings icon and no redundant update/configure text', () => {
  const top = source.slice(source.indexOf('function renderTop()'), source.indexOf('function renderPage()', source.indexOf('function renderTop()')));
  let html;
  const context = { ui: { page: 'devices' }, ICON: { settings: '<settings-icon>', back: '' }, BACK_KEYS: 'Alt+Left', $: () => ({ set innerHTML(value) { html = value; } }) };
  vm.createContext(context); vm.runInContext(top, context); context.renderTop();
  assert.match(html, /data-page="settings"/); assert.match(html, /<settings-icon>/);
  assert.doesNotMatch(html, /Updates automatically|Configure in Settings/);
});

test('Models and All switch immediately from one snapshot without new IPC requests', async () => {
  let calls = 0;
  const views = [{ minutes: 30, scope: 'model', traffic: { requests: 3 } }, { minutes: 30, scope: 'all', traffic: { requests: 5 } }, { minutes: 360, scope: 'all', traffic: { requests: 8 } }];
  const context = { Channel: class {}, trafficViews: { scope: 'model' }, ui: { page: 'devices', deviceTrafficMinutes: 30 }, invoke: async () => { calls++; return views; }, render() {} };
  context.deviceList = deviceListOf(context);
  await context.deviceList.loadMerged();
  assert.equal(context.deviceList.merged.traffic.requests, 3);
  for (let i=0;i<10;i++) {
    context.trafficViews.scope = i%2 ? 'model' : 'all';
    assert.equal(context.deviceList.select(),true);
    assert.equal(context.deviceList.merged.traffic.requests,i%2 ? 3 : 5);
  }
  context.ui.deviceTrafficMinutes = 360; context.trafficViews.scope = 'all';
  assert.equal(context.deviceList.select(),true);
  assert.equal(context.deviceList.merged.traffic.requests,8);
  assert.equal(calls,1);
});

test('Settings devices retain verified results and label older results as cached', () => {
  const h = harness(async () => []);
  h.deviceList.devices = [{ id: 'ok', name: 'Working', ssh: { host: 'ok' }, data: { transport: 'ssh' } }, { id: 'bad', name: 'Failed', ssh: { host: 'bad' }, data: { transport: 'ssh' } }];
  h.deviceList.fetchedAt = Date.now();
  h.deviceList.views = { '30:model': { sources: [{ name: 'Working', included: true }, { name: 'Failed', included: false, error: 'Data rejected' }] } };
  let html = h.deviceSettings();
  assert.match(html, /data-action="device-new"[^>]*><plus>/);
  assert.match(html, /data-action="device-remove"[^>]*><trash>/);
  assert.match(html, /dot running/); assert.match(html, /dot failed/);
  assert.doesNotMatch(html, />Remove<|>Add Device</);
  h.deviceList.fetchedAt = Date.now() - 46000;
  html = h.deviceSettings(); assert.match(html, /dot running/); assert.match(html, /succeeded \(cached\)/); assert.match(html, /dot failed/);
});

test('device cards show transport beside the same verified status light as Settings', () => {
  const h = harness(async () => []);
  h.deviceTrafficContent = () => '';
  h.message = (_, text) => text;
  h.deviceList.devices = [
    { id: 'ssh', name: 'SSH peer', ssh: { host: 'peer' }, data: { transport: 'ssh' } },
    { id: 'http', name: 'HTTP peer', data: { transport: 'http', url: 'http://10.0.0.2' } },
    { id: 'https', name: 'HTTPS peer', data: { transport: 'http', url: 'https://peer.example' } },
  ];
  h.deviceList.fetchedAt = Date.now();
  h.deviceList.merged = { sources: h.deviceList.devices.map(d => ({ name: d.name, included: true })) };
  let html = h.devicesPage();
  for (const protocol of ['SSH', 'HTTP', 'HTTPS']) {
    assert.match(html, new RegExp(`class="state"><span class="dot running"[^>]*></span>${protocol}</span>`));
  }
  assert.doesNotMatch(html, /SSH request|HTTP \/ HTTPS/);
  h.deviceList.merged.sources[0] = { name: 'SSH peer', error: 'Unavailable' };
  html = h.devicesPage();
  assert.match(html, /dot failed"[^>]*><\/span>SSH/);
  h.deviceList.fetchedAt = Date.now() - 46000;
  html = h.devicesPage();
  assert.match(html, /dot running"[^>]*><\/span>(HTTP|HTTPS)/);
  assert.match(html, /succeeded \(cached\)/);
  assert.match(html, /dot failed"[^>]*><\/span>SSH/);
});

test('renaming only a device alias preserves all cached traffic and does not refetch it', async () => {
  const old = { id: 'stable', name: 'Old name', ssh: { host: 'MS', binary: '' }, data: { transport: 'ssh' } };
  const next = { ...old, name: 'New name' };
  const calls = [];
  const h = harness(async command => { calls.push(command); return [next]; });
  h.deviceList.devices = [old]; h.deviceList.loaded = true;
  const view = { traffic: { requests: 99 }, sources: [{ name: 'Old name', included: true }] };
  h.deviceList.views = { '30:model': view }; h.deviceList.merged = view;
  const changed = await h.deviceList.loadDefinitions();
  assert.equal(changed,false);
  assert.equal(h.deviceList.merged,view);
  assert.equal(h.deviceList.views['30:model'].traffic.requests,99);
  assert.equal(view.sources[0].name,'New name');
  assert.deepEqual(calls,['get_devices']);
});

test('statistics query events display local aliases, transport and failure/recovery clearly', () => {
  const ui = { expanded: new Set() };
  const deviceList = { devices: [{ id: 'device-id', name: 'MS' }] };
  const context = { ui, deviceList, esc: value => String(value).replace(/</g, '&lt;'), fmtTime: () => '12:00', fmtBytes: String, fmtMs: value => `${value} ms`, statusClass: () => '' };
  vm.createContext(context);
  vm.runInContext(source.slice(source.indexOf('function requestRow('), source.indexOf('function detail(')), context);
  const entry = { event: 'device_statistics_succeeded', time: 1, durationMs: 42, fields: { device_id: 'device-id', transport: 'ssh', query_count: 3 } };
  let html = context.requestRow(entry, true);
  assert.match(html, /Statistics query · MS/); assert.match(html, /SSH/);
  assert.match(html, /3 queries since previous event/); assert.match(html, /42 ms/);
  assert.match(html, /class="status s2"/);
  entry.event = 'device_statistics_failed'; entry.error = true;
  html = context.requestRow(entry, true); assert.match(html, />ERR</); assert.match(html, /Statistics query failed/);
  entry.event = 'device_statistics_recovered'; entry.error = false;
  html = context.requestRow(entry, true); assert.match(html, />OK</); assert.match(html, /Statistics connection recovered/);
  deviceList.devices[0].name = '<Renamed>';
  assert.match(context.requestRow(entry, true), /Statistics query · &lt;Renamed>/);
  deviceList.devices = [];
  assert.match(context.requestRow(entry, true), /Statistics query · Device device-i/);
});


test('This Device renders the shared local view before remote statistics arrive', async () => {
  const h = harness(async () => { throw new Error('Cached rendering must not query'); });
  h.ui.deviceTrafficMinutes = 30; h.trafficViews.scope = 'model';
  const view = { requests: 17, credentials: [{ credential: 'Known', requests: 17 }] };
  await seedTraffic(h, { local: { '30:model': view } });
  const shown = [];
  h.deviceTrafficContent = (traffic, scope, key) => { shown.push(traffic); return `${key}:${scope}:${traffic.requests}:${traffic.credentials.length}`; };
  let html = h.devicesPage();
  assert.match(html, /local:model:17:1/);
  // Home, Activity and This Device show the very same object.
  assert.equal(shown[0], h.trafficViews.localView(30, 'model'));
  assert.doesNotMatch(html, /Loading traffic/);
  h.trafficViews.scope = 'all';
  assert.doesNotMatch(h.devicesPage(), /local:model:17:1/);
  // Merged statistics never replace this device's live view.
  h.trafficViews.scope = 'model';
  h.deviceList.merged = { minutes: 30, scope: 'model', sources: [] };
  assert.match(h.devicesPage(), /local:model:17:1/);
});

test('switching range during a remote refresh waits for it instead of reading the cache', async () => {
  const pending = [];
  const local = [];
  const h = harness((command, args) => {
    if (command === 'get_merged_data') return new Promise(resolve => pending.push({ args, resolve }));
    if (command === 'get_traffic') { local.push(args); return Promise.resolve({ requests: 7, credentials: [], targets: [] }); }
    return Promise.resolve([]);
  });
  h.Channel = class {};
  h.deviceList.loadMerged = h.loadMerged;
  h.deviceTrafficContent = (traffic, scope, key) => `${key}:${scope}:${traffic.requests}`;
  h.deviceList.devices = [{ id: 'ms', name: 'MS', ssh: { host: 'MS' } }];
  h.ui.deviceTrafficMinutes = 30; h.trafficViews.scope = 'model'; h.deviceList.request = 0;
  const view = minutes => ({ minutes, scope: 'model', local: { requests: minutes }, traffic: { requests: minutes }, sources: [{ name: 'MS', included: true, traffic: { requests: minutes } }] });
  const refresh = h.deviceList.loadMerged(true, true);
  pending[0].args.onUpdate.onmessage([view(30)]);
  const start = source.indexOf('  if (event.target.id === "devices-traffic-scope" || event.target.id === "devices-traffic-range") {');
  const handler = vm.runInContext(`(async event => { ${source.slice(start, source.indexOf('  if (event.target.id === "home-traffic-scope"', start))} })`, h);
  const switching = handler({ target: { id: 'devices-traffic-range', value: '10080' } });
  await Promise.resolve();
  assert.equal(pending.length, 1, 'A cache read must not supersede the remote refresh');
  await switching;
  assert.equal(h.deviceList.merged, null);
  // This device is read locally at once; only the peer waits for the refresh.
  assert.equal(JSON.stringify(local), JSON.stringify([{ minutes: 10080, scope: 'model' }]));
  assert.match(h.devicesPage(), /local:model:7/);
  assert.match(h.devicesPage(), /MS[\s\S]*Loading traffic…/);
  assert.doesNotMatch(h.devicesPage(), /Not refreshed/);
  pending[0].resolve([view(30), view(10080)]);
  await refresh;
  assert.equal(h.deviceList.merged.minutes, 10080);
  assert.match(h.devicesPage(), /device\/ms:model:10080/);
  // Without a remote refresh in flight, a missing range is read from the cache.
  const cacheRead = handler({ target: { id: 'devices-traffic-range', value: '360' } });
  assert.equal(pending.length, 2);
  assert.equal(pending[1].args.refreshRemote, false);
  pending[1].resolve([view(360)]);
  await cacheRead;
  assert.equal(h.deviceList.merged.minutes, 360);
});

function devicesHarness(invoke) {
  const h = harness(invoke);
  h.Channel = class {};
  h.deviceList.loadMerged = h.loadMerged;
  h.mergedDataBlock = h.real.mergedDataBlock;
  h.trafficControls = () => '';
  h.deviceTrafficContent = (traffic, scope, key) => `${key}:${scope}:${traffic.requests}`;
  h.deviceList.devices = [{ id: 'ms', name: 'MS', ssh: { host: 'MS' } }];
  h.deviceList.loaded = true;
  h.ui.deviceTrafficMinutes = 30; h.trafficViews.scope = 'model'; h.deviceList.request = 0;
  return h;
}

test('cache reads keep peer states steady and Refresh stays available', async () => {
  const pending = [];
  const h = devicesHarness((command, args) => command === 'get_merged_data' ? new Promise((resolve, reject) => pending.push({ args, resolve, reject })) : Promise.resolve([]));
  const cached = { minutes: 30, scope: 'model', local: { requests: 1 }, traffic: { requests: 1 }, sources: [{ name: 'MS', included: false, exclusion: 'not_refreshed' }] };
  h.deviceList.merged = cached;
  const read = h.deviceList.loadMerged(true, false);
  // A timer's cache read must not flash loading states or block Refresh.
  assert.match(h.devicesPage(), /MS[\s\S]*Not refreshed/);
  assert.doesNotMatch(h.mergedDataBlock(), /merged-refresh" disabled/);
  assert.equal(h.deviceList.status(h.deviceList.devices[0]).label, 'Not checked');
  const refreshing = h.action('merged-refresh');
  await Promise.resolve();
  assert.equal(pending.length, 2, 'Refresh supersedes the cache read');
  assert.equal(pending[1].args.refreshRemote, true);
  assert.match(h.devicesPage(), /MS[\s\S]*Loading traffic…/);
  assert.match(h.mergedDataBlock(), /merged-refresh" disabled/);
  assert.equal(h.deviceList.status(h.deviceList.devices[0]).label, 'Refreshing statistics…');
  // A refresh that cannot align asks for another Refresh instead of "Updating traffic…".
  pending[1].reject('TRAFFIC_UPDATING');
  await refreshing;
  pending[0].resolve([cached]);
  await read;
  assert.match(h.deviceList.error, /Press Refresh/);
  assert.equal(h.deviceList.updating, false);
  assert.ok(!h.deviceList.fetchedAt, 'Only a completed refresh counts as fresh');
});

test('waiting for a range keeps peer errors, statuses and queued work intact', async () => {
  const pending = [];
  const calls = [];
  const h = devicesHarness((command, args) => {
    calls.push([command, args]);
    if (command === 'get_merged_data') return new Promise(resolve => pending.push({ args, resolve }));
    if (command === 'get_devices') return Promise.resolve([{ id: 'ms', name: 'MS', ssh: { host: 'MS' } }]);
    if (command === 'get_traffic') return Promise.resolve({ requests: 3, credentials: [], targets: [] });
    return Promise.resolve([]);
  });
  const refresh = h.deviceList.loadMerged(true, true);
  pending[0].args.onUpdate.onmessage([{ minutes: 30, scope: 'model', local: { requests: 1 }, traffic: { requests: 1 }, sources: [{ name: 'MS', included: false, error: 'Permission denied' }] }]);
  const start = source.indexOf('  if (event.target.id === "devices-traffic-scope" || event.target.id === "devices-traffic-range") {');
  const handler = vm.runInContext(`(async event => { ${source.slice(start, source.indexOf('  if (event.target.id === "home-traffic-scope"', start))} })`, h);
  await handler({ target: { id: 'devices-traffic-range', value: '10080' } });
  assert.equal(h.deviceList.merged, null);
  assert.match(h.devicesPage(), /MS: Permission denied|Permission denied/);
  assert.match(h.devicesPage(), /Traffic unavailable/);
  assert.equal(h.deviceList.status(h.deviceList.devices[0]).state, 'failed');
  assert.match(h.mergedDataBlock(), /Loading traffic…/);
  // Saving an unchanged device set in Settings must not supersede the refresh.
  await h.action('device-new');
  h.fields['device-host'].value = 'MS';
  await h.action('device-save');
  assert.equal(calls.filter(([command]) => command === 'get_merged_data').length, 1);
  pending[0].resolve([]);
  await refresh;
});

test('a known capability failure outranks an older successful SSH check', () => {
  const h = devicesHarness(async () => []);
  h.deviceList.startup = { ms: { host: 'MS', state: 'running', label: 'SSH reachable at startup' } };
  h.deviceList.capabilities = { ms: { error: 'Permission denied', capabilities: null } };
  assert.equal(h.deviceList.status(h.deviceList.devices[0]).state, 'failed');
  h.deviceList.capabilities = { ms: { capabilities: { version: '1' } } };
  h.deviceList.startup.ms = { host: 'MS', state: 'failed', label: 'Startup SSH check failed' };
  assert.equal(h.deviceList.status(h.deviceList.devices[0]).state, 'running');
});

test('a Devices visit during another device load is queued, not dropped', async () => {
  let resolveForwarders;
  const calls = [];
  const h = devicesHarness((command, args) => {
    calls.push([command, args]);
    if (command === 'get_device_forwarders' && !resolveForwarders) return new Promise(resolve => { resolveForwarders = resolve; });
    if (command === 'get_devices') return Promise.resolve([{ id: 'ms', name: 'MS', ssh: { host: 'MS' } }]);
    return Promise.resolve([]);
  });
  // Settings is still reading forwarding status when the user opens Devices.
  h.ui.page = 'settings';
  const settings = h.deviceList.load();
  for (let i = 0; i < 5; i++) await Promise.resolve();
  h.ui.page = 'devices';
  await h.deviceList.load();
  assert.equal(calls.filter(([command]) => command === 'get_merged_data').length, 0);
  resolveForwarders([]);
  await settings;
  for (let i = 0; i < 10; i++) await Promise.resolve();
  const reads = calls.filter(([command]) => command === 'get_merged_data');
  assert.equal(reads.length, 1, 'The queued visit starts its due refresh');
  assert.equal(reads[0][1].refreshRemote, true);
});

test('This Device distinguishes failures and pending operations from stopped', () => {
  const h = harness(async () => []);
  h.message = (_, text) => `<warning>${text}</warning>`;
  h.ui.snap.phase = { state: 'failed', port: 8787, error: 'Cannot stop proxy daemon: unavailable' };
  let html = h.devicesPage();
  assert.match(html, /Status unavailable/);
  assert.match(html, /Cannot stop proxy daemon/);
  assert.doesNotMatch(html, />Stopped</);
  h.ui.snap.phase = { state: 'running', port: 8787, busy: true };
  html = h.devicesPage();
  assert.match(html, /Updating…/);
  h.ui.snap.phase = { state: 'stopped', port: 8787 };
  assert.match(h.devicesPage(), />Stopped</);
});

test('duplicate devices show an exclusion notice instead of a connection failure', () => {
  const h = harness(async () => []);
  h.deviceList.devices = [{ id: 'duplicate', name: 'Duplicate', ssh: { host: 'peer' } }];
  h.deviceList.fetchedAt = Date.now();
  h.deviceList.merged = { sources: [{ name: 'Duplicate', included: false, error: null, exclusion: 'duplicate' }] };
  h.message = (_, text) => text;
  let html = h.devicesPage();
  assert.match(html, /Already counted via another entry/);
  assert.match(html, /Statistics available/);
  assert.doesNotMatch(html.slice(html.indexOf('Duplicate<div')), /dot failed|Traffic unavailable|Loading traffic/);
  html = h.deviceSettings();
  assert.match(html, /Statistics available; already counted/);
  assert.doesNotMatch(html, /dot failed/);
});

test('forwarding appears below its available SSH device only in Settings', () => {
  const h = harness(async () => []);
  const device = { id: 'ssh', name: 'Remote', ssh: { host: 'host' } };
  h.deviceList.devices = [device, { id: 'http', name: 'HTTP peer', data: { transport: 'http', url: 'http://host' } }];
  h.deviceList.capabilities = { ssh: { capabilities: { forwarding: true, forwardingAvailable: true } } };
  let html = h.deviceSettings();
  assert.match(html, /^Devices/);
  assert.doesNotMatch(html, /Request forwarding|Start forwarding|Stop forwarding|Updating…/);
  assert.match(html, /class="switch" role="switch" aria-checked="false"[^>]*data-action="device-forward" data-id="ssh"[^>]*><\/button>/);
  const groups = html.split(/<div class="device-settings-item"[^>]*>/).slice(1);
  assert.match(groups[0], /Remote[\s\S]*SSH forwarding/);
  assert.doesNotMatch(groups[1], /SSH forwarding|data-action="device-forward"/);
  assert.doesNotMatch(html, /data-action="forwarding-restore"/);
  assert.doesNotMatch(h.devicesPage(), /data-action="device-forward"|data-action="forwarding-restore"/);
  h.deviceList.forwarders = [{ deviceId: 'ssh', port: 23456, error: '<connection lost>' }];
  h.deviceList.capabilities.ssh = { error: 'offline' };
  html = h.deviceSettings();
  assert.match(html, /aria-checked="true"[^>]*data-action="device-forward"[^>]*><\/button>/);
  assert.match(html, /SSH forwarding[\s\S]*data-action="device-forward"[\s\S]*Restore on startup[\s\S]*data-action="forwarding-restore"/);
  assert.equal((html.match(/data-action="forwarding-restore"/g) || []).length, 1);
  assert.match(html, /&lt;connection lost>/);
  assert.doesNotMatch(html, /23456|Local endpoint/);
  assert.doesNotMatch(h.devicesPage(), /data-action="device-forward"|23456|connection lost/);
  h.ui.deviceForwardingBusy = 'ssh';
  assert.match(h.deviceSettings(), /aria-checked="true" aria-busy="true"[^>]*disabled><\/button>/);
  h.deviceList.devices = [];
  assert.doesNotMatch(h.deviceSettings(), /SSH forwarding|forwarding-restore|Add an SSH device above/);
});

test('unknown, unsupported, pending and failed SSH devices have no forwarding switch', () => {
  const calls = [];
  const h = harness(async command => { calls.push(command); return []; });
  h.deviceList.devices = [{ id: 'ssh', name: 'Remote', ssh: { host: 'host' } }];
  const capable = { forwarding: true, forwardingAvailable: true };
  for (const entry of [undefined, { pending: true, capabilities: capable }, { error: 'denied', capabilities: capable },
    { capabilities: { forwarding: false, forwardingAvailable: true } },
    { capabilities: { forwarding: true, forwardingAvailable: false } }]) {
    h.deviceList.capabilities = { ssh: entry };
    assert.doesNotMatch(h.deviceSettings(), /data-action="device-forward"|data-action="forwarding-restore"/);
  }
  h.deviceList.capabilities = { ssh: { capabilities: capable, checkedAt: 1 } };
  h.deviceList.startup = { ssh: { host: 'host', state: 'failed', label: 'SSH failed', checkedAt: 2 } };
  assert.doesNotMatch(h.deviceSettings(), /data-action="device-forward"/);
  assert.deepEqual(calls, []);
});

test('forwarding toggles only the selected device and refreshes confirmed state', async () => {
  const calls = [];
  const h = harness(async (command, args) => { calls.push([command, args]); return command === 'get_device_forwarders' ? [{ deviceId: 'remote', port: 23456 }] : null; });
  h.el = { dataset: { id: 'remote', enabled: 'true' } };
  await h.action('device-forward');
  assert.equal(calls[0][0], 'set_device_forwarding');
  assert.equal(calls[0][1].id, 'remote');
  assert.equal(calls[0][1].enabled, true);
  assert.equal(calls[1][0], 'get_device_forwarders');
  assert.equal(h.deviceList.forwarders[0].port, 23456);
  assert.equal(h.ui.deviceForwardingBusy, null);
});


test('home highlights remote mode and collapses inactive local configuration', () => {
  const start = source.indexOf('function main()');
  const end = source.indexOf('function proxyBlock(', start);
  const context = { ui: { snap: { forwarding: { name: 'MS', error: null }, phase: { port: 8787 } } },
    ICON: { chevron: '' }, fmtBytes: v => `${v} B`, esc: v => v, block: (title, aside, body) => title + body,
    message: (_, text) => text, connectBlock: () => '[Connect]', proxyBlock: () => '[Proxy]',
    trafficBlock: () => '[Traffic]', recentBlock: () => '[Recent]', proxiesBlock: () => '[Proxies]', routingBlock: () => '[Routing]' };
  vm.createContext(context); vm.runInContext(source.slice(start,end), context);
  const html = context.main();
  assert.match(html, /SSH forwarding/);
  assert.match(html, /MS/);
  assert.match(html, /127\.0\.0\.1:8787/);
  assert.match(html, /Client address unchanged/);
  // Traffic shows the forwarded device, so it stays outside the inactive local section.
  assert.match(html, /Client address unchanged[\s\S]*\[Connect\]\[Traffic\]<details[^>]*><summary>Local proxy configuration and history \(inactive\)<\/summary>\[Recent\]\[Proxies\]\[Routing\]<\/details>/);
  assert.doesNotMatch(html, /Remote device · all requests/);
  assert.doesNotMatch(html, /<details[^>]*open/);
  context.ui.snap.forwarding = null;
  assert.equal(context.main(), '[Proxy][Connect][Traffic][Recent][Proxies][Routing]');
});

test('Activity says forwarded requests are logged on the destination', () => {
  const start = source.indexOf('function renderActivityForwarding(');
  const end = source.indexOf('function activityShell(', start);
  const el = { hidden: true, innerHTML: '', dataset: {} };
  const context = { ui: { snap: { forwarding: { name: 'MS', deviceId: 'ms' } } }, esc: v => v, $: id => id === 'activity-forwarding' ? el : null };
  vm.createContext(context); vm.runInContext(source.slice(start, end), context);
  context.renderActivityForwarding();
  assert.equal(el.hidden, false);
  // A page change turns this notice into the destination's Devices card.
  assert.equal(el.dataset.morph, 'device/ms');
  assert.match(el.innerHTML, /Forwarding to MS/);
  assert.match(el.innerHTML, /logged on MS/);
  assert.match(el.innerHTML, /data-action="page" data-page="devices"/);
  // Repeated refreshes keep the same nodes.
  const html = el.innerHTML;
  Object.defineProperty(el, 'innerHTML', { get: () => html, set: () => assert.fail('re-rendered unchanged notice'), configurable: true });
  context.renderActivityForwarding();
  Object.defineProperty(el, 'innerHTML', { value: html, writable: true, configurable: true });
  context.ui.snap.forwarding = null;
  context.renderActivityForwarding();
  assert.equal(el.hidden, true);
  assert.equal(el.innerHTML, '');
  assert.equal(el.dataset.morph, '');
});

test('entering Settings through navigation reads definitions without remote statistics', async () => {
  const calls = [];
  const h = harness(async command => { calls.push(command); return []; });
  h.fields.content = { scrollTop: 100 };
  h.render = () => {};
  h.deviceList.loadMerged = () => calls.push('remote-statistics');
  await h.action('page', { dataset: { page: 'settings' } });
  await Promise.resolve();
  assert.equal(h.ui.page, 'settings');
  // The login item status is a local system read.
  assert.deepEqual(calls, ['get_devices', 'refresh_launch_at_login', 'get_device_forwarders', 'get_device_capabilities']);
});

test('leaving Devices while definitions load prevents starting remote statistics', async () => {
  let resolve;
  const calls = [];
  const h = harness(command => command === 'get_devices' ? new Promise(r => { resolve = r; }) : Promise.resolve([]));
  h.deviceList.loadMerged = () => calls.push('remote-statistics');
  const loading = h.deviceList.load();
  h.ui.page = 'settings';
  resolve([]);
  await loading;
  assert.deepEqual(calls, []);
});

test('Devices statistics start without waiting for forwarding or capability reads', async () => {
  const pending = [];
  const calls = [];
  const h = harness(command => {
    calls.push(command);
    if (command === 'get_devices') return Promise.resolve([]);
    return new Promise(resolve => pending.push(resolve));
  });
  h.deviceList.loadMerged = (force, refreshRemote) => { calls.push(['statistics', refreshRemote]); };
  const loading = h.deviceList.load();
  for (let i = 0; i < 5; i++) await Promise.resolve();
  // A visit with no recent refresh reads remote statistics without waiting for local reads.
  assert.deepEqual(calls, ['get_devices', 'get_device_forwarders', 'get_device_capabilities', 'get_traffic', ['statistics', true]]);
  for (const resolve of pending) resolve([]);
  await loading;
  assert.deepEqual(h.deviceList.forwarders, []);
});

test('Settings timer never polls remote devices', () => {
  const calls = [];
  let tick;
  vm.runInNewContext(source.slice(source.indexOf('// Refresh even when no new requests arrive')), {
    ui: { page: 'settings', devices: [{ id: 'remote' }] }, document: { hidden: false },
    setInterval: callback => { tick = callback; },
    refresh: () => calls.push('refresh'), loadDevices: () => calls.push('devices'),
    loadMergedData: () => calls.push('remote-statistics'), refreshForwardingStatus: () => calls.push('local-forwarding-status'),
  });
  for (let i = 0; i < 5; i++) tick();
  assert.deepEqual(calls, Array(5).fill('local-forwarding-status'));
});

test('explicit proxy Test and Test All still invoke the requested probe', async () => {
  const calls = [];
  const h = harness(async (command, args) => { calls.push([command, args.name]); return []; });
  h.refresh = async () => {};
  await h.action('probe', { dataset: { name: 'selected' } });
  await h.action('probe', { dataset: {} });
  assert.deepEqual(calls, [['probe_proxy', 'selected'], ['probe_proxy', null]]);
});


test('startup checks only SSH devices once and retains failed checks without retries', async () => {
  const devices = [{ id: 'up', ssh: { host: 'one' } }, { id: 'down', ssh: { host: 'two' } }, { id: 'http', data: { url: 'https://example.invalid' } }];
  const calls = [];
  const h = harness(async (command, args) => {
    calls.push([command, args?.id]);
    if (command === 'get_devices') return devices;
    assert.equal(command, 'check_ssh_device');
    if (args.id === 'down') throw new Error('unavailable');
  });
  await h.deviceList.checkSsh();
  await h.deviceList.checkSsh();
  assert.deepEqual(calls, [['get_devices', undefined], ['check_ssh_device', 'up'], ['check_ssh_device', 'down']]);
  assert.equal(h.deviceList.status(devices[0]).label, 'SSH reachable at startup');
  assert.equal(h.deviceList.status(devices[1]).state, 'failed');
  assert.match(h.deviceList.status(devices[1]).label, /unavailable/);
  assert.equal(h.deviceList.startup.http, undefined);
  devices[0].ssh.host = 'changed';
  assert.equal(h.deviceList.status(devices[0]).label, 'Not checked');
});

test('startup SSH checks have at most four connections in flight', async () => {
  const devices = Array.from({ length: 7 }, (_, i) => ({ id: String(i), ssh: { host: `host-${i}` } }));
  const pending = [];
  let active = 0, maximum = 0, checked = 0;
  const h = harness(async command => {
    if (command === 'get_devices') return devices;
    active++; maximum = Math.max(maximum, active); checked++;
    await new Promise(resolve => pending.push(() => { active--; resolve(); }));
  });
  const started = h.deviceList.checkSsh();
  for (let i = 0; i < 20 && checked < devices.length; i++) {
    await Promise.resolve(); await Promise.resolve();
    const batch = pending.splice(0); batch.forEach(resolve => resolve());
  }
  pending.splice(0).forEach(resolve => resolve());
  await started;
  assert.equal(checked, 7);
  assert.equal(maximum, 4);
});

test('remote home distinguishes health and recovery from whole-device statistics', () => {
  const start = source.indexOf('function main()');
  const end = source.indexOf('function proxyBlock(', start);
  const context = { ui: { snap: { phase: { port: 8787 }, forwarding: { name: 'MS', health: { state: 'connected', latencyMs: 52, lastConnectedAt: Date.now(), recoveredAt: Date.now() }, traffic: { connections: 3, active: 1, failures: 1, uploadBytes: 20, downloadBytes: 40, downloadBytesPerSecond: 8 }, remoteTraffic: { requests: 10, errors: 2, fetchedAt: Date.now() } } } }, esc: v => v, fmtBytes: v => `${v} B`, block: (title, _, body) => title + body, message: (_, text) => text };
  vm.createContext(context); vm.runInContext(source.slice(start,end),context);
  let html = context.remoteForwardingBlock();
  assert.match(html,/Connected/); assert.match(html,/52 ms/); assert.match(html,/Connection restored/);
  assert.match(html,/3 \/ 1 \/ 1/); assert.match(html,/8 B\/s/);
  // Whole-device statistics belong to Home Traffic, not to this session's block.
  assert.doesNotMatch(html,/10 \/ 20.0%/);
  context.ui.snap.forwarding.health.state='disconnected'; context.ui.snap.forwarding.error='offline';
  html=context.remoteForwardingBlock();
  assert.match(html,/Disconnected · retrying/); assert.match(html,/never fall back/);
  assert.doesNotMatch(html,/Connection restored/);
});

test('restore switch changes only after daemon confirms persistence', async () => {
  const calls=[];
  const h=harness(async (command,args)=>{calls.push([command,args]);});
  h.ui.snap.forwardingRestoreSupported=true;
  h.deviceList.devices=[{id:'ssh',name:'Remote',ssh:{host:'host'}}];
  h.deviceList.capabilities={ssh:{capabilities:{forwarding:true,forwardingAvailable:true}}};
  h.deviceList.forwarders=[{deviceId:'ssh',name:'Remote'}];
  assert.match(h.deviceSettings(),/data-action="forwarding-restore"/);
  await h.action('forwarding-restore');
  assert.equal(calls[0][0],'set_forwarding_restore'); assert.equal(calls[0][1].enabled,true);
  assert.equal(h.ui.snap.forwardingRestore,true);
  h.invoke=async()=>{throw new Error('cannot save')};
  await h.action('forwarding-restore');
  assert.equal(h.ui.snap.forwardingRestore,true); assert.equal(h.ui.forwardingRestoreBusy,false);
});

test('cached daemon capabilities hide unsupported switches and retain active forwarding controls', async () => {
  const results = [{ deviceId:'ms', pending:false, capabilities:{version:'0.1.0',runningVersion:'0.1.0',statistics:true,forwarding:false,running:true,forwardingAvailable:false}, error:null }];
  const calls=[];
  const h=harness(async command=>{calls.push(command);return results;});
  const device={id:'ms',name:'MS',ssh:{host:'MS',binary:''}};h.deviceList.devices=[device];
  await h.deviceList.loadCapabilities();
  assert.deepEqual(calls,['get_device_capabilities']);
  assert.match(h.deviceCapabilities(device),/Coport 0.1.0/);
  assert.doesNotMatch(h.deviceSettings(),/Check version|device-capabilities|<br>|Up to 32|SSH also supports/);
  assert.doesNotMatch(h.deviceSettings(),/data-action="device-forward"/);
  results[0].capabilities.forwarding=true;results[0].capabilities.forwardingAvailable=true;
  await h.deviceList.loadCapabilities();
  assert.match(h.deviceSettings(),/data-action="device-forward"[^>]*data-enabled="(?:true|false)" ><\/button>/);
  results[0].capabilities=null;results[0].error='old daemon has no capabilities command';
  await h.deviceList.loadCapabilities();
  assert.equal(h.deviceCapabilities(device), "Version unavailable: old daemon has no capabilities command");
  assert.match(h.deviceSettings(), /data-tip="Version unavailable: old daemon has no capabilities command" tabindex="0"/);
  assert.doesNotMatch(h.deviceSettings(), />Version unavailable|setting-description/);
  assert.doesNotMatch(h.deviceSettings(),/data-action="device-forward"/);
  h.deviceList.forwarders=[{deviceId:'ms',name:'MS'}];
  assert.match(h.deviceSettings(),/data-action="device-forward"[^>]*data-enabled="(?:true|false)" ><\/button>/);
});


test('forwarding settings keep mechanisms in documentation instead of repeated help text', () => {
  const h = harness(async () => []);
  h.ui.snap.forwardingRestoreSupported = true;
  h.deviceList.devices = [{ id: 'ms', name: 'MS', ssh: { host: 'MS' } }];
  h.deviceList.capabilities = { ms: { capabilities: { forwarding: true, forwardingAvailable: true } } };
  const html = h.deviceSettings();
  assert.doesNotMatch(html, /Restore on startup|forwarding-restore/);
  assert.doesNotMatch(html, /placeholder|Clients keep|Switching disconnects|Only one remote|If the remote is offline|daemon runs/);
});

test('selected device traffic renders before completion and ignores superseded channel updates', async () => {
  const pending = [];
  let renders = 0;
  const context = { Channel: class {}, trafficViews: { scope: 'all' }, ui: { page: 'devices', deviceTrafficMinutes: 1440 },
    invoke: (command, args) => new Promise((resolve, reject) => pending.push({ command, args, resolve, reject })),
    render() { renders++; } };
  context.deviceList = deviceListOf(context);
  const first = context.deviceList.loadMerged();
  assert.equal(pending[0].args.minutes, 1440);
  assert.equal(pending[0].args.scope, 'all');
  const initial = { minutes: 1440, scope: 'all', traffic: { requests: 7 }, sources: [] };
  pending[0].args.onUpdate.onmessage([initial]);
  assert.equal(context.deviceList.merged, initial);
  assert.equal(context.deviceList.loading, true);
  assert.equal(renders, 1);

  context.ui.deviceTrafficMinutes = 360;
  const second = context.deviceList.loadMerged(true);
  const next = { minutes: 360, scope: 'all', traffic: { requests: 9 }, sources: [] };
  pending[1].args.onUpdate.onmessage([next]);
  pending[0].args.onUpdate.onmessage([{ ...next, traffic: { requests: 99 } }]);
  assert.equal(context.deviceList.merged, next);
  pending[0].resolve([initial]); await first;
  assert.equal(context.deviceList.loading, true);
  pending[1].resolve([initial, next]); await second;
  assert.equal(context.deviceList.loading, false);
  assert.equal(Object.keys(context.deviceList.views).length, 2);
  pending[1].args.onUpdate.onmessage([{ ...next, traffic: { requests: 100 } }]);
  assert.equal(context.deviceList.merged, next);
});

test('a background failure retains the selected traffic already rendered', async () => {
  let update, reject;
  const context = { Channel: class {}, trafficViews: { scope: 'model' }, ui: { page: 'devices', deviceTrafficMinutes: 30 },
    invoke: (_, args) => { update = args.onUpdate; return new Promise((_, fail) => { reject = fail; }); },
    render() {} };
  context.deviceList = deviceListOf(context);
  const loading = context.deviceList.loadMerged();
  const view = { minutes: 30, scope: 'model', traffic: { requests: 5 }, sources: [] };
  update.onmessage([view]);
  reject('Background statistics failed');
  await loading;
  assert.equal(context.deviceList.merged, view);
  assert.equal(context.deviceList.loading, false);
  assert.equal(context.deviceList.error, 'Background statistics failed');
});

test('a periodic refresh keeps shown peers until they answer instead of flashing them away', async () => {
  const pending = [];
  const h = devicesHarness((command, args) => command === 'get_merged_data' ? new Promise((resolve, reject) => pending.push({ args, resolve, reject })) : Promise.resolve([]));
  await seedTraffic(h, { local: { '30:model': { requests: 1, start: 0, end: 60_000, bucketMinutes: 1, counts: [1], errorCounts: [0], tokenCounts: [0], credentials: [] } } });
  const counted = requests => ({ requests, start: 0, end: 60_000, bucketMinutes: 1, counts: [requests], errorCounts: [0], tokenCounts: [0], credentials: [] });
  // Each frame of the range comes completed with this device and Merged Traffic.
  const view = (requests, error) => ({ minutes: 30, scope: 'model', start: 0, end: 60_000, bucketMinutes: 1,
    local: counted(1), traffic: counted(1 + requests),
    sources: [{ name: 'MS', included: !error, ...(error ? { error } : { traffic: counted(requests) }) }] });
  const shown = view(5);
  h.deviceList.views = { '30:model': shown };
  h.deviceList.select();
  assert.match(h.devicesPage(), /device\/ms:model:5/);
  assert.match(h.mergedDataBlock(), /merged:model:6/);

  const refresh = h.deviceList.loadMerged(true, true);
  // The backend's first refresh frame lists every peer as pending.
  pending[0].args.onUpdate.onmessage([view(0, 'TRAFFIC_UPDATING')]);
  assert.equal(h.deviceList.merged, shown);
  assert.match(h.devicesPage(), /device\/ms:model:5/);
  assert.match(h.mergedDataBlock(), /merged:model:6/);
  assert.doesNotMatch(h.mergedDataBlock(), /Loading traffic…/);
  // The peer's answer replaces the shown view at once.
  pending[0].args.onUpdate.onmessage([view(8)]);
  assert.match(h.devicesPage(), /device\/ms:model:8/);
  assert.match(h.mergedDataBlock(), /merged:model:9/);
  // A real failure is shown, never hidden behind the earlier success.
  pending[0].args.onUpdate.onmessage([view(0, 'Permission denied')]);
  assert.match(h.mergedDataBlock(), /MS: Permission denied/);
  assert.match(h.devicesPage(), /Traffic unavailable/);
  pending[0].resolve([view(8)]);
  await refresh;

  // With nothing shown yet, a cold refresh still renders its placeholders.
  h.deviceList.views = Object.create(null); h.deviceList.merged = null;
  const cold = h.deviceList.loadMerged(true, true);
  pending[1].args.onUpdate.onmessage([view(0, 'TRAFFIC_UPDATING')]);
  assert.match(h.mergedDataBlock(), /MS: Loading traffic…/);
  pending[1].resolve([view(3)]);
  await cold;
});

test('open Devices refreshes remote statistics every 30 seconds; Settings never does', async () => {
  const calls=[];
  const h=harness(async(command,args)=>{calls.push([command,args]);return command==='get_devices' ? [{id:'ms',name:'MS',ssh:{host:'MS'}}] : [];});
  h.deviceList.request=0;h.ui.deviceTrafficMinutes=30;h.trafficViews.scope = 'model';h.deviceList.select=()=>false;h.Channel=class {};
  h.deviceList.loadMerged = h.loadMerged;
  const remote=()=>calls.filter(([command,args])=>command==='get_merged_data' && args.refreshRemote===true).length;
  // Repeated visits and timer ticks within 30 seconds read the cache.
  for(let i=0;i<4;i++) {h.ui.page='devices';await h.deviceList.load();h.ui.page='settings';await h.deviceList.load();}
  const reads=calls.filter(([command])=>command==='get_merged_data');
  assert.equal(reads.length,4,'Settings must not read statistics');
  assert.deepEqual(reads.map(([,args])=>args.refreshRemote),[true,false,false,false]);
  assert.equal(h.deviceList.error,'');
  // Once the last attempt is 30 seconds old, the next visit or tick refreshes.
  h.deviceList.remoteAttemptAt=Date.now()-30000;
  h.ui.page='devices';await h.deviceList.load();
  assert.equal(remote(),2);
  await h.deviceList.load();
  assert.equal(remote(),2);
  // Refresh is always available and restarts the interval.
  await h.action('merged-refresh');
  assert.equal(remote(),3);
  await h.deviceList.load();
  assert.equal(remote(),3);
  // Leaving Devices stops automatic refreshes even when one is due.
  h.deviceList.remoteAttemptAt=Date.now()-30000;
  h.ui.page='settings';await h.deviceList.load();
  assert.equal(remote(),3);
});

test('a slow remote refresh is neither repeated nor superseded by the next tick', async () => {
  const pending=[];
  const h=harness((command,args)=>command==='get_merged_data' ? new Promise(resolve=>pending.push({args,resolve})) : Promise.resolve(command==='get_devices' ? [{id:'ms',name:'MS',ssh:{host:'MS'}}] : []));
  h.deviceList.request=0;h.ui.deviceTrafficMinutes=30;h.trafficViews.scope = 'model';h.deviceList.select=()=>false;h.Channel=class {};
  h.deviceList.loadMerged = h.loadMerged;
  const first=h.deviceList.load();
  for(let i=0;i<5 && !pending.length;i++) await Promise.resolve();
  assert.equal(pending.length,1);
  assert.equal(pending[0].args.refreshRemote,true);
  // The peer takes longer than the interval to answer.
  h.deviceList.remoteAttemptAt=Date.now()-30000;
  h.deviceList.refreshing=false;
  const tick=h.deviceList.load();
  for(let i=0;i<5;i++) await Promise.resolve();
  assert.equal(pending.length,1,'A due tick must not start a second SSH read');
  pending[0].resolve([]);
  await Promise.all([first,tick]);
});

test('cached statistics retain success without hiding a later failure', () => {
  const h=harness(async()=>[]);const device={id:'peer',name:'Peer',ssh:{host:'test'}};
  h.deviceList.startup={peer:{host:'test',state:'running',label:'SSH reachable at startup'}};
  h.deviceList.fetchedAt=Date.now()-60000;
  h.deviceList.merged={sources:[{name:'Peer',error:'connection refused'}]};
  assert.equal(h.deviceList.status(device).state,'failed');
  h.deviceList.merged.sources[0]={name:'Peer',included:true,error:null};
  assert.equal(h.deviceList.status(device).state,'running');
  assert.match(h.deviceList.status(device).label,/cached/);
  h.deviceList.merged.sources[0]={name:'Peer',error:'new failure'};
  assert.equal(h.deviceList.status(device).label,'new failure');
});

test('Devices Refresh performs only one explicitly authorized statistics read', async () => {
  const calls = [];
  const h = harness(async (command, args) => {
    calls.push([command, args]);
    assert.ok(['get_devices', 'get_device_forwarders', 'get_device_capabilities', 'get_merged_data'].includes(command), `Unexpected networking command: ${command}`);
    return [];
  });
  h.Channel = class {};
  h.deviceList.request = 0;
  h.ui.deviceTrafficMinutes = 30;
  h.trafficViews.scope = 'model';
  h.deviceList.select = () => false;
  h.deviceList.loadMerged = h.loadMerged;
  h.deviceList.startupRefreshDone = true;
  await h.action('merged-refresh');
  const reads = calls.filter(([command]) => command === 'get_merged_data');
  assert.equal(reads.length, 1, 'Refresh must not wait for a redundant local full-history merge');
  assert.equal(reads[0][1].refreshRemote, true);
  assert.equal(h.deviceList.error, '');
  calls.length = 0;
  for (let i = 0; i < 3; i++) await h.deviceList.load();
  assert.ok(calls.filter(([command]) => command === 'get_merged_data').every(([,args]) => args.refreshRemote === false));
});


test('Settings status uses successful cached SSH capabilities without connecting', () => {
  const h = harness(() => { throw new Error('Status rendering must not invoke IPC'); });
  const device = { id: 'peer', name: 'Peer', ssh: { host: 'test' } };
  h.deviceList.merged = { sources: [{ name: 'Peer', exclusion: 'not_refreshed' }] };
  h.deviceList.capabilities = { peer: { pending: false, capabilities: { running: true, forwarding: true } } };
  assert.equal(h.deviceList.status(device).state, 'running');
  assert.match(h.deviceList.status(device).label, /cached/);
  h.deviceList.capabilities = {};
  assert.equal(h.deviceList.status(device).state, '');
});


test('newest SSH evidence wins across startup, capabilities and statistics', () => {
  const h = harness(async () => []);
  const device = {id:'ms', name:'MS', ssh:{host:'MS'}};
  h.deviceList.startup = {ms:{host:'MS', state:'failed', label:'startup failed', checkedAt:100}};
  h.deviceList.merged = {sources:[{name:'MS',error:'old statistics failure'}]};
  h.deviceList.fetchedAt = 150;
  h.deviceList.capabilities = {ms:{host:'MS', checkedAt:200, capabilities:{statistics:true,forwarding:true}}};
  assert.equal(h.deviceList.status(device).state, 'running');
  h.deviceList.fetchedAt = 300;
  assert.equal(h.deviceList.status(device).label, 'old statistics failure');
  h.deviceList.capabilities.ms.checkedAt = 400;
  assert.equal(h.deviceList.status(device).state, 'running');
  h.deviceList.capabilities.ms.host = 'other';
  assert.equal(h.deviceList.status(device).state, 'failed', 'A previous host must not mark an edited device reachable');
});

test('SSH recovery refreshes peers only while Devices is open', async () => {
  for (const page of ['settings','main','devices']) {
    let value = {deviceId:'ms',host:'MS',checkedAt:200,capabilities:{statistics:true,forwarding:true},error:null};
    const h = harness(async () => [value]);
    const reads=[];
    h.deviceList.loadMerged = async (...args) => { reads.push(args); h.deviceList.recoveryPending=false; h.deviceList.remoteAttemptAt=Date.now(); };
    h.ui.page=page; h.deviceList.devices=[{id:'ms',name:'MS',ssh:{host:'MS'}}];
    h.deviceList.startupRefreshDone=true;
    h.deviceList.capabilities={ms:{host:'MS',checkedAt:100,error:'offline'}};
    await h.deviceList.loadCapabilities();
    assert.equal(h.deviceList.status(h.deviceList.devices[0]).state,'running');
    // Settings and Home only update the light; leaving Devices stops peer reads.
    assert.deepEqual(reads,page==='devices'?[[true,true]]:[]);
    await h.deviceList.loadCapabilities();
    value={...value,checkedAt:300};
    await h.deviceList.loadCapabilities();
    assert.equal(reads.length,page==='devices'?1:0,'Healthy polling must not repeatedly refresh remote statistics');
  }
});

test('a statistics failure under a healthy tunnel does not retry SSH before the Devices cadence', () => {
  const h=harness(()=>{throw new Error('Synchronizing cached evidence must not start a probe')});
  const device={id:'ms',name:'MS',ssh:{host:'MS'}};
  h.deviceList.devices=[device];h.ui.page='devices';h.deviceList.startupRefreshDone=true;
  let refreshes=0;
  h.deviceList.loadMerged=async()=>{refreshes++;h.deviceList.recoveryPending=false;h.deviceList.remoteAttemptAt=Date.now();};
  // Each statistics read fails while the forwarding health check, checked later, stays connected.
  for (let i=1;i<=4;i++) {
    h.deviceList.merged={sources:[{name:'MS',error:'Permission denied'}]};h.deviceList.fetchedAt=i*100;
    assert.equal(h.deviceList.status(device).state,'failed');
    h.deviceList.setForwarders([{deviceId:'ms',health:{state:'connected',lastCheckedAt:i*100+50}}]);
    assert.equal(h.deviceList.status(device).state,'running');
  }
  assert.equal(refreshes,1,'Recovery evidence must not bypass the Devices refresh cadence');
});

test('SSH recovery does not read remote statistics before Devices has been visited', async () => {
  const h=harness(async()=>[{deviceId:'ms',host:'MS',checkedAt:200,capabilities:{statistics:true}}]);
  h.ui.page='settings';h.deviceList.devices=[{id:'ms',name:'MS',ssh:{host:'MS'}}];
  h.deviceList.startup={ms:{host:'MS',checkedAt:100,state:'failed'}};
  h.deviceList.loadMerged=()=>{throw new Error('Unexpected statistics request')};
  await h.deviceList.loadCapabilities();
  assert.equal(h.deviceList.status(h.deviceList.devices[0]).state,'running');
  assert.ok(!h.deviceList.recoveryPending);
});

test('recovery during an in-flight statistics request waits for the Devices cadence, then refreshes once', async () => {
  const pending=[];
  const h=harness(async (command,args)=>{
    if(command==='get_device_capabilities') return [{deviceId:'ms',host:'MS',checkedAt:200,capabilities:{statistics:true}}];
    assert.equal(command,'get_merged_data');
    return new Promise(resolve=>pending.push({args,resolve}));
  });
  h.Channel=class {};h.deviceList.request=0;h.ui.deviceTrafficMinutes=30;h.trafficViews.scope = 'model';
  h.ui.page='devices';h.deviceList.devices=[{id:'ms',name:'MS',ssh:{host:'MS'}}];
  h.deviceList.startupRefreshDone=true;h.deviceList.capabilities={ms:{host:'MS',checkedAt:100,error:'offline'}};
  h.deviceList.loadMerged = h.loadMerged;
  const first=h.deviceList.loadMerged(true,true);
  await h.deviceList.loadCapabilities();
  await h.deviceList.loadCapabilities();
  assert.equal(pending.length,1);
  assert.equal(h.deviceList.recoveryPending,true);
  pending[0].resolve([{minutes:30,scope:'model',sources:[]}]);await first;
  // The attempt that just finished is recent, so the recovery waits.
  assert.equal(pending.length,1);
  assert.equal(h.deviceList.recoveryPending,true);
  h.deviceList.remoteAttemptAt=Date.now()-30000;
  const second=h.deviceList.refreshRecovered();
  assert.equal(pending.length,2);
  assert.equal(pending[1].args.refreshRemote,true);
  pending[1].resolve([{minutes:30,scope:'model',sources:[]}]);await second;
  assert.equal(h.deviceList.loading,false);
  assert.equal(h.deviceList.recoveryPending,false);
  assert.equal(pending.length,2);
});

test('unchanged legacy capability results retain their original observation time', async () => {
  const h=harness(async()=>[{deviceId:'ms',capabilities:{statistics:true}}]);
  h.deviceList.devices=[{id:'ms',name:'MS',ssh:{host:'MS'}}];
  await h.deviceList.loadCapabilities();
  const at=h.deviceList.capabilities.ms.checkedAt;
  h.deviceList.merged={sources:[{name:'MS',error:'new failure'}]};h.deviceList.fetchedAt=at+1;
  await h.deviceList.loadCapabilities();
  assert.equal(h.deviceList.capabilities.ms.checkedAt,at);
  assert.equal(h.deviceList.status(h.deviceList.devices[0]).state,'failed');
});


test('existing forwarding and statistics probes synchronize lights without reading peers outside Devices', () => {
  const h=harness(()=>{throw new Error('Synchronizing cached evidence must not start a probe')});
  const device={id:'ms',name:'MS',ssh:{host:'MS'}};
  h.deviceList.devices=[device];h.ui.page='settings';h.deviceList.startupRefreshDone=true;
  h.deviceList.startup={ms:{host:'MS',state:'failed',checkedAt:100,label:'old startup failure'}};
  let refreshes=0;h.deviceList.loadMerged=async()=>{refreshes++;};
  h.deviceList.setForwarders([{deviceId:'ms',health:{state:'connected',lastCheckedAt:200},remoteTraffic:{fetchedAt:220}}]);
  assert.equal(h.deviceList.status(device).state,'running');
  h.deviceList.setForwarders([{deviceId:'ms',health:{state:'connected',lastCheckedAt:300},remoteTraffic:{fetchedAt:320},remoteTrafficError:'statistics disconnected',remoteTrafficCheckedAt:400}]);
  assert.equal(h.deviceList.status(device).label,'statistics disconnected');
  h.deviceList.setForwarders([{deviceId:'ms',health:{state:'connected',lastCheckedAt:500},remoteTraffic:{fetchedAt:520},remoteTrafficError:null,remoteTrafficCheckedAt:520}]);
  assert.equal(h.deviceList.status(device).state,'running');
  assert.equal(refreshes,0,'Settings reads only cached evidence');
  // The recovery is remembered; the next due Devices refresh picks it up.
  h.ui.page='devices';
  h.deviceList.setForwarders([{deviceId:'ms',health:{state:'connected',lastCheckedAt:600},remoteTraffic:{fetchedAt:620}}]);
  assert.equal(refreshes,1);
});

test('forwarding evidence never changes an HTTP statistics connection or starts an unvisited Devices refresh', () => {
  const h=harness(()=>{throw new Error('No IPC expected')});
  const ssh={id:'ssh',name:'SSH',ssh:{host:'MS'}};
  const http={id:'http',name:'HTTP',ssh:{host:'MS'},data:{transport:'http',url:'https://example.invalid'}};
  h.deviceList.devices=[ssh,http];h.deviceList.fetchedAt=100;
  h.deviceList.merged={sources:[{name:'SSH',error:'old failure'},{name:'HTTP',error:'HTTP failed'}]};
  h.deviceList.loadMerged=()=>{throw new Error('Devices has not been visited')};
  h.deviceList.setForwarders([{deviceId:'ssh',health:{state:'connected',lastCheckedAt:200}},{deviceId:'http',health:{state:'connected',lastCheckedAt:200}}]);
  assert.equal(h.deviceList.status(ssh).state,'running');
  assert.equal(h.deviceList.status(http).state,'failed');
});

test('Merged Traffic and every card show the views of one completed frame', async () => {
  const shown = [];
  const h = devicesHarness(async () => []);
  h.deviceTrafficContent = (traffic, scope, key) => { shown.push([key, traffic]); return `${key}:${traffic.requests};`; };
  h.message = (_, text) => text;
  const view = (requests) => ({ requests, errors: 0, bytes: requests, start: 0, end: 60000, bucketMinutes: 1, counts: [requests], errorCounts: [0], tokenCounts: [0], credentials: [] });
  h.deviceList.devices = [{ id: 'ms', name: 'MS', ssh: { host: 'MS' } }, { id: 'fw', name: 'Forwarded', ssh: { host: 'fw' } }, { id: 'dup', name: 'Duplicate', ssh: { host: 'dup' } }];
  // Shared views read for Home differ from the frame's; the frame wins once completed.
  await seedTraffic(h, { local: { '30:model': view(100) } });
  h.ui.snap.forwarding = { deviceId: 'fw' };
  await seedTraffic(h, { forwarded: { 'fw:30:model': { ...view(400), device: 'fw' } } });
  const frame = { minutes: 30, scope: 'model', windowStart: 0, windowEnd: 60000, bucketMinutes: 1, sources: [
    { name: 'MS', included: true, traffic: view(2) },
    { name: 'Forwarded', included: true, traffic: view(3) },
    { name: 'Duplicate', included: false, exclusion: 'duplicate' },
  ] };
  // Until the backend completes the frame, shared views fill their cards and Merged Traffic waits.
  h.deviceList.merged = frame;
  assert.match(h.devicesPage(), /local:100;[\s\S]*device\/ms:2;[\s\S]*device\/fw:400;/);
  assert.match(h.mergedDataBlock(), /Loading traffic…/);
  // The forwarded device's card is its live view; Merged Traffic is the backend's sum of these cards.
  Object.assign(frame, { local: view(1), forwarded: { device: 'fw', traffic: view(4) }, traffic: view(7) });
  shown.length = 0;
  assert.match(h.devicesPage(), /local:1;[\s\S]*device\/ms:2;[\s\S]*device\/fw:4;/);
  const merged = h.mergedDataBlock();
  assert.match(merged, /merged:7;/);
  assert.equal(shown.find(([key]) => key === 'merged')[1], frame.traffic);
  assert.match(merged, /Duplicate: Already counted via another entry/);
  assert.doesNotMatch(merged, /Loading traffic…/);
});

test('a forwarded device already counted via another entry gets no card', async () => {
  const h = devicesHarness(async () => []);
  h.deviceTrafficContent = (traffic, scope, key) => `${key}:${traffic.requests};`;
  h.message = (_, text) => text;
  const view = (requests) => ({ requests, errors: 0, bytes: requests, start: 0, end: 60000, bucketMinutes: 1, counts: [requests], errorCounts: [0], tokenCounts: [0], credentials: [] });
  // Both entries point at one machine; the HTTP entry is listed first and counted.
  h.deviceList.devices = [{ id: 'http', name: 'Machine HTTP', data: { transport: 'http', url: 'https://example.invalid' } }, { id: 'fw', name: 'Machine SSH', ssh: { host: 'fw' } }];
  h.ui.snap.forwarding = { deviceId: 'fw' };
  await seedTraffic(h, { forwarded: { 'fw:30:model': { ...view(4), device: 'fw' } }, local: { '30:model': view(1) } });
  // The backend leaves the duplicate's live view out of the frame and the sum.
  h.deviceList.merged = { minutes: 30, scope: 'model', windowStart: 0, windowEnd: 60000, bucketMinutes: 1, local: view(1), traffic: view(5), sources: [
    { name: 'Machine HTTP', included: true, traffic: view(4) },
    { name: 'Machine SSH', included: false, exclusion: 'duplicate' },
  ] };
  const cards = h.devicesPage();
  assert.doesNotMatch(cards, /device\/fw:/);
  assert.match(cards, /Already counted via another entry/);
  assert.match(h.mergedDataBlock(), /merged:5;/);
  // Before completion the shared live view is not shown for a duplicate either.
  delete h.deviceList.merged.local;
  assert.doesNotMatch(h.devicesPage(), /device\/fw:/);
});

test('a frame that could not read this device says why instead of loading forever', async () => {
  const h = devicesHarness(async () => []);
  h.deviceTrafficContent = (traffic, scope, key) => `${key}:${traffic.requests};`;
  h.ui.page = 'devices';
  h.deviceList.devices = [];
  h.deviceList.merged = { minutes: 30, scope: 'model', windowStart: 0, windowEnd: 60000, bucketMinutes: 1, localError: 'Cannot read proxy.log', sources: [] };
  const merged = h.mergedDataBlock();
  assert.match(merged, /This device: Cannot read proxy\.log/);
  assert.doesNotMatch(merged, /Loading traffic/);
  assert.match(h.devicesPage(), /This Device[\s\S]*<\/div>Cannot read proxy\.log/);
  // Such a frame is complete: the next tick waits like after a successful read.
  assert.ok(h.deviceList.frameFresh({ ...h.deviceList.merged, loadedAt: Date.now() }));
  // This device's own view from another read still fills its card.
  await seedTraffic(h, { local: { '30:model': { requests: 7, credentials: [] } } });
  assert.match(h.devicesPage(), /local:7;/);
  // A read that failed as a whole shows its error, not loading.
  h.deviceList.merged = null;
  h.deviceList.error = 'Cannot read traffic history';
  const failed = h.mergedDataBlock();
  assert.match(failed, /Cannot read traffic history/);
  assert.doesNotMatch(failed, /Loading traffic/);
});

test('between refreshes the Devices timer reads no statistics for peers', async () => {
  const calls = [];
  const h = devicesHarness(async (command, args) => { calls.push(command); return command === 'get_devices' ? [{ id: 'ms', name: 'MS', ssh: { host: 'MS' } }] : []; });
  h.ui.page = 'devices';
  const frame = () => ({ '30:model': { minutes: 30, scope: 'model', local: { requests: 1, credentials: [] }, loadedAt: Date.now(), sources: [] } });
  h.deviceList.views = frame();
  h.deviceList.remoteAttemptAt = Date.now();
  await seedTraffic(h, { local: { '30:model': { requests: 1, credentials: [], loadedAt: Date.now() } } });
  for (let i = 0; i < 3; i++) await h.deviceList.load();
  // Peers' views stay as they are, and this device's fresh view is reused.
  assert.ok(!calls.includes('get_merged_data'));
  assert.ok(!calls.includes('get_traffic'));
  // A frame older than the Devices cadence is completed again from the cache.
  h.deviceList.views['30:model'].loadedAt -= 30000;
  await h.deviceList.load();
  assert.equal(calls.filter((command) => command === 'get_merged_data').length, 1);
  h.deviceList.views = frame();
  // Once due, the next tick refreshes peers.
  h.deviceList.remoteAttemptAt = Date.now() - 30000;
  await h.deviceList.load();
  assert.equal(calls.filter((command) => command === 'get_merged_data').length, 2);
});


test('startup restore follows only the active device when forwarding switches targets', () => {
  const h = harness(async () => []);
  h.deviceList.devices = ['first', 'second'].map(id => ({ id, name: id, ssh: { host: id } }));
  h.deviceList.capabilities = Object.fromEntries(h.deviceList.devices.map(d => [d.id, { capabilities: { forwarding: true, forwardingAvailable: true } }]));
  h.ui.snap.forwardingRestoreSupported = true;
  h.ui.snap.forwardingRestore = true;
  for (const selected of ['first', 'second']) {
    h.deviceList.forwarders = [{ deviceId: selected }];
    const html = h.deviceSettings();
    assert.equal((html.match(/data-action="device-forward"/g) || []).length, 2);
    assert.equal((html.match(/data-action="forwarding-restore"/g) || []).length, 1);
    const groups = html.split(/<div class="device-settings-item"[^>]*>/).slice(1);
    for (let i = 0; i < 2; i++) {
      if (h.deviceList.devices[i].id === selected) {
        assert.match(groups[i], /SSH forwarding[\s\S]*aria-checked="true"[^>]*data-action="device-forward"[\s\S]*Restore on startup[\s\S]*aria-checked="true"[^>]*data-action="forwarding-restore"/);
      } else {
        assert.match(groups[i], /aria-checked="false"[^>]*data-action="device-forward"/);
        assert.doesNotMatch(groups[i], /forwarding-restore/);
      }
    }
  }
  h.deviceList.forwarders = [];
  assert.doesNotMatch(h.deviceSettings(), /forwarding-restore/);
  assert.equal(h.ui.snap.forwardingRestore, true);
});

test('removing a device forgets its startup check and reads peers only while devices remain', async () => {
  const calls = [];
  let devices = [{ id: 'a', name: 'A', ssh: { host: 'a' } }, { id: 'b', name: 'B', ssh: { host: 'b' } }];
  const ui = { page: 'devices', deviceTrafficMinutes: 30 };
  const list = createDeviceList({
    invoke: async (command, args) => {
      calls.push(command);
      if (command === 'remove_device') devices = devices.filter(device => device.id !== args.id);
      if (command === 'get_device_forwarders') return [{ deviceId: 'b' }];
      return command === 'get_devices' ? devices : [];
    },
    Channel: class {}, trafficViews: { scope: 'model' }, page: () => ui.page, minutes: () => ui.deviceTrafficMinutes,
    snap: () => null, render() {}, toast: message => { throw new Error(message); },
  });
  const reads = [];
  list.loadMerged = (...args) => reads.push(args);
  await list.loadDefinitions();
  list.startup.a = { host: 'a', state: 'failed' };
  await list.remove('a');
  assert.equal(list.startup.a, undefined);
  assert.deepEqual(list.devices.map(device => device.id), ['b']);
  assert.deepEqual(reads, [[true]]);
  await list.remove('b');
  assert.deepEqual(reads, [[true]]);
  assert.deepEqual(await list.forward('b', true), { deviceId: 'b' });
  assert.deepEqual(calls, ['get_devices', 'remove_device', 'get_devices', 'remove_device', 'get_devices', 'set_device_forwarding', 'get_device_forwarders']);
});
