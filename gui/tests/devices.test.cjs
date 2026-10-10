const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const { test } = require('node:test');
const source = fs.readFileSync(require('node:path').join(__dirname, '../ui/app.js'), 'utf8');
const start = source.indexOf('function deviceConnectionKey(');
const code = source.slice(start, source.indexOf('// ---------------------------------------------------------------- settings', start));
function harness(invoke) {
  const ui = { page: 'devices', devices: [], snap: { phase: { state: 'running', port: 8787 } } };
  const fields = {}, listeners = {};
  const context = { ui, invoke, Promise, ICON: { plus: '<plus>', trash: '<trash>' }, mergedDataBlock: () => '', loadMergedData() {}, toast() {},
    message: (_, text) => text, esc: v => String(v).replace(/</g, '&lt;'), block: (title, aside, body) => `${title}${aside}${body}`,
    $: id => fields[id], document: { addEventListener: (event, callback) => { listeners[event] = callback; } } };
  context.render = () => {
    for (const key of Object.keys(fields)) delete fields[key];
    if (!ui.deviceFormOpen) return;
    const html = context.deviceSettings();
    for (const match of html.matchAll(/<input[^>]*id="([^"]+)"[^>]*value="([^"]*)"/g)) fields[match[1]] = { value: match[2], id: match[1], focus() {} };
    fields['device-transport'] = { value: ui.deviceDraft.transport, id: 'device-transport' };
  };
  vm.createContext(context);
  const relabelStart = source.indexOf("function relabelDeviceViews(");
  const relabelEnd = source.indexOf("async function loadDeviceDefinitions",relabelStart);
  vm.runInContext(source.slice(relabelStart,relabelEnd),context);
  vm.runInContext(code, context);
  context.fields = fields; context.listeners = listeners;
  context.action = async (name, el = context.el || { dataset: {} }) => {
    context.el = el;
    const start = source.indexOf(`    case "${name}":`);
    const end = source.indexOf('    case ', start + 10);
    await vm.runInContext(`(async () => { switch ('${name}') { ${source.slice(start, end)} } })()`, context);
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
  await h.loadDevices();
  assert.deepEqual(calls, ['get_devices', 'get_device_forwarders', 'get_device_capabilities']);
  const html = h.devicesPage();
  assert.match(html, /&lt;remote>/); assert.doesNotMatch(html, /Read-only statistics/);
  assert.doesNotMatch(html, /device-control|Remote Port|PID|Uptime/);
  assert.doesNotMatch(html, /data-action="power"/);
  assert.doesNotMatch(source, /control_device|device-management|device-ssh-enabled|device-data-enabled/);
});

test('configuration lives in Settings and Settings loading makes no remote requests', async () => {
  const calls = [];
  const h = harness(async command => { calls.push(command); return []; });
  h.ui.page = 'settings'; await h.loadDeviceDefinitions(); assert.deepEqual(calls, ['get_devices']);
  const html = h.devicesPage(); assert.doesNotMatch(html, /device-save|Configure in Settings|Updates automatically/);
});

test('Settings places forwarding immediately after SSH device configuration', () => {
  const code = source.slice(source.indexOf('function settings()'), source.indexOf('/// The fixed configuration'));
  const rendered = vm.runInNewContext(code + '\nsettings()', {
    ui: { snap: { settings: {} } }, ICON: {}, panelSelect: () => '',
    block: title => `[${title}]`, configSettings: () => '[Network]', filesBlock: () => '[Files]', forwardingSettings: () => '[Request forwarding]', deviceSettings: () => '[Devices]',
  });
  assert.ok(rendered.includes('[Files]\n    [Devices]\n    [Request forwarding]'));
  assert.ok(rendered.trim().endsWith('[Request forwarding]')); assert.equal((rendered.match(/\[Devices\]/g) || []).length, 1);
});

test('each device renders full Traffic metrics and an offline peer leaves other devices visible', () => {
  const h = harness(async () => []);
  Object.assign(h, { chart: () => '<svg class="chart"></svg>', stat: (name, value) => `${name}:${value};`, fmtMs: v => `${v} ms`, fmtBytes: v => `${v} B`, modelTokenStats: t => `Input Tokens:${t.inputTokens};Hit Rate:${t.cacheHitRate};`, trafficShare: () => '<div class="traffic-share">Account usage</div>', serviceMark: () => '', message: (kind, value) => value, ICON: { chevron: '' } });
  const start = source.indexOf('function deviceTrafficContent(');
  vm.runInContext(source.slice(start, source.indexOf('function mergedDataBlock(', start)), h);
  const traffic = { requests: 5, errors: 1, avgMs: 200, bytes: 1234, inputTokens: 100, cacheHitRate: 0.3, credentials: [] };
  h.ui.devices = [{ name: 'Online', ssh: { host: 'online' }, data: { transport: 'ssh' } }, { name: 'Offline', ssh: { host: 'offline' }, data: { transport: 'ssh' } }];
  h.ui.mergedData = { scope: 'model', local: traffic, sources: [{ name: 'Online', traffic }, { name: 'Offline', error: 'Unavailable' }] };
  const html = h.devicesPage();
  assert.equal((html.match(/Calls:5/g) || []).length, 2);
  for (const metric of ['Error Rate:20.0%', 'Avg. Time:200 ms', 'Received:1234 B', 'Input Tokens:100', 'Hit Rate:0.3', 'traffic-share', 'class="chart"']) assert.ok(html.includes(metric), metric);
  assert.match(html, /Offline/); assert.match(html, /Traffic unavailable/);
});

test('traffic range changes reject late responses from the previous selection', async () => {
  const pending = [];
  const context = { Channel: class {}, ui: { page: 'devices', deviceTrafficMinutes: 30, trafficScope: 'model', mergedDataRequest: 0 }, invoke: (command, args) => new Promise(resolve => pending.push({ command, args, resolve })), render() {} };
  vm.createContext(context);
  const start = source.indexOf('async function loadMergedData(');
  vm.runInContext(source.slice(start, source.indexOf('function deviceTrafficContent(', start)), context);
  const first = context.loadMergedData();
  context.ui.deviceTrafficMinutes = 1440; context.ui.trafficScope = 'all';
  const next = context.loadMergedData(true);
  assert.equal(pending[0].command, 'get_merged_data');
  assert.equal(pending[1].command, 'get_merged_data');
  pending[1].resolve({ selected: 'new' }); await next;
  pending[0].resolve({ selected: 'old' }); await first;
  assert.equal(context.ui.mergedData.selected, 'new');
  assert.equal(context.ui.mergedDataLoading, false);
});

test('Devices button is visible only with configured peers and the redundant Devices block is removed', () => {
  const top = source.slice(source.indexOf('function renderTop()'), source.indexOf('function renderPage(', source.indexOf('function renderTop()')));
  let html;
  const context = { ui: { page: 'main', snap: { version: 'test', settings: { deviceCount: 0 } } }, esc: v => v, $: () => ({ set innerHTML(value) { html = value; } }) };
  vm.createContext(context); vm.runInContext(top, context);
  context.renderTop(); assert.doesNotMatch(html, /data-page="devices"/);
  context.ui.snap.settings.deviceCount = 1; context.renderTop(); assert.match(html, /data-page="devices"/);
  context.ui.devicesLoaded = true; context.ui.devices = []; context.renderTop(); assert.doesNotMatch(html, /data-page="devices"/);
  const devices = source.slice(source.indexOf('function devicesPage()'), source.indexOf('function deviceSettings()'));
  assert.doesNotMatch(devices, /block\("Devices"/);
});

test('automatic alignment retains previous traffic while refreshing without an error message', async () => {
  const context = { Channel: class {}, ui: { page: 'devices', deviceTrafficMinutes: 30, trafficScope: 'model', mergedDataRequest: 0, mergedData: { selected: 'last-good' } }, invoke: async () => { throw 'TRAFFIC_UPDATING'; }, render() {} };
  vm.createContext(context);
  const start = source.indexOf('async function loadMergedData(');
  vm.runInContext(source.slice(start, source.indexOf('function deviceTrafficContent(', start)), context);
  await context.loadMergedData();
  assert.equal(context.ui.mergedData.selected, 'last-good');
  assert.equal(context.ui.mergedDataError, '');
  assert.equal(context.ui.mergedDataUpdating, true);
});

test('upstream account disclosures retain per-device expansion and place rings before charts', () => {
  const h = harness(async () => []);
  const events = {};
  Object.assign(h, { trafficReview: () => '', chart: () => '<chart>', trafficShare: () => '<ring>', modelTokenStats: () => '', stat: () => '', fmtMs: () => '', fmtBytes: () => '', serviceMark: () => '', ICON: { chevron: '' }, document: { addEventListener: (name, callback) => { events[name] = callback; } } });
  const start = source.indexOf('function deviceTrafficContent(');
  vm.runInContext(source.slice(start, source.indexOf('function mergedDataBlock(', start)), h);
  const traffic = { requests: 1, errors: 0, credentials: [{ credential: 'Known', service: 'Claude', requests: 1, errors: 0 }] };
  events.toggle({ target: { isConnected: true, matches: () => true, dataset: { deviceTraffic: 'device/a' }, open: true } });
  let html = h.deviceTrafficContent(traffic, 'model', 'device/a');
  assert.match(html, /data-device-traffic="device\/a" open/);
  assert.ok(html.indexOf('<ring>') < html.indexOf('<chart>'));
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
  const context = { Channel: class {}, ui: { page: 'devices', deviceTrafficMinutes: 30, trafficScope: 'model', mergedDataRequest: 0 }, invoke: async () => { calls++; return views; }, render() {} };
  vm.createContext(context);
  const a = source.indexOf('function relabelDeviceViews(');
  vm.runInContext(source.slice(a,source.indexOf('async function loadDeviceDefinitions',a)),context);
  const start = source.indexOf('function selectDeviceTraffic(');
  vm.runInContext(source.slice(start, source.indexOf('function deviceTrafficContent(', start)), context);
  await context.loadMergedData();
  assert.equal(context.ui.mergedData.traffic.requests, 3);
  for (let i=0;i<10;i++) {
    context.ui.trafficScope = i%2 ? 'model' : 'all';
    assert.equal(context.selectDeviceTraffic(),true);
    assert.equal(context.ui.mergedData.traffic.requests,i%2 ? 3 : 5);
  }
  context.ui.deviceTrafficMinutes = 360; context.ui.trafficScope = 'all';
  assert.equal(context.selectDeviceTraffic(),true);
  assert.equal(context.ui.mergedData.traffic.requests,8);
  assert.equal(calls,1);
});

test('Settings devices retain verified results and label older results as cached', () => {
  const h = harness(async () => []);
  h.ui.devices = [{ id: 'ok', name: 'Working', ssh: { host: 'ok' }, data: { transport: 'ssh' } }, { id: 'bad', name: 'Failed', ssh: { host: 'bad' }, data: { transport: 'ssh' } }];
  h.ui.deviceTrafficFetchedAt = Date.now();
  h.ui.deviceTrafficViews = { '30:model': { sources: [{ name: 'Working', included: true }, { name: 'Failed', included: false, error: 'Data rejected' }] } };
  let html = h.deviceSettings();
  assert.match(html, /data-action="device-new"[^>]*><plus>/);
  assert.match(html, /data-action="device-remove"[^>]*><trash>/);
  assert.match(html, /dot running/); assert.match(html, /dot failed/);
  assert.doesNotMatch(html, />Remove<|>Add Device</);
  h.ui.deviceTrafficFetchedAt = Date.now() - 46000;
  html = h.deviceSettings(); assert.match(html, /dot running/); assert.match(html, /succeeded \(cached\)/); assert.match(html, /dot failed/);
});

test('device cards show transport beside the same verified status light as Settings', () => {
  const h = harness(async () => []);
  h.deviceTrafficContent = () => '';
  h.message = (_, text) => text;
  h.ui.devices = [
    { id: 'ssh', name: 'SSH peer', ssh: { host: 'peer' }, data: { transport: 'ssh' } },
    { id: 'http', name: 'HTTP peer', data: { transport: 'http', url: 'http://10.0.0.2' } },
    { id: 'https', name: 'HTTPS peer', data: { transport: 'http', url: 'https://peer.example' } },
  ];
  h.ui.deviceTrafficFetchedAt = Date.now();
  h.ui.mergedData = { sources: h.ui.devices.map(d => ({ name: d.name, included: true })) };
  let html = h.devicesPage();
  for (const protocol of ['SSH', 'HTTP', 'HTTPS']) {
    assert.match(html, new RegExp(`class="state"><span class="dot running"[^>]*></span>${protocol}</span>`));
  }
  assert.doesNotMatch(html, /SSH request|HTTP \/ HTTPS/);
  h.ui.mergedData.sources[0] = { name: 'SSH peer', error: 'Unavailable' };
  html = h.devicesPage();
  assert.match(html, /dot failed"[^>]*><\/span>SSH/);
  h.ui.deviceTrafficFetchedAt = Date.now() - 46000;
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
  h.ui.devices = [old]; h.ui.devicesLoaded = true;
  const view = { traffic: { requests: 99 }, sources: [{ name: 'Old name', included: true }] };
  h.ui.deviceTrafficViews = { '30:model': view }; h.ui.mergedData = view;
  const changed = await h.loadDeviceDefinitions();
  assert.equal(changed,false);
  assert.equal(h.ui.mergedData,view);
  assert.equal(h.ui.deviceTrafficViews['30:model'].traffic.requests,99);
  assert.equal(view.sources[0].name,'New name');
  assert.deepEqual(calls,['get_devices']);
});

test('statistics query events display local aliases, transport and failure/recovery clearly', () => {
  const ui = { devices: [{ id: 'device-id', name: 'MS' }], expanded: new Set() };
  const context = { ui, esc: value => String(value).replace(/</g, '&lt;'), fmtTime: () => '12:00', fmtBytes: String, fmtMs: value => `${value} ms`, statusClass: () => '' };
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
  ui.devices[0].name = '<Renamed>';
  assert.match(context.requestRow(entry, true), /Statistics query · &lt;Renamed>/);
  ui.devices = [];
  assert.match(context.requestRow(entry, true), /Statistics query · Device device-i/);
});


test('first Devices render reuses local Traffic before remote statistics arrive', () => {
  const h = harness(async () => { throw new Error('Cached rendering must not query'); });
  vm.runInContext(source.slice(source.indexOf('function rememberLocalTraffic('), source.indexOf('async function loadHomeTraffic(')), h);
  h.ui.deviceTrafficMinutes = 30; h.ui.trafficScope = 'model';
  h.rememberLocalTraffic({ summary: { requests: 17 }, credentials: [{ credential: 'Known', requests: 17 }] }, 30, 'model');
  h.deviceTrafficContent = (traffic, scope, key) => `${key}:${scope}:${traffic.requests}:${traffic.credentials.length}`;
  let html = h.devicesPage();
  assert.match(html, /local:model:17:1/);
  assert.doesNotMatch(html, /Loading traffic/);
  h.ui.trafficScope = 'all';
  assert.doesNotMatch(h.devicesPage(), /local:model:17:1/);
  h.ui.trafficScope = 'model';
  h.ui.mergedData = { scope: 'model', local: { requests: 18, credentials: [] }, sources: [] };
  assert.match(h.devicesPage(), /local:model:18:0/);
});

test('switching range during a remote refresh waits for it instead of reading the cache', async () => {
  const pending = [];
  const local = [];
  const h = harness((command, args) => {
    if (command === 'get_merged_data') return new Promise(resolve => pending.push({ args, resolve }));
    if (command === 'get_traffic') { local.push(args); return Promise.resolve({ summary: { requests: 7 }, credentials: [], targets: [] }); }
    return Promise.resolve([]);
  });
  h.Channel = class {};
  vm.runInContext(source.slice(source.indexOf('function rememberLocalTraffic('), source.indexOf('async function loadHomeTraffic(')), h);
  vm.runInContext(source.slice(source.indexOf('function selectDeviceTraffic('), source.indexOf('function deviceTrafficContent(')), h);
  h.deviceTrafficContent = (traffic, scope, key) => `${key}:${scope}:${traffic.requests}`;
  h.ui.devices = [{ id: 'ms', name: 'MS', ssh: { host: 'MS' } }];
  h.ui.deviceTrafficMinutes = 30; h.ui.trafficScope = 'model'; h.ui.mergedDataRequest = 0;
  const view = minutes => ({ minutes, scope: 'model', local: { requests: minutes }, traffic: { requests: minutes }, sources: [{ name: 'MS', included: true, traffic: { requests: minutes } }] });
  const refresh = h.loadMergedData(true, true);
  pending[0].args.onUpdate.onmessage([view(30)]);
  const start = source.indexOf('  if (event.target.id === "devices-traffic-scope" || event.target.id === "devices-traffic-range") {');
  const handler = vm.runInContext(`(async event => { ${source.slice(start, source.indexOf('  if (event.target.id === "home-traffic-scope"', start))} })`, h);
  const switching = handler({ target: { id: 'devices-traffic-range', value: '10080' } });
  await Promise.resolve();
  assert.equal(pending.length, 1, 'A cache read must not supersede the remote refresh');
  await switching;
  assert.equal(h.ui.mergedData, null);
  // This device is read locally at once; only the peer waits for the refresh.
  assert.equal(JSON.stringify(local), JSON.stringify([{ minutes: 10080, scope: 'model' }]));
  assert.match(h.devicesPage(), /local:model:7/);
  assert.match(h.devicesPage(), /MS[\s\S]*Loading traffic…/);
  assert.doesNotMatch(h.devicesPage(), /Not refreshed/);
  pending[0].resolve([view(30), view(10080)]);
  await refresh;
  assert.equal(h.ui.mergedData.minutes, 10080);
  assert.match(h.devicesPage(), /device\/ms:model:10080/);
  // Without a remote refresh in flight, a missing range is read from the cache.
  const cacheRead = handler({ target: { id: 'devices-traffic-range', value: '360' } });
  assert.equal(pending.length, 2);
  assert.equal(pending[1].args.refreshRemote, false);
  pending[1].resolve([view(360)]);
  await cacheRead;
  assert.equal(h.ui.mergedData.minutes, 360);
});

function devicesHarness(invoke) {
  const h = harness(invoke);
  h.Channel = class {};
  vm.runInContext(source.slice(source.indexOf('function rememberLocalTraffic('), source.indexOf('async function loadHomeTraffic(')), h);
  vm.runInContext(source.slice(source.indexOf('function selectDeviceTraffic('), source.indexOf('function deviceTrafficContent(')), h);
  vm.runInContext(source.slice(source.indexOf('function mergedDataBlock('), source.indexOf('function deviceConnectionKey(')), h);
  h.trafficControls = () => '';
  h.deviceTrafficContent = (traffic, scope, key) => `${key}:${scope}:${traffic.requests}`;
  h.ui.devices = [{ id: 'ms', name: 'MS', ssh: { host: 'MS' } }];
  h.ui.devicesLoaded = true;
  h.ui.deviceTrafficMinutes = 30; h.ui.trafficScope = 'model'; h.ui.mergedDataRequest = 0;
  return h;
}

test('cache reads keep peer states steady and Refresh stays available', async () => {
  const pending = [];
  const h = devicesHarness((command, args) => command === 'get_merged_data' ? new Promise((resolve, reject) => pending.push({ args, resolve, reject })) : Promise.resolve([]));
  const cached = { minutes: 30, scope: 'model', local: { requests: 1 }, traffic: { requests: 1 }, sources: [{ name: 'MS', included: false, exclusion: 'not_refreshed' }] };
  h.ui.mergedData = cached;
  const read = h.loadMergedData(true, false);
  // A timer's cache read must not flash loading states or block Refresh.
  assert.match(h.devicesPage(), /MS[\s\S]*Not refreshed/);
  assert.doesNotMatch(h.mergedDataBlock(), /merged-refresh" disabled/);
  assert.equal(h.deviceConnectionStatus(h.ui.devices[0]).label, 'Not checked');
  const refreshing = h.action('merged-refresh');
  await Promise.resolve();
  assert.equal(pending.length, 2, 'Refresh supersedes the cache read');
  assert.equal(pending[1].args.refreshRemote, true);
  assert.match(h.devicesPage(), /MS[\s\S]*Loading traffic…/);
  assert.match(h.mergedDataBlock(), /merged-refresh" disabled/);
  assert.equal(h.deviceConnectionStatus(h.ui.devices[0]).label, 'Refreshing statistics…');
  // A refresh that cannot align asks for another Refresh instead of "Updating traffic…".
  pending[1].reject('TRAFFIC_UPDATING');
  await refreshing;
  pending[0].resolve([cached]);
  await read;
  assert.match(h.ui.mergedDataError, /Press Refresh/);
  assert.equal(h.ui.mergedDataUpdating, false);
  assert.ok(!h.ui.deviceTrafficFetchedAt, 'Only a completed refresh counts as fresh');
});

test('waiting for a range keeps peer errors, statuses and queued work intact', async () => {
  const pending = [];
  const calls = [];
  const h = devicesHarness((command, args) => {
    calls.push([command, args]);
    if (command === 'get_merged_data') return new Promise(resolve => pending.push({ args, resolve }));
    if (command === 'get_devices') return Promise.resolve([{ id: 'ms', name: 'MS', ssh: { host: 'MS' } }]);
    if (command === 'get_traffic') return Promise.resolve({ summary: { requests: 3 }, credentials: [], targets: [] });
    return Promise.resolve([]);
  });
  const refresh = h.loadMergedData(true, true);
  pending[0].args.onUpdate.onmessage([{ minutes: 30, scope: 'model', local: { requests: 1 }, traffic: { requests: 1 }, sources: [{ name: 'MS', included: false, error: 'Permission denied' }] }]);
  const start = source.indexOf('  if (event.target.id === "devices-traffic-scope" || event.target.id === "devices-traffic-range") {');
  const handler = vm.runInContext(`(async event => { ${source.slice(start, source.indexOf('  if (event.target.id === "home-traffic-scope"', start))} })`, h);
  await handler({ target: { id: 'devices-traffic-range', value: '10080' } });
  assert.equal(h.ui.mergedData, null);
  assert.match(h.devicesPage(), /MS: Permission denied|Permission denied/);
  assert.match(h.devicesPage(), /Traffic unavailable/);
  assert.equal(h.deviceConnectionStatus(h.ui.devices[0]).state, 'failed');
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
  h.ui.deviceStates = { ms: { host: 'MS', state: 'running', label: 'SSH reachable at startup' } };
  h.ui.deviceCapabilities = { ms: { error: 'Permission denied', capabilities: null } };
  assert.equal(h.deviceConnectionStatus(h.ui.devices[0]).state, 'failed');
  h.ui.deviceCapabilities = { ms: { capabilities: { version: '1' } } };
  h.ui.deviceStates.ms = { host: 'MS', state: 'failed', label: 'Startup SSH check failed' };
  assert.equal(h.deviceConnectionStatus(h.ui.devices[0]).state, 'failed');
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
  const settings = h.loadDevices();
  for (let i = 0; i < 5; i++) await Promise.resolve();
  h.ui.page = 'devices';
  await h.loadDevices();
  assert.equal(calls.filter(([command]) => command === 'get_merged_data').length, 0);
  resolveForwarders([]);
  await settings;
  for (let i = 0; i < 10; i++) await Promise.resolve();
  const reads = calls.filter(([command]) => command === 'get_merged_data');
  assert.equal(reads.length, 1, 'The queued first visit starts its refresh');
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
  h.ui.devices = [{ id: 'duplicate', name: 'Duplicate', ssh: { host: 'peer' } }];
  h.ui.deviceTrafficFetchedAt = Date.now();
  h.ui.mergedData = { sources: [{ name: 'Duplicate', included: false, error: null, exclusion: 'duplicate' }] };
  h.message = (_, text) => text;
  let html = h.devicesPage();
  assert.match(html, /Already counted via another entry/);
  assert.match(html, /Statistics available/);
  assert.doesNotMatch(html.slice(html.indexOf('Duplicate<div')), /dot failed|Traffic unavailable|Loading traffic/);
  html = h.deviceSettings();
  assert.match(html, /Statistics available; already counted/);
  assert.doesNotMatch(html, /dot failed/);
});

test('Forwarding controls and status appear only in Settings', () => {
  const h = harness(async () => []);
  const device = { id: 'ssh', name: 'Remote', ssh: { host: 'host' } };
  h.ui.devices = [device, { id: 'http', name: 'HTTP peer', data: { transport: 'http', url: 'http://host' } }];
  let html = h.forwardingSettings();
  assert.match(html, /^Request forwarding/);
  assert.match(html, /class="switch" role="switch" aria-checked="false"[^>]*data-action="device-forward"[^>]*><\/button>/);
  assert.doesNotMatch(html, /Start forwarding|Stop forwarding|Updating…/);
  assert.doesNotMatch(html, /HTTP peer/);
  assert.doesNotMatch(h.devicesPage(), /data-action="device-forward"|Start forwarding|Stop forwarding/);
  assert.doesNotMatch(h.devicesPage(), /Request forwarding|>Off</);
  assert.doesNotMatch(html, /class="state"|>Off<|>Enabled<|<span class="row-label">Request forwarding/);
  h.ui.deviceForwarders = [{ deviceId: 'ssh', port: 23456, error: '<connection lost>' }];
  html = h.forwardingSettings();
  assert.match(html, /class="switch" role="switch" aria-checked="true"[^>]*><\/button>/);
  assert.doesNotMatch(html, /23456|Local endpoint/);
  assert.match(html, /&lt;connection lost>/);
  assert.doesNotMatch(html, /class="state"|>Off<|>Enabled<|Connection error/);
  assert.doesNotMatch(h.devicesPage(), /Request forwarding|data-action="device-forward"|23456|connection lost|Connection error|>Enabled</);
  h.ui.deviceForwardingBusy = 'ssh';
  assert.match(h.forwardingSettings(), /aria-checked="true" aria-busy="true"[^>]*disabled><\/button>/);
  h.ui.devices = [];
  assert.match(h.forwardingSettings(), /Add an SSH device above/);
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
  assert.equal(h.ui.deviceForwarders[0].port, 23456);
  assert.equal(h.ui.deviceForwardingBusy, null);
});


test('home highlights remote mode and collapses inactive local configuration', () => {
  const start = source.indexOf('function main()');
  const end = source.indexOf('function message(', start);
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
  assert.match(html, /<details[^>]*><summary>Local proxy configuration and history \(inactive\)<\/summary>\[Proxies\]/);
  assert.doesNotMatch(html, /<details[^>]*open/);
  context.ui.snap.forwarding = null;
  assert.equal(context.main(), '[Proxy][Traffic][Connect][Recent][Proxies][Routing]');
});

test('entering Settings through navigation reads definitions without remote statistics', async () => {
  const calls = [];
  const h = harness(async command => { calls.push(command); return []; });
  h.fields.content = { scrollTop: 100 };
  h.render = () => {};
  h.loadMergedData = () => calls.push('remote-statistics');
  await h.action('page', { dataset: { page: 'settings' } });
  await Promise.resolve();
  assert.equal(h.ui.page, 'settings');
  assert.deepEqual(calls, ['get_devices', 'get_device_forwarders', 'get_device_capabilities']);
});

test('leaving Devices while definitions load prevents starting remote statistics', async () => {
  let resolve;
  const calls = [];
  const h = harness(command => command === 'get_devices' ? new Promise(r => { resolve = r; }) : Promise.resolve([]));
  h.loadMergedData = () => calls.push('remote-statistics');
  const loading = h.loadDevices();
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
  h.loadMergedData = (force, refreshRemote) => { calls.push(['statistics', refreshRemote]); };
  const loading = h.loadDevices();
  for (let i = 0; i < 5; i++) await Promise.resolve();
  // The first visit after start also refreshes remote statistics, without waiting for local reads.
  assert.deepEqual(calls, ['get_devices', 'get_device_forwarders', 'get_device_capabilities', ['statistics', true]]);
  for (const resolve of pending) resolve([]);
  await loading;
  assert.deepEqual(h.ui.deviceForwarders, []);
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
  await h.checkConfiguredSsh();
  await h.checkConfiguredSsh();
  assert.deepEqual(calls, [['get_devices', undefined], ['check_ssh_device', 'up'], ['check_ssh_device', 'down']]);
  assert.equal(h.deviceConnectionStatus(devices[0]).label, 'SSH reachable at startup');
  assert.equal(h.deviceConnectionStatus(devices[1]).state, 'failed');
  assert.match(h.deviceConnectionStatus(devices[1]).label, /unavailable/);
  assert.equal(h.ui.deviceStates.http, undefined);
  devices[0].ssh.host = 'changed';
  assert.equal(h.deviceConnectionStatus(devices[0]).label, 'Not checked');
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
  const started = h.checkConfiguredSsh();
  for (let i = 0; i < 20 && checked < devices.length; i++) {
    await Promise.resolve(); await Promise.resolve();
    const batch = pending.splice(0); batch.forEach(resolve => resolve());
  }
  pending.splice(0).forEach(resolve => resolve());
  await started;
  assert.equal(checked, 7);
  assert.equal(maximum, 4);
});

test('remote home distinguishes health, recovery and whole-device statistics', () => {
  const start = source.indexOf('function main()');
  const end = source.indexOf('function message(', start);
  const context = { ui: { snap: { phase: { port: 8787 }, forwarding: { name: 'MS', health: { state: 'connected', latencyMs: 52, lastConnectedAt: Date.now(), recoveredAt: Date.now() }, traffic: { connections: 3, active: 1, failures: 1, uploadBytes: 20, downloadBytes: 40, downloadBytesPerSecond: 8 }, remoteTraffic: { requests: 10, errors: 2, fetchedAt: Date.now() } } } }, esc: v => v, fmtBytes: v => `${v} B`, block: (title, _, body) => title + body, message: (_, text) => text };
  vm.createContext(context); vm.runInContext(source.slice(start,end),context);
  let html = context.remoteForwardingBlock();
  assert.match(html,/Connected/); assert.match(html,/52 ms/); assert.match(html,/Connection restored/);
  assert.match(html,/3 \/ 1 \/ 1/); assert.match(html,/8 B\/s/);
  assert.match(html,/10 \/ 20.0%/); assert.match(html,/Includes other clients on MS/);
  context.ui.snap.forwarding.health.state='disconnected'; context.ui.snap.forwarding.error='offline';
  context.ui.snap.forwarding.remoteTrafficError='statistics offline';
  html=context.remoteForwardingBlock();
  assert.match(html,/Disconnected · retrying/); assert.match(html,/never fall back/); assert.match(html,/stale/);
  assert.doesNotMatch(html,/Connection restored/);
});

test('restore switch changes only after daemon confirms persistence', async () => {
  const calls=[];
  const h=harness(async (command,args)=>{calls.push([command,args]);});
  h.ui.snap.forwardingRestoreSupported=true;
  assert.match(h.forwardingSettings(),/data-action="forwarding-restore"/);
  await h.action('forwarding-restore');
  assert.equal(calls[0][0],'set_forwarding_restore'); assert.equal(calls[0][1].enabled,true);
  assert.equal(h.ui.snap.forwardingRestore,true);
  h.invoke=async()=>{throw new Error('cannot save')};
  await h.action('forwarding-restore');
  assert.equal(h.ui.snap.forwardingRestore,true); assert.equal(h.ui.forwardingRestoreBusy,false);
});

test('cached daemon capabilities disable unsupported switches without a check button', async () => {
  const results = [{ deviceId:'ms', pending:false, capabilities:{version:'0.1.0',runningVersion:'0.1.0',statistics:true,forwarding:false,running:true,forwardingAvailable:false}, error:null }];
  const calls=[];
  const h=harness(async command=>{calls.push(command);return results;});
  const device={id:'ms',name:'MS',ssh:{host:'MS',binary:''}};h.ui.devices=[device];
  await h.loadDeviceCapabilities();
  assert.deepEqual(calls,['get_device_capabilities']);
  assert.match(h.deviceCapabilities(device),/Coport 0.1.0/);
  assert.doesNotMatch(h.deviceSettings(),/Check version|device-capabilities|<br>|Up to 32|SSH also supports/);
  assert.match(h.forwardingSettings(),/data-action="device-forward"[^>]*disabled/);
  results[0].capabilities.forwarding=true;results[0].capabilities.forwardingAvailable=true;
  await h.loadDeviceCapabilities();
  assert.doesNotMatch(h.forwardingSettings(),/data-action="device-forward"[^>]*disabled/);
  results[0].capabilities=null;results[0].error='old daemon has no capabilities command';
  await h.loadDeviceCapabilities();
  assert.match(h.deviceCapabilities(device),/data-tip="old daemon/);
  assert.match(h.deviceCapabilities(device),/>Version unavailable<\/span>/);
  assert.match(h.forwardingSettings(),/data-action="device-forward"[^>]*disabled/);
  h.ui.deviceForwarders=[{deviceId:'ms',name:'MS'}];
  assert.doesNotMatch(h.forwardingSettings(),/data-action="device-forward"[^>]*disabled/);
});


test('forwarding settings keep mechanisms in documentation instead of repeated help text', () => {
  const h = harness(async () => []);
  h.ui.snap.forwardingRestoreSupported = true;
  h.ui.devices = [{ id: 'ms', name: 'MS', ssh: { host: 'MS' } }];
  const html = h.forwardingSettings();
  assert.match(html, /Restore on startup/);
  assert.doesNotMatch(html, /placeholder|Clients keep|Switching disconnects|Only one remote|If the remote is offline|daemon runs/);
});

test('selected device traffic renders before completion and ignores superseded channel updates', async () => {
  const pending = [];
  let renders = 0;
  const context = { Channel: class {}, ui: { page: 'devices', devices: [], deviceTrafficMinutes: 1440, trafficScope: 'all', mergedDataRequest: 0 },
    invoke: (command, args) => new Promise((resolve, reject) => pending.push({ command, args, resolve, reject })),
    render() { renders++; } };
  vm.createContext(context);
  const a = source.indexOf('function relabelDeviceViews(');
  vm.runInContext(source.slice(a, source.indexOf('async function loadDeviceDefinitions', a)), context);
  const b = source.indexOf('function selectDeviceTraffic(');
  vm.runInContext(source.slice(b, source.indexOf('function deviceTrafficContent(', b)), context);
  const first = context.loadMergedData();
  assert.equal(pending[0].args.minutes, 1440);
  assert.equal(pending[0].args.scope, 'all');
  const initial = { minutes: 1440, scope: 'all', traffic: { requests: 7 }, sources: [] };
  pending[0].args.onUpdate.onmessage([initial]);
  assert.equal(context.ui.mergedData, initial);
  assert.equal(context.ui.mergedDataLoading, true);
  assert.equal(renders, 1);

  context.ui.deviceTrafficMinutes = 360;
  const second = context.loadMergedData(true);
  const next = { minutes: 360, scope: 'all', traffic: { requests: 9 }, sources: [] };
  pending[1].args.onUpdate.onmessage([next]);
  pending[0].args.onUpdate.onmessage([{ ...next, traffic: { requests: 99 } }]);
  assert.equal(context.ui.mergedData, next);
  pending[0].resolve([initial]); await first;
  assert.equal(context.ui.mergedDataLoading, true);
  pending[1].resolve([initial, next]); await second;
  assert.equal(context.ui.mergedDataLoading, false);
  assert.equal(Object.keys(context.ui.deviceTrafficViews).length, 2);
  pending[1].args.onUpdate.onmessage([{ ...next, traffic: { requests: 100 } }]);
  assert.equal(context.ui.mergedData, next);
});

test('a background failure retains the selected traffic already rendered', async () => {
  let update, reject;
  const context = { Channel: class {}, ui: { page: 'devices', devices: [], deviceTrafficMinutes: 30, trafficScope: 'model', mergedDataRequest: 0 },
    invoke: (_, args) => { update = args.onUpdate; return new Promise((_, fail) => { reject = fail; }); },
    render() {} };
  vm.createContext(context);
  const a = source.indexOf('function relabelDeviceViews(');
  vm.runInContext(source.slice(a, source.indexOf('async function loadDeviceDefinitions', a)), context);
  const b = source.indexOf('function selectDeviceTraffic(');
  vm.runInContext(source.slice(b, source.indexOf('function deviceTrafficContent(', b)), context);
  const loading = context.loadMergedData();
  const view = { minutes: 30, scope: 'model', traffic: { requests: 5 }, sources: [] };
  update.onmessage([view]);
  reject('Background statistics failed');
  await loading;
  assert.equal(context.ui.mergedData, view);
  assert.equal(context.ui.mergedDataLoading, false);
  assert.equal(context.ui.mergedDataError, 'Background statistics failed');
});

test('only the first Devices visit after start refreshes remote statistics automatically', async () => {
  const calls=[];
  const h=harness(async(command,args)=>{calls.push([command,args]);return command==='get_devices' ? [{id:'ms',name:'MS',ssh:{host:'MS'}}] : [];});
  h.ui.mergedDataRequest=0;h.ui.deviceTrafficMinutes=30;h.ui.trafficScope='model';h.selectDeviceTraffic=()=>false;h.Channel=class {};
  vm.runInContext(source.slice(source.indexOf('async function loadMergedData('),source.indexOf('function deviceTrafficContent(')),h);
  for(let i=0;i<4;i++) {h.ui.page='devices';await h.loadDevices();h.ui.page='settings';await h.loadDevices();}
  const reads=calls.filter(([command])=>command==='get_merged_data');
  assert.equal(reads.length,4);
  assert.deepEqual(reads.map(([,args])=>args.refreshRemote),[true,false,false,false]);
  assert.equal(h.ui.mergedDataError,'');
  await h.action('merged-refresh');
  assert.equal(calls.filter(([command,args])=>command==='get_merged_data' && args.refreshRemote===true).length,2);
  h.ui.page='devices';await h.loadDevices();
  assert.equal(calls.filter(([command,args])=>command==='get_merged_data' && args.refreshRemote===true).length,2);
});


test('cached statistics retain success without hiding a later failure', () => {
  const h=harness(async()=>[]);const device={id:'peer',name:'Peer',ssh:{host:'test'}};
  h.ui.deviceStates={peer:{host:'test',state:'running',label:'SSH reachable at startup'}};
  h.ui.deviceTrafficFetchedAt=Date.now()-60000;
  h.ui.mergedData={sources:[{name:'Peer',error:'connection refused'}]};
  assert.equal(h.deviceConnectionStatus(device).state,'failed');
  h.ui.mergedData.sources[0]={name:'Peer',included:true,error:null};
  assert.equal(h.deviceConnectionStatus(device).state,'running');
  assert.match(h.deviceConnectionStatus(device).label,/cached/);
  h.ui.mergedData.sources[0]={name:'Peer',error:'new failure'};
  assert.equal(h.deviceConnectionStatus(device).label,'new failure');
});

test('saving a device merges only its list after asynchronous forwarding work', () => {
  const commands=fs.readFileSync(require('node:path').join(__dirname,'../src/commands.rs'),'utf8');
  const save=commands.slice(commands.indexOf('pub async fn save_device('),commands.indexOf('pub async fn remove_device('));
  const before=save.slice(0,save.indexOf('stop_device_forwarding(&id).await?'));
  const after=save.slice(save.indexOf('stop_device_forwarding(&id).await?'));
  assert.doesNotMatch(before,/settings\.clone\(\)/);
  assert.match(before,/managed_devices\.clone\(\)/);
  assert.match(after,/core\.settings\.clone\(\)/);
  assert.match(after,/settings\.managed_devices = devices/);
});


test('Devices Refresh performs only one explicitly authorized statistics read', async () => {
  const calls = [];
  const h = harness(async (command, args) => {
    calls.push([command, args]);
    assert.ok(['get_devices', 'get_device_forwarders', 'get_device_capabilities', 'get_merged_data'].includes(command), `Unexpected networking command: ${command}`);
    return [];
  });
  h.Channel = class {};
  h.ui.mergedDataRequest = 0;
  h.ui.deviceTrafficMinutes = 30;
  h.ui.trafficScope = 'model';
  h.selectDeviceTraffic = () => false;
  vm.runInContext(source.slice(source.indexOf('async function loadMergedData('), source.indexOf('function deviceTrafficContent(')), h);
  h.ui.deviceStartupRefreshDone = true;
  await h.action('merged-refresh');
  const reads = calls.filter(([command]) => command === 'get_merged_data');
  assert.equal(reads.length, 1, 'Refresh must not wait for a redundant local full-history merge');
  assert.equal(reads[0][1].refreshRemote, true);
  assert.equal(h.ui.mergedDataError, '');
  calls.length = 0;
  for (let i = 0; i < 3; i++) await h.loadDevices();
  assert.ok(calls.filter(([command]) => command === 'get_merged_data').every(([,args]) => args.refreshRemote === false));
});


test('Settings status uses successful cached SSH capabilities without connecting', () => {
  const h = harness(() => { throw new Error('Status rendering must not invoke IPC'); });
  const device = { id: 'peer', name: 'Peer', ssh: { host: 'test' } };
  h.ui.mergedData = { sources: [{ name: 'Peer', exclusion: 'not_refreshed' }] };
  h.ui.deviceCapabilities = { peer: { pending: false, capabilities: { running: true, forwarding: true } } };
  assert.equal(h.deviceConnectionStatus(device).state, 'running');
  assert.match(h.deviceConnectionStatus(device).label, /cached/);
  h.ui.deviceCapabilities = {};
  assert.equal(h.deviceConnectionStatus(device).state, '');
});
