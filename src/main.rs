use clap::Parser;
use coport::{
    Error, Result,
    config::{Config, expand},
    logger::Logger,
    server::Server,
};
use std::sync::Arc;

#[derive(Parser)]
#[command(
    version,
    args_conflicts_with_subcommands = true,
    about = "Loopback HTTP/SSE proxy. Configuration is loaded at startup; credentials refresh per request."
)]
struct Args {
    #[command(subcommand)]
    command: Option<Subcommand>,
    #[arg(long, default_value = "config.yaml")]
    config: String,
    #[arg(long)]
    log_file: Option<String>,
    #[arg(long)]
    check: bool,
    #[arg(long, hide = true)]
    print_listen_port: bool,
    /// Override a YAML setting using a dotted path (repeatable; last value wins).
    #[arg(short = 'c', value_name = "PATH=VALUE")]
    overrides: Vec<String>,
}
#[derive(clap::Subcommand)]
enum Subcommand {
    /// Install or manage the per-user background service.
    Service(coport::service::ServiceArgs),
}
#[tokio::main]
async fn main() {
    let mut args = Args::parse();
    if let Some(Subcommand::Service(service)) = args.command.take() {
        let result = (|| -> std::result::Result<i32, Box<dyn std::error::Error>> {
            let binary = service.binary.unwrap_or(std::env::current_exe()?);
            coport::service::Service::current()?.manage(
                service.action,
                &binary,
                service.config.as_deref(),
            )
        })();
        match result {
            Ok(code) => std::process::exit(code),
            Err(error) => {
                eprintln!("Service error: {error}");
                std::process::exit(1);
            }
        }
    }
    if let Err(e) = run(args).await {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
async fn run(args: Args) -> Result<()> {
    let path = expand(&args.config);
    let config = Config::read_with_overrides(&path, &args.overrides)?;
    if args.print_listen_port {
        println!("{}", config.listen_port);
        return Ok(());
    }
    if args.check {
        config.check_external_data()?;
        config.check_credentials().await?;
        println!(
            "Configuration, credential and route are valid. Proxy reachability was not tested."
        );
        return Ok(());
    }
    let log_path = args.log_file.map(|s| expand(&s)).unwrap_or_else(|| {
        path.parent()
            .unwrap_or(std::path::Path::new("."))
            .join("logs/proxy.log")
    });
    let logger = Arc::new(Logger::new(log_path));
    if config.allow_external_access {
        return Err(Error::config(
            "The read-only external data API is provided by coportd; the CLI proxy remains local.",
        ));
    }
    let listen_address = std::net::Ipv4Addr::LOCALHOST;
    let listener = tokio::net::TcpListener::bind((listen_address, config.listen_port))
        .await
        .map_err(|_| Error::config("Cannot bind proxy listener; check the address and port."))?;
    println!(
        "coport listening on http://{listen_address}:{}",
        config.listen_port
    );
    let tls_dir = coport::local_tls::dir_for(&std::path::absolute(&path).unwrap_or(path.clone()));
    let mut server = Server::new(config, logger.clone());
    match coport::local_tls::acceptor(&tls_dir) {
        Ok(tls) => {
            server = server.with_tls(tls);
            println!(
                "TLS on the same port; clients trust {}",
                tls_dir.join(coport::local_tls::CA_FILE).display()
            );
        }
        Err(e) => eprintln!("TLS disabled: {e}"),
    }
    logger.write("server_started", serde_json::Map::new());
    let server = Arc::new(server);
    server.startup_log().await;
    server
        .serve(listener, shutdown())
        .await
        .map_err(|_| Error::config("Listener failed."))?;
    logger.write("server_stopped", serde_json::Map::new());
    Ok(())
}
async fn shutdown() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler");
        tokio::select! { _=tokio::signal::ctrl_c()=>{}, _=term.recv()=>{} }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn service_options_belong_to_service_command() {
        let args = Args::try_parse_from([
            "coport",
            "service",
            "install",
            "--binary",
            "build/coport",
            "--config",
            "private.yaml",
        ])
        .unwrap();
        let Some(Subcommand::Service(service)) = args.command else {
            panic!("missing service command")
        };
        assert_eq!(service.action, coport::service::Action::Install);
        assert_eq!(
            service.binary.unwrap(),
            std::path::PathBuf::from("build/coport")
        );
        assert_eq!(
            service.config.unwrap(),
            std::path::PathBuf::from("private.yaml")
        );
        assert!(
            Args::try_parse_from(["coport", "--config", "ignored.yaml", "service", "install"])
                .is_err()
        );
    }
    #[test]
    fn proxy_options_remain_compatible() {
        let args = Args::try_parse_from([
            "coport",
            "--config",
            "private.yaml",
            "--check",
            "-c",
            "listen_port=8787",
        ])
        .unwrap();
        assert!(args.command.is_none());
        assert!(args.check);
        assert_eq!(args.config, "private.yaml");
        assert_eq!(args.overrides, ["listen_port=8787"]);
    }
}
