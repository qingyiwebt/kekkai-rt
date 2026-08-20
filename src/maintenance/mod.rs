mod check;
mod fix;
mod init;
mod init_support;
pub(crate) mod sysroot;

use clap::{Subcommand, ValueEnum};
use std::path::Path;

#[derive(Clone, Debug, Subcommand)]
pub(crate) enum Command {
    Check,
    Fix,
    Init {
        distribution: Distribution,
        #[arg(long)]
        version: Option<String>,
    },
}

#[derive(Clone, Debug, ValueEnum)]
pub(crate) enum Distribution {
    Alpine,
}

pub(crate) async fn run(command: Command, config_path: &Path) -> anyhow::Result<()> {
    match command {
        Command::Check => check::run(config_path).await,
        Command::Fix => fix::run(config_path).await,
        Command::Init {
            distribution: Distribution::Alpine,
            version,
        } => init::run(config_path, version.as_deref()).await,
    }
}
