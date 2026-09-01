use crate::{
    config::{CgroupMode, NetworkMode},
    host::HostCapabilities,
};
use serde::Serialize;
use std::{collections::BTreeMap, path::Path};
use uuid::Uuid;

#[derive(Serialize)]
struct GeneratedConfig {
    api: GeneratedApiConfig,
    sandbox: GeneratedSandboxConfig,
    features: GeneratedFeaturesConfig,
    mounts: BTreeMap<String, String>,
}

#[derive(Serialize)]
struct GeneratedApiConfig {
    listen_addr: String,
    token: String,
}

#[derive(Serialize)]
struct GeneratedSandboxConfig {
    rootfs_dir: String,
    backend: String,
    max_timeout_seconds: u64,
    network_mode: String,
    network_bridge: String,
    network_subnet: String,
    network_gateway: String,
    network_ip: String,
    network_dns: Vec<String>,
}

#[derive(Serialize)]
struct GeneratedFeaturesConfig {
    cgroups: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct InitFeatures {
    pub(crate) network_mode: NetworkMode,
    pub(crate) cgroups: CgroupMode,
}

pub(crate) fn detect_init_features(capabilities: &HostCapabilities) -> InitFeatures {
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

pub(crate) fn generated_config(
    rootfs: &Path,
    workspace: &Path,
    network_mode: NetworkMode,
    cgroups: CgroupMode,
) -> anyhow::Result<String> {
    let mut mounts = BTreeMap::new();
    mounts.insert(
        "/workspace".into(),
        workspace.to_string_lossy().into_owned(),
    );
    let config = GeneratedConfig {
        api: GeneratedApiConfig {
            listen_addr: "0.0.0.0:8080".into(),
            token: Uuid::new_v4().as_simple().to_string(),
        },
        sandbox: GeneratedSandboxConfig {
            rootfs_dir: rootfs.to_string_lossy().into_owned(),
            backend: "runsc".into(),
            max_timeout_seconds: 300,
            network_mode: network_mode.as_str().into(),
            network_bridge: "kekkai-rt0".into(),
            network_subnet: "10.200.0.0/24".into(),
            network_gateway: "10.200.0.1".into(),
            network_ip: "10.200.0.2".into(),
            network_dns: vec!["1.1.1.1".into(), "8.8.8.8".into()],
        },
        features: GeneratedFeaturesConfig {
            cgroups: cgroups.as_str().into(),
        },
        mounts,
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
