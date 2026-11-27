"use strict";

/// The Home page: the local proxy or forwarding to a Device, how to connect
/// each App, Home's traffic, recent requests, the proxies and the routing
/// table. Like the other pages it keeps no state of its own: what it shows is
/// `ui`'s and `trafficViews`'.

function main() {
  if (ui.snap.forwarding) {
    return remoteForwardingBlock() + connectBlock() + trafficBlock() + `<details class="disclosure"><summary>${ICON.chevron}Local proxy configuration and history (inactive)</summary>${[recentBlock(), proxiesBlock(), routingBlock()].join("")}</details>`;
  }
  return [proxyBlock(), connectBlock(), trafficBlock(), recentBlock(), proxiesBlock(), routingBlock()].join("");
}
function remoteForwardingBlock() {
  const target = ui.snap.forwarding;
  const health = target.health || {};
  const state = health.state || "checking";
  const label = { connected: "Connected", disconnected: "Disconnected · retrying", checking: "Checking connection…" }[state] || "Status unavailable";
  const stamp = value => value ? new Date(value).toLocaleTimeString() : "Not yet";
  let body = `<div class="row"><span class="row-label">${esc(target.name)}</span><span class="state">${label}</span></div><div class="row"><span class="row-label selectable">127.0.0.1:${ui.snap.phase.port}</span><span class="state">Client address unchanged</span></div><div class="row"><span class="row-label" data-tip="Includes SSH authentication and remote helper startup; not network ping">SSH connection time</span><span class="row-value">${health.latencyMs == null ? "—" : `${health.latencyMs} ms`}</span></div><div class="row"><span class="row-label">Last verified connection</span><span class="row-value">${esc(stamp(health.lastConnectedAt))}</span></div>`;
  if (target.error) body += message("warn", `${esc(target.error)} Automatic retries keep remote routing selected; requests never fall back to local routes.`);
  else if (state === "checking") body += message("info", "Checking the saved remote destination. Requests stay in remote mode.");
  else if (health.recoveredAt && Date.now() - health.recoveredAt < 120000) body += message("info", `Connection restored at ${esc(stamp(health.recoveredAt))}.`);
  const traffic = target.traffic || {};
  body += `<div class="block-head"><span class="block-title">This forwarding session</span></div><div class="row"><span class="row-label">Connections / active / failed</span><span class="row-value">${traffic.connections || 0} / ${traffic.active || 0} / ${traffic.failures || 0}</span></div><div class="row"><span class="row-label">Upload / download</span><span class="row-value">${fmtBytes(traffic.uploadBytes || 0)} / ${fmtBytes(traffic.downloadBytes || 0)}</span></div><div class="row"><span class="row-label">Upload / download speed</span><span class="row-value">${fmtBytes(traffic.uploadBytesPerSecond || 0)}/s / ${fmtBytes(traffic.downloadBytesPerSecond || 0)}/s</span></div>`;
  return block("SSH forwarding", "", body, `device/${target.deviceId}`);
}
function proxyBlock() {
  const s = ui.snap;
  const state = s.phase.state;
  // Failed covers start and stop errors and lost daemon contact; the error says which.
  const label = s.phase.busy ? "Updating…" : { running: "Running", stopped: "Stopped", failed: "Status unavailable" }[state];
  const head = `<span class="state" data-morph="proxy-state"><span class="dot ${state}"></span>${label}</span>
    <button class="switch" role="switch" aria-checked="${state === "running"}" data-action="power" ${s.phase.busy ? "disabled" : ""} aria-label="Start or stop the proxy"></button>`;
  let messages = "";
  if (!s.config.exists) {
    messages += message(
      "warn",
      "No configuration file yet.",
      `<button class="btn" data-action="create-config">Create from Example</button>
       <button class="btn" data-action="import-config">Import…</button>`
    );
  } else if (s.config.error) {
    messages += message("bad", esc(s.config.error), `<button class="btn" data-action="open" data-target="config">Edit Configuration</button>`);
  } else if (s.phase.error) {
    messages += message("bad", esc(s.phase.error));
  }
  if (s.config.changedSinceStart) {
    messages += message("warn", "The configuration changed since the proxy started.", `<button class="btn" data-action="restart">Restart to Apply</button>`);
  }
  if (s.proxyCa) messages += proxyCaMessage(s.proxyCa);
  const strip = `<div class="strip">
    ${stat("Address", `<span data-morph="address">127.0.0.1:${s.phase.port}</span>`)}
    ${stat("Uptime", state === "running" ? `<span id="uptime">${fmtUptime(uptime())}</span>` : "—")}
    ${stat("Config", s.config.error ? "Invalid" : s.config.exists ? "Valid" : "Missing", s.config.error ? "bad" : "")}
  </div>`;
  return block("Proxy", head, strip + messages, "this-device");
}
/// Hosts the trusted proxy CA does not cover stay undecrypted until the user
/// replaces it. States what still works; once kept, only a short note remains.
function proxyCaMessage(ca) {
  const hosts = ca.uncovered.map((a) => `<strong>${esc(a.replace(/:443$/, ""))}</strong>`).join(", ");
  const replace = (label, kind) => `<button class="${kind}" data-action="proxy-ca-replace">${label}</button>`;
  if (ca.kept) {
    const text = ca.uncovered.length
      ? `Proxy port skips ${hosts}; base URLs still work.`
      : "The proxy CA isn't limited to its hosts.";
    return message("info", text, replace("Replace CA…", "text-link"));
  }
  const others = ca.decrypted.length;
  const text = ca.uncovered.length
    ? `Proxy CA doesn't cover ${hosts}.<span class="message-note">Still working: base URLs${others ? ` and the proxy port for ${others} other host${others === 1 ? "" : "s"}` : ""}. Proxy-port clients can't reach ${ca.uncovered.length === 1 ? "this host" : "these hosts"}.${ca.unrestricted ? " The CA also isn't limited to its hosts." : ""}</span>`
    : `<strong>Proxy CA isn't limited to its hosts.</strong><span class="message-note">Everything still works, but it could vouch for any IP address.</span>`;
  return message("warn", text, `${replace("Replace &amp; Trust…", "btn")}<button class="btn" data-action="proxy-ca-keep">Keep Current CA</button>`);
}
/// Home shows the forwarded device's traffic while SSH forwarding is on, else this machine's.
function homeTrafficDevice() {
  return ui.snap?.forwarding?.deviceId ?? null;
}
async function loadHomeTraffic(force = false) {
  if (await trafficViews.loadHome(homeTrafficDevice(), force) && ui.page === "main") render();
}
function trafficBlock() {
  // Traffic read for another source (this machine or a former destination) is not shown.
  const st = trafficViews.home && (trafficViews.home.device ?? null) === homeTrafficDevice() ? trafficViews.home : null;
  const range = trafficControls("home-traffic", trafficViews.minutes, "Home traffic time range");
  const error = trafficViews.homeError === "TRAFFIC_UPDATING" ? "" : trafficViews.homeError || st?.remoteError || "";
  if (!st) return block("Traffic", range, `<div class="placeholder">${esc(error || "Loading traffic…")}</div>`, "traffic");
  return block(
    "Traffic",
    range,
    `<div class="traffic-content" aria-busy="${trafficViews.homeLoading}">
     ${error ? `<span class="traffic-update-error" title="${esc(error)}">Update failed</span>` : ""}
     ${trafficSummary(st, st.scope, st, st.device ? `device/${st.device}` : "local")}
     ${st.device ? `<div class="placeholder">Includes other clients on ${esc(st.deviceName)}. Updated ${esc(new Date(st.fetchedAt).toLocaleTimeString())}${Date.now() - st.fetchedAt > 90000 || st.remoteError ? " · stale" : ""}.</div>` : ""}</div>`,
    "traffic"
  );
}
function connectBlock() {
  const s = ui.snap;
  const row = (name, service, url) => `<div class="row">
      <span class="row-label"><span class="connect-app">${serviceMark(service)}${name}</span></span>
      <span class="row-value selectable">${esc(url)}</span>
      <button class="icon-btn" data-action="copy" data-text="${esc(url)}" data-tip="Copy" aria-label="Copy ${name} base URL">${ICON.copy}</button>
    </div>`;
  return block(
    "Connect",
    "",
    `${row("Codex", "Codex", s.urls.codex)}${row("Claude", "Claude", s.urls.claude)}${row("Antigravity", "Antigravity", s.urls.antigravity)}${row("Grok", "Grok", s.urls.grok)}
     <details class="disclosure" id="setup-snippets" ${ui.snippetsOpen ? "open" : ""}><summary>${ICON.chevron}Setup snippets</summary>
       ${snippet("Top of ~/.codex/config.toml, then restart Codex. Codex 0.156+ requires an HTTPS chatgpt_base_url", `openai_base_url = "${s.urls.base}/v1"\nchatgpt_base_url = "${s.urls.chatgpt}"`)}
       ${snippet("CA for the HTTPS chatgpt_base_url: add to the system trust store, or set SSL_CERT_FILE in ~/.codex/.env", s.urls.caCertificate)}
       ${snippet("~/.claude/settings.json, merged into existing settings", `{\n  "env": {\n    "ANTHROPIC_BASE_URL": "${s.urls.claude}"\n  }\n}`)}
       ${snippet("Antigravity CLI (agy): environment, so its Google login requests come here", agyEnvironment(s))}
       ${snippet(grokAccountRouting(s) ? "~/.grok/config.toml, so Grok Build's requests, sign-in and token renewal come here; then sign in with grok login --device-auth" : "~/.grok/config.toml, so Grok Build's login and API key requests come here", grokConfig(s))}
       ${ui.snap.config.details?.editable?.connect_credential_routing ? `
       ${snippet("Proxy port for clients using their original API URLs; set HTTPS_PROXY to the listener", `HTTPS_PROXY=${s.urls.base}\nNODE_EXTRA_CA_CERTS=${s.urls.proxyCaCertificate}\nCODEX_CA_CERTIFICATE=${s.urls.proxyCaCertificate}`)}` : ""}
     </details>`
  );
}
function snippet(label, code) {
  return `<div class="snippet"><div class="snippet-label">${label}</div><div class="snippet-code"><pre>${esc(code)}</pre>
    <button class="icon-btn" data-action="copy" data-text="${esc(code)}" aria-label="Copy snippet">${ICON.copy}</button></div></div>`;
}
// agy reads these only from its environment. Its token refresh has no URL
// setting: with the proxy port, agy's own proxy and CA variables send it here
// without HTTPS_PROXY reaching the commands it runs.
function agyEnvironment(s) {
  const lines = [
    `CLOUD_CODE_URL=${s.urls.antigravity}`,
    `USER_INFO_OVERRIDE_URL=${s.urls.antigravity}/https://www.googleapis.com/oauth2/v2/userinfo`,
    `AICODE_ENDPOINT_URL=${s.urls.antigravity}/https://aicode.googleapis.com`,
  ];
  if (s.config.details?.editable?.connect_credential_routing) {
    lines.push(`AGY_LLM_GATEWAY_PROXY_URL=${s.urls.base}`, `AGY_CA_CERT="${s.urls.proxyCaCertificate}"`);
  }
  return lines.join("\n");
}
// With account routes, Grok Build's issuer points here too, so its token
// renewal takes the login's route without HTTPS_PROXY. The issuer names the
// key Grok Build files its login under, so a device sign-in follows.
function grokAccountRouting(s) {
  const grok = s.config.details?.grok;
  return !!grok && (grok.accountRoutes.length > 0 || grok.fallbacks.some((f) => (f.key === "accountFallback" || f.key === "fallback") && f.proxies));
}
function grokConfig(s) {
  const lines = ["[endpoints]", `cli_chat_proxy_base_url = "${s.urls.grok}"`, `xai_api_base_url = "${s.urls.grok}"`];
  if (grokAccountRouting(s)) lines.push("", "[auth.oauth2]", `issuer = "${s.urls.base}/grok/https://auth.x.ai"`);
  return lines.join("\n");
}
function recentBlock() {
  const rows = ui.recent;
  const aside = rows.length ? `<button class="text-link" data-action="page" data-page="activity" data-target="log">View All</button>` : "";
  const body = rows.length
    ? rows.map((e) => requestRow(e, false)).join("")
    : `<div class="placeholder">${ui.snap.phase.state === "running" ? "Waiting for the first request…" : "Start the proxy to see requests."}</div>`;
  return block("Recent Requests", aside, body, "log");
}
function proxiesBlock() {
  const d = ui.snap.config.details;
  if (!d) return "";
  const aside = d.proxies.length ? `<button class="text-link" data-action="probe">Test All</button>` : "";
  const address = (p) => p.endpoint.replace(/^\w+:\/\//, "");
  const order = new Intl.Collator("en", { numeric: true, sensitivity: "base" });
  const body = d.proxies.length
    ? [...d.proxies]
        .sort((a, b) => order.compare(address(a), address(b)))
        .map((p) => {
          const probe = p.probe;
          const state = probe?.state ?? "idle";
          // A reachable proxy with failing destinations is not fully available.
          const routeFailing = state === "ok" && p.routeFailures?.length > 0;
          const dot = routeFailing ? "warn" : ({ ok: "running", error: "failed" }[state] ?? "");
          let value = state === "pending" ? "" : `<span class="faint">—</span>`;
          if (state === "ok") value = probe.ms == null ? "Available" : `${probe.ms} ms`;
          else if (state === "error") value = `<span class="bad">Unreachable</span>`;
          else if (state === "unavailable") value = "Unverified";
          // Plain HTTP is the common case; other schemes stay visible.
          const shown = p.endpoint.replace(/^http:\/\//, "");
          const mark = flag(probe?.country);
          const exit =
            p.local && probe?.exitIp
              ? ` → ${mark ? `<span class="flag" data-tip="${esc(probe.country)}">${mark}</span> ` : ""}${esc(probe.exitIp)}`
              : "";
          return `<div class="row proxy" data-tip-scope="proxy:${esc(p.name)}">
            <span class="dot ${dot}" ${probe?.error ? `data-tip="${esc(probe.error)}"` : ""}></span>
            <span class="proxy-heading">${tag(p.name)}<span class="row-value">${value}</span></span>
            <button class="icon-btn${state === "pending" ? " probing" : ""}" data-action="probe" data-name="${esc(p.name)}" data-tip="Test" aria-label="Test ${esc(p.name)}" aria-busy="${state === "pending"}">${ICON.restart}</button>
            <span class="row-sub">${esc(shown)}${exit}</span>
            ${routeFailing ? `<span class="row-sub proxy-route-warning">Route unavailable: ${p.routeFailures.map(esc).join(", ")}</span>` : ""}
          </div>`;
        })
        .join("")
    : `<div class="placeholder">No proxies defined; every route connects directly.</div>`;
  return block("Outbound Proxies", aside, body);
}
/// Special-purpose rules, shown de-emphasized below the regular routes.
const MINOR_FALLBACKS = new Set(["mcpFallback", "accountProbe"]);
const FALLBACK_LABEL = {
  accountFallback: "Account fallback",
  fallback: "Default fallback",
  mcpFallback: "Docs MCP fallback",
  accountProbe: "Account probe",
};
// Presentation only: selectors remain intact in configuration. Account names
// are shown shortened so the panel does not reveal a full identity.
function routeIdentity(selector, source) {
  if (source === "account") {
    return { name: maskAccountName(selector), kind: "account", type: "Account" };
  }
  if (source === "provider" || source === "profile") {
    return { name: selector, kind: "proxy", type: source === "provider" ? "Provider configuration" : "Profile configuration" };
  }
  if (source === "gateway") {
    try {
      const url = new URL(selector.startsWith("//") ? `https:${selector}` : /^[a-z]+:\/\//i.test(selector) ? selector : `https://${selector}`);
      return { name: url.host, kind: "gateway", type: "Gateway" };
    } catch (_) { /* Preserve an unrecognized selector verbatim. */ }
  }
  return source === "api_key"
    ? { name: selector, kind: "key", type: "API key" }
    : { name: selector, kind: "file", type: "Configuration (unresolved)" };
}
function routingChain(names) {
  return `<span class="chain">${names.map((name) => `<span class="route-proxy${name.length > 14 ? " route-proxy-long" : ""}" title="${esc(name === "none" ? "direct" : name)}">${tag(name)}</span>`).join('<span class="arrow">→</span>')}</span>`;
}
function routingRow(route, account, fileOnly = false, service = "") {
  const identity = routeIdentity(route.selector, route.kind || (account ? "account" : "unknown"));
  const warning = {
    remote: "No match in the local OAuth account cache. The account probe confirmed this account is available.",
    unknown: "No match in the local OAuth account cache. Its account is not identified yet; background discovery or its first request identifies it.",
  }[route.activation] ?? "No match in the local OAuth account cache. The account probe could not confirm availability. Check the credential and account probe connection.";
  const needsWarning = ["unknown", "remote", "probe_failed"].includes(route.activation);
  const activation = !account || !fileOnly ? ""
    : (route.activation === "inactive"
      ? '<span class="route-activation bad" title="No configured local credential currently selects this account route. Sign in or configure its credential source.">Inactive</span>' : "")
      + (route.activation === "probe_failed" ? '<span class="route-activation bad">Probe failed</span>' : "")
      + (needsWarning ? `<span class="route-activation route-warning tip-wide" tabindex="0" role="img" aria-label="${esc(warning)}" data-tip="${esc(warning)}">${ICON.warning}</span>` : "");
  if (account || route.kind === "api_key") {
    return `<div class="route-summary route-static" data-tip-scope="${tooltipScopeKey(JSON.stringify(["route", service, account, route.kind, route.selector]))}">
      <span class="route-identity"><span class="route-icon" aria-hidden="true">${ROUTE_ICON[identity.kind]}</span><span class="route-name" data-full-name="${esc(identity.name)}">${esc(identity.name)}</span>${activation}</span>
      <span class="route-destination">${routingChain(route.proxies)}</span>
    </div>`;
  }
  const extra = route.detail && route.detail !== route.selector ? route.detail : "";
  const title = `${identity.type}: ${route.selector}${extra ? `\n${extra}` : ""}`;
  return `<div class="route-summary" data-tip-scope="${tooltipScopeKey(JSON.stringify(["route", service, account, route.kind, route.selector]))}" title="${esc(title)}">
    <span class="route-identity"><span class="route-icon" aria-hidden="true">${ROUTE_ICON[identity.kind]}</span><span class="route-name">${esc(identity.name)}</span></span>
    <span class="route-destination">${routingChain(route.proxies)}</span>
  </div>`;
}
function routingBlock() {
  const s = ui.snap;
  const d = s.config.details;
  if (!d) return "";
  const report = Object.fromEntries(s.routes.map((r) => [r.name.toLowerCase(), r]));
  const section = (name, svc) => {
    const r = report[name.toLowerCase()];
    const checked = r?.checkedAt ? `Last checked: ${new Date(r.checkedAt * 1000).toLocaleTimeString()}` : "";
    const badge = r && !r.ok
      ? `<span class="bad credential-warning" tabindex="0" data-tip="${esc(`${r.reason ?? ""}\n${checked}`)}">Credential warning</span>` : "";
    const configured = svc.configured || svc.fallbacks.some((f) => f.proxies);
    if (!configured) {
      return `<div class="subhead"><span>${name}</span><span class="faint">Not configured</span></div>`;
    }
    const rows = svc.accountRoutes.map((route) => routingRow(route, true, svc.fileOnly, name)).join("")
      + svc.apiKeyRoutes.map((route) => routingRow(route, false, false, name)).join("");
    const fallbacks = svc.fallbacks.filter((f) => f.proxies)
      .sort((a, b) => MINOR_FALLBACKS.has(a.key) - MINOR_FALLBACKS.has(b.key))
      .map((f) => `<div class="route-default" data-tip-slot="${f.key}"><span class="route-default-name">${FALLBACK_LABEL[f.key]}</span><span class="route-destination">${routingChain(f.proxies)}</span></div>`).join("");
    return `<div class="routing-service" data-tip-scope="routing:${name}"><div class="subhead"><span>${name}</span>${badge}</div>${rows}
      ${fallbacks ? `<div class="route-defaults"><div class="route-defaults-title">Defaults &amp; helpers</div>${fallbacks}</div>` : ""}</div>`;
  };
  const accountError = s.accountStatesError ? `<div class="placeholder">Account status unavailable: ${esc(s.accountStatesError)}</div>` : "";
  const credentialError = s.credentialError ? `<div class="placeholder">Credential checks unavailable: ${esc(s.credentialError)}</div>` : "";
  return block("Routing", "", credentialError + accountError + section("Codex", d.codex) + section("Claude", d.claude) + section("Antigravity", d.antigravity) + section("Grok", d.grok));
}

/// Shows a routing table name's full text as its title when it is cut short.
function installHomePage() {
  document.addEventListener("pointerover", (event) => {
    const name = event.target.closest?.(".route-static .route-name");
    if (!name) return;
    if (name.scrollWidth > name.clientWidth || name.textContent !== name.dataset.fullName) {
      name.title = name.dataset.fullName;
    } else {
      name.removeAttribute("title");
    }
  });
}
