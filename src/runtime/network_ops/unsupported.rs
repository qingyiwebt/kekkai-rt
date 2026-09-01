use super::NetworkBackend;
use crate::config::NetworkSettings;
use async_trait::async_trait;

pub(super) struct UnsupportedNetworkBackend;

#[async_trait]
impl NetworkBackend for UnsupportedNetworkBackend {
    async fn prepare_nat(
        &self,
        _settings: &NetworkSettings,
        _host_veth: &str,
        _peer_veth: &str,
        _netns_name: &str,
    ) -> anyhow::Result<()> {
        anyhow::bail!("network_mode=nat requires a Linux host")
    }

    async fn cleanup_resources(&self, _host_veth: &str, _netns_name: &str) {}

    async fn ensure_bridge(&self, _settings: &NetworkSettings) -> anyhow::Result<()> {
        anyhow::bail!("network_mode=nat requires a Linux host")
    }

    async fn ensure_nat_rules(&self, _settings: &NetworkSettings) -> anyhow::Result<()> {
        anyhow::bail!("network_mode=nat requires a Linux host")
    }

    async fn configure_none(&self, _pid: i32) -> anyhow::Result<()> {
        anyhow::bail!("network namespace configuration requires a Linux host")
    }
}
