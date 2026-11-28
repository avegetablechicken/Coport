#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::io::Write;

fn main() {
    #[cfg(windows)]
    prevent_stdio_inheritance();
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.first().is_some_and(|arg| arg == "--capabilities") {
        match helper_directory(&args, 1) {
            Ok(dir) => println!(
                "{}",
                serde_json::to_string(&coport_gui::remote::capabilities_in(&dir)).unwrap()
            ),
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(1);
            }
        }
        return;
    }
    if args.first().is_some_and(|arg| arg == "--forward") {
        let result = helper_directory(&args, 1).and_then(|dir| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            let result = runtime.block_on(coport_gui::remote_forward::serve_stdio(&dir));
            // Tokio stdin uses a blocking read. Remote EOF must not wait for the
            // SSH client to close stdin before the helper process can exit.
            runtime.shutdown_background();
            result
        });
        if let Err(error) = result {
            eprintln!("{error}");
            std::process::exit(1);
        }
        return;
    }
    if args.first().is_some_and(|arg| arg == "--summary-stream") {
        let result = (|| {
            if args.get(1).is_none_or(|arg| arg != "--window") {
                return Err(std::io::Error::other("Expected --window MINUTES model|all"));
            }
            let minutes = args
                .get(2)
                .and_then(|v| v.to_str())
                .and_then(|v| v.parse().ok())
                .ok_or_else(|| std::io::Error::other("Expected traffic range"))?;
            let scope = match args.get(3).and_then(|v| v.to_str()) {
                Some("model") => coport_gui::traffic::TrafficScope::Model,
                Some("all") => coport_gui::traffic::TrafficScope::All,
                _ => return Err(std::io::Error::other("Expected model or all")),
            };
            coport_gui::data_api::stream_summary(
                &helper_directory(&args, 4)?,
                minutes,
                scope,
                coport_gui::data_api::ReaderServices::of_helper(),
                std::io::stdout().lock(),
            )
        })();
        if let Err(error) = result {
            eprintln!("{error}");
            std::process::exit(1);
        }
        return;
    }
    if args.first().is_some_and(|arg| arg == "--summary") {
        let result = helper_directory(&args, 1).and_then(|dir| {
            coport_gui::data_api::read_summary(
                &dir,
                coport_gui::data_api::ReaderServices::of_helper(),
            )
        });
        match result {
            Ok(summary) => println!("{}", serde_json::to_string(&summary).unwrap()),
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(1);
            }
        }
        return;
    }
    if args.len() >= 2 && args[0] == "--control" {
        let result = control(&args[1..]);
        match result {
            Ok(status) => {
                let _ = writeln!(
                    std::io::stdout(),
                    "{}",
                    serde_json::to_string(&status).unwrap()
                );
            }
            Err(error) => {
                let _ = writeln!(std::io::stderr(), "{error}");
                std::process::exit(1);
            }
        }
        return;
    }
    if args.len() != 3 {
        eprintln!(
            "Usage: coportd <state-dir> <config-path> <log-path> | --control <status|start|stop|restart [--force | --wait-idle SECONDS]> [--state-dir PATH] | --summary [--state-dir PATH] | --summary-stream --window MINUTES model|all [--state-dir PATH] | --forward [--state-dir PATH] | --capabilities [--state-dir PATH]"
        );
        std::process::exit(2);
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("daemon runtime");
    if let Err(error) = runtime.block_on(coport_gui::daemon::serve(
        args[0].as_ref(),
        args[1].as_ref(),
        args[2].as_ref(),
    )) {
        eprintln!("Cannot run proxy daemon: {error}");
        std::process::exit(1);
    }
}

/// A detached Daemon must not keep a control command's pipes open. Windows
/// inherits every inheritable handle, even when the child's stdio is redirected.
#[cfg(windows)]
fn prevent_stdio_inheritance() {
    use std::os::windows::io::AsRawHandle;
    const HANDLE_FLAG_INHERIT: u32 = 1;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn SetHandleInformation(handle: *mut std::ffi::c_void, mask: u32, flags: u32) -> i32;
    }
    for handle in [
        std::io::stdin().as_raw_handle(),
        std::io::stdout().as_raw_handle(),
        std::io::stderr().as_raw_handle(),
    ] {
        // SAFETY: these are borrowed standard handles; changing only the
        // inheritance flag neither closes them nor changes this process's I/O.
        // Missing console handles are harmless and need no update.
        unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) };
    }
}

fn helper_directory(
    args: &[std::ffi::OsString],
    count: usize,
) -> std::io::Result<std::path::PathBuf> {
    if args.len() == count {
        return Ok(coport_gui::settings::app_dir());
    }
    if args.len() == count + 2 && args[count] == "--state-dir" {
        let dir = std::path::PathBuf::from(&args[count + 1]);
        if dir.is_absolute() {
            return Ok(dir);
        }
    }
    Err(std::io::Error::other(
        "Expected --state-dir with an absolute path",
    ))
}

/// `--control ACTION [--force | --wait-idle SECONDS] [--state-dir PATH]`.
/// Output is best effort: a Restart goes on when its terminal is gone.
fn control(args: &[std::ffi::OsString]) -> std::io::Result<coport_gui::remote::ControlStatus> {
    use coport_gui::{
        daemon_lifecycle::{Progress, Wait},
        remote::Action,
    };
    let mut action = args[0]
        .to_str()
        .and_then(Action::parse)
        .ok_or_else(|| std::io::Error::other("Expected status, start, stop or restart"))?;
    let mut rest = &args[1..];
    if let Action::Restart(wait) = &mut action {
        match rest.first().and_then(|arg| arg.to_str()) {
            Some("--force") => {
                *wait = Wait::Force;
                rest = &rest[1..];
            }
            Some("--wait-idle") => {
                let seconds = rest
                    .get(1)
                    .and_then(|arg| arg.to_str())
                    .and_then(|arg| arg.parse().ok())
                    .filter(|seconds| (1..=3600).contains(seconds))
                    .ok_or_else(|| std::io::Error::other("Expected --wait-idle 1 to 3600"))?;
                *wait = Wait::Idle(std::time::Duration::from_secs(seconds));
                rest = &rest[2..];
            }
            _ => {}
        }
    }
    let mut command = vec![args[0].clone()];
    command.extend_from_slice(rest);
    let dir = helper_directory(&command, 1)?;
    coport_gui::remote::control_in(action, &dir, &mut |progress| {
        let note = match progress {
            Progress::Joining => "Another restart is in progress; waiting for it.".to_owned(),
            Progress::Waiting(requests) => format!(
                "Waiting for {requests} request(s) in flight; run restart --force to interrupt them."
            ),
            Progress::Stopping => {
                // Past this point an interrupted restart would leave nothing
                // serving: finish it, even if the terminal goes away.
                #[cfg(unix)]
                for signal in [libc::SIGINT, libc::SIGHUP, libc::SIGTERM] {
                    // SAFETY: ignoring a signal installs no handler code.
                    unsafe { libc::signal(signal, libc::SIG_IGN) };
                }
                "Stopping the proxy daemon.".to_owned()
            }
            Progress::Stopped => "Starting the new proxy daemon.".to_owned(),
        };
        let _ = writeln!(std::io::stderr(), "{note}");
    })
}
