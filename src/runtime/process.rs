use crate::{
    config::{CgroupAction, NetworkMode, RuntimeBackend},
    runtime::tasks::ExecRequest,
};
use anyhow::{anyhow, bail, Context};
use std::{path::Path, process::Stdio};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
};
use tracing::{debug, info, warn};

use super::args::{probe_args, run_args};
use super::container::{self, RuncState};

pub struct RunningExec {
    pub child: Child,
    pub stdin: Option<tokio::process::ChildStdin>,
    pub stdout: Option<tokio::process::ChildStdout>,
    pub stderr: Option<tokio::process::ChildStderr>,
}

#[derive(Clone)]
pub struct RuntimeClient {
    plan: RuntimePlan,
    container_id: String,
    program: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimePlan {
    pub backend: RuntimeBackend,
    pub network_mode: NetworkMode,
    pub cgroups: CgroupAction,
    pub allow_host_uds: bool,
    pub persist_rootfs: bool,
}

impl RuntimePlan {
    pub fn from_settings(
        backend: RuntimeBackend,
        network_mode: NetworkMode,
        cgroups: CgroupAction,
        has_tool_proxy: bool,
    ) -> Self {
        Self {
            backend,
            network_mode,
            cgroups,
            allow_host_uds: backend.is_runsc() && has_tool_proxy,
            persist_rootfs: backend.is_runsc(),
        }
    }

    pub fn program(&self) -> &'static str {
        self.backend.as_str()
    }
}

impl RuntimeClient {
    pub fn new(plan: RuntimePlan, container_id: impl Into<String>) -> Self {
        Self {
            program: plan.program().into(),
            plan,
            container_id: container_id.into(),
        }
    }

    #[cfg(test)]
    pub fn new_with_program(
        plan: RuntimePlan,
        container_id: impl Into<String>,
        program: impl Into<String>,
    ) -> Self {
        Self {
            plan,
            container_id: container_id.into(),
            program: program.into(),
        }
    }

    pub fn program(&self) -> &str {
        &self.program
    }

    pub fn container_id(&self) -> &str {
        &self.container_id
    }

    pub fn backend(&self) -> RuntimeBackend {
        self.plan.backend
    }

    pub async fn probe(program: &str) -> anyhow::Result<()> {
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

    pub async fn state(&self) -> anyhow::Result<Option<RuncState>> {
        container::read_state(&self.program, &self.container_id).await
    }

    pub async fn remove(&self) -> anyhow::Result<()> {
        container::remove(&self.program, &self.container_id).await
    }

    pub fn spawn_container(&self, bundle_dir: &Path) -> anyhow::Result<Child> {
        let mut command = Command::new(&self.program);
        command
            .args(run_args(&self.plan, bundle_dir, &self.container_id))
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

    pub async fn exec(&self, req: &ExecRequest) -> anyhow::Result<RunningExec> {
        self.exec_with_options(req, false).await
    }

    pub async fn exec_interactive(&self, req: &ExecRequest) -> anyhow::Result<RunningExec> {
        self.exec_with_options(req, true).await
    }

    pub async fn exec_attached(
        &self,
        req: &ExecRequest,
        interactive: bool,
    ) -> anyhow::Result<std::process::ExitStatus> {
        if req.argv.is_empty() {
            return Err(anyhow!("argv must not be empty"));
        }

        let mut command = Command::new(&self.program);
        command
            .args(self.exec_args(req, interactive))
            .envs(std::env::vars_os())
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        debug!(
            runtime = %self.program,
            container_id = %self.container_id,
            program = %req.argv.first().map(String::as_str).unwrap_or(""),
            argc = req.argv.len(),
            interactive,
            attached = true,
            "starting attached sandbox exec"
        );

        let mut child = command.spawn().context("spawn attached runtime exec")?;
        child.wait().await.context("wait for attached runtime exec")
    }

    async fn exec_with_options(
        &self,
        req: &ExecRequest,
        interactive: bool,
    ) -> anyhow::Result<RunningExec> {
        if req.argv.is_empty() {
            return Err(anyhow!("argv must not be empty"));
        }
        let mut command = Command::new(&self.program);
        command.args(self.exec_args(req, interactive));
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        debug!(
            runtime = %self.program,
            container_id = %self.container_id,
            program = %req.argv.first().map(String::as_str).unwrap_or(""),
            argc = req.argv.len(),
            interactive,
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

    pub(crate) fn exec_args(&self, req: &ExecRequest, interactive: bool) -> Vec<String> {
        let mut args = vec!["exec".to_owned()];
        if interactive && matches!(self.plan.backend, RuntimeBackend::Runc) {
            args.push("-i".to_owned());
        }
        if let Some(cwd) = &req.cwd {
            args.extend(["--cwd".to_owned(), cwd.clone()]);
        }
        for (key, value) in &req.env {
            args.extend(["--env".to_owned(), format!("{key}={value}")]);
        }
        args.push(self.container_id.clone());
        args.extend(req.argv.iter().cloned());
        args
    }

    pub async fn stop(&self, mut child: Child) -> Vec<anyhow::Error> {
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

#[cfg(test)]
mod tests {
    use super::{
        super::args::{probe_args, run_args},
        RuntimeClient, RuntimePlan,
    };
    use crate::config::{CgroupAction, NetworkMode, RuntimeBackend};
    use crate::runtime::tasks::ExecRequest;
    use std::collections::HashMap;

    #[test]
    fn uses_iproute2_compatible_version_flag() {
        assert_eq!(probe_args("ip"), &["-V"]);
        assert_eq!(probe_args("runsc"), &["--version"]);
    }

    #[test]
    fn runsc_disables_temporary_rootfs_overlay() {
        let runtime = RuntimeClient::new(
            RuntimePlan::from_settings(
                RuntimeBackend::Runsc,
                NetworkMode::Nat,
                CgroupAction::Use,
                false,
            ),
            "kekkai-rt-abcd1234",
        );
        assert_eq!(
            run_args(
                &runtime.plan,
                std::path::Path::new("/bundle"),
                &runtime.container_id,
            ),
            vec![
                "--overlay2=none",
                "run",
                "--bundle",
                "/bundle",
                "kekkai-rt-abcd1234",
            ]
        );
    }

    #[test]
    fn runsc_receives_network_and_cgroup_compatibility_flags() {
        let args = run_args(
            &RuntimePlan::from_settings(
                RuntimeBackend::Runsc,
                NetworkMode::Host,
                CgroupAction::Ignore,
                false,
            ),
            std::path::Path::new("/bundle"),
            "id",
        );
        assert_eq!(
            args,
            vec![
                "--ignore-cgroups",
                "--network=host",
                "--overlay2=none",
                "run",
                "--bundle",
                "/bundle",
                "id",
            ]
        );

        let args = run_args(
            &RuntimePlan::from_settings(
                RuntimeBackend::Runsc,
                NetworkMode::None,
                CgroupAction::Use,
                false,
            ),
            std::path::Path::new("/bundle"),
            "id",
        );
        assert_eq!(args[0], "--network=none");
        assert!(args.iter().any(|arg| arg == "run"));
    }

    #[test]
    fn runc_does_not_receive_runsc_flags() {
        let args = run_args(
            &RuntimePlan::from_settings(
                RuntimeBackend::Runc,
                NetworkMode::Host,
                CgroupAction::Ignore,
                true,
            ),
            std::path::Path::new("/bundle"),
            "id",
        );
        assert_eq!(args, vec!["run", "--bundle", "/bundle", "id"]);
    }

    #[test]
    fn interactive_exec_adds_i_only_for_runc() {
        let request = ExecRequest {
            argv: vec!["/bin/sh".into()],
            cwd: None,
            env: HashMap::new(),
            stdin: None,
            timeout_seconds: None,
        };
        let runc = RuntimeClient::new(
            RuntimePlan::from_settings(
                RuntimeBackend::Runc,
                NetworkMode::Host,
                CgroupAction::Ignore,
                false,
            ),
            "container",
        );
        assert_eq!(
            runc.exec_args(&request, true),
            vec!["exec", "-i", "container", "/bin/sh"]
        );

        let runsc = RuntimeClient::new(
            RuntimePlan::from_settings(
                RuntimeBackend::Runsc,
                NetworkMode::Host,
                CgroupAction::Ignore,
                false,
            ),
            "container",
        );
        assert_eq!(
            runsc.exec_args(&request, true),
            vec!["exec", "container", "/bin/sh"]
        );
    }
}
