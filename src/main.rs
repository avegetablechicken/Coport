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
    about = "Loopback HTTP/SSE proxy. Configuration is loaded at startup; credentials refresh per request."
)]
struct Args {
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
#[tokio::main]
async fn main() {
    if let Err(e) = run(Args::parse()).await {
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
    let listener =
        tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, config.listen_port))
            .await
            .map_err(|_| Error::config("Cannot bind loopback listener; check the port."))?;
    println!(
        "coport listening on http://127.0.0.1:{}",
        config.listen_port
    );
    logger.write("server_started", serde_json::Map::new());
    let server = Arc::new(Server::new(config, logger.clone()));
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
