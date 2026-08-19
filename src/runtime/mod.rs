mod bundle;
mod container;
mod network;

use crate::{config::SandboxConfig, tasks::ExecRequest};
use anyhow::{anyhow, bail, Context};
use std::{process::Stdio, sync::Arc, time::Duration};
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
    sync::Mutex,
    time::{sleep, timeout, Instant},
};
use tracing::{debug, info, warn};

const CONTAINER_ID: &str = "agent-cell";
const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
const STATE_POLL_INTERVAL: Duration = Duration::from_millis(25);

pub struct Sandbox {
    runtime: String,
    container_id: String,
    session: Option<Arc<ContainerSession>>,
}

struct ContainerSession {
    runtime: String,
    container_id: String,
    child: Mutex<Option<Child>>,
    stopped: AtomicBool,
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

        let bundle_dir = bundle::prepare_managed_bundle(cfg, &settings)?;
        let container_id = CONTAINER_ID.to_string();
        container::remove(&runtime, &container_id).await?;
        network::cleanup_session().await;

        info!(
            runtime = %runtime,
            container_id = %container_id,
            bundle = %bundle_dir.display(),
            network_mode = %settings.mode.as_str(),
            "starting sandbox container"
        );

        let child = spawn_runtime(&runtime, &bundle_dir, &container_id)?;
        let session = Arc::new(ContainerSession {
            runtime: runtime.clone(),
            container_id: container_id.clone(),
            child: Mutex::new(Some(child)),
            stopped: AtomicBool::new(false),
        });
        let sandbox = Self {
            runtime,
            container_id,
            session: Some(session),
        };

        let startup = async {
            let state = sandbox.wait_until_running().await?;
            network::configure_network(&settings, state.pid)
                .await
                .context("configure sandbox network")?;
            info!(
                runtime = %sandbox.runtime,
                container_id = %sandbox.container_id,
                pid = state.pid,
                "sandbox container is ready"
            );
            Ok::<_, anyhow::Error>(())
        };

        if let Err(error) = timeout(STARTUP_TIMEOUT, startup)
            .await
            .context("wait for sandbox startup")?
        {
            if let Err(cleanup_error) = sandbox.shutdown().await {
                warn!(error = %cleanup_error, "sandbox startup cleanup failed");
            }
            return Err(error);
        }

        Ok(sandbox)
    }

    async fn wait_until_running(&self) -> anyhow::Result<container::RuncState> {
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        loop {
            if let Some(state) = container::read_state(&self.runtime, &self.container_id).await? {
                return Ok(state);
            }

            let runtime_exit = {
                let mut child = self
                    .session
                    .as_ref()
                    .expect("running sandbox has a container session")
                    .child
                    .lock()
                    .await;
                child
                    .as_mut()
                    .expect("running sandbox owns its runtime child")
                    .try_wait()?
            };
            if let Some(status) = runtime_exit {
                container::log_runtime_exit(&self.runtime, &self.container_id, status);
                bail!(
                    "runtime {} exited before container {} became ready",
                    self.runtime,
                    self.container_id
                );
            }
            if Instant::now() >= deadline {
                bail!(
                    "container {} did not become ready within {} seconds",
                    self.container_id,
                    STARTUP_TIMEOUT.as_secs()
                );
            }
            debug!(
                runtime = %self.runtime,
                container_id = %self.container_id,
                "waiting for sandbox container state"
            );
            sleep(STATE_POLL_INTERVAL).await;
        }
    }

    pub async fn shutdown(&self) -> anyhow::Result<()> {
        let Some(session) = &self.session else {
            return Ok(());
        };
        session.shutdown().await
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
        debug!(program, "dependency probe succeeded");
        Ok(())
    }

    pub async fn exec(&self, req: &ExecRequest) -> anyhow::Result<RunningExec> {
        if req.argv.is_empty() {
            return Err(anyhow!("argv must not be empty"));
        }
        let mut cmd = Command::new(&self.runtime);
        cmd.arg("exec");
        if let Some(cwd) = &req.cwd {
            cmd.args(["--cwd", cwd]);
        }
        for (key, value) in &req.env {
            cmd.arg("--env").arg(format!("{key}={value}"));
        }
        cmd.arg(&self.container_id);
        cmd.args(&req.argv)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        debug!(
            runtime = %self.runtime,
            container_id = %self.container_id,
            program = %req.argv.first().map(String::as_str).unwrap_or(""),
            argc = req.argv.len(),
            "starting sandbox exec"
        );
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
            runtime: "test-runtime".into(),
            container_id: "test-container".into(),
            session: None,
        }
    }
}

impl ContainerSession {
    async fn shutdown(&self) -> anyhow::Result<()> {
        if self.stopped.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let mut errors = Vec::new();
        info!(
            runtime = %self.runtime,
            container_id = %self.container_id,
            "stopping sandbox container"
        );

        if let Some(mut child) = self.child.lock().await.take() {
            match child.try_wait() {
                Ok(None) => {
                    if let Err(error) = child.kill().await {
                        errors.push(anyhow!(error).context("kill foreground runtime"));
                    }
                }
                Ok(Some(status)) => {
                    container::log_runtime_exit(&self.runtime, &self.container_id, status);
                }
                Err(error) => errors.push(anyhow!(error).context("inspect foreground runtime")),
            }
            if let Err(error) = child.wait().await {
                errors.push(anyhow!(error).context("wait for foreground runtime"));
            }
        }

        if let Err(error) = container::remove(&self.runtime, &self.container_id).await {
            errors.push(error.context("remove sandbox container"));
        }
        network::cleanup_session().await;

        if errors.is_empty() {
            info!(
                runtime = %self.runtime,
                container_id = %self.container_id,
                "sandbox container stopped"
            );
            Ok(())
        } else {
            Err(anyhow!("sandbox shutdown failed: {}", format_errors(errors)))
        }
    }
}

fn spawn_runtime(runtime: &str, bundle_dir: &std::path::Path, container_id: &str) -> anyhow::Result<Child> {
    let mut command = Command::new(runtime);
    command
        .args([
            "run",
            "--bundle",
            bundle_dir.to_string_lossy().as_ref(),
            container_id,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    container::configure_parent_death_signal(&mut command);
    let mut child = command.spawn().context("spawn foreground runtime")?;
    if let Some(stderr) = child.stderr.take() {
        let runtime = runtime.to_owned();
        let container_id = container_id.to_owned();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => debug!(%runtime, %container_id, %line, "runtime stderr"),
                    Ok(None) => break,
                    Err(error) => {
                        warn!(%runtime, %container_id, error = %error, "read runtime stderr");
                        break;
                    }
                }
            }
        });
    }
    Ok(child)
}

fn format_errors(errors: Vec<anyhow::Error>) -> String {
    errors
        .into_iter()
        .map(|error| error.to_string())
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        path::{Path, PathBuf},
    };
    use tempfile::tempdir;

    struct FakeRuntime {
        executable: PathBuf,
        log: PathBuf,
        mode: PathBuf,
    }

    impl FakeRuntime {
        fn new(root: &Path) -> Self {
            let executable = root.join("fake-runtime");
            let log = root.join("runtime.log");
            let state = root.join("runtime.state");
            let mode = root.join("runtime.mode");
            let script = format!(
                r#"#!/bin/sh
set -eu
log={log}
state={state}
mode={mode}
printf '%s\n' "$*" >> "$log"
case "$1" in
  --version) echo fake-runtime ;;
  state)
    if [ -f "$state" ]; then
      printf '{{"pid":%s,"status":"running"}}\n' "$(cat "$state")"
    else
      exit 1
    fi
    ;;
  run)
    if [ -f "$mode" ]; then exit 17; fi
    echo "$$" > "$state"
    trap 'exit 0' TERM INT
    while :; do sleep 1; done
    ;;
  kill) exit 0 ;;
  delete) rm -f "$state" ;;
  exec) exit 0 ;;
  *) exit 2 ;;
esac
"#,
                log = shell_quote(&log),
                state = shell_quote(&state),
                mode = shell_quote(&mode),
            );
            fs::write(&executable, script).unwrap();
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
            Self {
                executable,
                log,
                mode,
            }
        }

        fn config(&self, root: &Path) -> SandboxConfig {
            let rootfs = root.join("rootfs");
            fs::create_dir_all(rootfs.join("bin")).unwrap();
            fs::write(rootfs.join("bin/sh"), b"shell").unwrap();
            crate::config::prepare_sysroot(&rootfs, None).unwrap();
            let mut config: SandboxConfig = toml::from_str("rootfs_dir = \".\"").unwrap();
            config.rootfs_dir = rootfs;
            config.backend = self.executable.to_string_lossy().into_owned();
            config.network_mode = "host".into();
            config.managed_bundle_dir = root.join("bundle");
            config
        }

        fn log_lines(&self) -> Vec<String> {
            fs::read_to_string(&self.log)
                .unwrap_or_default()
                .lines()
                .map(str::to_owned)
                .collect()
        }
    }

    fn shell_quote(path: &Path) -> String {
        format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
    }

    #[tokio::test]
    async fn foreground_runtime_is_owned_and_cleaned() {
        let temp = tempdir().unwrap();
        let fake = FakeRuntime::new(temp.path());
        let sandbox = Sandbox::start(&fake.config(temp.path())).await.unwrap();

        let lines = fake.log_lines();
        let run = lines.iter().find(|line| line.starts_with("run ")).unwrap();
        assert!(!run.contains("--detach"));
        assert!(lines.iter().position(|line| line.starts_with("delete ")).unwrap()
            < lines.iter().position(|line| line.starts_with("run ")).unwrap());

        let mut env = std::collections::HashMap::new();
        env.insert("FOO".into(), "bar".into());
        let request = ExecRequest {
            argv: vec!["/bin/echo".into(), "hello".into()],
            cwd: Some("/workspace".into()),
            env,
            stdin: None,
            timeout_seconds: None,
        };
        let running = sandbox.exec(&request).await.unwrap();
        assert!(running.child.wait_with_output().await.unwrap().status.success());
        let exec = fake
            .log_lines()
            .into_iter()
            .find(|line| line.starts_with("exec "))
            .unwrap();
        assert_eq!(
            exec,
            "exec --cwd /workspace --env FOO=bar agent-cell /bin/echo hello"
        );

        sandbox.shutdown().await.unwrap();
        sandbox.shutdown().await.unwrap();
        let lines = fake.log_lines();
        assert_eq!(lines.iter().filter(|line| line.starts_with("delete ")).count(), 2);
    }

    #[tokio::test]
    async fn runtime_exit_during_startup_is_cleaned() {
        let temp = tempdir().unwrap();
        let fake = FakeRuntime::new(temp.path());
        fs::write(&fake.mode, b"exit").unwrap();

        let error = match Sandbox::start(&fake.config(temp.path())).await {
            Ok(_) => panic!("sandbox unexpectedly started"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("exited before container"));
        let lines = fake.log_lines();
        assert!(lines.iter().any(|line| line.starts_with("delete ")));
    }
}
