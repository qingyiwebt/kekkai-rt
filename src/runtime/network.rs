use crate::config::{NetworkMode, NetworkSettings, RuntimeBackend};
use std::sync::Arc;
use tracing::info;

use super::{command::CommandExecutor, network_ops as ops};

pub struct NetworkSession {
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
    pub async fn prepare(
        settings: &NetworkSettings,
        instance_id: &str,
        executor: Arc<dyn CommandExecutor>,
    ) -> anyhow::Result<Self> {
        let (host_veth, peer_veth) = ops::veth_names(instance_id);
        let netns_name = format!("kekkai-rt-ns-{instance_id}");
        ops::cleanup_resources(executor.as_ref(), &host_veth, &netns_name).await;
        if !matches!(settings.mode, NetworkMode::Nat) {
            return Ok(Self {
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
        ops::ensure_bridge(executor.as_ref(), settings).await?;
        ops::ensure_nat_rules(executor.as_ref(), settings).await?;
        ops::prepare_nat(
            executor.as_ref(),
            settings,
            &host_veth,
            &peer_veth,
            &netns_name,
        )
        .await?;
        Ok(Self {
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
            NetworkMode::None => {
                ops::nsenter_ip(self.executor.as_ref(), pid, ["link", "set", "lo", "up"]).await
            }
            NetworkMode::Nat => {
                ops::configure_dns(self.executor.as_ref(), settings, runtime, container_id).await
            }
        }
    }

    pub async fn cleanup(self) -> anyhow::Result<()> {
        if let (Some(host_veth), Some(netns_name)) = (self.host_veth, self.netns_name) {
            ops::cleanup_resources(self.executor.as_ref(), &host_veth, &netns_name).await;
        }
        Ok(())
    }
}

pub async fn prepare_network(
    settings: &NetworkSettings,
    instance_id: &str,
    executor: Arc<dyn CommandExecutor>,
) -> anyhow::Result<NetworkSession> {
    NetworkSession::prepare(settings, instance_id, executor).await
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
    use super::{ops, prepare_network, NetworkAttachment, NetworkSession};
    use crate::config::{NetworkMode, RuntimeBackend, SandboxConfig};
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};

    struct FakeExecutor {
        commands: Mutex<Vec<CommandSpec>>,
        failed_context: Option<&'static str>,
    }
    impl FakeExecutor {
        fn new(failed_context: Option<&'static str>) -> Arc<Self> {
            Arc::new(Self {
                commands: Mutex::new(Vec::new()),
                failed_context,
            })
        }
        fn commands(&self) -> Vec<CommandSpec> {
            self.commands.lock().unwrap().clone()
        }
    }
    #[async_trait]
    impl CommandExecutor for FakeExecutor {
        async fn execute(&self, command: &CommandSpec) -> anyhow::Result<CommandOutput> {
            self.commands.lock().unwrap().push(command.clone());
            let success = self.failed_context != Some(command.context)
                && !matches!(
                    command.context,
                    "remove sandbox host veth"
                        | "remove sandbox network namespace"
                        | "check sandbox bridge"
                        | "check sandbox NAT firewall rule"
                );
            Ok(CommandOutput::new(
                success,
                if success { "" } else { "fake command failure" },
            ))
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
        let executor = FakeExecutor::new(None);
        let session = prepare_network(
            &settings(NetworkMode::Nat),
            "kekkai-rt-0123456789",
            executor.clone(),
        )
        .await
        .unwrap();
        let commands = executor.commands();
        let add_veth = commands
            .iter()
            .position(|command| command.context == "create sandbox veth pair")
            .unwrap();
        let move_peer = commands
            .iter()
            .position(|command| command.context == "move sandbox veth into network namespace")
            .unwrap();
        let rename_peer = commands
            .iter()
            .position(|command| {
                command.args
                    == vec![
                        "netns",
                        "exec",
                        "kekkai-rt-ns-kekkai-rt-0123456789",
                        "ip",
                        "link",
                        "set",
                        "kc-p-0123456789",
                        "name",
                        "eth0",
                    ]
            })
            .unwrap();
        assert!(add_veth < move_peer && move_peer < rename_peer);
        assert_eq!(commands[add_veth].args[7], "kc-p-0123456789");
        session.cleanup().await.unwrap();
    }
    #[tokio::test]
    async fn nat_failure_cleans_namespace_and_host_veth() {
        let executor = FakeExecutor::new(Some("create sandbox veth pair"));
        assert!(prepare_network(
            &settings(NetworkMode::Nat),
            "kekkai-rt-0123456789",
            executor.clone()
        )
        .await
        .is_err());
        let commands = executor.commands();
        assert!(commands
            .iter()
            .any(|command| command.context == "remove sandbox host veth"));
        assert!(commands
            .iter()
            .any(|command| command.context == "remove sandbox network namespace"));
    }
    #[tokio::test]
    async fn runsc_none_skips_nsenter_but_runc_configures_loopback() {
        let executor = FakeExecutor::new(None);
        let session = NetworkSession {
            executor: executor.clone(),
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
        assert!(executor
            .commands()
            .iter()
            .all(|command| command.program != "nsenter"));
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
        assert!(executor
            .commands()
            .iter()
            .any(|command| command.program == "nsenter"));
    }
}
