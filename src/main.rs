use clap::Parser;
use kekkai_rt::maintenance::{self, Command};
use std::path::PathBuf;

mod application;

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
        None => application::run_server(&args.config).await,
        Some(command) => maintenance::run(command, &args.config).await,
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
        assert!(matches!(before.command, Some(Command::Check)));
        assert_eq!(before.config, PathBuf::from("one.toml"));

        let after = Args::try_parse_from(["kekkai-rt", "fix", "--config", "two.toml"]).unwrap();
        assert!(matches!(after.command, Some(Command::Fix)));
        assert_eq!(after.config, PathBuf::from("two.toml"));
    }

    #[test]
    fn init_takes_oci_image_as_a_positional_argument() {
        let args = Args::try_parse_from(["kekkai-rt", "init", "image.tar"]).unwrap();
        assert!(matches!(
            args.command,
            Some(Command::Init { image }) if image == *"image.tar"
        ));
        assert!(
            Args::try_parse_from(["kekkai-rt", "init", "alpine", "--version", "3.24.1"]).is_err()
        );
    }
}
