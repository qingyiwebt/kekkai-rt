use crate::config::NetworkSettings;
use async_trait::async_trait;
use std::sync::Arc;

use super::command::{CommandExecutor, CommandSpec};

#[cfg(target_os = "linux")]
mod linux;

#[cfg(not(target_os = "linux"))]
mod unsupported;

#[async_trait]
pub(super) trait NetworkBackend: Send + Sync {
    async fn prepare_nat(
        &self,
        settings: &NetworkSettings,
        host_veth: &str,
        peer_veth: &str,
        netns_name: &str,
    ) -> anyhow::Result<()>;

    async fn cleanup_resources(&self, host_veth: &str, netns_name: &str);

    async fn ensure_bridge(&self, settings: &NetworkSettings) -> anyhow::Result<()>;

    async fn ensure_nat_rules(&self, settings: &NetworkSettings) -> anyhow::Result<()>;

    async fn configure_none(&self, pid: i32) -> anyhow::Result<()>;
}

pub(super) fn default_backend() -> Arc<dyn NetworkBackend> {
    #[cfg(target_os = "linux")]
    {
        Arc::new(linux::LinuxNetworkBackend)
    }
    #[cfg(not(target_os = "linux"))]
    {
        Arc::new(unsupported::UnsupportedNetworkBackend)
    }
}

pub(super) async fn prepare_nat(
    backend: &dyn NetworkBackend,
    settings: &NetworkSettings,
    host_veth: &str,
    peer_veth: &str,
    netns_name: &str,
) -> anyhow::Result<()> {
    backend
        .prepare_nat(settings, host_veth, peer_veth, netns_name)
        .await
}

pub(super) async fn cleanup_resources(
    backend: &dyn NetworkBackend,
    host_veth: &str,
    netns_name: &str,
) {
    backend.cleanup_resources(host_veth, netns_name).await;
}

pub(super) async fn ensure_bridge(
    backend: &dyn NetworkBackend,
    settings: &NetworkSettings,
) -> anyhow::Result<()> {
    backend.ensure_bridge(settings).await
}

pub(super) async fn ensure_nat_rules(
    backend: &dyn NetworkBackend,
    settings: &NetworkSettings,
) -> anyhow::Result<()> {
    backend.ensure_nat_rules(settings).await
}

pub(super) async fn configure_dns(
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
    let output = executor
        .execute(&CommandSpec::new(
            program,
            args.into_iter().map(|arg| arg.as_ref().to_owned()),
            context,
        ))
        .await?;
    if !output.success() {
        anyhow::bail!("{context}: {}", output.stderr());
    }
    Ok(())
}

pub(super) fn veth_names(instance_id: &str) -> (String, String) {
    let suffix = &instance_id[instance_id.len().saturating_sub(10)..];
    (format!("kc-v-{suffix}"), format!("kc-p-{suffix}"))
}
