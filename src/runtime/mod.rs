mod bundle;
mod container;
mod network;

use crate::{config::SandboxConfig, tasks::ExecRequest};
use anyhow::{anyhow, bail, Context};
use std::{process::Stdio, sync::Arc};
use tokio::process::{Child, Command};

const CONTAINER_ID: &str = "agent-cell";
const CONFIG_HASH_ANNOTATION: &str = "io.agentcell.config-hash";

#[derive(Clone)]
pub struct Sandbox {
    runtime: Arc<String>,
    container_id: Arc<String>,
}

pub struct RunningExec {
    pub child: Child,
    pub stdin: Option<tokio::process::ChildStdin>,
    pub stdout: Option<tokio::process::ChildStdout>,
    pub stderr: Option<tokio::process::ChildStderr>,
}

impl Sandbox {
    pub async fn start(cfg: &SandboxConfig) -> anyhow::Result<Self> {
        let sysroot_issues = crate::config::sysroot_issues(cfg);
        if !sysroot_issues.is_empty() {
            let details = sysroot_issues
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            bail!(
                "sandbox sysroot is not ready: {details}; run `agent-cell fix` to repair directories"
            );
        }

        let settings = cfg
            .network_settings()
            .map_err(|error| anyhow!("invalid sandbox network configuration: {error}"))?;
        let runtime = cfg.backend.clone();
        Self::probe_program(&runtime).await.with_context(|| {
            format!("runtime {runtime} is not installed; install it before starting AgentCell")
        })?;

        let (bundle_dir, config_hash) = bundle::prepare_managed_bundle(cfg, &settings)?;

        let id = Arc::new(CONTAINER_ID.to_string());
        let mut state = container::read_state(&runtime, &id).await?;
        if let Some(existing) = &state {
            if !container::container_config_matches(existing, &settings.mode, &config_hash)? {
                container::recreate_container(&runtime, &id).await?;
                state = None;
            }
        }

        let mut created = false;
        if state.is_none() {
            let output = Command::new(&runtime)
                .args([
                    "run",
                    "--detach",
                    "--bundle",
                    bundle_dir.to_string_lossy().as_ref(),
                    &id,
                ])
                .output()
                .await
                .context("start sandbox container")?;
            if !output.status.success() {
                let details = String::from_utf8_lossy(&output.stderr).trim().to_string();
                if details.is_empty() {
                    return Err(anyhow!("failed to start container {id} with {runtime}"));
                }
                return Err(anyhow!(
                    "failed to start container {id} with {runtime}: {details}"
                ));
            }
            created = true;
        }

        let state = container::read_state(&runtime, &id)
            .await?
            .ok_or_else(|| anyhow!("container {id} did not remain running after startup"))?;
        if let Err(error) = network::configure_network(&settings, state.pid).await {
            if created {
                let _ = container::recreate_container(&runtime, &id).await;
            }
            return Err(error.context("configure sandbox network"));
        }

        Ok(Self {
            runtime: Arc::new(runtime),
            container_id: id,
        })
    }

    pub(crate) async fn probe_program(program: &str) -> anyhow::Result<()> {
        let output = Command::new(program)
            .arg("--version")
            .output()
            .await
            .with_context(|| format!("execute {program} --version"))?;
        if !output.status.success() {
            bail!(
                "{program} failed --version: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    }

    pub async fn exec(&self, req: &ExecRequest) -> anyhow::Result<RunningExec> {
        if req.argv.is_empty() {
            return Err(anyhow!("argv must not be empty"));
        }
        let mut cmd = Command::new(&*self.runtime);
        cmd.arg("exec").arg(&*self.container_id);
        if let Some(cwd) = &req.cwd {
            cmd.args(["--cwd", cwd]);
        }
        for (key, value) in &req.env {
            cmd.arg("--env").arg(format!("{key}={value}"));
        }
        cmd.args(&req.argv)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().context("spawn runtime exec")?;
        Ok(RunningExec {
            stdin: child.stdin.take(),
            stdout: child.stdout.take(),
            stderr: child.stderr.take(),
            child,
        })
    }

    #[cfg(test)]
    pub(crate) fn test_instance() -> Self {
        Self {
            runtime: Arc::new("test-runtime".into()),
            container_id: Arc::new("test-container".into()),
        }
    }
}
