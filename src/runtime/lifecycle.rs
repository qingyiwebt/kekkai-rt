use super::{
    bundle,
    command::{CommandExecutor, TokioCommandExecutor},
    network::NetworkSession,
    process::{RuntimeClient, RuntimePlan},
    session::ContainerSession,
    Sandbox,
};
use crate::{
    config::{FeaturesConfig, NetworkMode, SandboxConfig, ToolConfig},
    proxy::ToolProxy,
};
use anyhow::{anyhow, bail, Context};
use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
    time::Duration,
};
use tokio::time::{sleep, timeout, Instant};
use tracing::{debug, info, warn};

use super::host::HostCapabilities;

pub(crate) const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
const STATE_POLL_INTERVAL: Duration = Duration::from_millis(25);

pub(crate) enum RuntimeProgram {
    Configured,
    #[cfg(test)]
    Test(std::path::PathBuf),
}

struct StartupResources {
    network: Option<NetworkSession>,
    proxy: Option<ToolProxy>,
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

pub(crate) async fn start(
    cfg: &SandboxConfig,
    features: &FeaturesConfig,
    mounts: &BTreeMap<std::path::PathBuf, std::path::PathBuf>,
    configured_tools: &HashMap<String, ToolConfig>,
    runtime_program: RuntimeProgram,
) -> anyhow::Result<Sandbox> {
    let instance_id = super::instance::id(cfg);
    let instance_lock = super::instance::acquire_lock(cfg)?;
    let sysroot_issues = crate::maintenance::sysroot::sysroot_issues(cfg, mounts);
    if !sysroot_issues.is_empty() {
        let details = sysroot_issues
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        bail!("sandbox sysroot is not ready: {details}; run `kekkai-rt fix` to repair directories");
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
        bail!("sandbox network_mode=nat requires Linux network administration and route/netfilter support; unavailable: {reasons}; use network_mode=\"host\" or grant the missing capability");
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
    Sandbox::probe_program(runtime.program())
        .await
        .with_context(|| {
            format!(
                "runtime {} is not installed; install it before starting Kekkai Runtime",
                runtime.program()
            )
        })?;
    runtime.remove().await?;
    let executor: Arc<dyn CommandExecutor> = Arc::new(TokioCommandExecutor);
    let network = super::network::prepare_network(
        settings,
        &instance_id,
        super::network_ops::default_backend(),
        executor,
    )
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
    info!(runtime = %runtime.program(), container_id = %runtime.container_id(), bundle = %bundle_dir.display(), network_mode = %settings.mode.as_str(), "starting sandbox container");
    let child = match runtime.spawn_container(&bundle_dir) {
        Ok(child) => child,
        Err(error) => {
            resources.cleanup().await;
            return Err(error);
        }
    };
    let (network, tool_proxy) = resources.into_parts();
    let sandbox = Sandbox {
        session: Some(Arc::new(ContainerSession::new(
            runtime, child, network, tool_proxy,
        ))),
        _instance_lock: Some(instance_lock),
    };
    let startup = async {
        let state = wait_until_running(&sandbox).await?;
        let session = sandbox
            .session
            .as_ref()
            .expect("running sandbox has a container session");
        session
            .configure_network(settings, state.pid)
            .await
            .context("configure sandbox network")?;
        info!(runtime = %session.runtime.program(), container_id = %session.runtime.container_id(), pid = state.pid, "sandbox container is ready");
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

pub(crate) async fn wait_until_running(
    sandbox: &Sandbox,
) -> anyhow::Result<super::container::RuncState> {
    let session = sandbox
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
                    super::container::log_runtime_exit(
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
        debug!(runtime = %session.runtime.program(), container_id = %session.runtime.container_id(), "waiting for sandbox container state");
        sleep(STATE_POLL_INTERVAL).await;
    }
}
