"use strict";

/// The Settings page: General, Network and its configuration fields, Files,
/// Devices (devices_page.js) and other logins. Like the other pages it keeps
/// no state of its own; changes to the configuration go through `invoke`.

function settings() {
  const s = ui.snap;
  const set = s.settings;
  return `
    ${block(
      "General",
      "",
      `<div class="row"><span class="row-label">Launch at login</span>
         <button class="switch" role="switch" aria-checked="${set.launchAtLogin !== "off"}" data-action="launch-at-login" aria-label="Launch at login"></button></div>
       ${set.launchAtLogin === "needsApproval" ? message("warn", "macOS blocks Coport from opening at login. Allow it in Login Items.", `<button class="btn" data-action="open-login-items">Open Login Items</button>`) : ""}
       <div class="row"><span class="row-label">Start proxy on startup</span>
         <button class="switch" role="switch" aria-checked="${set.startProxyOnLaunch}" data-action="auto-start" aria-label="Start proxy on startup"></button></div>
       <div class="row"><span class="row-label" data-tip="Reopen the app to manage or stop the background proxy.">Keep proxy running after quit</span>
         <button class="switch" role="switch" aria-checked="${set.keepProxyRunningOnQuit}" data-action="keep-running" aria-label="Keep proxy running after quit" data-tip="Reopen the app to manage or stop the background proxy."></button></div>
       <div class="row"><span class="row-label" data-tip="Applies to traffic and routing views.">Mask account names</span>
         <button class="switch" role="switch" aria-checked="${set.maskAccountNames}" data-action="mask-account-names" aria-label="Mask account names" data-tip="Applies to traffic and routing views."></button></div>
       <div class="row"><span class="row-label">Appearance</span>
         ${panelSelect("appearance", [["System", "System"], ["Light", "Light"], ["Dark", "Dark"]], set.appearance, "Appearance", "", 'data-setting="appearance"')}</div>`
    )}
    ${configSettings()}
    ${filesBlock()}
    ${deviceSettings()}`;
}
/// The fixed configuration and log files: a status per file, actions in a ⋯ menu.
function filesBlock() {
  const { config, settings } = ui.snap;
  const status = (dot, text) => `<span class="file-status">${dot ? `<span class="dot ${dot}"></span>` : ""}${text}</span>`;
  const configStatus = !config.exists ? status("", "Not created")
    : config.error ? status("failed", "Invalid")
    : config.changedSinceStart ? status("warn", "Changed")
    : status("running", "Valid");
  const configMenu = [
    config.exists ? { label: "Edit", action: "open", target: "config" } : { label: "Create from Example", action: "create-config" },
    { label: "Import…", action: "import-config" },
    config.changedSinceStart && { label: "Restart Proxy to Apply", action: "restart" },
    null,
    { label: REVEAL_LABEL, action: "open", target: "config-reveal" },
  ].filter((item) => item !== false);
  const logMenu = [
    { label: "Open", action: "open", target: "log" },
    null,
    { label: REVEAL_LABEL, action: "open", target: "log-reveal" },
  ];
  const compat = settings.trafficCompatibility;
  const compatStatus = compat.error ? status("failed", "Invalid")
    : compat.choices ? status("", `${compat.choices} ${compat.choices === 1 ? "choice" : "choices"}`)
    : status("", "No choices");
  const compatMenu = [
    compat.exists && { label: "Open", action: "open", target: "traffic-compatibility" },
    compat.exists && { label: "Clear Choices…", action: "clear-traffic-compatibility" },
    compat.exists && null,
    { label: REVEAL_LABEL, action: "open", target: "traffic-compatibility-reveal" },
  ].filter((item) => item !== false);
  const row = (name, value, items) => `<div class="row"><span class="row-label row-name">${name}</span>${value}
    <button type="button" class="icon-btn more" data-menu="${esc(JSON.stringify(items))}" aria-label="${name} actions"
      aria-haspopup="menu" aria-expanded="false">${ICON.more}</button></div>`;
  return block(
    "Files",
    "",
    `${config.error ? message("bad", esc(config.error)) : ""}
     ${row("Configuration", configStatus, configMenu)}
     ${row("Request Log", status("", settings.logBytes == null ? "Empty" : fmtBytes(settings.logBytes)), logMenu)}
     ${row("Traffic Compatibility", compatStatus, compatMenu)}`
  );
}
/// Network and Subscription Logins: values written to the YAML configuration, applied on restart.
function configSettings() {
  const v = ui.snap.config.details?.editable;
  if (!v) return "";
  const field = (key, label, unit = "", description = "") => {
    const id = `config-${key.replaceAll(".", "-")}`;
    const tip = description ? ` data-tip="${esc(description)}"` : "";
    return `<div class="row" data-tip-scope="config:${key}"><span class="row-label"${tip}>${label}</span>
      <span class="number-field"><input class="field" id="${id}" data-config="${key}" inputmode="decimal" spellcheck="false"
        value="${esc(String(v[key]))}" aria-label="${label}"${tip} /><span class="unit">${unit}</span></span></div>`;
  };
  // An inverted switch is on while its configuration value is false.
  const flag = (key, label, description, inverted = false) => {
    const tip = ` data-tip="${esc(description)}"`;
    const invert = inverted ? ' data-inverted="true"' : "";
    return `<div class="row" data-tip-scope="config:${key}"><span class="row-label"${tip}>${label}</span>
      <button class="switch" role="switch" aria-checked="${v[key] !== inverted}" data-action="config-flag" data-key="${key}"${invert} aria-label="${label}"${tip}></button></div>`;
  };
  return `${block(
    "Network",
    "",
    `${field("listen_port", "Listen port")}
     ${flag("allow_external_access", "Share statistics", "Requires external_data in the configuration file. Shares read-only statistics. Restart to apply.")}
     ${flag("connect_credential_routing", "Enable HTTPS proxy", "Other apps can use Coport via HTTPS_PROXY. Routes by account or API key. Requires trusting Coport's CA. Restart to apply.")}
     ${field("request_timeout_seconds", "Request timeout", "s")}
     <details class="disclosure" id="websocket-timeouts" ${ui.websocketOpen ? "open" : ""}><summary>${ICON.chevron}WebSocket timeouts</summary>
       ${field("websocket.first_message_seconds", "First message", "s")}
       ${field("websocket.first_output_seconds", "First output", "s")}
       ${field("websocket.read_seconds", "Read", "s")}
       ${field("websocket.write_seconds", "Write", "s")}
       ${field("websocket.inter_turn_idle_seconds", "Between turns", "s", "0 disables this timer.")}
     </details>`
  )}
  ${block(
    "Subscription Logins",
    "",
    `${flag("auto_refresh_access_token", "Auto-refresh logins", "If a Codex, Claude or Antigravity login expires, retries the request once and saves the updated login. Restart the proxy to apply.")}
     ${otherLogins(v, flag)}`
  )}`;
}
/// Providers that can accept logins other than the ones saved in Coport.
const OTHER_LOGIN_PROVIDERS = [
  ["codex", "Codex", "Accepts any ChatGPT login and routes it by the account its token names."],
  ["claude", "Claude", "Accepts any Claude login and looks up its account to route it."],
  ["antigravity", "Antigravity", "Accepts any Google login and looks up its email to route it."],
  ["grok", "Grok", "Accepts any xAI login and routes it by the account its token names."],
];
/// One "Accept other logins" disclosure: its summary names the providers that
/// accept them, its rows switch `account_auth_file_only` off per provider.
function otherLogins(v, flag) {
  const on = OTHER_LOGIN_PROVIDERS.filter(([id]) => !v[`${id}.account_auth_file_only`]);
  const status = on.length === 0 ? "None"
    : on.length === OTHER_LOGIN_PROVIDERS.length ? "All"
    : on.map(([, name]) => name).join(", ");
  return `<details class="disclosure" id="other-logins" ${ui.otherLoginsOpen ? "open" : ""}>
    <summary data-tip="By default, Coport only accepts the logins saved in it and rejects others, such as a login from another device or account.">${ICON.chevron}Accept other logins<span class="disclosure-value">${status}</span></summary>
    ${OTHER_LOGIN_PROVIDERS.map(([id, name, tip]) => flag(`${id}.account_auth_file_only`, name, tip, true)).join("")}
  </details>`;
}
/// Saves a number field when it differs from the configuration. Called on
/// focus loss rather than `change`: a re-render recreates the focused field,
/// which resets the baseline `change` compares against.
async function commitConfigField(input) {
  const key = input.dataset.config;
  const text = input.value.trim();
  const value = Number(text);
  if (text === "" || !Number.isFinite(value)) {
    toast("Enter a number");
    render();
  } else if (value !== ui.snap.config.details?.editable[key]) {
    await setConfigValue(key, value);
  }
}
async function setConfigValue(key, value) {
  try {
    await invoke("set_config_value", { key, value });
  } catch (e) {
    toast(String(e));
  }
  await refresh();
}
