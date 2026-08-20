use crate::tasks::ExecRequest;
use anyhow::{anyhow, bail, Context};
use std::{path::Path, process::Stdio};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
};
use tracing::{debug, info, warn};

use super::container::{self, RuncState};

pub struct RunningExec {
    pub child: Child,
    pub stdin: Option<tokio::process::ChildStdin>,
    pub stdout: Option<tokio::process::ChildStdout>,
    pub stderr: Option<tokio::process::ChildStderr>,
}

#[derive(Clone)]
pub(super) struct RuntimeClient {
    program: String,
    container_id: String,
}

impl RuntimeClient {
    pub(super) fn new(program: impl Into<String>, container_id: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            container_id: container_id.into(),
        }
    }

    pub(super) fn program(&self) -> &str {
        &self.program
    }

    pub(super) fn container_id(&self) -> &str {
        &self.container_id
    }

    pub(super) async fn probe(program: &str) -> anyhow::Result<()> {
        let version_args = probe_args(program);
        let output = Command::new(program)
            .args(version_args)
            .output()
            .await
            .with_context(|| format!("execute {program} {}", version_args.join(" ")))?;
        if !output.status.success() {
            bail!(
                "{program} failed {}: {}",
                version_args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        debug!(program, "dependency probe succeeded");
        Ok(())
    }

    pub(super) async fn state(&self) -> anyhow::Result<Option<RuncState>> {
        container::read_state(&self.program, &self.container_id).await
    }

    pub(super) async fn remove(&self) -> anyhow::Result<()> {
        container::remove(&self.program, &self.container_id).await
    }

    pub(super) fn spawn_container(&self, bundle_dir: &Path) -> anyhow::Result<Child> {
        let mut command = Command::new(&self.program);
        command
            .args([
                "run",
                "--bundle",
                bundle_dir.to_string_lossy().as_ref(),
                &self.container_id,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        container::configure_parent_death_signal(&mut command);
        let mut child = command.spawn().context("spawn foreground runtime")?;
        if let Some(stderr) = child.stderr.take() {
            let program = self.program.clone();
            let container_id = self.container_id.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                loop {
                    match lines.next_line().await {
                        Ok(Some(line)) => debug!(%program, %container_id, %line, "runtime stderr"),
                        Ok(None) => break,
                        Err(error) => {
                            warn!(%program, %container_id, error = %error, "read runtime stderr");
                            break;
                        }
                    }
                }
            });
        }
        Ok(child)
    }

    pub(super) async fn exec(&self, req: &ExecRequest) -> anyhow::Result<RunningExec> {
        if req.argv.is_empty() {
            return Err(anyhow!("argv must not be empty"));
        }
        let mut command = Command::new(&self.program);
        command.arg("exec");
        if let Some(cwd) = &req.cwd {
            command.args(["--cwd", cwd]);
        }
        for (key, value) in &req.env {
            command.arg("--env").arg(format!("{key}={value}"));
        }
        command
            .arg(&self.container_id)
            .args(&req.argv)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        debug!(
            runtime = %self.program,
            container_id = %self.container_id,
            program = %req.argv.first().map(String::as_str).unwrap_or(""),
            argc = req.argv.len(),
            "starting sandbox exec"
        );
        let mut child = command.spawn().context("spawn runtime exec")?;
        Ok(RunningExec {
            stdin: child.stdin.take(),
            stdout: child.stdout.take(),
            stderr: child.stderr.take(),
            child,
        })
    }

    pub(super) async fn stop(&self, mut child: Child) -> Vec<anyhow::Error> {
        let mut errors = Vec::new();
        match child.try_wait() {
            Ok(None) => {
                if let Err(error) = child.kill().await {
                    errors.push(anyhow!(error).context("kill foreground runtime"));
                }
            }
            Ok(Some(status)) => {
                container::log_runtime_exit(&self.program, &self.container_id, status)
            }
            Err(error) => errors.push(anyhow!(error).context("inspect foreground runtime")),
        }
        if let Err(error) = child.wait().await {
            errors.push(anyhow!(error).context("wait for foreground runtime"));
        }
        if errors.is_empty() {
            info!(
                runtime = %self.program,
                container_id = %self.container_id,
                "foreground runtime stopped"
            );
        }
        errors
    }
}

fn probe_args(program: &str) -> &'static [&'static str] {
    if program == "ip" {
        &["-V"]
    } else {
        &["--version"]
    }
}

#[cfg(test)]
mod tests {
    use super::probe_args;

    #[test]
    fn uses_iproute2_compatible_version_flag() {
        assert_eq!(probe_args("ip"), &["-V"]);
        assert_eq!(probe_args("runsc"), &["--version"]);
    }
}
