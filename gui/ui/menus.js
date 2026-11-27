"use strict";

// Native select popups can take OS focus away from the tray window, which
// dismisses the panel. Keep every selector's options inside the WebView.
function panelSelect(id, options, selected, label, cls = "", attrs = "") {
  const value = String(selected);
  const text = options.find(([v]) => String(v) === value)?.[1] ?? "";
  return `<button type="button" id="${id}" class="select ${cls}" value="${esc(value)}"
    data-options="${esc(JSON.stringify(options))}" aria-label="${esc(label)}"
    aria-haspopup="listbox" aria-expanded="false" ${attrs}>${esc(text)}</button>`;
}

/// The panel's one open menu, a select's options or an action menu: placed
/// below its trigger, and closed by a choice, Escape, a press elsewhere,
/// scrolling or resizing. `rerender()` redraws what a render held back while
/// the menu was open.
function createMenus({ document, window, rerender }) {
  let open = null;
  let renderPending = false;

  function close(restoreFocus = false) {
    if (!open) return;
    const { trigger, menu } = open;
    open = null;
    trigger.setAttribute("aria-expanded", "false");
    trigger.removeAttribute("aria-controls");
    menu.remove();
    if (restoreFocus && trigger.isConnected) trigger.focus();
    // Live traffic can refresh while a choice is being made. Replace controls
    // only after the menu closes, so its trigger and options stay mounted.
    if (renderPending) {
      renderPending = false;
      queueMicrotask(rerender);
    }
  }
  function showSelect(trigger, last = false) {
    const wasOpen = open?.trigger === trigger;
    close();
    if (wasOpen) return;
    const options = JSON.parse(trigger.dataset.options);
    const menu = document.createElement("div");
    menu.id = "panel-select-options";
    menu.className = "select-menu";
    menu.setAttribute("role", "listbox");
    menu.setAttribute("aria-label", trigger.getAttribute("aria-label"));
    menu.innerHTML = options.map(([value, label]) => `<button type="button" role="option"
      tabindex="-1" value="${esc(value)}" aria-selected="${String(value) === trigger.value}">${esc(label)}</button>`).join("");
    // At least as wide as its trigger before it is placed against it.
    menu.style.minWidth = `${trigger.getBoundingClientRect().width}px`;
    place(trigger, menu);
    const choices = [...menu.children];
    (last ? choices.at(-1) : choices.find((el) => el.value === trigger.value) ?? choices[0]).focus();
    menu.addEventListener("click", (event) => {
      const option = event.target.closest('[role="option"]');
      if (!option) return;
      const changed = trigger.value !== option.value;
      trigger.value = option.value;
      trigger.textContent = option.textContent;
      close(true);
      if (changed) trigger.dispatchEvent(new Event("change", { bubbles: true }));
    });
  }
  /// Opens `menu` below `trigger`, right-aligned and kept inside the panel.
  function place(trigger, menu) {
    open = { trigger, menu };
    trigger.setAttribute("aria-expanded", "true");
    trigger.setAttribute("aria-controls", menu.id);
    document.body.append(menu);
    // One highlight, as in a native menu: the item under the pointer takes
    // focus, so the one focused on opening or by keys does not stay lit.
    menu.addEventListener("pointermove", (event) => {
      const item = event.target.closest?.("button");
      if (item && item !== document.activeElement) item.focus({ preventScroll: true });
    });
    const rect = trigger.getBoundingClientRect();
    menu.style.maxHeight = `${Math.max(0, window.innerHeight - 8)}px`;
    const bounds = menu.getBoundingClientRect();
    menu.style.left = `${Math.max(4, Math.min(rect.right - bounds.width, window.innerWidth - bounds.width - 4))}px`;
    menu.style.top = `${Math.max(4, Math.min(rect.bottom + 3, window.innerHeight - bounds.height - 4))}px`;
  }
  /// An action menu: placed and dismissed like a select, but each item runs its
  /// own `data-action`. `null` entries become separators.
  function showMenu(trigger, last = false) {
    const wasOpen = open?.trigger === trigger;
    close();
    if (wasOpen) return;
    const menu = document.createElement("div");
    menu.id = "panel-select-options";
    menu.className = "select-menu action-menu";
    menu.setAttribute("role", "menu");
    menu.setAttribute("aria-label", trigger.getAttribute("aria-label"));
    menu.innerHTML = JSON.parse(trigger.dataset.menu).map((item) => item?.heading
      ? `<div class="menu-heading" role="presentation"><strong>${esc(item.heading)}</strong>${item.detail ? `<span>${esc(item.detail)}</span>` : ""}</div>`
      : item
      ? `<button type="button" role="${item.checked == null ? "menuitem" : "menuitemradio"}" tabindex="-1" data-action="${esc(item.action)}"${
        item.target ? ` data-target="${esc(item.target)}"` : ""}${item.checked == null ? "" : ` aria-checked="${item.checked}"`}>${esc(item.label)}</button>`
      : '<div class="menu-separator" role="separator"></div>').join("");
    place(trigger, menu);
    const options = items(menu);
    (last ? options.at(-1) : options.find((el) => el.getAttribute("aria-checked") === "true") ?? options[0])?.focus();
    // Close before the document's click handler runs the item's action.
    menu.addEventListener("click", (event) => {
      if (event.target.closest("[data-action]")) close(true);
    });
  }
  function items(menu) {
    return [...menu.querySelectorAll("button")];
  }

  return {
    /// The open menu's trigger and menu, or null.
    current: () => open,
    /// Whether a menu is open, so a render must wait: it then runs
    /// `rerender` once the menu closes.
    holdRender() {
      if (!open) return false;
      renderPending = true;
      return true;
    },
    close,
    showSelect,
    showMenu,
    items,
    /// Closes the menu on a press outside it, a scroll elsewhere or a resize.
    install() {
      document.addEventListener("pointerdown", (event) => {
        if (open && !open.menu.contains(event.target) && !open.trigger.contains(event.target)) close();
      });
      document.addEventListener("scroll", (event) => {
        if (open && !open.menu.contains(event.target)) close();
      }, true);
      window.addEventListener("resize", () => close());
    },
  };
}
