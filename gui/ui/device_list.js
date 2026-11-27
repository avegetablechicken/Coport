"use strict";

/// The Devices and what is known about each: the Device list, each SSH
/// Device's check at startup, the daemon's capability checks, the
/// forwarders, and the shared traffic views (Merged Traffic and each
/// device's) of every range from the last read of the peers. `page()` is the
/// panel's page, `minutes()` the shared range and `snap()` the last state;
/// `render()` redraws the panel and `toast(message)` reports an error.
function createDeviceList({ invoke, Channel, trafficViews, page, minutes, snap, render, toast, now = () => Date.now() }) {
  const list = {
    devices: [],
    /// Whether `devices` was read from the app yet.
    loaded: false,
    /// Each SSH Device's check at startup, by ID.
    startup: Object.create(null),
    sshStarted: false,
    /// The daemon's capability checks, by Device ID.
    capabilities: null,
    capabilitiesLoading: false,
    capabilitiesError: "",
    forwarders: null,
    /// Merged Traffic and every device's traffic, by `minutes:scope`.
    views: Object.create(null),
    /// The views of the shared range and category, and the last ones shown.
    merged: null,
    latest: null,
    loading: false,
    /// Whether the read in flight reads the peers again.
    remote: false,
    request: 0,
    error: "",
    updating: false,
    /// When the peers were last read, and last attempted.
    fetchedAt: 0,
    remoteAttemptAt: 0,
    recoveryPending: false,
    startupRefreshDone: false,
    refreshing: false,
    refreshQueued: false,
  };

  /// A Devices frame completed with this device's traffic, read within the
  /// Devices cadence; a Devices visit reads it again only after that.
  function frameFresh(frame) {
    return !!(frame?.local || frame?.localError) && now() - (frame.loadedAt || 0) < DEVICE_FRAME_MS;
  }
  const DEVICE_FRAME_MS = 30000;

  function select() {
    const cached = list.views?.[`${minutes()}:${trafficViews.scope}`];
    if (!cached) return false;
    list.merged = cached;
    return true;
  }
  /// A refresh first publishes every peer as pending. Such a frame would drop
  /// peers the shown view already has, so that view stays until they answer.
  function dropsShownPeers(view, shown) {
    return !!shown?.sources?.some(old => old.traffic
      && view.sources?.find(source => source.name === old.name)?.error === "TRAFFIC_UPDATING");
  }
  async function loadMerged(force = false, refreshRemote = false) {
    if (list.loading && !force) return;
    const request = ++list.request;
    list.loading = true;
    list.remote = refreshRemote;
    if (refreshRemote) {
      list.recoveryPending = false;
      // Failed attempts count too, so an unreachable peer is not retried sooner.
      list.remoteAttemptAt = now();
    }
    let completed = false, completeSelected = false;
    const requestedDevices = list.devices ? list.devices.map(d => ({ id: d.id, name: d.name })) : [];
    const requestedKey = `${minutes() || 30}:${trafficViews.scope || "model"}`;
    try {
      const onUpdate = new Channel();
      onUpdate.onmessage = data => {
        if (completed || request !== list.request) return;
        relabel(data, requestedDevices, list.devices || []);
        list.views ||= Object.create(null);
        for (const view of data) {
          view.loadedAt = now();
          const key = `${view.minutes}:${view.scope}`;
          if (!dropsShownPeers(view, list.views[key])) list.views[key] = view;
        }
        list.select();
        // Statuses and errors stay visible while another range waits for peers.
        list.latest = list.merged || data[0];
        list.error = ""; list.updating = false;
        if (page() === "devices" || page() === "settings") render();
      };
      const data = await invoke("get_merged_data", {
        minutes: minutes() || 30, scope: trafficViews.scope || "model", onUpdate, refreshRemote,
      });
      if (request !== list.request) return;
      if (Array.isArray(data)) {
        for (const view of data) view.loadedAt = now();
        relabel(data, requestedDevices, list.devices || []);
        list.views = Object.fromEntries(data.map(view => [`${view.minutes}:${view.scope}`, view]));
        list.select();
      } else { list.merged = data; }
      list.latest = list.merged || list.views?.["30:model"] || list.latest;
      if (refreshRemote) list.fetchedAt = now();
      list.error = ""; list.updating = false;
      // A range chosen during a refresh gets its peers from it, and this device from a read now.
      const key = `${minutes() || 30}:${trafficViews.scope || "model"}`;
      completeSelected = refreshRemote && key !== requestedKey && !list.views?.[key]?.local;
    } catch (e) {
      if (request === list.request) {
        // A cache read retries on its own; a refresh needs the user to press Refresh again.
        const skew = String(e).includes("TRAFFIC_UPDATING");
        list.updating = skew && !refreshRemote;
        list.error = skew && refreshRemote ? "Devices reported different time boundaries. Press Refresh to try again." : list.updating ? "" : String(e);
      }
    }
    finally {
      completed = true;
      if (request === list.request) {
        list.loading = list.remote = false;
        if (page() === "devices" || page() === "settings") render();
        if (list.recoveryPending) list.refreshRecovered();
        if (completeSelected) list.loadMerged(true);
      }
    }
  }

  function connectionKey(devices) {
    return JSON.stringify(devices.map(d => {
      const transport = d.data?.transport || (d.ssh ? "ssh" : "http");
      return [d.id, transport, transport === "ssh"
        ? [d.ssh?.host, d.ssh?.binary === "coportd" ? "" : d.ssh?.binary || ""]
        : [d.data?.url, d.data?.tokenFile, d.data?.tokenEnv, d.data?.caCertificate]];
    }).sort((a,b) => String(a[0]).localeCompare(String(b[0]))));
  }
  function relabel(views, previous, current) {
    const renamed = new Map(previous.map(d => [d.name, current.find(c => c.id === d.id)?.name || d.name]));
    for (const view of new Set(views)) {
      for (const source of view?.sources || []) source.name = renamed.get(source.name) || source.name;
    }
  }
  async function loadDefinitions() {
    let changed = false;
    try {
      const devices = await invoke("get_devices");
      changed = list.loaded && connectionKey(devices) !== connectionKey(list.devices);
      if (changed) {
        ++list.request; list.loading = false;
        list.views = Object.create(null); list.merged = list.latest = null;
      } else {
        relabel([...Object.values(list.views || {}), list.merged], list.devices, devices);
      }
      list.devices = devices; list.loaded = true;
    }
    catch (error) { toast(String(error)); }
    if (page() === "settings") render();
    return changed;
  }
  async function checkSsh() {
    if (list.sshStarted) return;
    list.sshStarted = true;
    await list.loadDefinitions();
    const pending = list.devices.filter(device => device.ssh);
    list.startup ||= Object.create(null);
    const update = () => { if (snap() && (page() === "settings" || page() === "devices")) render(); };
    for (const device of pending) {
      list.startup[device.id] = { host: device.ssh.host, state: "warn", label: "Checking SSH…", checkedAt: now() };
    }
    update();
    // Bound startup connections and check each configured device only once.
    await Promise.all(Array.from({ length: Math.min(4, pending.length) }, async () => {
      while (pending.length) {
        const device = pending.shift();
        const result = list.startup[device.id];
        // Removed while it waited.
        if (!result) continue;
        result.checkedAt = now();
        try {
          await invoke("check_ssh_device", { id: device.id });
          result.state = "running"; result.label = "SSH reachable at startup";
        } catch (error) {
          result.state = "failed"; result.label = `Startup SSH check failed: ${String(error)}`;
        }
        update();
      }
    }));
  }
  // This device's card and the forwarded device's come from the shared views, so
  // they show while a remote refresh is still collecting peers. Errors leave the placeholder.
  async function loadCards() {
    const range = minutes() || 30, scope = trafficViews.scope || "model";
    const forwarded = snap()?.forwarding?.deviceId;
    await Promise.all([
      trafficViews.local(range, scope).catch(() => {}),
      forwarded && trafficViews.forwarded(forwarded, range, scope).catch(() => {}),
    ]);
    if (page() === "devices") render();
  }
  /// While Devices is open, its visits and timer refresh peers this often.
  const DEVICE_REFRESH_MS = 30000;
  async function load(refreshRemote = false) {
    // A visit arriving during another load (e.g. from Settings) runs afterwards,
    // so the visit still starts a due refresh.
    if (list.refreshing) { list.refreshQueued = true; return; }
    list.refreshing = true;
    try {
      await list.loadDefinitions();
      // Forwarding status comes only from the local daemon; read it alongside
      // statistics instead of delaying the first traffic frame behind it.
      const local = Promise.all([
        invoke("get_device_forwarders").then(setForwarders),
        list.loadCapabilities(),
      ]);
      // Remote statistics must not start when Settings is open or the user has left Devices.
      if (page() !== "devices") { await local; return; }
      // Visits and timer ticks refresh peers once the last attempt is old enough;
      // a refresh still in flight is neither repeated nor superseded. Between
      // refreshes the peers' views stay as they are, so they are not merged again.
      const due = refreshRemote || now() - (list.remoteAttemptAt || 0) >= DEVICE_REFRESH_MS;
      const remote = due && !(list.loading && list.remote);
      // This device's traffic in the frame is reused as briefly as every page reuses it.
      const missing = !list.frameFresh(list.views?.[`${minutes() || 30}:${trafficViews.scope || "model"}`]);
      list.startupRefreshDone = true;
      await Promise.all([local, list.loadCards(), (remote || missing) && list.loadMerged(remote, remote)]);

    } catch (error) { toast(String(error)); }
    finally {
      list.refreshing = false;
      if (page() === "devices" || page() === "settings") render();
      if (list.refreshQueued) { list.refreshQueued = false; if (page() === "devices") list.load(); }
      if (list.recoveryPending) list.refreshRecovered();
    }
  }

  function synchronizeRecovery(before) {
    for (const device of list.devices) {
      if (before.get(device.id) === "failed" && list.status(device).state === "running"
          && list.startupRefreshDone) list.recoveryPending = true;
    }
    if (list.recoveryPending) list.refreshRecovered();
  }
  function setForwarders(forwarders) {
    const before = new Map(list.devices.map(device => [device.id, list.status(device).state]));
    list.forwarders = forwarders;
    synchronizeRecovery(before);
  }
  async function refreshRecovered() {
    if (!list.startupRefreshDone || !list.recoveryPending || list.loading || list.refreshing) return;
    // Peers are read only while Devices is open and no sooner than its cadence,
    // so a light flipping between a failed read and a healthy tunnel cannot
    // retry SSH. Elsewhere only the light follows; a due Devices visit refreshes.
    if (page() !== "devices" || now() - (list.remoteAttemptAt || 0) < DEVICE_REFRESH_MS) return;
    list.recoveryPending = false;
    await list.loadMerged(true, true);
  }
  function capability(device) {
    const entry = list.capabilities?.[device.id];
    if (entry?.host && (entry.host !== device.ssh?.host
        || (entry.binary || "coportd") !== (device.ssh?.binary || "coportd"))) return null;
    return entry;
  }
  async function loadCapabilities() {
    if (list.capabilitiesLoading) return;
    list.capabilitiesLoading = true;
    try {
      const values = await invoke("get_device_capabilities");
      const previous = list.capabilities || {};
      const before = new Map(list.devices.map(device => [device.id, list.status(device).state]));
      const signature = entry => JSON.stringify([entry?.host, entry?.binary, entry?.capabilities, entry?.error]);
      list.capabilities = Object.fromEntries(values.map(value => {
        const old = previous[value.deviceId];
        // Legacy daemons have no timestamps. Re-reading an unchanged cache must
        // not make that evidence newer than a real SSH/statistics failure.
        const checkedAt = value.checkedAt ?? (signature(old) === signature(value) ? old?.checkedAt : now());
        return [value.deviceId, { ...value, checkedAt }];
      }));
      list.capabilitiesError = "";
      synchronizeRecovery(before);
    } catch (error) { list.capabilitiesError = String(error); }
    finally {
      list.capabilitiesLoading = false;
      if (snap() && (page() === "settings" || page() === "devices")) render();
    }
  }

  function status(device) {
    const age = now() - (list.fetchedAt || 0);
    const source = (list.merged || list.latest || list.views?.["30:model"])?.sources?.find(s => s.name === device.name);
    const evidence = [];
    const ssh = device.ssh && (!device.data || device.data.transport === "ssh");
    if (ssh) {
      const startup = list.startup?.[device.id];
      if (startup?.host === device.ssh.host && startup.state !== "warn") evidence.push(startup);
      const cap = list.capability(device);
      if (cap?.error) evidence.push({ state: "failed", label: `SSH check failed: ${cap.error}`, checkedAt: cap.checkedAt });
      else if (cap?.capabilities) evidence.push({ state: "running", label: "SSH capability check succeeded (cached)", checkedAt: cap.checkedAt });
      const forwarding = list.forwarders?.find(item => item.deviceId === device.id);
      if (forwarding?.health?.state === "connected") evidence.push({ state: "running", label: "SSH forwarding connection verified", checkedAt: forwarding.health.lastCheckedAt });
      else if (forwarding?.health?.state === "disconnected") evidence.push({ state: "failed", label: forwarding.health.error || "SSH forwarding disconnected", checkedAt: forwarding.health.lastCheckedAt });
      if (forwarding?.remoteTrafficError && forwarding.remoteTrafficCheckedAt) evidence.push({ state: "failed", label: forwarding.remoteTrafficError, checkedAt: forwarding.remoteTrafficCheckedAt });
      else if (forwarding?.remoteTraffic) evidence.push({ state: "running", label: "Read-only statistics available", checkedAt: forwarding.remoteTraffic.fetchedAt });
    }
    let statistics;
    if (source?.error && source.error !== "TRAFFIC_UPDATING") statistics = { state: "failed", label: String(source.error) };
    else if (source?.exclusion === "duplicate") statistics = { state: "warn", label: "Statistics available; already counted via another entry." };
    else if (source?.included) statistics = { state: "running", label: age < 0 || age > 45000 || list.error ? "Last statistics refresh succeeded (cached)" : "Read-only statistics available" };
    if (statistics) evidence.push({ ...statistics, checkedAt: list.fetchedAt });
    // Compare evidence by time, not by whether it succeeded. A startup failure
    // cannot permanently override a later successful capability/statistics check.
    if (evidence.length) return evidence.reduce((latest, item) => (item.checkedAt || 0) >= (latest.checkedAt || 0) ? item : latest);
    if (source?.error === "TRAFFIC_UPDATING") return { state: "warn", label: "Loading traffic…" };
    if (!source || source.exclusion === "not_refreshed") return list.remote ? { state: "warn", label: "Refreshing statistics…" } : { state: "", label: "Not checked" };
    return { state: "warn", label: "Statistics not included" };
  }

  /// Read the peers again, unless a read of them is in flight.
  async function refresh() {
    if (list.loading && list.remote) return;
    await list.loadMerged(true, true);
  }
  /// Save `device`, a Device form's result, then read what changed. `saved`
  /// runs once the app kept it.
  async function save(device, saved = () => {}) {
    await invoke("save_device", { device });
    saved();
    const changed = await list.loadDefinitions();
    await list.loadCapabilities();
    if (changed || (!list.merged && !(list.loading && list.remote))) list.loadMerged(true);
  }
  async function remove(id) {
    await invoke("remove_device", { id });
    delete list.startup[id];
    await list.loadDefinitions();
    if (list.devices.length) list.loadMerged(true);
  }
  /// Forward to the Device `id`, or stop; the forwarding target after it.
  async function forward(id, enabled) {
    await invoke("set_device_forwarding", { id, enabled });
    list.setForwarders(await invoke("get_device_forwarders"));
    return list.forwarders[0] || null;
  }

  return Object.assign(list, {
    frameFresh, select, loadMerged, loadDefinitions, checkSsh, loadCards, load, setForwarders,
    refreshRecovered, capability, loadCapabilities, status, refresh, save, remove, forward,
  });
}

if (typeof module !== "undefined") module.exports = { createDeviceList };
