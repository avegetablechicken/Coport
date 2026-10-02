#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

fn main() {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.first().is_some_and(|arg| arg == "--summary") {
        let result =
            helper_directory(&args, 1).and_then(|dir| coport_gui::data_api::read_summary(&dir));
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
        let dir = helper_directory(&args, 2);
        let result = args[1]
            .to_str()
            .and_then(coport_gui::remote::Action::parse)
            .ok_or_else(|| std::io::Error::other("Expected status, start, stop or restart"))
            .and_then(|action| coport_gui::remote::control_in(action, &dir?));
        match result {
            Ok(status) => println!("{}", serde_json::to_string(&status).unwrap()),
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(1);
            }
        }
        return;
    }
    if args.len() != 3 {
        eprintln!(
            "Usage: coportd <state-dir> <config-path> <log-path> | --control <status|start|stop|restart> [--state-dir PATH] | --summary [--state-dir PATH]"
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
