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
  assert.deepEqual(calls, ['get_devices', 'get_device_forwarders']);
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
  const context = { ui: { page: 'devices', deviceTrafficMinutes: 30, trafficScope: 'model', mergedDataRequest: 0 }, invoke: (command, args) => new Promise(resolve => pending.push({ command, args, resolve })), render() {} };
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
  const context = { ui: { page: 'devices', deviceTrafficMinutes: 30, trafficScope: 'model', mergedDataRequest: 0, mergedData: { selected: 'last-good' } }, invoke: async () => { throw 'TRAFFIC_UPDATING'; }, render() {} };
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
  Object.assign(h, { chart: () => '<chart>', trafficShare: () => '<ring>', modelTokenStats: () => '', stat: () => '', fmtMs: () => '', fmtBytes: () => '', serviceMark: () => '', ICON: { chevron: '' }, document: { addEventListener: (name, callback) => { events[name] = callback; } } });
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
  const context = { ui: { page: 'devices', deviceTrafficMinutes: 30, trafficScope: 'model', mergedDataRequest: 0 }, invoke: async () => { calls++; return views; }, render() {} };
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

test('Settings devices use add/remove icons and show green only for recent verified data', () => {
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
  html = h.deviceSettings(); assert.doesNotMatch(html, /dot running/);
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
  assert.doesNotMatch(html, /dot running"[^>]*><\/span>(SSH|HTTP|HTTPS)/);
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
    ICON: { chevron: '' }, esc: v => v, block: (title, aside, body) => title + body,
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
  assert.deepEqual(calls, ['get_devices', 'get_device_forwarders']);
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

test('Settings timer never polls remote devices', () => {
  const calls = [];
  let tick;
  vm.runInNewContext(source.slice(source.indexOf('// Refresh even when no new requests arrive')), {
    ui: { page: 'settings', devices: [{ id: 'remote' }] }, document: { hidden: false },
    setInterval: callback => { tick = callback; },
    refresh: () => calls.push('refresh'), loadDevices: () => calls.push('devices'),
    loadMergedData: () => calls.push('remote-statistics'),
  });
  for (let i = 0; i < 5; i++) tick();
  assert.deepEqual(calls, []);
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
  assert.equal(h.deviceConnectionStatus(devices[0]).label, 'Not checked recently');
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
