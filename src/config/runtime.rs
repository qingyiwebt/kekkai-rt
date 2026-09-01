use super::{NetworkSettings, SandboxConfig};
use serde::{Deserialize, Serialize};
use std::{fmt, time::Duration};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RuntimeBackend {
    Runsc,
    Runc,
}

impl RuntimeBackend {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Runsc => "runsc",
            Self::Runc => "runc",
        }
    }

    pub fn is_runsc(self) -> bool {
        matches!(self, Self::Runsc)
    }
}

impl fmt::Display for RuntimeBackend {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Debug)]
pub struct SandboxSettings {
    pub backend: RuntimeBackend,
    pub max_timeout: Duration,
    pub network: NetworkSettings,
}

impl SandboxConfig {
    pub fn resolved(&self) -> Result<SandboxSettings, String> {
        Ok(SandboxSettings {
            backend: self.backend,
            max_timeout: Duration::from_secs(self.max_timeout_seconds),
            network: self.network_settings()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::RuntimeBackend;
    use crate::config::{NetworkMode, SandboxConfig};

    #[test]
    fn parses_toml_backend_and_network_values_as_typed_enums() {
        let config: SandboxConfig =
            toml::from_str("rootfs_dir = \".\"\nbackend = \"runc\"\nnetwork_mode = \"none\"")
                .unwrap();
        assert_eq!(config.backend, RuntimeBackend::Runc);
        assert_eq!(config.network_mode, NetworkMode::None);
    }

    #[test]
    fn rejects_unknown_backend_values() {
        let config: Result<SandboxConfig, _> =
            toml::from_str("rootfs_dir = \".\"\nbackend = \"fake\"");
        assert!(config.is_err());
    }
}
