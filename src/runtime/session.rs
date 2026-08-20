use super::{network::NetworkSession, process::RuntimeClient};
use anyhow::anyhow;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::{process::Child, sync::Mutex};
use tracing::info;

pub(super) struct ContainerSession {
    pub(super) runtime: RuntimeClient,
    pub(super) child: Mutex<Option<Child>>,
    pub(super) network: Mutex<Option<NetworkSession>>,
    stopped: AtomicBool,
}

impl ContainerSession {
    pub(super) fn new(runtime: RuntimeClient, child: Child, network: NetworkSession) -> Self {
        Self {
            runtime,
            child: Mutex::new(Some(child)),
            network: Mutex::new(Some(network)),
            stopped: AtomicBool::new(false),
        }
    }

    pub(super) async fn configure_network(
        &self,
        settings: &crate::config::NetworkSettings,
        pid: i32,
    ) -> anyhow::Result<()> {
        let network = self.network.lock().await;
        let network = network
            .as_ref()
            .ok_or_else(|| anyhow!("sandbox network session is unavailable"))?;
        super::network::configure_network(
            network,
            settings,
            pid,
            self.runtime.program(),
            self.runtime.container_id(),
        )
        .await
    }

    pub(super) async fn shutdown(&self) -> anyhow::Result<()> {
        if self.stopped.swap(true, Ordering::AcqRel) {
            return Ok(());
        }

        info!(
            runtime = %self.runtime.program(),
            container_id = %self.runtime.container_id(),
            "stopping sandbox container"
        );
        let mut errors = Vec::new();
        if let Some(child) = self.child.lock().await.take() {
            errors.extend(self.runtime.stop(child).await);
        }
        if let Err(error) = self.runtime.remove().await {
            errors.push(error.context("remove sandbox container"));
        }
        if let Some(network) = self.network.lock().await.take() {
            if let Err(error) = network.cleanup().await {
                errors.push(error.context("cleanup sandbox network"));
            }
        }

        if errors.is_empty() {
            info!(
                runtime = %self.runtime.program(),
                container_id = %self.runtime.container_id(),
                "sandbox container stopped"
            );
            Ok(())
        } else {
            Err(anyhow!(
                "sandbox shutdown failed: {}",
                errors
                    .into_iter()
                    .map(|error| error.to_string())
                    .collect::<Vec<_>>()
                    .join("; ")
            ))
        }
    }
}
