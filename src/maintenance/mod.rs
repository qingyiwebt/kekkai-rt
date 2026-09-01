mod archive;
mod check;
mod config_template;
mod files;
mod fix;
mod init;
mod init_support;
mod shell;
pub mod sysroot;

use clap::Subcommand;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Subcommand)]
pub enum Command {
    Check,
    Fix,
    Init {
        image: PathBuf,
    },
    Shell {
        #[arg(long)]
        shell: Option<String>,
    },
}

pub enum CommandResult {
    Completed,
    Exit(i32),
}

pub async fn run(command: Command, config_path: &Path) -> anyhow::Result<CommandResult> {
    match command {
        Command::Check => {
            check::run(config_path).await?;
            Ok(CommandResult::Completed)
        }
        Command::Fix => {
            fix::run(config_path).await?;
            Ok(CommandResult::Completed)
        }
        Command::Init { image } => {
            init::run(config_path, &image).await?;
            Ok(CommandResult::Completed)
        }
        Command::Shell { shell } => shell::run(config_path, shell.as_deref()).await,
    }
}
