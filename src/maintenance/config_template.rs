use crate::{
    config::{ApiConfig, CgroupMode, Config, FeaturesConfig, NetworkMode, SandboxConfig},
    runtime::host::HostCapabilities,
};
use std::{collections::HashMap, path::Path};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InitFeatures {
    pub network_mode: NetworkMode,
    pub cgroups: CgroupMode,
}

pub fn detect_init_features(capabilities: &HostCapabilities) -> InitFeatures {
    InitFeatures {
        network_mode: if capabilities.nat_available() {
            NetworkMode::Nat
        } else {
            NetworkMode::Host
        },
        cgroups: if capabilities.cgroups.memory_controller {
            CgroupMode::Required
        } else {
            CgroupMode::Disabled
        },
    }
}

pub fn generated_config(
    rootfs: &Path,
    workspace: &Path,
    network_mode: NetworkMode,
    cgroups: CgroupMode,
) -> anyhow::Result<String> {
    let config = Config {
        api: ApiConfig {
            listen_addr: "0.0.0.0:8080".parse()?,
            token: Uuid::new_v4().as_simple().to_string(),
        },
        sandbox: SandboxConfig {
            rootfs_dir: rootfs.to_path_buf(),
            backend: crate::config::RuntimeBackend::Runsc,
            max_timeout_seconds: 300,
            network_mode,
            network_bridge: "kekkai-rt0".into(),
            network_subnet: "10.200.0.0/24".into(),
            network_gateway: "10.200.0.1".into(),
            network_ip: "10.200.0.2".into(),
            network_dns: vec!["1.1.1.1".into(), "8.8.8.8".into()],
            managed_bundle_dir: Default::default(),
        },
        features: FeaturesConfig {
            cgroups,
            ..FeaturesConfig::default()
        },
        mounts: [("/workspace".into(), workspace.to_path_buf())]
            .into_iter()
            .collect(),
        tools: HashMap::new(),
    };
    Ok(format!("{}\n", toml::to_string_pretty(&config)?))
}

#[cfg(test)]
mod tests {
    use super::generated_config;
    use crate::config::{CgroupMode, NetworkMode};
    use tempfile::tempdir;

    #[test]
    fn generated_config_records_detected_nat_and_cgroup_features() {
        let temp = tempdir().unwrap();
        let rootfs = temp.path().join("sysroot");
        let workspace = temp.path().join("workspace");

        let config =
            generated_config(&rootfs, &workspace, NetworkMode::Nat, CgroupMode::Required).unwrap();
        assert!(config.contains("network_mode = \"nat\""));
        assert!(config.contains("cgroups = \"required\""));
        let parsed: toml::Value = toml::from_str(&config).unwrap();
        assert_eq!(parsed["features"]["cgroups"].as_str(), Some("required"));
        assert_eq!(
            parsed["mounts"]["/workspace"].as_str(),
            Some(
                rootfs
                    .parent()
                    .unwrap()
                    .join("workspace")
                    .to_string_lossy()
                    .as_ref()
            )
        );
        assert!(!temp.path().join("config.toml").exists());
    }

    #[test]
    fn generated_config_records_host_fallback_and_disabled_cgroups() {
        let temp = tempdir().unwrap();
        let config = generated_config(
            &temp.path().join("sysroot"),
            &temp.path().join("workspace"),
            NetworkMode::Host,
            CgroupMode::Disabled,
        )
        .unwrap();
        assert!(config.contains("network_mode = \"host\""));
        assert!(config.contains("cgroups = \"disabled\""));
    }
}
