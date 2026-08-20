use crate::config::{NetworkMode, NetworkSettings, SandboxConfig};
use anyhow::Context;
use serde::Serialize;
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Serialize)]
struct OciSpec {
    #[serde(rename = "ociVersion")]
    oci_version: &'static str,
    process: OciProcess,
    root: OciRoot,
    hostname: &'static str,
    mounts: Vec<OciMount>,
    linux: OciLinux,
}

#[derive(Serialize)]
struct OciProcess {
    terminal: bool,
    args: [&'static str; 3],
    env: [&'static str; 1],
    cwd: &'static str,
}

#[derive(Serialize)]
struct OciRoot {
    path: String,
    readonly: bool,
}

#[derive(Serialize)]
struct OciMount {
    destination: &'static str,
    #[serde(rename = "type")]
    kind: &'static str,
    source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    options: Option<Vec<&'static str>>,
}

#[derive(Serialize)]
struct OciLinux {
    namespaces: Vec<OciNamespace>,
}

#[derive(Serialize)]
struct OciNamespace {
    #[serde(rename = "type")]
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
}

pub(super) fn prepare_managed_bundle(
    cfg: &SandboxConfig,
    settings: &NetworkSettings,
    network_namespace_path: Option<&str>,
) -> anyhow::Result<PathBuf> {
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
    let mut mounts = standard_mounts();
    if let Some(workspace_dir) = &cfg.workspace_dir {
        mounts.push(OciMount {
            destination: "/workspace",
            kind: "bind",
            source: workspace_dir.to_string_lossy().into_owned(),
            options: Some(vec!["rbind", "rw"]),
        });
    }

    let mut namespaces = standard_namespaces();
    if !matches!(settings.mode, NetworkMode::Host) {
        namespaces.push(OciNamespace {
            kind: "network",
            path: network_namespace_path.map(str::to_owned),
        });
    }

    let spec = OciSpec {
        oci_version: "1.0.2",
        process: OciProcess {
            terminal: false,
            args: ["/bin/sh", "-c", "while :; do sleep 3600; done"],
            env: ["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"],
            cwd: "/",
        },
        root: OciRoot {
            path: cfg.rootfs_dir.to_string_lossy().into_owned(),
            readonly: false,
        },
        hostname: "agent-cell",
        mounts,
        linux: OciLinux { namespaces },
    };

    let serialized = serde_json::to_vec_pretty(&spec).context("serialize managed OCI config")?;
    fs::write(&config_path, serialized)
        .with_context(|| format!("write managed OCI config {}", config_path.display()))?;
    Ok(bundle_dir)
}

fn standard_mounts() -> Vec<OciMount> {
    vec![
        OciMount {
            destination: "/proc",
            kind: "proc",
            source: "proc".into(),
            options: None,
        },
        OciMount {
            destination: "/dev",
            kind: "tmpfs",
            source: "tmpfs".into(),
            options: Some(vec!["nosuid", "strictatime", "mode=755", "size=65536k"]),
        },
        OciMount {
            destination: "/dev/pts",
            kind: "devpts",
            source: "devpts".into(),
            options: Some(vec![
                "nosuid",
                "noexec",
                "newinstance",
                "ptmxmode=0666",
                "mode=0620",
                "gid=5",
            ]),
        },
        OciMount {
            destination: "/dev/shm",
            kind: "tmpfs",
            source: "shm".into(),
            options: Some(vec![
                "nosuid",
                "noexec",
                "nodev",
                "mode=1777",
                "size=65536k",
            ]),
        },
        OciMount {
            destination: "/dev/mqueue",
            kind: "mqueue",
            source: "mqueue".into(),
            options: Some(vec!["nosuid", "noexec", "nodev"]),
        },
        OciMount {
            destination: "/sys",
            kind: "sysfs",
            source: "sysfs".into(),
            options: Some(vec!["nosuid", "noexec", "nodev", "ro"]),
        },
        OciMount {
            destination: "/sys/fs/cgroup",
            kind: "cgroup",
            source: "cgroup".into(),
            options: Some(vec!["nosuid", "noexec", "nodev", "relatime", "ro"]),
        },
    ]
}

fn standard_namespaces() -> Vec<OciNamespace> {
    ["pid", "mount", "ipc", "uts", "cgroup"]
        .into_iter()
        .map(|kind| OciNamespace { kind, path: None })
        .collect()
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

        let bundle =
            prepare_managed_bundle(&cfg, &settings, Some("/run/netns/agentcellns")).unwrap();
        let spec: Value =
            serde_json::from_slice(&fs::read(bundle.join("config.json")).unwrap()).unwrap();
        assert_eq!(spec["root"]["path"], rootfs.to_string_lossy().as_ref());
        assert!(spec.get("annotations").is_none());
        assert_eq!(spec["process"]["args"][0], "/bin/sh");
        assert_eq!(spec["mounts"][7]["destination"], "/workspace");
        assert_eq!(
            spec["mounts"][7]["source"],
            workspace.to_string_lossy().as_ref()
        );
        let network_namespace = spec["linux"]["namespaces"]
            .as_array()
            .unwrap()
            .iter()
            .find(|namespace| namespace["type"] == "network")
            .unwrap();
        assert_eq!(network_namespace["path"], "/run/netns/agentcellns");
    }
}
