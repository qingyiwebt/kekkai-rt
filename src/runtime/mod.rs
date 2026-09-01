mod bundle;
mod command;
mod container;
mod network;
mod process;
mod session;

use crate::{
    config::{FeaturesConfig, NetworkMode, SandboxConfig, ToolConfig},
    host::HostCapabilities,
    proxy::ToolProxy,
    tasks::ExecRequest,
};
use anyhow::{anyhow, bail, Context};
use command::{CommandExecutor, TokioCommandExecutor};
use network::NetworkSession;
use process::{RuntimeClient, RuntimePlan};
use session::ContainerSession;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap},
    fs::{self, File},
    os::unix::io::AsRawFd,
    sync::Arc,
    time::Duration,
};
use tokio::time::{sleep, timeout, Instant};
use tracing::{debug, info, warn};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
const STATE_POLL_INTERVAL: Duration = Duration::from_millis(25);

pub struct Sandbox {
    session: Option<Arc<ContainerSession>>,
    _instance_lock: Option<File>,
}

struct StartupResources {
    network: Option<NetworkSession>,
    proxy: Option<ToolProxy>,
}

enum RuntimeProgram {
    Configured,
    #[cfg(test)]
    Test(std::path::PathBuf),
}

impl StartupResources {
    fn new(network: NetworkSession) -> Self {
        Self {
            network: Some(network),
            proxy: None,
        }
    }

    fn proxy_mounts(&self) -> Option<Vec<crate::proxy::ToolSocketMount>> {
        self.proxy.as_ref().map(ToolProxy::socket_mounts)
    }

    fn into_parts(mut self) -> (NetworkSession, Option<ToolProxy>) {
        (
            self.network
                .take()
                .expect("startup resources contain a network session"),
            self.proxy.take(),
        )
    }

    async fn cleanup(mut self) {
        if let Some(proxy) = self.proxy.take() {
            if let Err(error) = proxy.shutdown().await {
                warn!(error = %error, "startup tool proxy cleanup failed");
            }
        }
        if let Some(network) = self.network.take() {
            if let Err(error) = network.cleanup().await {
                warn!(error = %error, "startup network cleanup failed");
            }
        }
    }
}

pub use process::RunningExec;

impl Sandbox {
    pub async fn start(
        cfg: &SandboxConfig,
        features: &FeaturesConfig,
        mounts: &BTreeMap<std::path::PathBuf, std::path::PathBuf>,
        configured_tools: &HashMap<String, ToolConfig>,
    ) -> anyhow::Result<Self> {
        Self::start_impl(
            cfg,
            features,
            mounts,
            configured_tools,
            RuntimeProgram::Configured,
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
        Self::start_impl(
            cfg,
            features,
            mounts,
            configured_tools,
            RuntimeProgram::Test(program.to_owned()),
        )
        .await
    }

    async fn start_impl(
        cfg: &SandboxConfig,
        features: &FeaturesConfig,
        mounts: &BTreeMap<std::path::PathBuf, std::path::PathBuf>,
        configured_tools: &HashMap<String, ToolConfig>,
        runtime_program: RuntimeProgram,
    ) -> anyhow::Result<Self> {
        let instance_id = instance_id(cfg);
        let instance_lock = acquire_instance_lock(cfg)?;
        let sysroot_issues = crate::maintenance::sysroot::sysroot_issues(cfg, mounts);
        if !sysroot_issues.is_empty() {
            let details = sysroot_issues
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            bail!(
                "sandbox sysroot is not ready: {details}; run `kekkai-rt fix` to repair directories"
            );
        }

        let resolved = cfg
            .resolved()
            .map_err(|error| anyhow!("invalid sandbox configuration: {error}"))?;
        let settings = &resolved.network;
        let capabilities = HostCapabilities::detect();
        let resolved_features = features
            .resolve(&capabilities)
            .map_err(|error| anyhow!("resolve runtime features: {error}"))?;
        if matches!(settings.mode, NetworkMode::Nat) && !capabilities.nat_available() {
            let reasons = capabilities.nat_unavailability_reasons().join(", ");
            bail!(
                "sandbox network_mode=nat requires CAP_NET_ADMIN and executable ip, nsenter, and iptables; unavailable: {reasons}; use network_mode=\"host\" or grant the missing capability"
            );
        }
        let plan = RuntimePlan::from_settings(
            resolved.backend,
            settings.mode,
            resolved_features.cgroups,
            !configured_tools.is_empty(),
        );
        let runtime = match runtime_program {
            RuntimeProgram::Configured => RuntimeClient::new(plan, &instance_id),
            #[cfg(test)]
            RuntimeProgram::Test(program) => RuntimeClient::new_with_program(
                plan,
                &instance_id,
                program.to_string_lossy().into_owned(),
            ),
        };
        Self::probe_program(runtime.program())
            .await
            .with_context(|| {
                format!(
                    "runtime {} is not installed; install it before starting Kekkai Runtime",
                    runtime.program()
                )
            })?;

        runtime.remove().await?;
        let command_executor: Arc<dyn CommandExecutor> = Arc::new(TokioCommandExecutor);
        let network = network::prepare_network(settings, &instance_id, command_executor)
            .await
            .context("prepare sandbox network")?;
        let mut resources = StartupResources::new(network);
        resources.proxy = match ToolProxy::start(cfg, configured_tools).await {
            Ok(proxy) => proxy,
            Err(error) => {
                resources.cleanup().await;
                return Err(error.context("start tool proxy"));
            }
        };
        let tool_mounts = resources.proxy_mounts();
        let bundle_dir = match bundle::prepare_managed_bundle(
            cfg,
            resources
                .network
                .as_ref()
                .expect("startup resources contain a network session")
                .attachment(),
            mounts,
            tool_mounts.as_deref(),
        ) {
            Ok(path) => path,
            Err(error) => {
                resources.cleanup().await;
                return Err(error);
            }
        };

        info!(
            runtime = %runtime.program(),
            container_id = %runtime.container_id(),
            bundle = %bundle_dir.display(),
            network_mode = %settings.mode.as_str(),
            "starting sandbox container"
        );

        let child = match runtime.spawn_container(&bundle_dir) {
            Ok(child) => child,
            Err(error) => {
                resources.cleanup().await;
                return Err(error);
            }
        };
        let (network, tool_proxy) = resources.into_parts();
        let session = Arc::new(ContainerSession::new(runtime, child, network, tool_proxy));
        let sandbox = Self {
            session: Some(session),
            _instance_lock: Some(instance_lock),
        };

        let startup = async {
            let state = sandbox.wait_until_running().await?;
            let session = sandbox
                .session
                .as_ref()
                .expect("running sandbox has a container session");
            session
                .configure_network(settings, state.pid)
                .await
                .context("configure sandbox network")?;
            info!(
                runtime = %session.runtime.program(),
                container_id = %session.runtime.container_id(),
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
        let session = self
            .session
            .as_ref()
            .expect("running sandbox has a container session");
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        loop {
            if let Some(state) = session.runtime.state().await? {
                return Ok(state);
            }

            {
                let mut child = session.child.lock().await;
                if let Some(child) = child.as_mut() {
                    if let Some(status) = child.try_wait()? {
                        container::log_runtime_exit(
                            session.runtime.program(),
                            session.runtime.container_id(),
                            status,
                        );
                        bail!(
                            "runtime {} exited before container {} became ready",
                            session.runtime.program(),
                            session.runtime.container_id()
                        );
                    }
                } else {
                    bail!("sandbox runtime exited before the container became ready");
                }
            }
            if Instant::now() >= deadline {
                bail!(
                    "container {} did not become ready within {} seconds",
                    session.runtime.container_id(),
                    STARTUP_TIMEOUT.as_secs()
                );
            }
            debug!(
                runtime = %session.runtime.program(),
                container_id = %session.runtime.container_id(),
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
        RuntimeClient::probe(program).await
    }

    pub async fn exec(&self, req: &ExecRequest) -> anyhow::Result<RunningExec> {
        let session = self
            .session
            .as_ref()
            .ok_or_else(|| anyhow!("sandbox is not running"))?;
        session.runtime.exec(req).await
    }

    #[cfg(test)]
    pub(crate) fn test_instance() -> Self {
        Self {
            session: None,
            _instance_lock: None,
        }
    }
}

fn instance_id(cfg: &SandboxConfig) -> String {
    let source = cfg.managed_bundle_dir.to_string_lossy();
    let digest = Sha256::digest(source.as_bytes());
    let suffix = digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("kekkai-rt-{suffix}")
}

fn acquire_instance_lock(cfg: &SandboxConfig) -> anyhow::Result<File> {
    fs::create_dir_all(&cfg.managed_bundle_dir).with_context(|| {
        format!(
            "create managed bundle directory {}",
            cfg.managed_bundle_dir.display()
        )
    })?;
    let path = cfg.managed_bundle_dir.join(".lock");
    let file = File::options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("open sandbox instance lock {}", path.display()))?;
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result != 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::WouldBlock {
            bail!(
                "sandbox instance is already running for {}",
                cfg.managed_bundle_dir.display()
            );
        }
        return Err(error).context("acquire sandbox instance lock");
    }
    Ok(file)
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
