//! The tray panel window: shown next to the tray icon, sized to its content,
//! hidden when it loses focus.

use crate::placement::{self, Rect};
use std::{
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tauri::{AppHandle, Emitter, Manager, PhysicalPosition, WebviewWindow, WindowEvent};

pub const LABEL: &str = "panel";
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
    if anchor.is_some() {
        *panel.anchor.lock().unwrap() = anchor;
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

/// Resizes the panel to the frontend's content height and re-anchors it.
pub fn fit(app: &AppHandle, height: f64) {
    let panel = state(app);
    *panel.content_height.lock().unwrap() = Some(height);
    if let Some(w) = window(app) {
        layout(&w, &panel);
    }
}

/// Sizes and positions the window on the monitor holding the anchor.
fn layout(w: &WebviewWindow, panel: &PanelState) {
    let anchor = *panel.anchor.lock().unwrap();
    let monitor = anchor
        .and_then(|a| {
            let (x, y) = a.center();
            w.available_monitors().ok()?.into_iter().find(|m| {
                let (p, s) = (m.position(), m.size());
                x >= p.x as f64
                    && x < p.x as f64 + s.width as f64
                    && y >= p.y as f64
                    && y < p.y as f64 + s.height as f64
            })
        })
        .or_else(|| w.primary_monitor().ok().flatten())
        .or_else(|| w.current_monitor().ok().flatten());
    let Some(m) = monitor else {
        return;
    };
    let scale = m.scale_factor();
    let (p, s) = (m.position(), m.size());
    let screen = Rect {
        x: p.x as f64,
        y: p.y as f64,
        w: s.width as f64,
        h: s.height as f64,
    };
    let max_height = (screen.h / scale * 0.85).max(MIN_HEIGHT);
    let height = panel
        .content_height
        .lock()
        .unwrap()
        .unwrap_or(560.0)
        .clamp(MIN_HEIGHT, max_height);
    let _ = w.set_size(tauri::LogicalSize::new(WIDTH, height));
    let (x, y) = placement::place(anchor, (WIDTH * scale, height * scale), screen, scale);
    let _ = w.set_position(PhysicalPosition::new(x, y));
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
                    let current = state(&handle).generation.load(Ordering::SeqCst) == token;
                    let focused = window(&handle).is_some_and(|w| w.is_focused().unwrap_or(false));
                    if current && !focused {
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
