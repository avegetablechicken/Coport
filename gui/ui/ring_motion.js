"use strict";

/// Share rings that move from what the ring of the same source last showed
/// whenever a render rebuilds them. `still()` says when a ring takes its
/// new shape at once: during a page change, with reduced motion, or while
/// the panel is hidden. `morphMs` is how long a ring takes to move.
function createRingMotion({ document, morphMs, still }) {
  // Every render rebuilds the rings. One whose numbers changed (a refresh, or
  // All and Models) moves its slices from what the ring of the same source last
  // showed: each grows or shrinks to its new part, new ones grow in and gone ones
  // shrink away. A render in the middle carries the motion on in the new ring.
  const ringShown = new Map();
  const RING_SVG = "http://www.w3.org/2000/svg";
  /// The `d` each slice ends with, as trafficShare drew it; also marks the rings
  /// already taken up.
  const ringFinal = new WeakMap();
  const ringEase = cubicBezier(0.32, 0.72, 0, 1);
  function cubicBezier(x1, y1, x2, y2) {
    const at = (a, b, t) => 3 * a * t * (1 - t) ** 2 + 3 * b * t * t * (1 - t) + t ** 3;
    return (x) => {
      let lo = 0;
      let hi = 1;
      for (let i = 0; i < 24; i++) {
        const mid = (lo + hi) / 2;
        if (at(x1, x2, mid) < x) lo = mid;
        else hi = mid;
      }
      return at(y1, y2, (lo + hi) / 2);
    };
  }
  function ringLayout(ring) {
    return [...ring.querySelectorAll("path[data-slice]:not([data-ghost])")].map((p) => ({
      key: p.dataset.slice, slice: p.dataset.slice, width: Number(p.dataset.width), color: p.style.getPropertyValue("--slice").trim(),
    }));
  }
  function sameRing(a, b) {
    return a.length === b.length && a.every((s, i) => s.key === b[i].key && s.color === b[i].color && Math.abs(s.width - b[i].width) < 1e-6);
  }
  function ringProgress(motion, now) {
    return Math.min(1, Math.max(0, (now - motion.start) / morphMs));
  }
  /// The layout a ring shows at `now`, by slice: mid-motion, each slice where it
  /// is drawn, in the color it is heading to.
  function ringAt(shown, now) {
    if (!shown.motion) return shown.layout;
    const progress = ringProgress(shown.motion, now);
    if (progress >= 1) return shown.layout;
    const parts = blendRing(shown.motion.from, shown.motion.to, ringEase(progress));
    const staying = new Set(parts.filter((s) => !s.gone).map((s) => s.slice));
    return parts.filter((s) => s.to - s.from > 1e-6).map((s) => {
      const key = s.gone && staying.has(s.slice) ? `${s.slice}|gone` : s.slice;
      return { key, slice: key, width: s.to - s.from, color: s.toColor ?? s.fromColor };
    });
  }
  /// Where each source's ring sits below the top of its row, and its move there:
  /// a longer or shorter legend recenters the ring, which then slides from where
  /// it was drawn rather than jumping.
  const ringPlace = new Map();
  const RING_EASING = "cubic-bezier(0.32, 0.72, 0, 1)";
  /// How far below its row's top a ring of `place` is drawn at `now`.
  function ringOffset(place, now) {
    const progress = Math.min(1, Math.max(0, (now - place.start) / morphMs));
    return place.top + (place.from - place.top) * (1 - ringEase(progress));
  }
  function placeRing(ring, source, now, still) {
    const row = ring.closest(".traffic-share");
    const box = ring.getBoundingClientRect();
    if (!row || !box.height) return;
    const top = box.top - row.getBoundingClientRect().top;
    const place = ringPlace.get(source);
    const at = place ? ringOffset(place, now) : top;
    // A ring redrawn mid-move heading the same place carries on with it.
    const going = !still && place && place.from !== place.top && Math.abs(place.top - top) < 0.5 && now - place.start < morphMs;
    if (!going && (still || Math.abs(at - top) < 0.5)) {
      ringPlace.set(source, { top, from: top, start: now });
      return;
    }
    const next = going ? place : { top, from: at, start: now };
    ringPlace.set(source, next);
    ring.animate([{ transform: `translateY(${next.from - top}px)` }, { transform: "none" }],
      { duration: morphMs, delay: next.start - now, easing: RING_EASING });
  }
  function showRing(ring, now) {
    const source = ring.dataset.ring;
    if (ringFinal.has(ring)) return;
    ringFinal.set(ring, true);
    placeRing(ring, source, now, still());
    const layout = ringLayout(ring);
    for (const path of ring.querySelectorAll("path[data-slice]")) ringFinal.set(path, path.getAttribute("d"));
    const shown = ringShown.get(source);
    if (shown?.motion && ringProgress(shown.motion, now) < 1 && sameRing(shown.layout, layout)) {
      shown.motion.rings.push(ring);
      drawRing(shown.motion, ring, now);
      runRing(source, shown.motion);
      return;
    }
    const from = shown && ringAt(shown, now);
    // The first ring of a source, a page change (its copies show the result)
    // and reduced motion show the numbers at once.
    if (!from || sameRing(from, layout) || still()) {
      if (shown?.motion) cancelAnimationFrame(shown.motion.frame);
      ringShown.set(source, { layout, motion: null });
      return;
    }
    if (shown.motion) cancelAnimationFrame(shown.motion.frame);
    const [start, end] = matchRing(from, layout);
    const motion = { from: start, to: end, start: now, rings: [ring], frame: 0 };
    ringShown.set(source, { layout, motion });
    drawRing(motion, ring, now);
    runRing(source, motion);
  }
  /// Draws `motion`'s rings each frame until it ends or none is shown.
  function runRing(source, motion) {
    if (motion.frame) return;
    const step = (time) => {
      motion.frame = 0;
      motion.rings = motion.rings.filter((r) => r.isConnected);
      for (const r of motion.rings) drawRing(motion, r, time);
      if (ringProgress(motion, time) >= 1) {
        if (ringShown.get(source)?.motion === motion) ringShown.get(source).motion = null;
      } else if (motion.rings.length) motion.frame = requestAnimationFrame(step);
    };
    motion.frame = requestAnimationFrame(step);
  }
  /// Draws `ring` as `motion` shows it at `now`; at the end, as it was rendered.
  function drawRing(motion, ring, now) {
    const svg = ring.querySelector("svg");
    if (!svg) return;
    const progress = ringProgress(motion, now);
    const all = [...svg.querySelectorAll("path[data-slice]")];
    if (progress >= 1) {
      for (const path of all) {
        if (path.hasAttribute("data-ghost")) path.remove();
        else {
          delete path.dataset.blend;
          path.style.removeProperty("fill");
          if (ringFinal.has(path)) path.setAttribute("d", ringFinal.get(path));
        }
      }
      return;
    }
    // Paths already moving go by their part of the motion, the rest by slice.
    const moving = new Map(all.filter((p) => p.dataset.blend).map((p) => [p.dataset.blend, p]));
    const drawn = new Map(all.filter((p) => !p.dataset.blend && !p.hasAttribute("data-ghost")).map((p) => [p.dataset.slice, p]));
    const eased = ringEase(progress);
    const outer = SHARE_RING_SIZE / 2;
    for (const s of blendRing(motion.from, motion.to, eased)) {
      let path = moving.get(s.key) ?? (s.gone ? null : drawn.get(s.slice));
      if (!path) {
        // A slice the numbers no longer have.
        path = document.createElementNS(RING_SVG, "path");
        path.dataset.slice = s.slice;
        path.setAttribute("data-ghost", "");
      }
      path.dataset.blend = s.key;
      svg.append(path);
      path.setAttribute("d", ringSector(s.from, s.to, outer, outer * SHARE_RING_INNER));
      const fill = !s.fromColor || !s.toColor || s.fromColor === s.toColor
        ? s.toColor ?? s.fromColor
        : `color-mix(in srgb, ${s.toColor} ${(eased * 100).toFixed(1)}%, ${s.fromColor})`;
      path.style.setProperty("fill", fill || "var(--share-unidentified)");
    }
  }

  return {
    show: showRing,
    place: placeRing,
    /// How far below its row's top the ring of `source` is drawn at `now`.
    offset: (source, now) => ringOffset(ringPlace.get(source), now),
    /// Moves each share ring `content` gains from what it last showed.
    install(content) {
      new MutationObserver((records) => {
        const now = performance.now();
        for (const record of records) {
          for (const node of record.addedNodes) {
            if (node.nodeType !== Node.ELEMENT_NODE) continue;
            const rings = node.matches(".share-ring[data-ring]") ? [node] : node.querySelectorAll(".share-ring[data-ring]");
            for (const ring of rings) if (ring.isConnected) showRing(ring, now);
          }
        }
      }).observe(content, { childList: true, subtree: true });
    },
  };
}
