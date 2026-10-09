#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

fn main() {
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
            "Usage: coportd <state-dir> <config-path> <log-path> | --control <status|start|stop|restart> [--state-dir PATH] | --summary [--state-dir PATH] | --summary-stream --window MINUTES model|all [--state-dir PATH] | --forward [--state-dir PATH] | --capabilities [--state-dir PATH]"
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
