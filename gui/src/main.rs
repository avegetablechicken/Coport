//! Menu bar (macOS) / system tray (Windows, Linux) app for coport.
//! The panel UI is HTML/CSS in `../ui`, rendered by the system WebView.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod activity;
mod commands;
mod config_edit;
mod core;
mod describe;
mod icon;
mod logs;
mod panel;
mod placement;
mod platform;
mod single_instance;
mod traffic;
mod traffic_identity;
mod tray;

use coport_gui::{proxy, settings};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tauri::{Emitter, Manager, RunEvent};

pub struct AppState {
    pub core: Mutex<core::Core>,
}

const USAGE: &str = "\
Usage: coport-gui [--background]

  --background                 Start in the menu bar / tray without the panel
                               (used by Launch at Login)
  --export-icon <png> [size]   Write the app icon as PNG and exit
";

/// The status item is positioned shortly after launch; wait before anchoring to it.
const LAUNCH_OPEN_DELAY: Duration = Duration::from_millis(400);

fn main() {
    let settings = settings::Settings::load();
    // Opening the app by hand shows the panel; login items start quietly.
    let mut open = true;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--background" => open = false,
            "--open" => open = true,
            "--export-icon" => {
                let Some(dest) = args.next() else {
                    return usage_error();
                };
                let size = args.next().and_then(|s| s.parse().ok()).unwrap_or(1024);
                let png = icon::png(size, &icon::badge(size, None));
                if let Err(e) = std::fs::write(&dest, png) {
                    eprintln!("Cannot write {dest}: {e}");
                    std::process::exit(1);
                }
                return;
            }
            "-V" | "--version" => {
                println!("coport-gui {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            "-h" | "--help" => {
                print!("{USAGE}");
                return;
            }
            _ => return usage_error(),
        }
    }

    let instance = match single_instance::acquire(settings::app_dir()) {
        single_instance::Instance::Primary(guard) => guard,
        single_instance::Instance::Secondary => {
            eprintln!("Coport is already running; showing its panel.");
            return;
        }
    };

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(panel::PanelState::default())
        .invoke_handler(tauri::generate_handler![
            commands::get_state,
            commands::get_activity,
            commands::get_traffic,
            commands::set_traffic_assignment,
            commands::clear_traffic_compatibility,
            commands::set_running,
            commands::restart_proxy,
            commands::probe_proxy,
            commands::update_settings,
            commands::set_launch_at_login,
            commands::import_config,
            commands::set_config_value,
            commands::create_example_config,
            commands::open_path,
            commands::copy_text,
            commands::fit_panel,
            commands::hide_panel,
            commands::quit_app,
        ])
        .setup(move |app| {
            // Menu bar app: no Dock icon and no app menu.
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            if let Err(error) = coport_gui::data_migration::prepare() {
                use tauri_plugin_dialog::DialogExt;
                app.dialog()
                    .message(format!("Cannot migrate Coport data: {error}"))
                    .title("Coport data migration")
                    .kind(tauri_plugin_dialog::MessageDialogKind::Error)
                    .show(|_| std::process::exit(1));
                app.manage(Mutex::new(instance));
                return Ok(());
            }
            let handle = app.handle().clone();
            let notify = tray_notifier(handle);
            let appearance = settings.appearance;
            let mut core = core::Core::new(settings.clone(), notify);
            let start = settings.start_proxy_on_launch && core.config_exists();
            open |= !core.config_exists();
            if start && !core.controller.is_running() {
                core.start();
            }
            app.manage(AppState {
                core: Mutex::new(core),
            });
            apply_appearance(app.handle(), appearance);
            tray::create(app.handle())?;

            let mut instance = instance;
            let handle = app.handle().clone();
            instance.listen(move || {
                let h = handle.clone();
                let _ = handle.run_on_main_thread(move || {
                    let anchor = h.state::<tray::Tray>().rect();
                    panel::show(&h, anchor);
                });
            });
            // The guard holds the instance lock for the life of the app.
            app.manage(Mutex::new(instance));
            if open {
                let handle = app.handle().clone();
                std::thread::spawn(move || {
                    std::thread::sleep(LAUNCH_OPEN_DELAY);
                    let h = handle.clone();
                    let _ = handle.run_on_main_thread(move || {
                        let anchor = h.state::<tray::Tray>().rect();
                        panel::show(&h, anchor);
                    });
                });
            }
            Ok(())
        })
        .on_window_event(panel::on_window_event)
        .build(tauri::generate_context!());
    let app = match app {
        Ok(app) => app,
        Err(e) => {
            eprintln!("Cannot start: {e}");
            std::process::exit(1);
        }
    };
    app.run(|app, event| {
        if app.try_state::<AppState>().is_none() {
            return; // Startup migration error: only the error dialog is active.
        }
        match event {
            RunEvent::ExitRequested { api, .. } => {
                let result = {
                    let state = app.state::<AppState>();
                    let mut core = state.core.lock().unwrap();
                    let keep = core.settings.keep_proxy_running_on_quit;
                    core.controller.on_app_exit(keep)
                };
                if let Err(error) = result {
                    // A failed stop must remain visible and retryable.
                    eprintln!("{error}");
                    api.prevent_exit();
                    panel::show(app, None);
                }
            }
            #[cfg(target_os = "macos")]
            RunEvent::Reopen { .. } => panel::show(app, None),
            RunEvent::Exit => {
                let state = app.state::<AppState>();
                let mut core = state.core.lock().unwrap();
                let keep = core.settings.keep_proxy_running_on_quit;
                let _ = core.controller.on_app_exit(keep);
            }
            _ => {}
        }
    });
}

/// Builds the change notification used by the proxy and the log tailer.
///
/// Tray updates lock the shared state, and notifications can fire while a
/// command on the main thread holds that lock. `run_on_main_thread` runs its
/// closure immediately when called from the main thread, so updates are
/// queued from a worker thread instead (which also coalesces bursts).
fn tray_notifier(app: tauri::AppHandle) -> proxy::Notify {
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let worker = app.clone();
    std::thread::Builder::new()
        .name("tray-sync".into())
        .spawn(move || {
            while rx.recv().is_ok() {
                std::thread::sleep(Duration::from_millis(50));
                while rx.try_recv().is_ok() {}
                let h = worker.clone();
                let _ = worker.run_on_main_thread(move || tray::sync(&h));
            }
        })
        .expect("tray sync thread");
    let tx = Mutex::new(tx);
    Arc::new(move || {
        let _ = app.emit("state-changed", ());
        let _ = tx.lock().unwrap().send(());
    })
}

/// Applies the theme preference to the panel; the WebView follows it through
/// `prefers-color-scheme`.
pub fn apply_appearance(app: &tauri::AppHandle, appearance: settings::Appearance) {
    if let Some(w) = app.get_webview_window(panel::LABEL) {
        let _ = w.set_theme(match appearance {
            settings::Appearance::System => None,
            settings::Appearance::Light => Some(tauri::Theme::Light),
            settings::Appearance::Dark => Some(tauri::Theme::Dark),
        });
    }
}

fn usage_error() {
    eprint!("{USAGE}");
    std::process::exit(2);
}
