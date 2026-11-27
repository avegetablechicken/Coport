"use strict";

/// One renderer for data-tip, HTML titles and SVG titles on every platform.
/// Native titles are held while open to prevent a second desktop tooltip.
/// Keyboard focus shows one as well; a click does not.
/// `hide()` closes the tooltip; `refresh()` follows its owner through a
/// render that replaced it, or closes it when the owner is gone.
function createRouteTooltip({ document, window }) {
  let routeTooltipTimer;
  let routeTooltipOwner;
  let routeTooltipMode;
  let routeTooltipPointer;
  let routeTooltipIdentity;
  let routeTooltipAnchor;
  const heldTooltipTitles = new Map();
  function tooltipText(owner) {
    return owner.dataset?.tip ?? owner.getAttribute?.("title") ?? heldTooltipTitles.get(owner)?.text
      ?? owner.querySelector?.(":scope > title")?.textContent;
  }
  function tooltipOwner(node) {
    if (node?.nodeType === 3) node = node.parentElement;
    for (; node; node = node.parentElement) {
      if (tooltipText(node) != null) return tooltipText(node) ? node : null;
    }
    return null;
  }
  // Use stable controls/scopes where present, then a structural path within
  // that scope. Tooltip text and status classes may change on every refresh.
  function tooltipIdentity(owner) {
    if (owner.dataset.tipKey) return JSON.stringify(["key", owner.dataset.tipKey]);
    const parts = [];
    for (let node = owner; node; node = node.parentElement) {
      const anchor = node.id ? ["id", node.id]
        : node.dataset.tipScope ? ["scope", node.dataset.tipScope]
        : node.dataset.morph ? ["morph", node.dataset.morph] : null;
      if (anchor) { parts.push(anchor); break; }
      if (node.dataset.tipSlot || node.dataset.slice) {
        parts.push([node.tagName, node.dataset.tipSlot ?? node.dataset.slice]);
      } else if (node.dataset.action) {
        parts.push([node.tagName, ...["action", "name", "target", "id", "key", "service"].map(key => node.dataset[key] ?? "")]);
      } else if (node.matches?.(".stat")) {
        parts.push(["stat", node.querySelector(".stat-label")?.textContent]);
      } else {
        parts.push([node.tagName, node.parentElement ? [...node.parentElement.children].indexOf(node) : 0]);
      }
    }
    return JSON.stringify(parts);
  }
  function holdTooltipTitles(owner) {
    for (let node = owner; node; node = node.parentElement) {
      const title = node.getAttribute?.("title");
      if (title != null) {
        heldTooltipTitles.set(node, { ...heldTooltipTitles.get(node), text: title, title });
        node.removeAttribute("title");
      }
      const shape = node.querySelector?.(":scope > title");
      if (shape) {
        const held = heldTooltipTitles.get(node);
        heldTooltipTitles.set(node, { ...held, text: held?.title ?? shape.textContent, shape });
        shape.remove();
      }
    }
  }
  function releaseTooltipTitles() {
    for (const [node, { title, shape }] of heldTooltipTitles) {
      if (shape) {
        if (!node.querySelector(":scope > title")) node.prepend(shape);
      }
      if (title != null && !node.hasAttribute("title")) node.setAttribute("title", title);
    }
    heldTooltipTitles.clear();
  }
  function hideRouteTooltip() {
    clearTimeout(routeTooltipTimer);
    routeTooltipOwner?.removeAttribute("aria-describedby");
    routeTooltipOwner = null;
    $("route-tooltip").hidden = true;
    routeTooltipIdentity = null;
    routeTooltipAnchor = null;
    releaseTooltipTitles();
  }
  function routeTooltipPosition(anchor, size, width, height) {
    const margin = 12;
    const left = Math.max(margin, Math.min(anchor.left, width - size.width - margin));
    const above = anchor.top - size.height - 6;
    const top = Math.max(margin, Math.min(above >= margin ? above : anchor.bottom + 6, height - size.height - margin));
    return { left, top };
  }
  function paintRouteTooltip() {
    const owner = routeTooltipOwner;
    if (!owner?.isConnected) return hideRouteTooltip();
    holdTooltipTitles(owner);
    const tip = $("route-tooltip");
    const text = tooltipText(owner);
    if (!text) return hideRouteTooltip();
    if (tip.textContent !== text) tip.textContent = text;
    tip.hidden = false;
    tip.style.visibility = "hidden";
    const anchor = owner.getBoundingClientRect();
    routeTooltipAnchor = anchor;
    const height = document.documentElement.clientHeight;
    // Long notes scroll on the roomier side instead of covering their trigger
    // and causing a pointerout/show loop.
    tip.style.maxHeight = `${Math.max(0, anchor.top - 18, height - anchor.bottom - 18)}px`;
    const { left, top } = routeTooltipPosition(anchor, tip.getBoundingClientRect(), document.documentElement.clientWidth, height);
    tip.style.left = `${left}px`;
    tip.style.top = `${top}px`;
    tip.style.visibility = "visible";
    owner.setAttribute("aria-describedby", "route-tooltip");
  }
  function transferRouteTooltip(owner) {
    routeTooltipOwner?.removeAttribute("aria-describedby");
    releaseTooltipTitles();
    routeTooltipOwner = owner;
    holdTooltipTitles(owner);
    if (!$("route-tooltip").hidden) paintRouteTooltip();
  }
  // Let the pointer cross the six-pixel gap to read or scroll a long note.
  function tooltipBridge(point) {
    const tip = $("route-tooltip");
    if (!routeTooltipOwner || tip.hidden || !point) return false;
    const anchor = routeTooltipOwner.isConnected ? routeTooltipOwner.getBoundingClientRect() : routeTooltipAnchor;
    if (!anchor) return false;
    const box = tip.getBoundingClientRect();
    return point.x >= Math.min(anchor.left, box.left) && point.x <= Math.max(anchor.right, box.right)
      && ((box.bottom <= anchor.top && point.y >= box.bottom && point.y <= anchor.top)
        || (box.top >= anchor.bottom && point.y >= anchor.bottom && point.y <= box.top));
  }
  // Keep both visible and pending tooltips across replacement of any panel
  // region, including the top bar, native titles and SVG chart titles.
  function refreshRouteTooltip() {
    const owner = routeTooltipOwner;
    if (!owner) return;
    if (owner.isConnected) {
      holdTooltipTitles(owner);
      if (!$("route-tooltip").hidden) paintRouteTooltip();
      return;
    }
    const hit = routeTooltipPointer && document.elementFromPoint(routeTooltipPointer.x, routeTooltipPointer.y);
    const overTip = (hit && $("route-tooltip").contains(hit)) || tooltipBridge(routeTooltipPointer);
    const next = routeTooltipMode === "focus" || overTip
      ? [$("top"), $("content")].filter(Boolean).flatMap(root => [...root.querySelectorAll("[data-tip], [title], svg title")])
          .map(el => el.tagName.toLowerCase() === "title" ? el.parentElement : el)
          .find(el => tooltipIdentity(el) === routeTooltipIdentity)
      : tooltipOwner(hit);
    if (!next || tooltipIdentity(next) !== routeTooltipIdentity
        || next.getBoundingClientRect().width === 0) return hideRouteTooltip();
    transferRouteTooltip(next);
    if (routeTooltipMode === "focus") next.focus({ preventScroll: true });
  }
  function showRouteTooltip(owner, mode = "pointer") {
    routeTooltipMode = mode;
    if (routeTooltipOwner === owner) return;
    if (routeTooltipOwner && !routeTooltipOwner.isConnected
        && tooltipIdentity(owner) === routeTooltipIdentity) {
      transferRouteTooltip(owner);
      return;
    }
    hideRouteTooltip();
    routeTooltipOwner = owner;
    routeTooltipIdentity = tooltipIdentity(owner);
    holdTooltipTitles(owner);
    routeTooltipTimer = setTimeout(paintRouteTooltip, owner.matches(".chart-bar, [data-slice]") ? 100 : 500);
  }

  return {
    hide: hideRouteTooltip,
    refresh: refreshRouteTooltip,
    /// Shows a tooltip on hover or keyboard focus, and closes it when the
    /// pointer or focus leaves, on scrolling, resizing, a press elsewhere,
    /// Escape or when the panel loses focus.
    install() {
      document.addEventListener("pointermove", (event) => {
        routeTooltipPointer = { x: event.clientX, y: event.clientY };
        // WebKit may not send pointerout for a newly replaced hovered node.
        if (routeTooltipOwner?.isConnected && routeTooltipMode === "pointer"
            && !routeTooltipOwner.contains(event.target) && !$("route-tooltip").contains(event.target)
            && !tooltipBridge(routeTooltipPointer)) {
          if (routeTooltipOwner.matches(":focus-visible")) routeTooltipMode = "focus";
          else hideRouteTooltip();
        }
      });
      for (const type of ["pointerover", "focusin"]) {
        document.addEventListener(type, (event) => {
          const owner = tooltipOwner(event.target);
          if (type === "focusin" && !owner?.matches(":focus-visible")) return;
          if (type === "pointerover") routeTooltipPointer = { x: event.clientX, y: event.clientY };
          if (owner) showRouteTooltip(owner, type === "focusin" ? "focus" : "pointer");
        });
      }
      for (const type of ["pointerout", "focusout"]) {
        document.addEventListener(type, (event) => {
          if ($("route-tooltip").contains(event.target)) {
            if (!$("route-tooltip").contains(event.relatedTarget) && !routeTooltipOwner?.contains(event.relatedTarget)) hideRouteTooltip();
            return;
          }
          const owner = tooltipOwner(event.target);
          if (!owner || owner !== routeTooltipOwner || owner.contains(event.relatedTarget)) return;
          if (!owner.isConnected) return;
          if (type === "pointerout" && $("route-tooltip").contains(event.relatedTarget)) return;
          if (type === "pointerout" && tooltipBridge({ x: event.clientX, y: event.clientY })) return;
          // Some engines dispatch focusout just before innerHTML detaches the
          // focused node. Let that replacement finish before deciding to close.
          if (type === "focusout") {
            queueMicrotask(() => {
              if (owner !== routeTooltipOwner) return;
              if (!owner.isConnected) return refreshRouteTooltip();
              if (owner.matches(":hover")) routeTooltipMode = "pointer";
              else hideRouteTooltip();
            });
            return;
          }
          if (type === "pointerout" && owner.matches(":focus-visible")) { routeTooltipMode = "focus"; return; }
          hideRouteTooltip();
        });
      }
      document.addEventListener("scroll", (event) => {
        if (event.target !== $("route-tooltip")) hideRouteTooltip();
      }, true);
      window.addEventListener("resize", hideRouteTooltip);
      window.addEventListener("pointerdown", event => {
        if (!$("route-tooltip").contains(event.target)) hideRouteTooltip();
      }, true);
      window.addEventListener("blur", hideRouteTooltip);
      window.addEventListener("keydown", event => {
        if (event.key === "Escape") hideRouteTooltip();
      });
    },
  };
}
