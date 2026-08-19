use super::CONFIG_HASH_ANNOTATION;
use crate::config::{NetworkMode, NetworkSettings, SandboxConfig};
use anyhow::Context;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
};

pub(super) fn prepare_managed_bundle(
    cfg: &SandboxConfig,
    settings: &NetworkSettings,
) -> anyhow::Result<(PathBuf, String)> {
    let bundle_dir = if cfg.managed_bundle_dir.as_os_str().is_empty() {
        cfg.rootfs_dir
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("bundle")
    } else {
        cfg.managed_bundle_dir.clone()
    };
    fs::create_dir_all(&bundle_dir)
        .with_context(|| format!("create managed OCI bundle {}", bundle_dir.display()))?;

    let config_path = bundle_dir.join("config.json");
    let root_path = cfg.rootfs_dir.to_string_lossy().into_owned();
    let mut mounts = vec![
        json!({"destination":"/proc","type":"proc","source":"proc"}),
        json!({"destination":"/dev","type":"tmpfs","source":"tmpfs","options":["nosuid","strictatime","mode=755","size=65536k"]}),
        json!({"destination":"/dev/pts","type":"devpts","source":"devpts","options":["nosuid","noexec","newinstance","ptmxmode=0666","mode=0620","gid=5"]}),
        json!({"destination":"/dev/shm","type":"tmpfs","source":"shm","options":["nosuid","noexec","nodev","mode=1777","size=65536k"]}),
        json!({"destination":"/dev/mqueue","type":"mqueue","source":"mqueue","options":["nosuid","noexec","nodev"]}),
        json!({"destination":"/sys","type":"sysfs","source":"sysfs","options":["nosuid","noexec","nodev","ro"]}),
        json!({"destination":"/sys/fs/cgroup","type":"cgroup","source":"cgroup","options":["nosuid","noexec","nodev","relatime","ro"]}),
    ];
    if let Some(workspace_dir) = &cfg.workspace_dir {
        mounts.push(json!({
            "destination": "/workspace",
            "type": "bind",
            "source": workspace_dir.to_string_lossy(),
            "options": ["rbind", "rw"]
        }));
    }

    let mut namespaces = vec![
        json!({"type":"pid"}),
        json!({"type":"mount"}),
        json!({"type":"ipc"}),
        json!({"type":"uts"}),
        json!({"type":"cgroup"}),
    ];
    if !matches!(settings.mode, NetworkMode::Host) {
        namespaces.push(json!({"type":"network"}));
    }

    let mut spec = json!({
        "ociVersion": "1.0.2",
        "process": {
            "terminal": false,
            "args": ["/bin/sh", "-c", "while :; do sleep 3600; done"],
            "env": ["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"],
            "cwd": "/"
        },
        "root": {"path": root_path, "readonly": false},
        "hostname": "agent-cell",
        "mounts": mounts,
        "linux": {"namespaces": namespaces}
    });

    let config_hash = {
        let serialized = serde_json::to_vec(&spec).context("serialize OCI config for hashing")?;
        let digest = Sha256::digest(serialized);
        format!("{digest:x}")
    };
    spec["annotations"] = json!({
        CONFIG_HASH_ANNOTATION: config_hash,
        "io.agentcell.network-mode": settings.mode.as_str()
    });

    let serialized = serde_json::to_vec_pretty(&spec).context("serialize managed OCI config")?;
    fs::write(&config_path, serialized)
        .with_context(|| format!("write managed OCI config {}", config_path.display()))?;
    Ok((bundle_dir, config_hash))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use tempfile::tempdir;

    #[test]
    fn generated_config_contains_rootfs_system_mounts_and_workspace() {
        let temp = tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(rootfs.join("bin")).unwrap();
        fs::write(rootfs.join("bin/sh"), b"placeholder").unwrap();
        let mut cfg: SandboxConfig = toml::from_str(
            r#"
rootfs_dir = "."
workspace_dir = "."
"#,
        )
        .unwrap();
        cfg.rootfs_dir = rootfs.clone();
        cfg.workspace_dir = Some(workspace.clone());
        cfg.managed_bundle_dir = temp.path().join("bundle");
        let settings = cfg.network_settings().unwrap();

        let (bundle, hash) = prepare_managed_bundle(&cfg, &settings).unwrap();
        let spec: Value =
            serde_json::from_slice(&fs::read(bundle.join("config.json")).unwrap()).unwrap();
        assert_eq!(spec["root"]["path"], rootfs.to_string_lossy().as_ref());
        assert_eq!(spec["annotations"][CONFIG_HASH_ANNOTATION], hash);
        assert_eq!(spec["annotations"]["io.agentcell.network-mode"], "nat");
        assert_eq!(spec["process"]["args"][0], "/bin/sh");
        assert_eq!(spec["mounts"][7]["destination"], "/workspace");
        assert_eq!(
            spec["mounts"][7]["source"],
            workspace.to_string_lossy().as_ref()
        );
        assert!(spec["linux"]["namespaces"]
            .as_array()
            .unwrap()
            .iter()
            .any(|namespace| namespace["type"] == "network"));
    }
}
