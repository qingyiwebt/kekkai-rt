use super::{NetworkSettings, SandboxConfig};
use std::time::Duration;

#[derive(Clone, Debug)]
pub(crate) struct SandboxSettings {
    pub(crate) backend: String,
    pub(crate) max_timeout: Duration,
    pub(crate) network: NetworkSettings,
}

impl SandboxConfig {
    pub(crate) fn resolved(&self) -> Result<SandboxSettings, String> {
        Ok(SandboxSettings {
            backend: self.backend.clone(),
            max_timeout: Duration::from_secs(self.max_timeout_seconds),
            network: self.network_settings()?,
        })
    }
}
