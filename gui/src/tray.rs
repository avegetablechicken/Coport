//! Menu bar (macOS) / notification area (Windows) / StatusNotifierItem (Linux).
//!
//! A left click toggles the panel; the context menu (right click, or any
//! click on GNOME) only offers the essentials.

use crate::{
    AppState,
    icon::{self, Status},
    panel,
    placement::Rect,
    proxy::Phase,
};
use std::sync::Mutex;
use tauri::{
    AppHandle, Manager, Wry,
    image::Image,
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent},
};

/// What the tray currently shows, so updates only touch what changed.
#[derive(Clone, PartialEq)]
struct View {
    status: Status,
    headline: String,
    stats: String,
    running: bool,
}

pub struct Tray {
    icon: TrayIcon,
    headline: MenuItem<Wry>,
    stats: MenuItem<Wry>,
    open: MenuItem<Wry>,
    toggle: MenuItem<Wry>,
    restart: MenuItem<Wry>,
    quit: MenuItem<Wry>,
    view: Mutex<Option<View>>,
}

pub fn create(app: &AppHandle) -> tauri::Result<()> {
    let item = |id: &str, enabled: bool| MenuItem::with_id(app, id, "", enabled, None::<&str>);
    let headline = item("headline", false)?;
    let stats = item("stats", false)?;
    let open = item("open", true)?;
    let toggle = item("toggle", true)?;
    let restart = item("restart", true)?;
    let quit = item("quit", true)?;
    let menu = Menu::with_items(
        app,
        &[
            &headline,
            &stats,
            &PredefinedMenuItem::separator(app)?,
            &open,
            &PredefinedMenuItem::separator(app)?,
            &toggle,
            &restart,
            &PredefinedMenuItem::separator(app)?,
            &quit,
        ],
    )?;
    let icon = TrayIconBuilder::with_id("main")
        .icon(image(Status::Stopped))
        .icon_as_template(cfg!(target_os = "macos"))
        .tooltip("Coding Agent Proxy")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "open" => {
                let anchor = app.state::<Tray>().rect();
                panel::show(app, anchor);
            }
            "toggle" => {
                let state = app.state::<AppState>();
                let mut core = state.core.lock().unwrap();
                if core.controller.is_running() {
                    core.stop();
                } else {
                    core.start();
                }
            }
            "restart" => app.state::<AppState>().core.lock().unwrap().start(),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                rect,
                position,
                ..
            } = event
            {
                let p = rect.position.to_physical::<f64>(1.0);
                let s = rect.size.to_physical::<f64>(1.0);
                let anchor = if s.width > 0.0 {
                    Rect {
                        x: p.x,
                        y: p.y,
                        w: s.width,
                        h: s.height,
                    }
                } else {
                    // Some StatusNotifierItem hosts report only the click point.
                    Rect {
                        x: position.x,
                        y: position.y,
                        w: 0.0,
                        h: 0.0,
                    }
                };
                panel::toggle(tray.app_handle(), Some(anchor));
            }
        })
        .build(app)?;
    app.manage(Tray {
        icon,
        headline,
        stats,
        open,
        toggle,
        restart,
        quit,
        view: Mutex::new(None),
    });
    sync(app);
    Ok(())
}

impl Tray {
    /// The icon's on-screen rectangle, when the platform reports one.
    pub fn rect(&self) -> Option<Rect> {
        let r = self.icon.rect().ok().flatten()?;
        let p = r.position.to_physical::<f64>(1.0);
        let s = r.size.to_physical::<f64>(1.0);
        Some(Rect {
            x: p.x,
            y: p.y,
            w: s.width,
            h: s.height,
        })
    }
}

/// Pushes proxy state to the icon, tooltip and menu.
pub fn sync(app: &AppHandle) {
    let Some(tray) = app.try_state::<Tray>() else {
        return;
    };
    let view = {
        let state = app.state::<AppState>();
        let core = state.core.lock().unwrap();
        let phase = core.controller.phase();
        let (status, headline) = match &phase {
            Phase::Running { port, .. } => {
                (Status::Running, format!("Running on 127.0.0.1:{port}"))
            }
            Phase::Stopped => (Status::Stopped, "Proxy stopped".to_owned()),
            Phase::Failed(_) => (Status::Failed, "Proxy failed to start".to_owned()),
        };
        let stats = if matches!(phase, Phase::Running { .. }) {
            let stats = core.logs.stats();
            format!("{} requests · {} errors", stats.requests, stats.errors)
        } else {
            "Not listening".to_owned()
        };
        View {
            status,
            headline,
            stats,
            running: matches!(phase, Phase::Running { .. }),
        }
    };
    let mut last = tray.view.lock().unwrap();
    let old = last.as_ref();
    if old.map(|v| v.status) != Some(view.status) {
        let _ = tray.icon.set_icon(Some(image(view.status)));
        #[cfg(target_os = "macos")]
        let _ = tray.icon.set_icon_as_template(true);
    }
    if old.map(|v| &v.headline) != Some(&view.headline) {
        let _ = tray.headline.set_text(&view.headline);
        let _ = tray
            .icon
            .set_tooltip(Some(format!("Coding Agent Proxy — {}", view.headline)));
    }
    if old.map(|v| &v.stats) != Some(&view.stats) {
        let _ = tray.stats.set_text(&view.stats);
    }
    let relabel = old.is_none();
    if relabel || old.map(|v| v.running) != Some(view.running) {
        let _ = tray.toggle.set_text(if view.running {
            "Stop Proxy"
        } else {
            "Start Proxy"
        });
        let _ = tray.restart.set_enabled(view.running);
    }
    if relabel {
        let _ = tray.open.set_text("Open Panel");
        let _ = tray.restart.set_text("Restart Proxy");
        let _ = tray.quit.set_text("Quit Coding Agent Proxy");
    }
    *last = Some(view);
}

fn image(status: Status) -> Image<'static> {
    #[cfg(target_os = "macos")]
    let (size, rgba) = (44, icon::template(44, status));
    #[cfg(not(target_os = "macos"))]
    let (size, rgba) = (64, icon::badge(64, Some(status)));
    Image::new_owned(rgba, size, size)
}
