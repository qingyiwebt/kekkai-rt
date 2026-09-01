use crate::{
    config::Config,
    runtime::{tasks::ExecRequest, Sandbox},
};
use anyhow::{bail, Context};
use std::path::Path;

const DEFAULT_SHELL_SCRIPT: &str = "command -v bash >/dev/null 2>&1 && exec bash || exec sh";

pub async fn run(
    config_path: &Path,
    requested_shell: Option<&str>,
) -> anyhow::Result<super::CommandResult> {
    let config = Config::load(config_path).context("load configuration")?;
    let shell = shell_argv(requested_shell)?;
    let request = ExecRequest {
        argv: shell,
        cwd: None,
        env: Default::default(),
        stdin: None,
        timeout_seconds: None,
    };
    let status = Sandbox::exec_existing_attached(&config.sandbox, &request, true)
        .await
        .context("exec shell in sandbox")?;

    Ok(super::CommandResult::Exit(status.code().unwrap_or(1)))
}

fn shell_argv(requested_shell: Option<&str>) -> anyhow::Result<Vec<String>> {
    match requested_shell {
        Some(shell) if shell.is_empty() => bail!("--shell must not be empty"),
        Some(shell) => Ok(vec![shell.to_owned()]),
        None => Ok(vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            DEFAULT_SHELL_SCRIPT.to_owned(),
        ]),
    }
}

#[cfg(test)]
mod tests {
    use super::shell_argv;

    #[test]
    fn default_shell_prefers_bash_and_falls_back_to_sh() {
        assert_eq!(
            shell_argv(None).unwrap(),
            vec![
                "/bin/sh",
                "-c",
                "command -v bash >/dev/null 2>&1 && exec bash || exec sh"
            ]
        );
    }

    #[test]
    fn explicit_shell_is_passed_unchanged() {
        assert_eq!(shell_argv(Some("zsh")).unwrap(), vec!["zsh"]);
        assert!(shell_argv(Some("")).is_err());
    }
}
