mod archive;
mod check;
mod config_template;
mod files;
mod fix;
mod init;
mod init_support;
pub mod sysroot;

use clap::Subcommand;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Subcommand)]
pub enum Command {
    Check,
    Fix,
    Init { image: PathBuf },
}

pub async fn run(command: Command, config_path: &Path) -> anyhow::Result<()> {
    match command {
        Command::Check => check::run(config_path).await,
        Command::Fix => fix::run(config_path).await,
        Command::Init { image } => init::run(config_path, &image).await,
    }
}
