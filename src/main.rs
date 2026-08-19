mod api;
mod config;
mod init;
mod maintenance;
mod runtime;
mod tasks;

use anyhow::Context;
use clap::{Parser, Subcommand, ValueEnum};
use config::Config;
use runtime::Sandbox;
use std::{path::PathBuf, sync::Arc};
use tracing::info;

#[derive(Parser, Debug)]
#[command(name = "agent-cell")]
struct Args {
    #[arg(short, long, global = true, default_value = "config.toml")]
    config: PathBuf,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Clone, Debug, Subcommand)]
enum Command {
    Check,
    Fix,
    Init {
        distribution: Distribution,
        #[arg(long)]
        version: Option<String>,
    },
}

#[derive(Clone, Debug, ValueEnum)]
enum Distribution {
    Alpine,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let args = Args::parse();
    match args.command {
        None => run_server(&args.config).await,
        Some(Command::Check) => maintenance::check(&args.config).await,
        Some(Command::Fix) => maintenance::fix(&args.config).await,
        Some(Command::Init {
            distribution: Distribution::Alpine,
            version,
        }) => init::run(&args.config, version.as_deref()).await,
    }
}

async fn run_server(config_path: &std::path::Path) -> anyhow::Result<()> {
    let config = Config::load(config_path).context("load configuration")?;
    let sandbox = Arc::new(
        Sandbox::start(&config.sandbox)
            .await
            .context("start sandbox")?,
    );
    let state = api::AppState::new(config.clone(), sandbox);
    let app = api::router(state.clone());
    let listener = tokio::net::TcpListener::bind(config.api.listen_addr).await?;
    info!(address = %listener.local_addr()?, backend = %config.sandbox.backend, "agent-cell listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown())
        .await?;
    Ok(())
}

async fn shutdown() {
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_command_means_server_mode() {
        let args = Args::try_parse_from(["agent-cell"]).unwrap();
        assert!(args.command.is_none());
        assert_eq!(args.config, PathBuf::from("config.toml"));
    }

    #[test]
    fn maintenance_commands_accept_config_before_or_after_subcommand() {
        let before = Args::try_parse_from(["agent-cell", "--config", "one.toml", "check"]).unwrap();
        assert!(matches!(before.command, Some(Command::Check)));
        assert_eq!(before.config, PathBuf::from("one.toml"));

        let after = Args::try_parse_from(["agent-cell", "fix", "--config", "two.toml"]).unwrap();
        assert!(matches!(after.command, Some(Command::Fix)));
        assert_eq!(after.config, PathBuf::from("two.toml"));
    }

    #[test]
    fn init_takes_distribution_as_a_positional_argument() {
        let args = Args::try_parse_from(["agent-cell", "init", "alpine"]).unwrap();
        assert!(matches!(args.command, Some(Command::Init { .. })));

        let args =
            Args::try_parse_from(["agent-cell", "init", "alpine", "--version", "3.24.1"]).unwrap();
        assert!(
            matches!(args.command, Some(Command::Init { version: Some(version), .. }) if version == "3.24.1")
        );
        assert!(Args::try_parse_from(["agent-cell", "init", "debian"]).is_err());
    }
}
