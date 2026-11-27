"use strict";

const { invoke, Channel } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const IS_MAC = navigator.userAgent.includes("Mac");
const REVEAL_LABEL = IS_MAC ? "Show in Finder" : navigator.userAgent.includes("Windows") ? "Show in Explorer" : "Show in Folder";

// ---------------------------------------------------------------- helpers

/// A proxy's colored label, its identity wherever it is named; `none` is
/// direct. Exit countries are not identities (proxies can share one), so
/// flags only annotate exit addresses.
function tag(name) {
  if (name === "none") return `<span class="tag direct">direct</span>`;
  // A proxy whose last test failed is grey wherever it is named.
  const down = ui.proxies[name]?.probe?.state === "error";
  return `<span class="tag ${down ? "down" : `c${ui.tagColors[name] ?? 0}`}">${esc(name)}</span>`;
}

let toastTimer;
function toast(text) {
  const el = $("toast");
  el.textContent = text;
  el.classList.add("show");
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => el.classList.remove("show"), 1600);
}

// ---------------------------------------------------------------- state

const ui = {
  snap: null,
  // Opened by keyboard navigation of the menu bar (macOS 27); see menuBarStep.
  menuBarNavigation: false,
  deviceTrafficMinutes: 30,
  deviceTrafficOpen: Object.create(null),
  fetchedAt: 0,
  page: "main",
  /// Opened from View All: keep the Log at the top until the user scrolls.
  pinLog: false,
  recent: [],
  expanded: new Set(),
  builtPage: null,
  websocketOpen: false,
  otherLoginsOpen: false,
  snippetsOpen: false,
  rendering: false,
  proxies: {},
  tagColors: {},
};

const trafficViews = createTrafficViews({ invoke });
const activityLog = createActivityPager({
  invoke,
  show(update, live) {
    const apply = () => { update(); renderActivityList(); };
    if (live) keepLogScroll(apply);
    else apply();
  },
  failed: (error) => toast(String(error)),
  now: currentMinute,
});
const deviceForm = createDeviceForm();
const deviceList = createDeviceList({
  invoke, Channel, trafficViews, page: () => ui.page, minutes: () => ui.deviceTrafficMinutes,
  snap: () => ui.snap, render: () => render(), toast: message => toast(message),
});
const refreshes = createLatest();

async function refresh() {
  const request = refreshes.begin();
  const [snap, recent] = await Promise.all([
    invoke("get_state"),
    invoke("get_activity", { filter: "requests", search: "" }),
  ]);
  // A later request still in flight must not discard this completed snapshot.
  // In particular, Restart waits until its fresh configuration is displayed.
  if (!request.apply()) return;
  ui.snap = snap;
  if (deviceList.loaded) deviceList.setForwarders(snap.forwarding ? [snap.forwarding] : []);
  ui.recent = recent.rows.slice(0, 5);
  const proxies = snap.config.details?.proxies ?? [];
  ui.proxies = Object.fromEntries(proxies.map((p) => [p.name, p]));
  ui.tagColors = tagColors(proxies.map((p) => p.name));
  ui.fetchedAt = Date.now();
  // Switching forwarding on or off changes which source Home reads at once,
  // even while the previous source's read runs.
  const moved = (trafficViews.home?.device ?? null) !== homeTrafficDevice();
  if (ui.page === "main" && (moved || Date.now() - trafficViews.homeFetchedAt >= 15000)) loadHomeTraffic(moved);
  if (ui.page === "activity") scheduleActivityLog();
  render();
}

/// Rereads the login item, which System Settings can change while the app runs.
/// Settings keeps the last Launch at login state it read when a read fails.
async function refreshLaunchAtLogin() {
  try {
    const state = await invoke("refresh_launch_at_login");
    if (ui.snap) ui.snap.settings.launchAtLogin = state;
  } catch {}
}

let refreshTimer;
function scheduleRefresh() {
  clearTimeout(refreshTimer);
  refreshTimer = setTimeout(refresh, 120);
}

/// Offset of the Log block from where the first block of the page starts.
function logOffset() {
  const content = $("content");
  const log = content.querySelector(".log-block");
  if (!log) return null;
  return log.getBoundingClientRect().top - content.getBoundingClientRect().top - parseFloat(getComputedStyle(content).paddingTop);
}

/// Scrolls the Activity page so the Log block starts at the top. Until the
/// user scrolls, Traffic loading above it keeps it there.
function scrollToLog() {
  $("content").scrollTop += logOffset() ?? 0;
}


/// Applies `update` without moving the Log rows in view when the list is
/// scrolled, so entries added above do not push them down.
function keepLogScroll(update) {
  const list = $("activity-list");
  if (!list) return update();
  const content = $("content");
  const top = content.getBoundingClientRect().top;
  const anchor = list.getBoundingClientRect().top < top
    ? [...list.querySelectorAll(".req[data-seq]")].find((el) => el.getBoundingClientRect().bottom > top)
    : null;
  const before = anchor?.getBoundingClientRect().top;
  update();
  const after = anchor && $("activity-list")?.querySelector(`.req[data-seq="${anchor.dataset.seq}"]`);
  if (after) content.scrollTop += after.getBoundingClientRect().top - before;
}

/// Lists log lines written since the last read when the range reaches the
/// present (or a preset moved with the clock). Waits while a choice is open or
/// an older page is loading.
function refreshActivityLog() {
  if (ui.page !== "activity" || document.hidden || menus.current() || activityLog.loadingMore) return;
  if (!$("range-editor")?.hidden) return;
  activityLog.refresh();
}

// New log lines arrive in bursts; list them at most every few seconds.
const LOG_REFRESH_MS = 3000;
let logRefreshAt = 0;
let logRefreshTimer = null;
function scheduleActivityLog() {
  if (logRefreshTimer) return;
  logRefreshTimer = setTimeout(() => {
    logRefreshTimer = null;
    logRefreshAt = Date.now();
    refreshActivityLog();
  }, Math.max(0, logRefreshAt + LOG_REFRESH_MS - Date.now()));
}

function uptime() {
  return ui.snap.phase.uptimeSecs + Math.floor((Date.now() - ui.fetchedAt) / 1000);
}

// ---------------------------------------------------------------- render

const BACK_KEYS = IS_MAC ? "Meta+[" : "Alt+ArrowLeft";

function render() {
  if (!ui.snap) return;
  if (menus.holdRender()) return;
  // The shown page stays until the next one has its data (act "page").
  if (ui.pageChange) return;
  const focus = focusPlace(document.activeElement);
  renderTop();
  renderPage();
  restoreFocus(focus);
  panelMotion.queueFit();
}

function renderTop() {
  if (ui.page === "main") {
    $("top").innerHTML = `
      <span class="app-name">Coport</span><span class="version">v${esc(ui.snap.version)}</span>
      <nav class="links">
        <button class="text-link" data-action="page" data-page="activity" data-morph="title:activity">Activity</button>
        ${(deviceList.loaded ? deviceList.devices.length : ui.snap.settings?.deviceCount || 0) > 0 ? `<button class="text-link" data-action="page" data-page="devices" data-morph="title:devices">Devices</button>` : ""}
        <button class="text-link" data-action="page" data-page="settings">Settings</button>
        <button class="text-link" data-action="quit">Quit</button>
      </nav>`;
    return;
  }
  const title = ui.page === "activity" ? "Activity" : ui.page === "devices" ? "Devices" : "Settings";
  const tools =
    ui.page === "activity"
      ? `<nav class="links">
          <button class="icon-btn" data-action="open" data-target="log" data-tip="Open log file" aria-label="Open log file">${ICON.folder}</button>
        </nav>`
      : ui.page === "devices" ? `<nav class="links"><button class="icon-btn" data-action="page" data-page="settings" data-tip="Configure devices" aria-label="Configure devices in Settings">${ICON.settings}</button></nav>` : "";
  $("top").innerHTML = `
    <button class="text-link back" data-action="page" data-page="main" aria-keyshortcuts="${BACK_KEYS}" data-morph="back">${ICON.back}Back</button>
    <span class="page-title" data-morph="title:${ui.page}">${title}</span>${tools}`;
}

function renderPage() {
  if (ui.builtPage !== ui.page) routeTooltip.hide();
  const content = $("content");
  for (const details of content.querySelectorAll("details[data-device-traffic]")) (ui.deviceTrafficOpen ||= Object.create(null))[details.dataset.deviceTraffic] = details.open;
  if (ui.page === "activity") {
    if (ui.builtPage !== "activity") {
      menus.close();
      content.innerHTML = activityShell();
      ui.builtPage = "activity";
      renderActivityTraffic();
    }
    renderActivityForwarding();
    renderActivityList();
    return;
  }
  menus.close();
  ui.builtPage = ui.page;
  // State refreshes must not discard what is being typed.
  const focused = content.contains(document.activeElement) && document.activeElement.matches("input[id]")
    ? document.activeElement : null;
  const typing = focused && { id: focused.id, value: focused.value, start: focused.selectionStart, end: focused.selectionEnd };
  ui.rendering = true;
  content.innerHTML = `<div class="page${ui.page === "settings" ? " settings-page" : ""}">${ui.page === "settings" ? settings() : ui.page === "devices" ? devicesPage() : main()}</div>`;
  const field = typing && $(typing.id);
  if (field) {
    field.value = typing.value;
    field.focus();
    field.setSelectionRange(typing.start, typing.end);
  }
  ui.rendering = false;
}

installHomePage();
const routeTooltip = createRouteTooltip({ document, window });
routeTooltip.install();

// ---------------------------------------------------------------- activity

const menus = createMenus({
  document,
  window,
  rerender() {
    render();
    if (ui.page === "activity") renderActivityTraffic();
  },
});
menus.install();
installActivityPage();

/// Home, Activity and Devices show traffic for one range, so changing pages
/// keeps it. Reads started for the previous range must not land afterwards.
function setTrafficMinutes(minutes) {
  trafficViews.setMinutes(minutes);
  ui.deviceTrafficMinutes = minutes;
}

async function loadTraffic() {
  $("activity-traffic")?.setAttribute("aria-busy", "true");
  if (!(await trafficViews.loadActivity())) return;
  if (ui.page === "activity") renderActivityTraffic();
  else if (ui.page === "devices") render();
}

// ---------------------------------------------------------------- devices

installDevicesPage();

// ---------------------------------------------------------------- page motion

const panelMotion = createPageMotion({ document, ui, invoke });
panelMotion.install();
// #content fills the window, so in-page growth (e.g. an opened disclosure)
// only shows up on the page element, which each render replaces.
new MutationObserver(() => {
  routeTooltip.refresh();
  panelMotion.watchPage($("content").firstElementChild);
}).observe($("content"), { childList: true, subtree: true, characterData: true, attributes: true, attributeFilter: ["title", "data-tip"] });
new MutationObserver(() => routeTooltip.refresh()).observe($("top"), {
  childList: true, subtree: true, characterData: true, attributes: true, attributeFilter: ["title", "data-tip"],
});

// ---------------------------------------------------------------- share rings

const ringMotion = createRingMotion({
  document,
  morphMs: MORPH_MS,
  still: () => panelMotion.moving() || REDUCED_MOTION.matches || document.hidden,
});
ringMotion.install($("content"));

// ---------------------------------------------------------------- actions

async function act(action, el) {
  const s = ui.snap;
  switch (action) {
    case "page": {
      const page = ui.page = el.dataset.page;
      // The shown page stays while the next one reads what it shows first, so
      // the change happens in one motion. A later change supersedes this one
      // and does its own reads.
      const change = ui.pageChange = (ui.pageChanges || 0) + 1;
      ui.pageChanges = change;
      try {
        if (page === "activity") {
          if (s.settings?.deviceCount > 0) {
            try { await deviceList.loadDefinitions(); } catch (error) { toast(String(error)); }
            if (ui.pageChanges !== change) break;
          }
          // Home's ring, already read, opens Activity unchanged.
          trafficViews.activity = trafficViews.cached(null) ?? trafficViews.activity;
          loadTraffic();
          // Presets follow the clock; a custom range stays as chosen.
          activityLog.resolveRange();
          await activityLog.load();
        } else if (page === "main") {
          trafficViews.home = trafficViews.cached(homeTrafficDevice()) ?? trafficViews.home;
          loadHomeTraffic();
        } else if (page === "devices") {
          // Peers' views for the shared range, from the last refresh, open with the page.
          deviceList.select();
          deviceList.load();
        } else if (page === "settings") await Promise.all([deviceList.load(), refreshLaunchAtLogin()]);
      } finally {
        if (ui.pageChange === change) ui.pageChange = null;
      }
      if (ui.pageChanges !== change) break;
      ui.pinLog = ui.page === "activity" && el.dataset.target === "log";
      panelMotion.changePage(() => {
        render();
        $("content").scrollTop = 0;
        if (ui.pinLog) scrollToLog();
      });
      break;
    }
    case "forwarding-restore": {
      if (ui.forwardingRestoreBusy) break;
      const enabled = !ui.snap.forwardingRestore;
      ui.forwardingRestoreBusy = true; render();
      try { await invoke("set_forwarding_restore", { enabled }); ui.snap.forwardingRestore = enabled; }
      catch (error) { toast(String(error)); }
      finally { ui.forwardingRestoreBusy = false; render(); }
      break;
    }
    case "device-forward": {
      if (ui.deviceForwardingBusy) break;
      ui.deviceForwardingBusy = el.dataset.id; render();
      try {
        ui.snap.forwarding = await deviceList.forward(el.dataset.id, el.dataset.enabled === "true");
      } catch (error) { toast(String(error)); }
      finally { ui.deviceForwardingBusy = null; render(); }
      break;
    }
    case "device-new":
      deviceForm.add(); render(); $("device-name")?.focus();
      break;
    case "device-cancel":
      deviceForm.close(); render();
      break;
    case "device-edit": {
      const d = deviceList.devices.find(device => device.id === el.dataset.id);
      if (d) {
        deviceForm.edit(d); render(); $("device-name")?.focus();
      }
      break;
    }
    case "merged-refresh":
      await deviceList.refresh();
      break;
    case "device-save": {
      captureDeviceDraft(); const device = deviceForm.device();
      try { await deviceList.save(device, () => deviceForm.close()); }
      catch (e) { toast(String(e)); }
      break;
    }
    case "device-remove":
      try { await deviceList.remove(el.dataset.id); }
      catch (e) { toast(String(e)); }
      break;
    case "power":
      try {
        await invoke("set_running", { running: s.phase.state !== "running" });
      } catch (e) {
        toast(String(e));
      }
      await refresh();
      break;
    case "restart": {
      let restarted = false;
      try {
        await invoke("restart_proxy");
        restarted = true;
      } catch (e) {
        toast(String(e));
      }
      await refresh();
      if (restarted) toast("Proxy restarted");
      break;
    }
    case "proxy-ca-replace":
      try {
        const trusted = await invoke("replace_proxy_ca");
        if (trusted === true) toast("Proxy CA replaced and trusted");
        else if (trusted === false) toast("Proxy CA replaced. Add it to your trust store");
      } catch (e) {
        toast(String(e));
      }
      await refresh();
      break;
    case "proxy-ca-keep":
      try {
        await invoke("keep_proxy_ca");
      } catch (e) {
        toast(String(e));
      }
      await refresh();
      break;
    case "copy": {
      try {
        await invoke("copy_text", { text: el.dataset.text });
      } catch (e) {
        toast(String(e));
        break;
      }
      // Another copy while the check shows restores the icon from before the first.
      clearTimeout(el.copiedTimer);
      el.copiedIcon ??= el.innerHTML;
      el.innerHTML = ICON.check;
      el.classList.add("done");
      el.copiedTimer = setTimeout(() => {
        el.innerHTML = el.copiedIcon;
        el.copiedIcon = undefined;
        el.classList.remove("done");
      }, 1200);
      break;
    }
    case "expand": {
      const seq = Number(el.dataset.seq);
      if (ui.expanded.has(seq)) ui.expanded.delete(seq);
      else ui.expanded.add(seq);
      renderActivityList();
      break;
    }
    case "filter":
      activityLog.filter = el.dataset.filter;
      await activityLog.load();
      break;
    case "traffic-assign":
      await assignTraffic(JSON.parse(el.dataset.target));
      break;
    case "traffic-review": {
      const first = document.querySelector(".traffic-review.review");
      first?.scrollIntoView({ block: "center", behavior: "smooth" });
      if (first) setTimeout(() => menus.showMenu(first), 250);
      break;
    }
    case "clear-traffic-compatibility":
      try {
        if (await invoke("clear_traffic_compatibility")) {
          toast("Traffic compatibility choices cleared");
        }
      } catch (e) {
        toast(String(e));
      }
      await refresh();
      break;
    case "activity-more":
      el.disabled = true;
      await activityLog.load("more");
      break;
    case "activity-preset":
      activityLog.preset = el.dataset.target;
      activityLog.resolveRange();
      renderActivityRange();
      await activityLog.load();
      break;
    case "range-edit":
      showRangeEditor(true);
      break;
    case "range-now":
      $("activity-to").value = localInput(currentMinute());
      break;
    case "range-cancel":
      showRangeEditor(false);
      break;
    case "range-apply":
      await applyRangeEditor();
      break;
    case "open":
      await invoke("open_path", { target: el.dataset.target });
      break;
    case "probe":
      await invoke("probe_proxy", { name: el.dataset.name ?? null });
      await refresh();
      break;
    case "launch-at-login":
      try {
        await invoke("set_launch_at_login", { enabled: el.getAttribute("aria-checked") !== "true" });
      } catch (e) {
        toast(String(e));
      }
      await refresh();
      break;
    case "open-login-items":
      await invoke("open_login_items");
      break;
    case "auto-start":
      await invoke("update_settings", { patch: { startProxyOnLaunch: el.getAttribute("aria-checked") !== "true" } });
      await refresh();
      break;
    case "keep-running":
      await invoke("update_settings", { patch: { keepProxyRunningOnQuit: el.getAttribute("aria-checked") !== "true" } });
      await refresh();
      break;
    case "mask-account-names":
      await invoke("update_settings", { patch: { maskAccountNames: el.getAttribute("aria-checked") !== "true" } });
      await refresh();
      break;
    case "config-flag":
      // The new configuration value: the switch's new state, unless inverted.
      await setConfigValue(el.dataset.key, (el.getAttribute("aria-checked") !== "true") !== (el.dataset.inverted === "true"));
      break;
    case "create-config":
      try {
        await invoke("create_example_config");
        toast("Example configuration created");
      } catch (e) {
        toast(String(e));
      }
      await refresh();
      break;
    case "import-config":
      try {
        if (await invoke("import_config")) toast("Configuration imported");
      } catch (e) {
        toast(String(e));
      }
      await refresh();
      break;
    case "quit":
      await invoke("quit_app");
      break;
  }
}

// Any scroll or input on the page ends a View All pin of the Log.
for (const type of ["wheel", "touchstart", "keydown", "pointerdown"]) {
  $("content").addEventListener(type, () => { ui.pinLog = false; }, { passive: true });
}

document.addEventListener("click", (event) => {
  const select = event.target.closest("[data-options]");
  if (select) { menus.showSelect(select); return; }
  const menu = event.target.closest("[data-menu]");
  if (menu) { menus.showMenu(menu); return; }
  const el = event.target.closest("[data-action]");
  if (el && !el.disabled) act(el.dataset.action, el);
});

document.addEventListener("change", async (event) => {
  if (event.target.id === "activity-side") {
    activityLog.upstream = event.target.value === "upstream";
    await activityLog.load();
    return;
  }
  if (event.target.id === "search-mode") {
    activityLog.searchMode = event.target.value;
    clearTimeout(searchTimer);
    $("search").placeholder = searchPlaceholder();
    await activityLog.load();
    return;
  }
  if (event.target.id === "devices-traffic-scope" || event.target.id === "devices-traffic-range") {
    if (event.target.id === "devices-traffic-scope") {
      trafficViews.setScope(event.target.value);
    } else { setTrafficMinutes(Number(event.target.value)); }
    const cards = deviceList.loadCards();
    const shown = deviceList.select();
    // A remote refresh delivers every range when it completes. A cache read now
    // would supersede it and show peers as not refreshed, so wait for it instead.
    if (deviceList.loading && deviceList.remote) { if (!shown) deviceList.merged = null; render(); await cards; return; }
    if (shown && deviceList.frameFresh(deviceList.merged)) { render(); await cards; return; }
    // A cached frame shows at once while a read completes it with this device.
    const loading = deviceList.loadMerged(true); render(); await Promise.all([loading, cards]);
    return;
  }
  if (event.target.id === "home-traffic-scope" || event.target.id === "traffic-scope") {
    // Both views share the category. In-flight results from the old category
    // must not repopulate either cache after a switch; the previous charts
    // stay mounted until the replacement data is ready.
    trafficViews.setScope(event.target.value);
    trafficViews.clearErrors();
    if (ui.page === "main") {
      const loading = loadHomeTraffic(true);
      render();
      await loading;
    } else {
      renderActivityTraffic();
      await loadTraffic();
    }
    return;
  }

  if (event.target.id === "home-traffic-range") {
    setTrafficMinutes(Number(event.target.value));
    // Launch immediately so concurrent refreshes cannot start an older range.
    const loading = loadHomeTraffic(true);
    render();
    await loading;
    return;
  }
  if (event.target.id === "traffic-range") {
    setTrafficMinutes(Number(event.target.value));
    renderActivityTraffic();
    await loadTraffic();
    return;
  }
  const key = event.target.dataset?.setting;
  if (!key) return;
  await invoke("update_settings", { patch: { [key]: event.target.value } });
  await refresh();
});

let searchTimer;
document.addEventListener("input", (event) => {
  if (event.target.id !== "search") return;
  activityLog.search = event.target.value;
  clearTimeout(searchTimer);
  searchTimer = setTimeout(() => activityLog.load(), 120);
});

/// Whether keys on `el` edit text, where ←/→ move the caret.
function textEntry(el) {
  return Boolean(el?.closest?.("textarea, [contenteditable], input:not([type=checkbox]):not([type=radio]):not([type=button])"));
}

/// While keyboard navigation of the menu bar holds the panel open (macOS 27),
/// the focused panel receives ←/→; those it does not use move the navigation
/// on to the neighbouring item, as from a menu. Every other key keeps its own
/// meaning. Returns "left", "right" or null.
function menuBarStep(event) {
  if (!ui.menuBarNavigation || event.defaultPrevented || event.isComposing) return null;
  if (event.metaKey || event.ctrlKey || event.altKey || event.shiftKey || textEntry(event.target)) return null;
  return event.key === "ArrowLeft" ? "left" : event.key === "ArrowRight" ? "right" : null;
}

document.addEventListener("keydown", (event) => {
  const editor = $("range-editor");
  if (editor && !editor.hidden && editor.contains(event.target) && (event.key === "Escape" || event.key === "Enter")) {
    event.preventDefault();
    if (event.key === "Escape") showRangeEditor(false);
    else applyRangeEditor();
    return;
  }
  const open = menus.current();
  if (open) {
    const { menu } = open;
    const items = menus.items(menu);
    const index = items.indexOf(document.activeElement);
    if (event.key === "Escape" || event.key === "Tab") {
      menus.close(true);
      if (event.key === "Escape") event.preventDefault();
      return;
    }
    if (["ArrowDown", "ArrowUp", "Home", "End"].includes(event.key)) {
      event.preventDefault();
      const next = event.key === "Home" ? 0 : event.key === "End" ? items.length - 1
        : (index + (event.key === "ArrowDown" ? 1 : -1) + items.length) % items.length;
      items[next].focus();
      return;
    }
  } else if (event.target.matches("[data-options], [data-menu]") && ["ArrowDown", "ArrowUp"].includes(event.key)) {
    event.preventDefault();
    (event.target.dataset.menu ? menus.showMenu : menus.showSelect)(event.target, event.key === "ArrowUp");
    return;
  }
  if (event.target.matches("[data-config]") && (event.key === "Enter" || event.key === "Escape")) {
    if (event.key === "Escape") event.target.value = ui.snap.config.details?.editable[event.target.dataset.config] ?? "";
    event.target.blur();
    return;
  }
  const step = menus.current() ? null : menuBarStep(event);
  if (step) {
    event.preventDefault();
    invoke("navigate_menu_bar", { right: step === "right" });
    return;
  }
  const mod = event.metaKey || event.ctrlKey;
  const back = !event.shiftKey && (IS_MAC
    ? event.metaKey && !event.ctrlKey && !event.altKey && event.key === "["
    : event.altKey && !event.ctrlKey && !event.metaKey && event.key === "ArrowLeft");
  if (event.key === "Escape") {
    if (ui.page !== "main") act("page", { dataset: { page: "main" } });
    else invoke("hide_panel");
  } else if (back) {
    // Same as the Back button, on every page that has one.
    event.preventDefault();
    if (ui.page !== "main") act("page", { dataset: { page: "main" } });
  } else if (mod && event.key === "q") {
    invoke("quit_app");
  } else if (mod && event.key === ",") {
    act("page", { dataset: { page: "settings" } });
  }
});

document.addEventListener("focusout", (event) => {
  if (!ui.rendering && event.target.matches?.("[data-config]")) commitConfigField(event.target);
});

// Re-renders rebuild disclosures, so their open state lives in `ui`.
document.addEventListener("toggle", (event) => {
  const { id, open } = event.target;
  if (id === "websocket-timeouts") ui.websocketOpen = open;
  else if (id === "other-logins") ui.otherLoginsOpen = open;
  else if (id === "setup-snippets") ui.snippetsOpen = open;
}, true);

document.addEventListener("contextmenu", (event) => {
  if (!event.target.closest(".selectable, input, pre, dd")) event.preventDefault();
});

// Live uptime without refetching state.
setInterval(() => {
  const el = $("uptime");
  if (el && ui.snap?.phase.state === "running") el.textContent = fmtUptime(uptime());
}, 1000);

// Check configured proxies at startup; the backend retests aged results on its own
// schedule, so opening the panel does not trigger tests.
const probeStale = () => invoke("probe_proxy", { name: null, staleOnly: true });

// Linux has no system material behind the panel: the backend sends the screen
// under it, blurred, and where the window lies on it. `image` is null when
// unchanged and empty without a backdrop.
window.__TAURI__.event
  .listen("panel-backdrop", ({ payload }) => {
    const root = document.documentElement;
    root.classList.toggle("composited", payload.composited);
    root.style.setProperty("--backdrop-size", `${payload.width}px ${payload.height}px`);
    root.style.setProperty("--backdrop-position", `${-payload.x}px ${-payload.y}px`);
    if (payload.image === null) return;
    root.style.setProperty("--backdrop-image", payload.image ? `url("${payload.image}")` : "none");
    root.classList.toggle("frosted", Boolean(payload.image));
  })
  .catch((error) => toast(error));

listen("state-changed", scheduleRefresh);
const PANEL_PAGE_TIMEOUT_MS = 60 * 1000;
let panelHiddenAt = null;
listen("panel-shown", async ({ payload } = {}) => {
  ui.menuBarNavigation = Boolean(payload?.keyboard);
  // Keys start on the panel, not in a text field left focused, where ←/→
  // would not move along the menu bar.
  if (ui.menuBarNavigation && textEntry(document.activeElement)) document.activeElement.blur();
  const returnHome = panelHiddenAt !== null && Date.now() - panelHiddenAt >= PANEL_PAGE_TIMEOUT_MS;
  panelHiddenAt = null;
  if (returnHome) {
    ui.page = "main";
    menus.close();
    render();
    $("content").scrollTop = 0;
  }
  if (deviceList.loaded) deviceList.loadCapabilities();
  // Returning from System Settings may have allowed or blocked the login item.
  if (ui.page === "settings") await refreshLaunchAtLogin();
  return refresh();
});
listen("panel-hidden", () => {
  ui.menuBarNavigation = false;
  routeTooltip.hide();
  // Repeated native hide events must not extend the previous page's lifetime.
  panelHiddenAt ??= Date.now();
  menus.close();
});
refresh().then(probeStale);
deviceList.checkSsh();

// Refresh even when no new requests arrive, so the rolling window advances.
setInterval(() => {
  if (document.hidden) return;
  if (ui.page === "activity") {
    deviceList.loadCapabilities();
    loadTraffic();
    scheduleActivityLog();
  } else if (ui.page === "devices") { refresh(); deviceList.load(); }
  else if (ui.page === "main") { refresh(); deviceList.loadCapabilities(); }
  else if (ui.page === "settings") refreshForwardingStatus();
}, 15000);
