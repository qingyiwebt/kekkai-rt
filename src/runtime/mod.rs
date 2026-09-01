mod args;
mod bundle;
mod command;
mod container;
pub(crate) mod execution;
pub(crate) mod host;
mod instance;
mod lifecycle;
mod network;
mod network_ops;
mod process;
mod session;
pub(crate) mod tasks;

use crate::config::{FeaturesConfig, SandboxConfig, ToolConfig};
use anyhow::{anyhow, bail, Context};
use process::{RuntimeClient, RuntimePlan};
use std::{
    collections::{BTreeMap, HashMap},
    fs::File,
    sync::Arc,
};
use tasks::ExecRequest;

pub struct Sandbox {
    pub(crate) session: Option<Arc<session::ContainerSession>>,
    pub(crate) _instance_lock: Option<File>,
}

pub use process::RunningExec;

impl Sandbox {
    pub async fn start(
        cfg: &SandboxConfig,
        features: &FeaturesConfig,
        mounts: &BTreeMap<std::path::PathBuf, std::path::PathBuf>,
        configured_tools: &HashMap<String, ToolConfig>,
    ) -> anyhow::Result<Self> {
        lifecycle::start(
            cfg,
            features,
            mounts,
            configured_tools,
            lifecycle::RuntimeProgram::Configured,
        )
        .await
    }

    #[cfg(test)]
    async fn start_with_test_program(
        cfg: &SandboxConfig,
        features: &FeaturesConfig,
        mounts: &BTreeMap<std::path::PathBuf, std::path::PathBuf>,
        configured_tools: &HashMap<String, ToolConfig>,
        program: &std::path::Path,
    ) -> anyhow::Result<Self> {
        lifecycle::start(
            cfg,
            features,
            mounts,
            configured_tools,
            lifecycle::RuntimeProgram::Test(program.to_owned()),
        )
        .await
    }

    pub async fn shutdown(&self) -> anyhow::Result<()> {
        let Some(session) = &self.session else {
            return Ok(());
        };
        session.shutdown().await
    }

    pub async fn probe_program(program: &str) -> anyhow::Result<()> {
        RuntimeClient::probe(program).await
    }

    pub async fn exec(&self, req: &ExecRequest) -> anyhow::Result<RunningExec> {
        let session = self
            .session
            .as_ref()
            .ok_or_else(|| anyhow!("sandbox is not running"))?;
        session.runtime.exec(req).await
    }

    pub async fn exec_existing(
        cfg: &SandboxConfig,
        req: &ExecRequest,
        interactive: bool,
    ) -> anyhow::Result<RunningExec> {
        let resolved = cfg
            .resolved()
            .map_err(|error| anyhow!("invalid sandbox configuration: {error}"))?;
        let runtime = RuntimeClient::new(
            RuntimePlan::from_settings(
                resolved.backend,
                resolved.network.mode,
                crate::config::CgroupAction::Ignore,
                false,
            ),
            instance::id(cfg),
        );
        if runtime
            .state()
            .await
            .context("read existing sandbox state")?
            .is_none()
        {
            bail!(
                "sandbox container {} is not running",
                runtime.container_id()
            );
        }
        if interactive {
            runtime.exec_interactive(req).await
        } else {
            runtime.exec(req).await
        }
    }

    #[cfg(test)]
    pub fn test_instance() -> Self {
        Self {
            session: None,
            _instance_lock: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::NetworkMode;
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
            crate::maintenance::sysroot::prepare_sysroot(&rootfs, &BTreeMap::new()).unwrap();
            let mut config: SandboxConfig = toml::from_str("rootfs_dir = \".\"").unwrap();
            config.rootfs_dir = rootfs;
            config.backend = crate::config::RuntimeBackend::Runc;
            config.network_mode = NetworkMode::Host;
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
        let sandbox = Sandbox::start_with_test_program(
            &fake.config(temp.path()),
            &FeaturesConfig::default(),
            &BTreeMap::new(),
            &HashMap::new(),
            &fake.executable,
        )
        .await
        .unwrap();

        let lines = fake.log_lines();
        let run = lines.iter().find(|line| line.starts_with("run ")).unwrap();
        assert!(!run.contains("--detach"));
        assert!(
            lines
                .iter()
                .position(|line| line.starts_with("delete "))
                .unwrap()
                < lines
                    .iter()
                    .position(|line| line.starts_with("run "))
                    .unwrap()
        );

        let mut env = HashMap::new();
        env.insert("FOO".into(), "bar".into());
        let request = ExecRequest {
            argv: vec!["/bin/echo".into(), "hello".into()],
            cwd: Some("/workspace".into()),
            env,
            stdin: None,
            timeout_seconds: None,
        };
        let running = sandbox.exec(&request).await.unwrap();
        assert!(running
            .child
            .wait_with_output()
            .await
            .unwrap()
            .status
            .success());
        let exec = fake
            .log_lines()
            .into_iter()
            .find(|line| line.starts_with("exec "))
            .unwrap();
        assert!(exec.starts_with("exec --cwd /workspace --env FOO=bar kekkai-rt-"));
        assert!(exec.ends_with(" /bin/echo hello"));

        sandbox.shutdown().await.unwrap();
        sandbox.shutdown().await.unwrap();
        let lines = fake.log_lines();
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.starts_with("delete "))
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn runtime_exit_during_startup_is_cleaned() {
        let temp = tempdir().unwrap();
        let fake = FakeRuntime::new(temp.path());
        fs::write(&fake.mode, b"exit").unwrap();

        let error = match Sandbox::start_with_test_program(
            &fake.config(temp.path()),
            &FeaturesConfig::default(),
            &BTreeMap::new(),
            &HashMap::new(),
            &fake.executable,
        )
        .await
        {
            Ok(_) => panic!("sandbox unexpectedly started"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("exited before container"));
        let lines = fake.log_lines();
        assert!(lines.iter().any(|line| line.starts_with("delete ")));
    }
}
