"use strict";

/// The form that adds or edits a device in Settings: its draft, and the
/// device the draft saves. SSH devices keep no HTTP fields and HTTP devices
/// no SSH connection, whatever was typed before the connection changed.
function createDeviceForm() {
  const form = {
    open: false,
    draft: null,

    add() {
      form.draft = { id: null, transport: "ssh" };
      form.open = true;
    },
    edit(d) {
      form.draft = { id: d.id, name: d.name, host: d.ssh?.host, binary: d.ssh?.binary,
        url: d.data?.url, key: d.data?.tokenFile, env: d.data?.tokenEnv, ca: d.data?.caCertificate, transport: d.data?.transport || (d.ssh ? "ssh" : "http") };
      form.open = true;
    },
    close() {
      form.open = false;
      form.draft = null;
    },
    /// Takes the shown fields into the draft; `value(id)` is a field's value,
    /// or undefined while it is not shown. Fields not shown keep their draft.
    capture(value) {
      if (value("device-name") === undefined) return;
      const d = form.draft || {};
      form.draft = { ...d, name: value("device-name"), transport: value("device-transport"),
        host: value("device-host") ?? d.host ?? "", binary: value("device-binary") ?? d.binary ?? "",
        url: value("device-url") ?? d.url ?? "", key: value("device-key-file") ?? d.key ?? "",
        env: value("device-key-env") ?? d.env ?? "", ca: value("device-ca-file") ?? d.ca ?? "" };
    },
    /// The device the draft saves. An empty name defaults to the SSH host or HTTP URL.
    device() {
      const d = form.draft;
      const ssh = d.transport === "ssh";
      return { id: d.id || null, name: d.name.trim() || (ssh ? d.host.trim() : d.url.trim()),
        ssh: ssh ? { host: d.host.trim(), binary: d.binary.trim() } : null,
        data: { transport: d.transport, url: ssh ? "" : d.url.trim(), tokenFile: ssh ? null : d.key.trim() || null, tokenEnv: ssh ? null : d.env.trim() || null, caCertificate: ssh ? null : d.ca.trim() || null } };
    },
  };
  return form;
}

if (typeof module !== "undefined") module.exports = { createDeviceForm };
