use crate::{
    config::Config,
    runtime::{tasks::ExecRequest, Sandbox},
};
use anyhow::{bail, Context};
use std::path::Path;
use tokio::io::{self, AsyncWriteExt};

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
    let mut exec = Sandbox::exec_existing(&config.sandbox, &request, true)
        .await
        .context("exec shell in sandbox")?;
    let mut stdin = exec
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("runtime did not provide shell stdin"))?;
    let mut stdout = exec
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("runtime did not provide shell stdout"))?;
    let mut stderr = exec
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("runtime did not provide shell stderr"))?;

    let stdin_task = tokio::spawn(async move {
        let mut input = io::stdin();
        let result = io::copy(&mut input, &mut stdin).await;
        let _ = stdin.shutdown().await;
        result
    });
    let stdout_task = tokio::spawn(async move {
        let mut output = io::stdout();
        io::copy(&mut stdout, &mut output).await
    });
    let stderr_task = tokio::spawn(async move {
        let mut output = io::stderr();
        io::copy(&mut stderr, &mut output).await
    });

    let status = exec.child.wait().await.context("wait for shell")?;
    let _ = stdin_task.await;
    forward_output(stdout_task.await.context("join shell stdout task")?)?;
    forward_output(stderr_task.await.context("join shell stderr task")?)?;

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

fn forward_output(result: io::Result<u64>) -> anyhow::Result<()> {
    result.map(|_| ()).context("forward shell output")
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
