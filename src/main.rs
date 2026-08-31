mod api;
mod config;
mod execution;
mod maintenance;
mod proxy;
mod runtime;
mod tasks;
mod workspace;

use anyhow::Context;
use clap::Parser;
use config::Config;
use maintenance::Command;
use runtime::Sandbox;
use std::{path::PathBuf, sync::Arc};
use tracing::info;

#[derive(Parser, Debug)]
#[command(name = "kekkai-rt")]
struct Args {
    #[arg(short, long, global = true, default_value = "config.toml")]
    config: PathBuf,
    #[command(subcommand)]
    command: Option<Command>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();
    let args = Args::parse();
    match args.command {
        None => run_server(&args.config).await,
        Some(command) => maintenance::run(command, &args.config).await,
    }
}

async fn run_server(config_path: &std::path::Path) -> anyhow::Result<()> {
    let config = Config::load(config_path).context("load configuration")?;
    let sandbox = Arc::new(
        Sandbox::start(&config.sandbox, &config.mounts, &config.tools)
            .await
            .context("start sandbox")?,
    );
    let state = api::AppState::new(config.clone(), sandbox.clone());
    let app = api::router(state.clone());
    let listener = match tokio::net::TcpListener::bind(config.api.listen_addr).await {
        Ok(listener) => listener,
        Err(error) => {
            if let Err(cleanup_error) = sandbox.shutdown().await {
                return Err(anyhow::anyhow!(
                    "bind API listener: {error}; sandbox shutdown failed: {cleanup_error}"
                ));
            }
            return Err(error.into());
        }
    };
    info!(address = %listener.local_addr()?, backend = %config.sandbox.backend, "kekkai-rt listening");
    let server_result = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown())
        .await;
    info!("HTTP server stopped; shutting down sandbox");
    let shutdown_result = state.shutdown().await;
    match (server_result, shutdown_result) {
        (Err(server_error), Err(shutdown_error)) => Err(anyhow::anyhow!(
            "server failed: {server_error}; sandbox shutdown failed: {shutdown_error}"
        )),
        (Err(error), Ok(())) => Err(error.into()),
        (Ok(()), Err(error)) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
    }
}

async fn shutdown() {
    use tokio::signal::unix::{signal, SignalKind};

    let mut terminate = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => info!("received Ctrl-C"),
        _ = terminate.recv() => info!("received SIGTERM"),
    }
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("kekkai_rt=info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_command_means_server_mode() {
        let args = Args::try_parse_from(["kekkai-rt"]).unwrap();
        assert!(args.command.is_none());
        assert_eq!(args.config, PathBuf::from("config.toml"));
    }

    #[test]
    fn maintenance_commands_accept_config_before_or_after_subcommand() {
        let before = Args::try_parse_from(["kekkai-rt", "--config", "one.toml", "check"]).unwrap();
        assert!(matches!(before.command, Some(maintenance::Command::Check)));
        assert_eq!(before.config, PathBuf::from("one.toml"));

        let after = Args::try_parse_from(["kekkai-rt", "fix", "--config", "two.toml"]).unwrap();
        assert!(matches!(after.command, Some(maintenance::Command::Fix)));
        assert_eq!(after.config, PathBuf::from("two.toml"));
    }

    #[test]
    fn init_takes_oci_image_as_a_positional_argument() {
        let args = Args::try_parse_from(["kekkai-rt", "init", "image.tar"]).unwrap();
        assert!(matches!(
            args.command,
            Some(maintenance::Command::Init { image }) if image == PathBuf::from("image.tar")
        ));
        assert!(
            Args::try_parse_from(["kekkai-rt", "init", "alpine", "--version", "3.24.1"]).is_err()
        );
    }
}
