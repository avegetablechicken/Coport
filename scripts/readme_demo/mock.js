// Stands in for the Tauri bridge so the real panel UI renders synthetic data
// for the README demos. Every account, address, device and number here is made up.
//
// Scenes (`?scene=`): `home` stays on Home while calls arrive; `tour` clicks
// through Home, Activity and Devices and returns Home.
"use strict";

(() => {
  const SCENE = new URLSearchParams(location.search).get("scene") ?? "home";

  // A fixed clock the recorder advances frame by frame.
  const RealDate = Date;
  const START = new RealDate(2026, 9, 14, 14, 31, 42).getTime();
  let now = START;
  class FakeDate extends RealDate {
    constructor(...args) {
      super(...(args.length ? args : [now]));
    }
    static now() {
      return now;
    }
  }
  window.Date = FakeDate;

  const MINUTE = 60_000;
  const BUCKETS = 30;

  // Deterministic noise, so every recording shows the same history.
  const rng = (seed) => () => {
    seed = (seed * 16807) % 2147483647;
    return seed / 2147483647;
  };

  const CODEX = { service: "Codex", path: "/backend-api/codex/responses" };
  const CLAUDE = { service: "Claude", path: "/v1/messages" };
  const ACCOUNTS = [
    { ...CODEX, credential: "work@example.com", proxy: "us", weight: 0.42 },
    { ...CODEX, credential: "personal@example.com", proxy: "jp", weight: 0.18 },
    { ...CLAUDE, credential: "you@example.com", proxy: "claude_official", weight: 0.3 },
    { ...CLAUDE, credential: "api", proxy: "none", weight: 0.1 },
  ];

  // Calls a device logged: the history before the recording plus live arrivals.
  function device(seed, accounts, busyness) {
    const random = rng(seed);
    const calls = [];
    let seq = 1000;
    const pick = () => {
      let r = random() * accounts.reduce((n, a) => n + a.weight, 0);
      for (const account of accounts) {
        if ((r -= account.weight) <= 0) return account;
      }
      return accounts[0];
    };
    const call = (time, live = false) => {
      const error = !live && random() < 0.03;
      const input = Math.round(18_000 + random() * 90_000);
      calls.push({
        seq: seq++, time, account: pick(), error,
        status: error ? 529 : 200,
        ms: Math.round(2_400 + random() * 14_000),
        bytes: Math.round(8_000 + random() * 60_000),
        input, output: Math.round(400 + random() * 4_000), cached: Math.round(input * (0.55 + random() * 0.35)),
      });
    };
    for (let minute = BUCKETS; minute >= 1; minute -= 1) {
      const busy = Math.round(busyness * (2 + 4 * (1 + Math.sin(minute / 4)) * random()));
      for (let i = 0; i < busy; i += 1) call(START - minute * MINUTE + random() * MINUTE);
    }
    calls.sort((a, b) => a.time - b.time);
    return { calls, call };
  }

  const local = device(7, ACCOUNTS, 1);
  const DEVICES = [
    {
      id: "6f1c2a90-0d1e-4c55-9a43-3d2b7f0e8a11", name: "mbp16", host: "mbp16",
      traffic: device(11, [ACCOUNTS[0], ACCOUNTS[2]], 0.8),
    },
    {
      id: "b3e8d4c7-52a9-4f0b-8e61-7c4d9a2f5b38", name: "studio", host: "studio",
      traffic: device(23, [ACCOUNTS[1], { ...CODEX, credential: "Unidentified", proxy: "none", weight: 0.3 }], 0.5),
    },
  ];

  // New calls arrive during the recording, in milliseconds from its start.
  const ARRIVALS = SCENE === "tour" ? [600, 2600, 5400, 6300, 9800, 12400] : [600, 1900, 2700, 4300, 5200, 6800, 8100, 9000];
  const arrived = new Set();

  const totals = (list) => {
    const sum = (key) => list.reduce((n, c) => n + c[key], 0);
    const input = sum("input");
    const cached = sum("cached");
    return {
      requests: list.length,
      errors: list.filter((c) => c.error).length,
      bytes: sum("bytes"),
      inputTokens: input,
      outputTokens: sum("output"),
      cachedInputTokens: cached,
      uncachedInputTokens: null,
      cacheWriteTokens: null,
      cacheHitRate: input ? cached / input : null,
      avgMs: list.length ? Math.round(sum("ms") / list.length) : null,
      latencyTotalMs: sum("ms"),
      latencySamples: list.length,
      cacheRead: cached,
      cachePrompt: input,
      sources: [],
    };
  };

  const window30 = () => {
    const end = Math.floor(now / MINUTE) * MINUTE + MINUTE;
    return { start: end - BUCKETS * MINUTE, end };
  };

  // A traffic view as the daemon reports it, for any device's calls.
  function view(calls) {
    const { start, end } = window30();
    const buckets = (list) => {
      const counts = Array(BUCKETS).fill(0);
      const errorCounts = Array(BUCKETS).fill(0);
      const tokenCounts = Array(BUCKETS).fill(0);
      for (const c of list) {
        const i = Math.floor((c.time - start) / MINUTE);
        if (i < 0 || i >= BUCKETS) continue;
        counts[i] += 1;
        errorCounts[i] += c.error ? 1 : 0;
        tokenCounts[i] += c.input + c.output;
      }
      return { counts, errorCounts, tokenCounts };
    };
    const inRange = calls.filter((c) => c.time >= start && c.time < end);
    const accounts = [...new Set(inRange.map((c) => c.account))];
    const credentials = accounts.map((account) => {
      const own = inRange.filter((c) => c.account === account);
      return { service: account.service, credential: account.credential, ...totals(own), ...buckets(own) };
    }).sort((a, b) => (a.credential === "Unidentified") - (b.credential === "Unidentified") || b.bytes - a.bytes);
    return { start, end, credentials, summary: { service: "", credential: "", ...totals(inRange), ...buckets(inRange) } };
  }

  function traffic() {
    const { start, end, credentials, summary } = view(local.calls);
    return { scope: "model", start, end, bucketMinutes: 1, credentials, targets: [], summary };
  }

  function mergedData() {
    const { start, end } = window30();
    return [{
      windowStart: start, windowEnd: end, minutes: 30, scope: "model", bucketMinutes: 1,
      sources: DEVICES.map((d) => {
        const { credentials, summary } = view(d.traffic.calls);
        return { name: d.name, included: true, exclusion: null, error: null, traffic: { ...summary, credentials, targets: [] } };
      }),
    }];
  }

  // Home shows the latest five requests; the Activity log shows a page of them.
  function activity({ range }) {
    const rows = local.calls.filter((c) => c.time <= now).slice(range ? -40 : -12).reverse().map((c) => ({
      seq: c.seq, time: c.time, event: "request_finished", status: c.status, method: "POST",
      path: c.account.path, service: c.account.service, proxy: c.account.proxy,
      durationMs: c.ms, bytes: c.bytes, error: c.error, reason: null, fields: {},
    }));
    return { rows, next: null };
  }

  // Proxies are being tested when the recording starts.
  let probed = false;
  const PROXIES = [
    { name: "us", endpoint: "http://127.0.0.1:8101", local: true, result: { state: "ok", ms: 182, exitIp: "203.0.113.24", country: "US" } },
    { name: "jp", endpoint: "socks5://127.0.0.1:8102", local: true, result: { state: "ok", ms: 96, exitIp: "198.51.100.7", country: "JP" } },
    { name: "claude_official", endpoint: "http://127.0.0.1:7893", local: true, result: { state: "ok", ms: 141, exitIp: "192.0.2.51", country: "SG" } },
  ];

  const route = (selector, kind, proxies) => ({ activation: "active", selector, kind, proxies, detail: null });
  const fallback = (key, proxies = null) => ({ key, proxies });

  function state() {
    return {
      forwarding: null,
      forwardingRestore: false,
      forwardingRestoreSupported: true,
      forwardingRestoreError: null,
      accountStatesError: null,
      version: "0.1.0",
      phase: { busy: false, state: "running", port: 8787, uptimeSecs: 8054 + Math.floor((now - START) / 1000), error: null },
      stats: { requests: 0, errors: 0, bytes: 0, avgMs: null, perMinute: [], errorsPerMinute: [] },
      routes: [
        { name: "codex", ok: true, reason: null, checkedAt: Math.floor(START / 1000) },
        { name: "claude", ok: true, reason: null, checkedAt: Math.floor(START / 1000) },
      ],
      credentialError: null,
      urls: {
        base: "http://127.0.0.1:8787",
        claude: "http://127.0.0.1:8787/anthropic",
        codex: "http://127.0.0.1:8787/v1",
        chatgpt: "https://127.0.0.1:8787/backend-api",
        caCertificate: "~/Library/Application Support/io.github.coport.gui/tls/ca.pem",
        proxyCaCertificate: "~/Library/Application Support/io.github.coport.gui/tls/proxy-ca.pem",
      },
      config: {
        exists: true,
        error: null,
        changedSinceStart: false,
        details: {
          listenPort: 8787,
          timeoutSecs: 300,
          editable: { connect_credential_routing: false },
          proxies: PROXIES.map(({ name, endpoint, local, result }) => ({
            name, endpoint, local, routeFailures: [],
            probe: probed ? { error: null, ...result } : { state: "pending", ms: null, error: null, exitIp: null, country: null },
          })),
          codex: {
            configured: true, upstreams: [], fileOnly: true,
            accountRoutes: [route("work@example.com", "account", ["us", "jp"]), route("personal@example.com", "account", ["jp"])],
            apiKeyRoutes: [],
            fallbacks: [fallback("accountFallback"), fallback("apiKeyFallback"), fallback("mcpFallback", ["none"])],
          },
          claude: {
            configured: true, upstreams: [], fileOnly: true,
            accountRoutes: [route("you@example.com", "account", ["claude_official"])],
            apiKeyRoutes: [route("api", "profile", ["none"])],
            fallbacks: [fallback("accountFallback"), fallback("apiKeyFallback"), fallback("accountProbe", ["claude_official"])],
          },
        },
      },
      settings: {
        deviceCount: DEVICES.length, appearance: "System", startProxyOnLaunch: true, keepProxyRunningOnQuit: true,
        launchAtLogin: true, logBytes: 3_481_202, trafficCompatibility: { exists: false, choices: 0, error: null },
      },
      proxyCa: null,
    };
  }

  const COMMANDS = {
    get_state: state,
    get_activity: activity,
    get_traffic: traffic,
    get_merged_data: mergedData,
    get_devices: () => DEVICES.map(({ id, name, host }) => ({ id, name, ssh: { host, binary: null }, data: null })),
    get_device_forwarders: () => [],
    get_device_capabilities: () => DEVICES.map(({ id, host }) => ({
      deviceId: id, host, binary: null, checkedAt: now, error: null,
      capabilities: { version: "0.1.0", statistics: true, forwarding: true, running: true, forwardingAvailable: true },
    })),
  };

  window.__TAURI__ = {
    core: {
      invoke: async (command, args = {}) => (COMMANDS[command] ? COMMANDS[command](args) : null),
      Channel: class {},
    },
    event: { listen: async () => () => {} },
  };

  // ------------------------------------------------------------- tour

  // When the tour switches pages, as the header links and Back would.
  const CLICKS = [
    { at: 2000, click: '#top [data-page="activity"]' },
    { at: 7000, click: "#top .back" },
    { at: 8200, click: '#top [data-page="devices"]' },
    { at: 12500, click: "#top .back" },
  ];
  // Activity cuts from its Traffic to the Log; a smooth scroll would redraw
  // every pixel in each frame and multiply the GIF's size.
  const LOG_AT = 4500;
  const clicked = new Set();

  // Clicks happen in order, even when a preview jumps past some of them.
  async function tour(elapsed) {
    for (const [i, step] of CLICKS.entries()) {
      if (step.at > elapsed || clicked.has(i)) continue;
      clicked.add(i);
      const el = document.querySelector(step.click);
      if (el) await act("page", el);
    }
  }

  // Moves the clock to `elapsed` milliseconds into the recording and renders
  // the panel as the app would after the calls that arrived by then.
  window.__demo = {
    async at(elapsed) {
      now = START + elapsed;
      probed = elapsed >= 1200 || SCENE === "tour";
      ARRIVALS.forEach((at, i) => {
        if (elapsed >= at && !arrived.has(i)) {
          arrived.add(i);
          local.call(START + at, true);
        }
      });
      if (SCENE === "tour") await tour(elapsed);
      resetTrafficViews();
      if (ui.page === "activity") {
        await Promise.all([loadTraffic(), loadActivity("live")]);
        const content = document.getElementById("content");
        content.scrollTop = elapsed >= LOG_AT ? content.scrollTop + (logOffset() ?? 0) : 0;
      } else if (ui.page === "devices") {
        await refresh();
        await loadDeviceCards();
      } else {
        await refresh();
        await loadHomeTraffic(true);
      }
      await new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve)));
    },
  };
})();
