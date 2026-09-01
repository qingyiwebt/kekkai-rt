use crate::config::{NetworkMode, NetworkSettings, RuntimeBackend};
use std::sync::Arc;
use tracing::info;

use super::{
    command::CommandExecutor,
    network_ops::{self as ops, NetworkBackend},
};

pub struct NetworkSession {
    backend: Arc<dyn NetworkBackend>,
    executor: Arc<dyn CommandExecutor>,
    attachment: NetworkAttachment,
    host_veth: Option<String>,
    netns_name: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NetworkAttachment {
    Host,
    Isolated { namespace_path: Option<String> },
}

impl NetworkSession {
    pub(super) async fn prepare(
        settings: &NetworkSettings,
        instance_id: &str,
        backend: Arc<dyn NetworkBackend>,
        executor: Arc<dyn CommandExecutor>,
    ) -> anyhow::Result<Self> {
        let (host_veth, peer_veth) = ops::veth_names(instance_id);
        let netns_name = format!("kekkai-rt-ns-{instance_id}");
        ops::cleanup_resources(backend.as_ref(), &host_veth, &netns_name).await;
        if !matches!(settings.mode, NetworkMode::Nat) {
            return Ok(Self {
                backend,
                executor,
                attachment: if matches!(settings.mode, NetworkMode::Host) {
                    NetworkAttachment::Host
                } else {
                    NetworkAttachment::Isolated {
                        namespace_path: None,
                    }
                },
                host_veth: None,
                netns_name: None,
            });
        }
        ops::ensure_bridge(backend.as_ref(), settings).await?;
        ops::ensure_nat_rules(backend.as_ref(), settings).await?;
        ops::prepare_nat(
            backend.as_ref(),
            settings,
            &host_veth,
            &peer_veth,
            &netns_name,
        )
        .await?;
        Ok(Self {
            backend,
            executor,
            attachment: NetworkAttachment::Isolated {
                namespace_path: Some(format!("/run/netns/{netns_name}")),
            },
            host_veth: Some(host_veth),
            netns_name: Some(netns_name),
        })
    }

    pub fn attachment(&self) -> &NetworkAttachment {
        &self.attachment
    }

    pub async fn configure(
        &self,
        settings: &NetworkSettings,
        pid: i32,
        runtime: &str,
        backend: RuntimeBackend,
        container_id: &str,
    ) -> anyhow::Result<()> {
        info!(mode = %settings.mode.as_str(), pid, "configuring sandbox network");
        match settings.mode {
            NetworkMode::Host => Ok(()),
            NetworkMode::None if backend.is_runsc() => Ok(()),
            NetworkMode::None => self.backend.configure_none(pid).await,
            NetworkMode::Nat => {
                ops::configure_dns(self.executor.as_ref(), settings, runtime, container_id).await
            }
        }
    }

    pub async fn cleanup(self) -> anyhow::Result<()> {
        if let (Some(host_veth), Some(netns_name)) = (self.host_veth, self.netns_name) {
            ops::cleanup_resources(self.backend.as_ref(), &host_veth, &netns_name).await;
        }
        Ok(())
    }
}

pub async fn prepare_network(
    settings: &NetworkSettings,
    instance_id: &str,
    backend: Arc<dyn NetworkBackend>,
    executor: Arc<dyn CommandExecutor>,
) -> anyhow::Result<NetworkSession> {
    NetworkSession::prepare(settings, instance_id, backend, executor).await
}

pub async fn configure_network(
    session: &NetworkSession,
    settings: &NetworkSettings,
    pid: i32,
    runtime: &str,
    backend: RuntimeBackend,
    container_id: &str,
) -> anyhow::Result<()> {
    session
        .configure(settings, pid, runtime, backend, container_id)
        .await
}

#[cfg(test)]
mod tests {
    use super::super::command::{CommandExecutor, CommandOutput, CommandSpec};
    use super::super::network_ops::NetworkBackend;
    use super::{ops, prepare_network, NetworkAttachment, NetworkSession};
    use crate::config::{NetworkMode, RuntimeBackend, SandboxConfig};
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};

    struct FakeBackend {
        operations: Mutex<Vec<&'static str>>,
        fail_prepare: bool,
    }

    impl FakeBackend {
        fn new(fail_prepare: bool) -> Arc<Self> {
            Arc::new(Self {
                operations: Mutex::new(Vec::new()),
                fail_prepare,
            })
        }

        fn operations(&self) -> Vec<&'static str> {
            self.operations.lock().unwrap().clone()
        }

        fn record(&self, operation: &'static str) {
            self.operations.lock().unwrap().push(operation);
        }
    }

    #[async_trait]
    impl NetworkBackend for FakeBackend {
        async fn prepare_nat(
            &self,
            _settings: &crate::config::NetworkSettings,
            host_veth: &str,
            _peer_veth: &str,
            netns_name: &str,
        ) -> anyhow::Result<()> {
            self.record("prepare_nat");
            if self.fail_prepare {
                self.cleanup_resources(host_veth, netns_name).await;
                anyhow::bail!("fake network setup failure");
            }
            Ok(())
        }

        async fn cleanup_resources(&self, _host_veth: &str, _netns_name: &str) {
            self.record("cleanup_resources");
        }

        async fn ensure_bridge(
            &self,
            _settings: &crate::config::NetworkSettings,
        ) -> anyhow::Result<()> {
            self.record("ensure_bridge");
            Ok(())
        }

        async fn ensure_nat_rules(
            &self,
            _settings: &crate::config::NetworkSettings,
        ) -> anyhow::Result<()> {
            self.record("ensure_nat_rules");
            Ok(())
        }

        async fn configure_none(&self, _pid: i32) -> anyhow::Result<()> {
            self.record("configure_none");
            Ok(())
        }
    }

    struct FakeExecutor {
        commands: Mutex<Vec<CommandSpec>>,
    }
    impl FakeExecutor {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                commands: Mutex::new(Vec::new()),
            })
        }
    }
    #[async_trait]
    impl CommandExecutor for FakeExecutor {
        async fn execute(&self, command: &CommandSpec) -> anyhow::Result<CommandOutput> {
            self.commands.lock().unwrap().push(command.clone());
            Ok(CommandOutput::new(true, ""))
        }
    }
    fn settings(mode: NetworkMode) -> crate::config::NetworkSettings {
        let mut config: SandboxConfig = toml::from_str("rootfs_dir = \".\"").unwrap();
        config.network_mode = mode;
        config.network_settings().unwrap()
    }
    #[test]
    fn veth_names_are_unique_and_fit_linux_interface_limit() {
        let (host, peer) = ops::veth_names("0123456789abcdef");
        assert_eq!(host, "kc-v-6789abcdef");
        assert_eq!(peer, "kc-p-6789abcdef");
        assert_ne!(host, peer);
        assert!(host.len() <= 15);
        assert!(peer.len() <= 15);
    }
    #[tokio::test]
    async fn nat_prepares_unique_peer_then_renames_it_inside_namespace() {
        let backend = FakeBackend::new(false);
        let executor = FakeExecutor::new();
        let session = prepare_network(
            &settings(NetworkMode::Nat),
            "kekkai-rt-0123456789",
            backend.clone(),
            executor.clone(),
        )
        .await
        .unwrap();
        assert_eq!(
            backend.operations(),
            vec![
                "cleanup_resources",
                "ensure_bridge",
                "ensure_nat_rules",
                "prepare_nat"
            ]
        );
        assert_eq!(
            session.attachment(),
            &NetworkAttachment::Isolated {
                namespace_path: Some(
                    "/run/netns/kekkai-rt-ns-kekkai-rt-0123456789".to_owned()
                )
            }
        );
        session.cleanup().await.unwrap();
        assert_eq!(
            backend.operations().last().copied(),
            Some("cleanup_resources")
        );
    }
    #[tokio::test]
    async fn nat_failure_cleans_namespace_and_host_veth() {
        let backend = FakeBackend::new(true);
        assert!(prepare_network(
            &settings(NetworkMode::Nat),
            "kekkai-rt-0123456789",
            backend.clone(),
            FakeExecutor::new(),
        )
        .await
        .is_err());
        assert_eq!(
            backend.operations(),
            vec![
                "cleanup_resources",
                "ensure_bridge",
                "ensure_nat_rules",
                "prepare_nat",
                "cleanup_resources"
            ]
        );
    }
    #[tokio::test]
    async fn runsc_none_skips_namespace_configuration_but_runc_configures_loopback() {
        let backend = FakeBackend::new(false);
        let session = NetworkSession {
            backend: backend.clone(),
            executor: FakeExecutor::new(),
            attachment: NetworkAttachment::Isolated {
                namespace_path: None,
            },
            host_veth: None,
            netns_name: None,
        };
        session
            .configure(
                &settings(NetworkMode::None),
                42,
                "runsc",
                RuntimeBackend::Runsc,
                "container",
            )
            .await
            .unwrap();
        assert!(backend.operations().is_empty());
        session
            .configure(
                &settings(NetworkMode::None),
                42,
                "runc",
                RuntimeBackend::Runc,
                "container",
            )
            .await
            .unwrap();
        assert_eq!(backend.operations(), vec!["configure_none"]);
    }
}
