"use strict";

/// Settings turns in like the next page and turns back out. Between the other
/// pages, what both show keeps its identity: an element with the same
/// `data-morph` name on both moves to its new place (a block also reshapes),
/// and the rest crossfades. A layer over the panel draws both pages meanwhile;
/// the live page underneath is already the new one, hidden until the end.
const REDUCED_MOTION = matchMedia("(prefers-reduced-motion: reduce)");
/// Keep the Linux panel responsive to direct clicks; its full-width turns and
/// shared-element morphs use a shorter cadence than the macOS presentation.
const TURN_MS = navigator.userAgent.includes("Linux") ? 170 : 340;
const MORPH_MS = navigator.userAgent.includes("Linux") ? 210 : 420;
const MOTION_EASE = "cubic-bezier(0.32, 0.72, 0, 1)";
/// Inherited text styles a moving element loses outside its page.
const MORPH_TEXT = ["color", "font-family", "font-size", "font-weight", "line-height", "letter-spacing", "white-space", "font-variant-numeric"];

/// The named elements in view or beside it, with their place on screen.
function morphTargets(region, view) {
  const found = new Map();
  for (const el of region.querySelectorAll("[data-morph]")) {
    const rect = el.getBoundingClientRect();
    if (!el.dataset.morph || !rect.width || !rect.height) continue;
    // A closed disclosure still lays out its content in WebKit, unseen.
    if (el.closest("details:not([open])") && !el.closest("summary")) continue;
    const near = rect.bottom > view.top - view.height && rect.top < view.bottom + view.height;
    if (!near) continue;
    const block = el.classList.contains("block");
    let text = null;
    let size = 1;
    if (!block) {
      const style = getComputedStyle(el);
      text = MORPH_TEXT.map((name) => [name, style.getPropertyValue(name)]);
      size = parseFloat(style.fontSize) || 1;
    }
    found.set(el.dataset.morph, { el, rect, block, text, size });
  }
  return found;
}
function motionLayer(className, rect, origin) {
  const el = document.createElement("div");
  el.className = className;
  if (rect) place(el, rect, origin);
  return el;
}
function place(el, rect, origin, height = false) {
  Object.assign(el.style, {
    position: "absolute", margin: "0",
    left: `${rect.left - origin.left}px`, top: `${rect.top - origin.top}px`, width: `${rect.width}px`,
    ...(height ? { height: `${rect.height}px` } : {}),
  });
}
// Snapshot row geometry while the original page is laid out.
function motionRows(region, view) {
  // The native window may grow during the transition. Keep enough rows for a
  // full monitor, even when the old page used a short viewport.
  const extent = Math.max(view.height, globalThis.screen?.height || 0);
  return [...region.querySelectorAll(".req")].map(el => {
    const rect = el.getBoundingClientRect();
    return { height: rect.height, keep: rect.bottom > view.top - extent && rect.top < view.top + extent * 2 };
  });
}
// Activity can contain 500 log rows. The animation copies need their geometry,
// but not the thousands of text nodes outside the moving viewport. Only trim
// copies (or the detached old page); the live page keeps every row and control.
function trimMotionRows(copy, rows) {
  let spacer = null;
  let height = 0;
  [...copy.querySelectorAll(".req")].forEach((el, i) => {
    const row = rows[i];
    if (!row || row.keep || !row.height || el.parentElement.id !== "activity-list") {
      spacer = null;
      return;
    }
    // The log list is normal block flow without row margins. Adjacent omitted
    // rows can share one exact-height spacer; expanded details break the run.
    if (spacer && spacer.nextSibling === el) {
      height += row.height;
      spacer.style.height = `${height}px`;
      el.remove();
    } else {
      spacer = document.createElement("div");
      height = row.height;
      spacer.style.height = `${height}px`;
      el.replaceWith(spacer);
    }
  });
}
/// A copy of a named element to move, without its name; named elements inside
/// it move on their own.
function morphCopy(target) {
  // Shared elements can travel vertically across the viewport; keep their
  // complete contents. Only the fixed-height page copies are trimmed below.
  const copy = target.el.cloneNode(true);
  copy.removeAttribute("data-morph");
  copy.removeAttribute("data-morph-paired");
  return copy;
}

/// Page changes in motion, and the panel's height fitted to its page: the
/// window resizes only before or after a page change moves. `ui` says which
/// page is built and shown; `invoke` asks the app to fit the panel.
function createPageMotion({ document, ui, invoke }) {
  let fitQueued = false;
  let lastHeight = 0;
  /// The page change in motion (changePage), if any.
  let pageMotion = null;
  function panelHeight(page) {
    const style = getComputedStyle($("content"));
    const padding = parseFloat(style.paddingTop) + parseFloat(style.paddingBottom);
    return Math.ceil($("top").offsetHeight + page.offsetHeight + padding);
  }
  function fitPanel(height) {
    if (Math.abs(height - lastHeight) <= 1) return;
    lastHeight = height;
    invoke("fit_panel", { height });
  }
  function queueFit() {
    // A page change resizes the window only before or after its motion.
    if (pageMotion) { pageMotion.fit = true; return; }
    if (fitQueued) return;
    fitQueued = true;
    requestAnimationFrame(() => {
      fitQueued = false;
      const page = $("content").firstElementChild;
      if (!page) return;
      if (pageMotion) pageMotion.fit = true;
      else fitPanel(panelHeight(page));
    });
  }
  // #content fills the window, so in-page growth (e.g. an opened disclosure)
  // only shows up on the page element, which each render replaces.
  const fitObserver = new ResizeObserver(queueFit);
  let observedPage = null;

  function endPageMotion() {
    const motion = pageMotion;
    if (!motion) return;
    pageMotion = null;
    motion.layer.remove();
    $("top").style.visibility = $("content").style.visibility = "";
    if (motion.fit) queueFit();
  }

  /// Shows `update`'s page change in motion. `update` renders the next page
  /// and scrolls it; what it shows afterwards is where everything lands.
  function changePage(update) {
    endPageMotion();
    const top = $("top");
    const content = $("content");
    const from = ui.builtPage;
    const oldPage = content.firstElementChild;
    if (!oldPage || REDUCED_MOTION.matches || document.hidden) return update();
    const topRect = top.getBoundingClientRect();
    const contentRect = content.getBoundingClientRect();
    const oldHead = [...top.childNodes];
    const oldPageRect = oldPage.getBoundingClientRect();
    const oldRows = motionRows(oldPage, contentRect);
    const before = { head: morphTargets(top, topRect), body: morphTargets(content, contentRect) };
    update();
    const page = content.firstElementChild;
    // A render held back (an open menu) changed nothing.
    if (!page || page === oldPage || ui.builtPage === from) return;
    const turn = from === "settings" || ui.builtPage === "settings";
    const panelRect = $("panel").getBoundingClientRect();
    const after = { head: morphTargets(top, topRect), body: morphTargets(content, contentRect) };
    // Measure the live page while layout is already current. Reading its height
    // after mounting the animated copies forces WebKitGTK to lay them all out
    // synchronously, blocking the first animation frame on long Activity pages.
    const newRows = motionRows(page, contentRect);
    const pageRect = page.getBoundingClientRect();
    const height = panelHeight(page);

    const layer = motionLayer("page-motion");
    const head = motionLayer("page-motion-head", { left: panelRect.left, top: topRect.top, width: panelRect.width }, panelRect);
    head.style.height = `${topRect.height}px`;
    const body = motionLayer("page-motion-body", { left: panelRect.left, top: contentRect.top, width: panelRect.width }, panelRect);
    layer.append(head, body);

    // Elements named on both pages, at least one of them in view, move; their
    // places in the page copies stay empty.
    const pairs = [];
    if (!turn) {
      for (const [region, origin, into] of [["head", topRect, head], ["body", contentRect, body]]) {
        const inView = (rect) => rect.bottom > origin.top && rect.top < origin.bottom;
        for (const [name, next] of after[region]) {
          const prev = before[region].get(name);
          if (!prev || !(inView(prev.rect) || inView(next.rect))) continue;
          prev.el.setAttribute("data-morph-paired", "");
          next.el.setAttribute("data-morph-paired", "");
          pairs.push({ prev, next, origin: { left: panelRect.left, top: origin.top }, into });
        }
      }
    }

    const oldTop = motionLayer("top page-motion-page");
    oldTop.append(...oldHead);
    const newTop = top.cloneNode(true);
    newTop.removeAttribute("id");
    newTop.className = "top page-motion-page";
    const newPage = page.cloneNode(true);
    newPage.classList.add("page-motion-page");
    oldPage.classList.add("page-motion-page");
    for (const [el, rect] of [[oldTop, topRect], [newTop, topRect]]) place(el, rect, { left: panelRect.left, top: topRect.top }, true);
    place(oldPage, oldPageRect, { left: panelRect.left, top: contentRect.top });
    place(newPage, pageRect, { left: panelRect.left, top: contentRect.top });
    head.append(oldTop, newTop);
    body.append(oldPage, newPage);

    const duration = turn ? TURN_MS : MORPH_MS;
    const timing = { duration, easing: MOTION_EASE, fill: "both" };
    const pending = [];
    const animate = (el, frames, options = timing) => pending.push({ el, frames, options });
    if (turn) {
      // Into Settings the next page comes from the right; back out it leaves there.
      const shift = (ui.builtPage === "settings" ? 1 : -1) * panelRect.width;
      for (const el of [oldTop, oldPage]) animate(el, [{ transform: "none", opacity: 1 }, { transform: `translateX(${-shift}px)`, opacity: 0.4 }]);
      for (const el of [newTop, newPage]) animate(el, [{ transform: `translateX(${shift}px)` }, { transform: "none" }]);
    } else {
      for (const el of [oldTop, oldPage]) animate(el, [{ opacity: 1 }, { opacity: 0 }], { ...timing, duration: duration * 0.6 });
      for (const el of [newTop, newPage]) animate(el, [{ opacity: 0 }, { opacity: 1 }], { ...timing, delay: duration * 0.2, duration: duration * 0.8 });
      // Blocks first, so a moving ring or row is never covered by a block passing it.
      pairs.sort((a, b) => b.next.block - a.next.block
        || (a.next.el.compareDocumentPosition(b.next.el) & Node.DOCUMENT_POSITION_FOLLOWING ? -1 : 1));
      for (const pair of pairs) morph(pair, animate, timing);
    }
    for (const { next } of pairs) next.el.removeAttribute("data-morph-paired");
    trimMotionRows(oldPage, oldRows);
    trimMotionRows(newPage, newRows);

    top.style.visibility = content.style.visibility = "hidden";
    $("panel").append(layer);
    const motion = { layer, fit: true };
    pageMotion = motion;
    // A taller page gets its room before moving in; a shorter one gives it back after.
    if (height > lastHeight) fitPanel(height);
    // Mount every copy before starting animations, so WebKit starts the effects
    // together on the next rendering update rather than on detached elements.
    const animations = pending.map(({ el, frames, options }) => el.animate(frames, options));
    // A cancelled animation ends the motion too, or the page stays hidden.
    Promise.all(animations.map((a) => a.finished)).catch(() => {}).then(() => {
      if (pageMotion === motion) endPageMotion();
    });
  }
  /// Moves one named element from its old place to its new one. The same
  /// markup moves as one piece; otherwise the old copy fades into the new one
  /// on the way. A block's box reshapes around both.
  function morph({ prev, next, origin, into }, animate, timing) {
    const a = morphCopy(prev);
    const b = morphCopy(next);
    const same = a.isEqualNode(b);
    const fade = motionLayer("morph-fade");
    if (prev.block) {
      const box = motionLayer("morph-box", prev.rect, origin);
      box.style.height = `${prev.rect.height}px`;
      const frame = (rect) => ({ left: `${rect.left - origin.left}px`, top: `${rect.top - origin.top}px`, width: `${rect.width}px`, height: `${rect.height}px` });
      animate(box, [frame(prev.rect), frame(next.rect)]);
      a.style.width = `${prev.rect.width}px`;
      b.style.width = `${next.rect.width}px`;
      box.append(fade);
      into.append(box);
    } else {
      // Text keeps its page's look; the copy is placed and sized like the original.
      for (const [copy, target] of [[a, prev], [b, next]]) {
        for (const [name, value] of target.text) copy.style.setProperty(name, value);
        place(copy, target.rect, origin, true);
        // Fractional widths must not wrap the text.
        copy.style.width = `${Math.ceil(target.rect.width)}px`;
      }
      const dx = next.rect.left - prev.rect.left;
      const dy = next.rect.top - prev.rect.top;
      // Text grows or shrinks to the next page's size; boxes may differ in width.
      const scale = next.size / prev.size;
      animate(a, [{ transform: "none" }, { transform: `translate(${dx}px, ${dy}px) scale(${scale})` }]);
      animate(b, [{ transform: `translate(${-dx}px, ${-dy}px) scale(${1 / scale})` }, { transform: "none" }]);
      into.append(fade);
    }
    a.classList.add("morph-copy");
    b.classList.add("morph-copy");
    if (same) {
      fade.append(b);
      return;
    }
    // Fading with the motion's easing settles with the place.
    fade.append(a, b);
    animate(a, [{ opacity: 1 }, { opacity: 0 }]);
    animate(b, [{ opacity: 0 }, { opacity: 1 }]);
  }

  return {
    changePage,
    queueFit,
    /// Whether a page change is in motion.
    moving: () => !!pageMotion,
    /// Fits the panel when `page`, the element each render replaces, grows
    /// or shrinks from within.
    watchPage(page) {
      if (page === observedPage) return;
      if (observedPage) fitObserver.unobserve(observedPage);
      if (page) fitObserver.observe(page);
      observedPage = page;
    },
    /// Fits the panel when #content, which fills the window, resizes.
    install() {
      fitObserver.observe($("content"));
    },
  };
}
