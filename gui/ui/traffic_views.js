"use strict";

/// Device traffic as Home, Activity and Devices show it. A view of one
/// device, range and category is read once and shared: a fresh one is
/// reused and a read in flight is joined, so no page reads the same traffic
/// twice. Home and Activity each keep the view they show for the shared range
/// and category, and the latest read for each wins. `invoke` reaches the app.
function createTrafficViews({ invoke, now = () => Date.now() }) {
  const { createLatest } = typeof module !== "undefined" ? require("./latest.js") : globalThis;
  /// A view read within this long is reused by every page that shows it.
  const REUSE_MS = 15000;
  const activityReads = createLatest();
  const homeReads = createLatest();
  let generation = 0;
  let stores;

  function shared(store, key, read) {
    const cached = stores[store].views[key];
    if (cached && now() - cached.loadedAt < REUSE_MS) return Promise.resolve(cached);
    const pending = stores[store].loads;
    if (pending[key]) return pending[key];
    const started = generation;
    const load = read().then((view) => {
      // A reset while reading (reassigned traffic) discards the old result.
      if (started !== generation) return shared(store, key, read);
      view.loadedAt = now();
      return (stores[store].views[key] = view);
    }).finally(() => { if (pending[key] === load) delete pending[key]; });
    pending[key] = load;
    return load;
  }

  const views = {
    /// The range in minutes and the category every page shows.
    minutes: 30,
    scope: "model",
    /// The view Activity shows, its last error and whether a read is running.
    activity: null,
    activityError: "",
    activityLoading: false,
    /// The view Home shows, its last error, when it was read and whether a read is running.
    home: null,
    homeError: "",
    homeFetchedAt: 0,
    homeLoading: false,

    /// This device's traffic: its window, totals and metrics.
    local(minutes, scope) {
      return shared("local", `${minutes}:${scope}`, async () => ({ ...(await invoke("get_traffic", { minutes, scope })), scope }));
    },
    /// The forwarded device's traffic. The daemon answers from its last
    /// forwarding summary; nothing is contacted.
    forwarded(device, minutes, scope) {
      return shared("forwarded", `${device}:${minutes}:${scope}`, async () => {
        const remote = await invoke("get_forwarding_traffic", { minutes, scope });
        return {
          ...remote.traffic, start: remote.windowStart, end: remote.windowEnd, bucketMinutes: remote.bucketMinutes, scope,
          device: remote.deviceId, deviceName: remote.name, fetchedAt: remote.fetchedAt, remoteError: remote.error,
        };
      });
    },
    /// The last read of this device's traffic, if any.
    localView(minutes, scope) {
      return stores.local.views[`${minutes}:${scope}`];
    },
    /// The last read of the forwarded `device`'s traffic, if any.
    forwardedView(device, minutes, scope) {
      return stores.forwarded.views[`${device}:${minutes}:${scope}`];
    },
    /// The last read for the shared range of `device`, or of this device, so
    /// Home and Activity open with each other's ring instead of waiting.
    cached(device) {
      return (device ? views.forwardedView(device, views.minutes, views.scope) : views.localView(views.minutes, views.scope)) ?? null;
    },
    /// Drops every view, e.g. after traffic is reassigned; reads in flight are repeated.
    reset() {
      generation++;
      const store = () => ({ views: Object.create(null), loads: Object.create(null) });
      stores = { local: store(), forwarded: store() };
    },
    /// Shows another range. Reads started for the previous one do not land.
    setMinutes(minutes) {
      views.minutes = minutes;
      activityReads.cancel();
      homeReads.cancel();
      views.activityLoading = views.homeLoading = false;
      views.activityError = views.homeError = "";
      views.homeFetchedAt = 0;
    },
    /// Shows another category. Reads started for the previous one do not
    /// land; the views shown stay until their replacements arrive.
    setScope(scope) {
      views.scope = scope;
      activityReads.cancel();
      homeReads.cancel();
      views.homeFetchedAt = 0;
      views.homeLoading = false;
    },
    clearErrors() {
      views.activityError = views.homeError = "";
    },
    /// Reads what Activity shows. Long ranges can take longer to read than
    /// the refresh interval, so a read of the same range and category still
    /// running is not started again. True when its result landed.
    async loadActivity() {
      const key = `${views.minutes}:${views.scope}`;
      if (views.activityLoading && activityReads.loading(key)) return false;
      const read = activityReads.begin(key);
      views.activityLoading = true;
      // Charts follow the category their data was read for, so a switch keeps
      // the previous charts consistent until the replacement arrives.
      const { minutes, scope } = views;
      try {
        const view = await views.local(minutes, scope);
        if (!read.current()) return false;
        views.activity = view;
        views.activityError = "";
      } catch (error) {
        if (!read.current()) return false;
        views.activityError = String(error);
      } finally {
        if (read.current()) views.activityLoading = false;
        read.done();
      }
      return true;
    },
    /// Reads what Home shows: the forwarded `device`'s traffic, or this
    /// device's. Unless `force`d, a read already running is not repeated.
    /// True when its result landed.
    async loadHome(device, force = false) {
      if (views.homeLoading && !force) return false;
      const read = homeReads.begin();
      views.homeLoading = true;
      const { minutes, scope } = views;
      try {
        const view = await (device ? views.forwarded(device, minutes, scope) : views.local(minutes, scope));
        if (!read.current()) return false;
        views.home = view;
        views.homeError = "";
        views.homeFetchedAt = now();
      } catch (error) {
        if (!read.current()) return false;
        views.homeError = String(error);
      } finally {
        if (read.current()) views.homeLoading = false;
      }
      return true;
    },
  };
  views.reset();
  return views;
}

if (typeof module !== "undefined") module.exports = { createTrafficViews };
