"use strict";

/// Keeps keyboard focus across renders: before a render, where focus is
/// among the controls of #top and #content; after it, the same control again
/// or the one now in its place. Reads only which page is built and shown.

/// What Tab moves through in the panel.
const FOCUSABLE = "button, select, input, textarea, summary, a[href], [tabindex]";
/// Attributes that change with what a control shows rather than which one it
/// is, as a toggle's state or a tooltip's text.
const FOCUS_VOLATILE = new Set(["data-tip", "data-text", "data-enabled", "data-options", "data-menu", "data-seq", "data-morph"]);
/// Names a control by what it does rather than by node, so the same one can be
/// found again after a render rebuilds the markup around it.
function focusName(el) {
  const named = [...el.attributes].filter((a) =>
    a.name === "id" || a.name === "name" || a.name === "type" || a.name === "role" || (a.name.startsWith("data-") && !FOCUS_VOLATILE.has(a.name)));
  return [el.tagName, ...named.map((a) => `${a.name}=${a.value}`).sort()].join(" ");
}
/// The controls in `root` Tab stops on, in order: not those inside a hidden
/// element or a closed <details> other than its summary, which the panel
/// does not render.
function tabStops(root) {
  return [...(root?.querySelectorAll(FOCUSABLE) ?? [])].filter((c) => !c.disabled && c.getAttribute("tabindex") !== "-1" && rendered(c));
}
function rendered(c) {
  if (c.closest("[hidden]")) return false;
  for (let details = c.closest("details"); details; details = details.parentElement?.closest("details")) {
    if (!details.open && !(c.tagName === "SUMMARY" && c.parentElement === details)) return false;
  }
  return true;
}
/// Where focus is among the panel's rebuilt parts: the page, the container,
/// the control's name and which of the controls so named it is, and its
/// position among all of them. Null outside them.
function focusPlace(el) {
  const root = el?.closest?.("#top, #content");
  if (!root || el === root) return null;
  const all = tabStops(root);
  const name = focusName(el);
  return { page: ui.builtPage, root: root.id, name, nth: all.filter((c) => focusName(c) === name).indexOf(el), index: all.indexOf(el) };
}
/// Puts focus back on the control it was on before a render of the same page
/// replaced it, so Tab goes on from there; else on the control now in its
/// place. Focus kept by the render itself (a field being typed in) stays, and
/// another page starts afresh.
function restoreFocus(place) {
  const active = document.activeElement;
  if (!place || place.page !== ui.page || (active && active !== document.body && active.isConnected)) return;
  const all = tabStops($(place.root));
  if (!all.length) return;
  const same = all.filter((c) => focusName(c) === place.name);
  const target = same[Math.max(0, place.nth)] ?? same[0] ?? all[Math.min(Math.max(0, place.index), all.length - 1)];
  target.focus({ preventScroll: true });
}
