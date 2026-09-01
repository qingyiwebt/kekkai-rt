use crate::config::{NetworkMode, NetworkSettings, RuntimeBackend};
use anyhow::bail;
use std::sync::Arc;
use tracing::{debug, info, warn};

use super::command::{CommandExecutor, CommandOutput, CommandSpec};

pub(super) struct NetworkSession {
    executor: Arc<dyn CommandExecutor>,
    attachment: NetworkAttachment,
    host_veth: Option<String>,
    netns_name: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum NetworkAttachment {
    Host,
    Isolated { namespace_path: Option<String> },
}

impl NetworkSession {
    pub(super) async fn prepare(
        settings: &NetworkSettings,
        instance_id: &str,
        executor: Arc<dyn CommandExecutor>,
    ) -> anyhow::Result<Self> {
        let (host_veth, peer_veth) = veth_names(instance_id);
        let netns_name = format!("kekkai-rt-ns-{instance_id}");
        cleanup_resources(executor.as_ref(), &host_veth, &netns_name).await;
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

        ensure_bridge(executor.as_ref(), settings).await?;
        ensure_nat_rules(executor.as_ref(), settings).await?;
        prepare_nat(
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

    pub(super) fn attachment(&self) -> &NetworkAttachment {
        &self.attachment
    }

    pub(super) async fn configure(
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
                nsenter_ip(self.executor.as_ref(), pid, ["link", "set", "lo", "up"]).await
            }
            NetworkMode::Nat => {
                configure_dns(self.executor.as_ref(), settings, runtime, container_id).await
            }
        }
    }

    pub(super) async fn cleanup(self) -> anyhow::Result<()> {
        if let (Some(host_veth), Some(netns_name)) = (self.host_veth, self.netns_name) {
            cleanup_resources(self.executor.as_ref(), &host_veth, &netns_name).await;
        }
        Ok(())
    }
}

pub(super) async fn prepare_network(
    settings: &NetworkSettings,
    instance_id: &str,
    executor: Arc<dyn CommandExecutor>,
) -> anyhow::Result<NetworkSession> {
    NetworkSession::prepare(settings, instance_id, executor).await
}

pub(super) async fn configure_network(
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

async fn prepare_nat(
    executor: &dyn CommandExecutor,
    settings: &NetworkSettings,
    host_veth: &str,
    peer_veth: &str,
    netns_name: &str,
) -> anyhow::Result<()> {
    run_checked(
        executor,
        "ip",
        &["netns", "add", netns_name],
        "create sandbox network namespace",
    )
    .await?;
    let result = async {
        run_checked(
            executor,
            "ip",
            &[
                "link", "add", host_veth, "type", "veth", "peer", "name", peer_veth,
            ],
            "create sandbox veth pair",
        )
        .await?;
        configure_veth(executor, settings, host_veth, peer_veth, netns_name).await
    }
    .await;
    if let Err(error) = result {
        cleanup_resources(executor, host_veth, netns_name).await;
        return Err(error);
    }
    Ok(())
}

async fn cleanup_resources(executor: &dyn CommandExecutor, host_veth: &str, netns_name: &str) {
    match run_command(
        executor,
        "ip",
        ["link", "del", host_veth],
        "remove sandbox host veth",
    )
    .await
    {
        Ok(output) if output.success() => {
            info!("removed sandbox host veth");
        }
        Ok(_) => {
            debug!("sandbox host veth was already absent");
        }
        Err(error) => {
            warn!(error = %error, "could not inspect sandbox host veth");
        }
    }
    match run_command(
        executor,
        "ip",
        ["netns", "del", netns_name],
        "remove sandbox network namespace",
    )
    .await
    {
        Ok(output) if output.success() => {
            info!("removed sandbox network namespace");
        }
        Ok(_) => {
            debug!("sandbox network namespace was already absent");
        }
        Err(error) => {
            warn!(error = %error, "could not inspect sandbox network namespace");
        }
    }
}

async fn ensure_bridge(
    executor: &dyn CommandExecutor,
    settings: &NetworkSettings,
) -> anyhow::Result<()> {
    debug!(bridge = %settings.bridge, "ensuring sandbox bridge");
    if !run_command(
        executor,
        "ip",
        ["link", "show", "dev", settings.bridge.as_str()],
        "check sandbox bridge",
    )
    .await?
    .success()
    {
        run_checked(
            executor,
            "ip",
            &["link", "add", "name", &settings.bridge, "type", "bridge"],
            "create sandbox bridge",
        )
        .await?;
    }
    run_checked(
        executor,
        "ip",
        &[
            "addr",
            "replace",
            &settings.subnet.address_with_prefix(settings.gateway),
            "dev",
            &settings.bridge,
        ],
        "assign sandbox bridge address",
    )
    .await?;
    run_checked(
        executor,
        "ip",
        &["link", "set", "dev", &settings.bridge, "up"],
        "activate sandbox bridge",
    )
    .await
}

async fn ensure_nat_rules(
    executor: &dyn CommandExecutor,
    settings: &NetworkSettings,
) -> anyhow::Result<()> {
    let subnet = settings.subnet.network_with_prefix();
    ensure_iptables_rule(
        executor,
        "nat",
        "POSTROUTING",
        vec![
            "-s".into(),
            subnet.clone(),
            "!".into(),
            "-o".into(),
            settings.bridge.clone(),
            "-j".into(),
            "MASQUERADE".into(),
        ],
    )
    .await?;
    ensure_iptables_rule(
        executor,
        "filter",
        "FORWARD",
        vec![
            "-i".into(),
            settings.bridge.clone(),
            "-s".into(),
            subnet.clone(),
            "-j".into(),
            "ACCEPT".into(),
        ],
    )
    .await?;
    ensure_iptables_rule(
        executor,
        "filter",
        "FORWARD",
        vec![
            "-o".into(),
            settings.bridge.clone(),
            "-d".into(),
            subnet,
            "-m".into(),
            "conntrack".into(),
            "--ctstate".into(),
            "ESTABLISHED,RELATED".into(),
            "-j".into(),
            "ACCEPT".into(),
        ],
    )
    .await
}

async fn ensure_iptables_rule(
    executor: &dyn CommandExecutor,
    table: &str,
    chain: &str,
    rule: Vec<String>,
) -> anyhow::Result<()> {
    let mut check = vec!["-t".into(), table.into(), "-C".into(), chain.into()];
    check.extend(rule.iter().cloned());
    if run_command(
        executor,
        "iptables",
        check.iter().map(String::as_str),
        "check sandbox NAT firewall rule",
    )
    .await?
    .success()
    {
        return Ok(());
    }
    let mut insert = vec![
        "-t".into(),
        table.into(),
        "-I".into(),
        chain.into(),
        "1".into(),
    ];
    insert.extend(rule);
    run_checked(
        executor,
        "iptables",
        insert.iter().map(String::as_str),
        "install sandbox NAT firewall rule",
    )
    .await
}

async fn configure_veth(
    executor: &dyn CommandExecutor,
    settings: &NetworkSettings,
    host_veth: &str,
    peer_veth: &str,
    netns_name: &str,
) -> anyhow::Result<()> {
    run_checked(
        executor,
        "ip",
        &["link", "set", host_veth, "master", &settings.bridge],
        "attach sandbox veth to bridge",
    )
    .await?;
    run_checked(
        executor,
        "ip",
        &["link", "set", host_veth, "up"],
        "activate sandbox host veth",
    )
    .await?;
    run_checked(
        executor,
        "ip",
        &["link", "set", peer_veth, "netns", netns_name],
        "move sandbox veth into network namespace",
    )
    .await?;

    netns_ip(
        executor,
        netns_name,
        ["link", "set", peer_veth, "name", "eth0"],
    )
    .await?;
    netns_ip(executor, netns_name, ["link", "set", "lo", "up"]).await?;
    netns_ip(
        executor,
        netns_name,
        [
            "addr",
            "replace",
            &settings.subnet.address_with_prefix(settings.address),
            "dev",
            "eth0",
        ],
    )
    .await?;
    netns_ip(executor, netns_name, ["link", "set", "eth0", "up"]).await?;
    netns_ip(
        executor,
        netns_name,
        [
            "route",
            "replace",
            "default",
            "via",
            &settings.gateway.to_string(),
            "dev",
            "eth0",
        ],
    )
    .await?;
    Ok(())
}

async fn configure_dns(
    executor: &dyn CommandExecutor,
    settings: &NetworkSettings,
    runtime: &str,
    container_id: &str,
) -> anyhow::Result<()> {
    if !settings.dns.is_empty() {
        let lines = settings
            .dns
            .iter()
            .map(|dns| format!("'nameserver {dns}'"))
            .collect::<Vec<_>>()
            .join(" ");
        let script = format!("printf '%s\\n' {lines} > /etc/resolv.conf");
        // runsc's mount namespace is intentionally empty: the container rootfs
        // is served by its gofer process and is not available to host-side
        // nsenter. Execute the write inside the container instead.
        run_checked(
            executor,
            runtime,
            &[
                "exec".into(),
                container_id.to_owned(),
                "/bin/sh".into(),
                "-c".into(),
                script,
            ],
            "configure sandbox DNS",
        )
        .await?;
    }
    Ok(())
}

async fn netns_ip<const N: usize>(
    executor: &dyn CommandExecutor,
    netns_name: &str,
    args: [&str; N],
) -> anyhow::Result<()> {
    let mut command_args = vec![
        "netns".into(),
        "exec".into(),
        netns_name.into(),
        "ip".into(),
    ];
    command_args.extend(args.into_iter().map(str::to_owned));
    run_checked(
        executor,
        "ip",
        command_args.iter().map(String::as_str),
        "configure sandbox network namespace",
    )
    .await
}

async fn nsenter_ip<const N: usize>(
    executor: &dyn CommandExecutor,
    pid: i32,
    args: [&str; N],
) -> anyhow::Result<()> {
    let mut command_args = vec!["-t".into(), pid.to_string(), "-n".into(), "ip".into()];
    command_args.extend(args.into_iter().map(str::to_owned));
    run_checked(
        executor,
        "nsenter",
        &command_args,
        "configure sandbox network namespace",
    )
    .await
}

async fn run_command<I, S>(
    executor: &dyn CommandExecutor,
    program: impl Into<String>,
    args: I,
    context: &'static str,
) -> anyhow::Result<CommandOutput>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let command = CommandSpec::new(
        program,
        args.into_iter().map(|arg| arg.as_ref().to_owned()),
        context,
    );
    executor.execute(&command).await
}

async fn run_checked<I, S>(
    executor: &dyn CommandExecutor,
    program: &str,
    args: I,
    context: &'static str,
) -> anyhow::Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let output = run_command(executor, program, args, context).await?;
    if !output.success() {
        bail!("{context}: {}", output.stderr());
    }
    Ok(())
}

fn veth_names(instance_id: &str) -> (String, String) {
    let suffix = &instance_id[instance_id.len().saturating_sub(10)..];
    (format!("kc-v-{suffix}"), format!("kc-p-{suffix}"))
}

#[cfg(test)]
mod tests {
    use super::{prepare_network, veth_names, NetworkAttachment, NetworkSession};
    use crate::config::{NetworkMode, RuntimeBackend, SandboxConfig};
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};

    use super::super::command::{CommandExecutor, CommandOutput, CommandSpec};

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
        let (host, peer) = veth_names("0123456789abcdef");

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
        let result = prepare_network(
            &settings(NetworkMode::Nat),
            "kekkai-rt-0123456789",
            executor.clone(),
        )
        .await;
        assert!(result.is_err());
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
        let runsc_session = NetworkSession {
            executor: executor.clone(),
            attachment: NetworkAttachment::Isolated {
                namespace_path: None,
            },
            host_veth: None,
            netns_name: None,
        };
        runsc_session
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

        runsc_session
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
