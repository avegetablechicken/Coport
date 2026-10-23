//! Menu bar (macOS) / notification area (Windows) / StatusNotifierItem (Linux).
//!
//! A left click toggles the panel; the context menu (right click, or any
//! click on GNOME) only offers the essentials. The lighthouse's lens turns
//! half a circle when a model call reaches its upstream, its light in the
//! app's color on the macOS menu bar.

use crate::{
    AppState,
    icon::{self, Status, Turn},
    panel,
    placement::Rect,
    proxy::Phase,
};
use coport::routing::Service;
use std::{
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tauri::{
    AppHandle, Manager, Wry,
    image::Image,
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent},
};

/// How long the lens takes for its half turn, and how many frames it shows.
const TURN: Duration = Duration::from_millis(2200);
const FRAMES: u32 = 66;

/// What the tray currently shows, so updates only touch what changed.
#[derive(Clone, PartialEq)]
struct View {
    status: Status,
    headline: String,
    stats: String,
    running: bool,
    busy: bool,
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
    /// Model calls sent upstream as of the last update.
    sent: AtomicU64,
    beacon: Mutex<Beacon>,
}

#[derive(Default)]
struct Beacon {
    playing: bool,
    /// The app of a call sent while a turn played, which turns once more.
    next: Option<Service>,
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
        .tooltip("Coport")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "open" => {
                let anchor = app.state::<Tray>().rect();
                // Let GNOME's tray menu fade out before capturing the screen.
                #[cfg(target_os = "linux")]
                {
                    // The pointer is still on the menu, under the tray icon.
                    let anchor = anchor.or_else(|| panel::menu_anchor(app));
                    let handle = app.clone();
                    tauri::async_runtime::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(250)).await;
                        let app = handle.clone();
                        let _ = handle.run_on_main_thread(move || panel::show(&app, anchor));
                    });
                }
                #[cfg(not(target_os = "linux"))]
                panel::show(app, anchor);
            }
            "toggle" => {
                let state = app.state::<AppState>();
                let mut core = state.core.lock().unwrap();
                if core.controller.is_running() {
                    let _ = core.stop();
                } else {
                    let _ = core.start();
                }
            }
            "restart" => {
                let _ = app.state::<AppState>().core.lock().unwrap().start();
            }
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
            #[cfg(all(target_os = "macos", tray_icon_attached_menu))]
            if let TrayIconEvent::Click {
                button: MouseButton::Right,
                button_state: MouseButtonState::Down,
                ..
            } = event
            {
                status_menu::show(tray);
            }
        })
        .build(app)?;
    #[cfg(all(target_os = "macos", tray_icon_attached_menu))]
    status_menu::detach(&icon);
    app.manage(Tray {
        icon,
        headline,
        stats,
        open,
        toggle,
        restart,
        quit,
        view: Mutex::new(None),
        sent: AtomicU64::new(
            app.state::<AppState>()
                .core
                .lock()
                .unwrap()
                .logs
                .sent()
                .count,
        ),
        beacon: Mutex::default(),
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

/// Schedule the entire update before taking any state/view locks. Native tray
/// setters wait for the main thread, so calling them from a worker while holding
/// the view lock can deadlock against an update already running on that thread.
pub fn sync(app: &AppHandle) {
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || sync_on_main_thread(&handle));
}

/// Icon status, headline and, unless running, the line shown instead of stats.
/// Failed covers start and stop errors and lost daemon contact, so it claims
/// neither that the proxy never started nor that it stopped listening.
fn describe(phase: &Phase) -> (Status, String, Option<&'static str>) {
    match phase {
        Phase::Running { port, .. } => (
            Status::Running,
            format!("Running on 127.0.0.1:{port}"),
            None,
        ),
        Phase::Stopped => (
            Status::Stopped,
            "Proxy stopped".to_owned(),
            Some("Not listening"),
        ),
        Phase::Failed(_) => (
            Status::Failed,
            "Proxy status unavailable".to_owned(),
            Some("Open the panel for details"),
        ),
    }
}

/// Pushes proxy state to the icon, tooltip and menu on the main thread only.
fn sync_on_main_thread(app: &AppHandle) {
    let Some(tray) = app.try_state::<Tray>() else {
        return;
    };
    let (view, sent) = {
        let state = app.state::<AppState>();
        let core = state.core.lock().unwrap();
        let phase = core.controller.phase();
        let (status, headline, idle) = describe(&phase);
        let stats = match idle {
            None => {
                let stats = core.logs.stats();
                format!("{} requests · {} errors", stats.requests, stats.errors)
            }
            Some(idle) => idle.to_owned(),
        };
        let view = View {
            status,
            headline,
            stats,
            running: matches!(phase, Phase::Running { .. }),
            busy: core.controller.busy(),
        };
        (view, core.logs.sent())
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
            .set_tooltip(Some(format!("Coport — {}", view.headline)));
    }
    if old.map(|v| &v.stats) != Some(&view.stats) {
        let _ = tray.stats.set_text(&view.stats);
    }
    let relabel = old.is_none();
    if relabel || old.map(|v| (v.running, v.busy)) != Some((view.running, view.busy)) {
        let _ = tray.toggle.set_text(if view.running {
            "Stop Proxy"
        } else {
            "Start Proxy"
        });
        let _ = tray.toggle.set_enabled(!view.busy);
        let _ = tray.restart.set_enabled(view.running && !view.busy);
    }
    if relabel {
        let _ = tray.open.set_text("Open Panel");
        let _ = tray.restart.set_text("Restart Proxy");
        let _ = tray.quit.set_text("Quit Coport");
    }
    let running = view.status == Status::Running;
    *last = Some(view);
    drop(last);
    if tray.sent.swap(sent.count, Ordering::Relaxed) < sent.count
        && running
        && let Some(service) = sent.latest
    {
        turn(app, &tray, service);
    }
}

/// Plays the turn from a worker thread, which renders each frame and queues it
/// on the main thread. Calls sent while it plays add one more turn, in the
/// color of the latest.
fn turn(app: &AppHandle, tray: &Tray, service: Service) {
    {
        let mut beacon = tray.beacon.lock().unwrap();
        if beacon.playing {
            beacon.next = Some(service);
            return;
        }
        beacon.playing = true;
    }
    let worker = app.clone();
    let spawned = std::thread::Builder::new()
        .name("tray-beacon".into())
        .spawn(move || {
            let mut service = service;
            loop {
                play(&worker, service);
                let tray = worker.state::<Tray>();
                let mut beacon = tray.beacon.lock().unwrap();
                match beacon.next.take() {
                    Some(next) => service = next,
                    None => {
                        let h = worker.clone();
                        let _ = worker.run_on_main_thread(move || show_frame(&h, None));
                        beacon.playing = false;
                        return;
                    }
                }
            }
        });
    if spawned.is_err() {
        tray.beacon.lock().unwrap().playing = false;
    }
}

/// One turn, paced by the clock so slow frames are not made up later.
fn play(app: &AppHandle, service: Service) {
    #[cfg(target_os = "macos")]
    let dark = menu_bar_is_dark(app);
    let start = Instant::now();
    for i in 1..FRAMES {
        let turn = Turn::at(i as f32 / FRAMES as f32);
        #[cfg(target_os = "macos")]
        let frame = Image::new_owned(icon::beacon(44, turn, service, dark), 44, 44);
        #[cfg(not(target_os = "macos"))]
        let frame = {
            let _ = service;
            Image::new_owned(icon::badge(64, Some(Status::Running), turn), 64, 64)
        };
        std::thread::sleep((start + TURN * i / FRAMES).saturating_duration_since(Instant::now()));
        let h = app.clone();
        let _ = app.run_on_main_thread(move || show_frame(&h, Some(frame)));
    }
    std::thread::sleep((start + TURN).saturating_duration_since(Instant::now()));
}

/// macOS 27 opens a status item's attached menu on any click before tray-icon's
/// click view sees it. Releases before 0.25.1 keep the menu attached (see
/// build.rs), so there it is attached only while a right click presents it
/// (tray-icon#365).
#[cfg(all(target_os = "macos", tray_icon_attached_menu))]
mod status_menu {
    use objc2::{MainThreadMarker, rc::Retained};
    use objc2_app_kit::NSMenu;
    use objc2_foundation::NSProcessInfo;
    use std::cell::RefCell;
    use tauri::tray::TrayIcon;

    thread_local! {
        static MENU: RefCell<Option<Retained<NSMenu>>> = const { RefCell::new(None) };
    }

    /// Takes the menu off the status item so that left clicks reach the panel.
    pub fn detach(tray: &TrayIcon) {
        if NSProcessInfo::processInfo()
            .operatingSystemVersion()
            .majorVersion
            < 27
        {
            return;
        }
        let _ = tray.with_inner_tray_icon(|inner| {
            let (Some(mtm), Some(item)) = (MainThreadMarker::new(), inner.ns_status_item()) else {
                return;
            };
            MENU.with(|menu| *menu.borrow_mut() = item.menu(mtm));
            item.setMenu(None);
        });
    }

    /// Presents a detached menu under the status item, as a click on an attached
    /// menu does.
    pub fn show(tray: &TrayIcon) {
        let _ = tray.with_inner_tray_icon(|inner| {
            let (Some(mtm), Some(item)) = (MainThreadMarker::new(), inner.ns_status_item()) else {
                return;
            };
            let (Some(menu), Some(button)) =
                (MENU.with(|menu| menu.borrow().clone()), item.button(mtm))
            else {
                return;
            };
            item.setMenu(Some(&menu));
            // SAFETY: `performClick:` accepts a nil sender.
            unsafe { button.performClick(None) };
            item.setMenu(None);
        });
    }
}

/// Whether the menu bar around the icon is dark, which can differ from the
/// app's appearance. Asked on the main thread, without holding any lock.
#[cfg(target_os = "macos")]
fn menu_bar_is_dark(app: &AppHandle) -> bool {
    use objc2_app_kit::{
        NSAppearanceCustomization, NSAppearanceNameAqua, NSAppearanceNameDarkAqua,
    };
    let (tx, rx) = std::sync::mpsc::channel();
    let h = app.clone();
    let _ = app.run_on_main_thread(move || {
        let dark = h.try_state::<Tray>().is_some_and(|tray| {
            tray.icon
                .with_inner_tray_icon(|inner| {
                    let mtm = objc2::MainThreadMarker::new()?;
                    let button = inner.ns_status_item()?.button(mtm)?;
                    // SAFETY: AppKit's appearance names are immutable statics.
                    let (aqua, dark) = unsafe { (NSAppearanceNameAqua, NSAppearanceNameDarkAqua) };
                    let names = objc2_foundation::NSArray::from_slice(&[aqua, dark]);
                    let best = button
                        .effectiveAppearance()
                        .bestMatchFromAppearancesWithNames(&names)?;
                    Some(&*best == dark)
                })
                .ok()
                .flatten()
                .unwrap_or(false)
        });
        let _ = tx.send(dark);
    });
    rx.recv_timeout(Duration::from_secs(1)).unwrap_or(false)
}

/// Shows a frame of the turn while the proxy still runs, or with `None` the
/// icon for the current status. A status update during a turn has already set
/// its own icon, which later frames leave alone.
fn show_frame(app: &AppHandle, frame: Option<Image<'static>>) {
    let Some(tray) = app.try_state::<Tray>() else {
        return;
    };
    let view = tray.view.lock().unwrap();
    let Some(status) = view.as_ref().map(|v| v.status) else {
        return;
    };
    match frame {
        Some(frame) if status == Status::Running => {
            let _ = tray.icon.set_icon_with_as_template(Some(frame), false);
        }
        Some(_) => {}
        None => {
            let _ = tray
                .icon
                .set_icon_with_as_template(Some(image(status)), cfg!(target_os = "macos"));
        }
    }
}

fn image(status: Status) -> Image<'static> {
    #[cfg(target_os = "macos")]
    let (size, rgba) = (44, icon::template(44, status));
    #[cfg(not(target_os = "macos"))]
    let (size, rgba) = (64, icon::badge(64, Some(status), Turn::REST));
    Image::new_owned(rgba, size, size)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failures_are_not_described_as_failed_starts() {
        for message in [
            "Lost contact with the proxy daemon. Restart to reconnect.",
            "Cannot stop proxy daemon: timed out",
            "Cannot start: address in use",
        ] {
            let (status, headline, idle) = describe(&Phase::Failed(message.into()));
            assert_eq!(status, Status::Failed);
            assert_eq!(headline, "Proxy status unavailable");
            assert_eq!(idle, Some("Open the panel for details"));
        }
        let (status, headline, idle) = describe(&Phase::Stopped);
        assert_eq!(
            (status, headline.as_str(), idle),
            (Status::Stopped, "Proxy stopped", Some("Not listening"))
        );
    }
}
