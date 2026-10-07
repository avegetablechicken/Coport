//! The tray panel window: shown next to the tray icon, sized to its content,
//! hidden when it loses focus.

use crate::placement::{self, Rect};
use std::{
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tauri::{AppHandle, Emitter, Manager, Monitor, WebviewWindow, WindowEvent};

pub const LABEL: &str = "panel";
/// macOS places windows in points, shared by screens of different scales;
/// elsewhere layout uses physical pixels.
const IN_POINTS: bool = cfg!(target_os = "macos");
/// Panel width in logical points; the height follows the content.
const WIDTH: f64 = 360.0;
const MIN_HEIGHT: f64 = 240.0;
/// Focus loss hides the panel after this delay unless a tray click intervenes:
/// clicking the icon of an open panel must close it, not close and reopen it.
const DISMISS_DELAY: Duration = Duration::from_millis(100);

#[derive(Default)]
pub struct PanelState {
    /// Bumped by every show and tray click; stale dismissals compare unequal.
    generation: AtomicU64,
    /// Set while a dialog opened from the panel holds focus.
    modal: AtomicBool,
    anchor: Mutex<Option<Rect>>,
    content_height: Mutex<Option<f64>>,
}

fn window(app: &AppHandle) -> Option<WebviewWindow> {
    app.get_webview_window(LABEL)
}

fn state(app: &AppHandle) -> tauri::State<'_, PanelState> {
    app.state::<PanelState>()
}

/// Shows the panel next to `anchor` (tray icon rectangle or click point).
pub fn show(app: &AppHandle, anchor: Option<Rect>) {
    let panel = state(app);
    panel.generation.fetch_add(1, Ordering::SeqCst);
    if let Some(anchor) = anchor {
        *panel.anchor.lock().unwrap() = Some(layout_anchor(app, anchor));
    }
    let Some(w) = window(app) else {
        return;
    };
    layout(&w, &panel);
    let _ = w.show();
    let _ = w.set_focus();
    let _ = w.emit("panel-shown", ());
}

pub fn hide(app: &AppHandle) {
    if let Some(w) = window(app) {
        let _ = w.hide();
        let _ = w.emit("panel-hidden", ());
    }
}

pub fn toggle(app: &AppHandle, anchor: Option<Rect>) {
    state(app).generation.fetch_add(1, Ordering::SeqCst);
    let visible = window(app).is_some_and(|w| w.is_visible().unwrap_or(false));
    if visible {
        hide(app);
    } else {
        show(app, anchor);
    }
}

/// Runs a blocking dialog (off the main thread) without the focus loss
/// dismissing the panel, then returns focus to the panel.
pub fn with_modal<T>(app: &AppHandle, dialog: impl FnOnce() -> T) -> T {
    state(app).modal.store(true, Ordering::SeqCst);
    let result = dialog();
    state(app).modal.store(false, Ordering::SeqCst);
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || show(&handle, None));
    result
}

/// Resizes the panel to the frontend's content height and re-anchors it.
pub fn fit(app: &AppHandle, height: f64) {
    let panel = state(app);
    *panel.content_height.lock().unwrap() = Some(height);
    if let Some(w) = window(app) {
        layout(&w, &panel);
    }
}

/// The tray's rectangle in layout units, taken while the pointer is still on
/// the clicked menu bar.
#[cfg(target_os = "macos")]
fn layout_anchor(app: &AppHandle, anchor: Rect) -> Rect {
    let primary = app.primary_monitor().ok().flatten();
    let mut monitors = app.available_monitors().unwrap_or_default();
    if let Some(primary) = &primary {
        monitors.sort_by_key(|m| m.position() != primary.position());
    }
    let screens: Vec<_> = monitors
        .iter()
        .map(|m| (bounds(m), m.scale_factor()))
        .collect();
    // tao reports the pointer in physical pixels of the primary screen.
    let scale = primary.map_or(1.0, |m| m.scale_factor());
    let pointer = app
        .cursor_position()
        .ok()
        .map(|p| (p.x / scale, p.y / scale));
    placement::status_item_points(anchor, &screens, pointer)
}

#[cfg(not(target_os = "macos"))]
fn layout_anchor(_: &AppHandle, anchor: Rect) -> Rect {
    anchor
}

/// A monitor's rectangle in layout units.
fn bounds(m: &Monitor) -> Rect {
    let unit = if IN_POINTS { m.scale_factor() } else { 1.0 };
    let (p, s) = (m.position(), m.size());
    Rect {
        x: p.x as f64 / unit,
        y: p.y as f64 / unit,
        w: s.width as f64 / unit,
        h: s.height as f64 / unit,
    }
}

/// Sizes and positions the window on the monitor holding the anchor.
fn layout(w: &WebviewWindow, panel: &PanelState) {
    let anchor = *panel.anchor.lock().unwrap();
    let monitor = anchor
        .and_then(|a| {
            let (x, y) = a.center();
            w.available_monitors()
                .ok()?
                .into_iter()
                .find(|m| bounds(m).contains(x, y))
        })
        .or_else(|| w.primary_monitor().ok().flatten())
        .or_else(|| w.current_monitor().ok().flatten());
    let Some(m) = monitor else {
        return;
    };
    let screen = bounds(&m);
    // Layout units per logical point.
    let unit = if IN_POINTS { 1.0 } else { m.scale_factor() };
    let max_height = (screen.h / unit * 0.85).max(MIN_HEIGHT);
    let height = panel
        .content_height
        .lock()
        .unwrap()
        .unwrap_or(560.0)
        .clamp(MIN_HEIGHT, max_height);
    let _ = w.set_size(tauri::LogicalSize::new(WIDTH, height));
    let (x, y) = placement::place(anchor, (WIDTH * unit, height * unit), screen, unit);
    let _ = if IN_POINTS {
        w.set_position(tauri::LogicalPosition::new(x, y))
    } else {
        w.set_position(tauri::PhysicalPosition::new(x, y))
    };
}

pub fn on_window_event(w: &tauri::Window, event: &WindowEvent) {
    if w.label() != LABEL {
        return;
    }
    let app = w.app_handle().clone();
    match event {
        WindowEvent::Focused(false) => {
            let token = state(&app).generation.load(Ordering::SeqCst);
            std::thread::spawn(move || {
                std::thread::sleep(DISMISS_DELAY);
                let handle = app.clone();
                let _ = app.run_on_main_thread(move || {
                    let panel = state(&handle);
                    let current = panel.generation.load(Ordering::SeqCst) == token;
                    let modal = panel.modal.load(Ordering::SeqCst);
                    let focused = window(&handle).is_some_and(|w| w.is_focused().unwrap_or(false));
                    if current && !modal && !focused {
                        hide(&handle);
                    }
                });
            });
        }
        WindowEvent::CloseRequested { api, .. } => {
            api.prevent_close();
            hide(&app);
        }
        _ => {}
    }
}
