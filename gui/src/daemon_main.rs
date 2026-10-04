#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

fn main() {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() != 3 {
        eprintln!("Usage: coport-daemon <state-dir> <config-path> <log-path>");
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
