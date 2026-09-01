use crate::{config::SandboxConfig, proxy::ToolSocketMount};
use anyhow::Context;
use serde::Serialize;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use super::network::NetworkAttachment;

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
    destination: String,
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

pub fn prepare_managed_bundle(
    cfg: &SandboxConfig,
    attachment: &NetworkAttachment,
    configured_mounts: &BTreeMap<PathBuf, PathBuf>,
    tool_mounts: Option<&[ToolSocketMount]>,
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
    for (destination, source) in configured_mounts {
        let metadata = fs::metadata(source)
            .with_context(|| format!("inspect mount source {}", source.display()))?;
        mounts.push(OciMount {
            destination: destination.to_string_lossy().into_owned(),
            kind: "bind",
            source: source.to_string_lossy().into_owned(),
            options: Some(if metadata.is_dir() {
                vec!["rbind", "rw"]
            } else {
                vec!["bind", "rw"]
            }),
        });
    }
    if let Some(tool_mounts) = tool_mounts {
        for mount in tool_mounts {
            mounts.push(OciMount {
                destination: mount.destination.into(),
                kind: "bind",
                source: mount.source.to_string_lossy().into_owned(),
                options: Some(vec!["bind"]),
            });
        }
    }

    let mut namespaces = standard_namespaces();
    if let NetworkAttachment::Isolated { namespace_path } = attachment {
        namespaces.push(OciNamespace {
            kind: "network",
            path: namespace_path.clone(),
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
        hostname: "kekkai-rt",
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
            destination: "/proc".into(),
            kind: "proc",
            source: "proc".into(),
            options: None,
        },
        OciMount {
            destination: "/dev".into(),
            kind: "tmpfs",
            source: "tmpfs".into(),
            options: Some(vec!["nosuid", "strictatime", "mode=755", "size=65536k"]),
        },
        OciMount {
            destination: "/dev/pts".into(),
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
            destination: "/dev/shm".into(),
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
            destination: "/dev/mqueue".into(),
            kind: "mqueue",
            source: "mqueue".into(),
            options: Some(vec!["nosuid", "noexec", "nodev"]),
        },
        OciMount {
            destination: "/sys".into(),
            kind: "sysfs",
            source: "sysfs".into(),
            options: Some(vec!["nosuid", "noexec", "nodev", "ro"]),
        },
        OciMount {
            destination: "/sys/fs/cgroup".into(),
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
        fs::create_dir_all(rootfs.join("bin")).unwrap();
        fs::write(rootfs.join("bin/sh"), b"placeholder").unwrap();
        let mut cfg: SandboxConfig = toml::from_str(
            r#"
rootfs_dir = "."
"#,
        )
        .unwrap();
        cfg.rootfs_dir = rootfs.clone();
        cfg.managed_bundle_dir = temp.path().join("bundle");
        let mut mounts = BTreeMap::new();
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        mounts.insert(PathBuf::from("/workspace"), workspace.clone());
        let bundle = prepare_managed_bundle(
            &cfg,
            &NetworkAttachment::Isolated {
                namespace_path: Some("/run/netns/kekkai-rtns".into()),
            },
            &mounts,
            None,
        )
        .unwrap();
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
        assert_eq!(network_namespace["path"], "/run/netns/kekkai-rtns");
    }

    #[test]
    fn generated_config_contains_tool_socket_mounts() {
        let temp = tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        fs::create_dir_all(rootfs.join("bin")).unwrap();
        fs::write(rootfs.join("bin/sh"), b"placeholder").unwrap();
        let mut cfg: SandboxConfig = toml::from_str("rootfs_dir = \".\"").unwrap();
        cfg.rootfs_dir = rootfs;
        cfg.managed_bundle_dir = temp.path().join("bundle");
        let mounts = vec![ToolSocketMount {
            source: temp.path().join("kekkai-rt-tools.socket"),
            destination: crate::proxy::SOCKET_DESTINATION,
        }];

        let bundle = prepare_managed_bundle(
            &cfg,
            &NetworkAttachment::Isolated {
                namespace_path: None,
            },
            &BTreeMap::new(),
            Some(&mounts),
        )
        .unwrap();
        let spec: Value =
            serde_json::from_slice(&fs::read(bundle.join("config.json")).unwrap()).unwrap();
        let tool_mounts = spec["mounts"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|mount| {
                mount["destination"]
                    .as_str()
                    .unwrap_or("")
                    .starts_with("/run/kekkai-rt-tools")
            })
            .collect::<Vec<_>>();
        assert_eq!(tool_mounts.len(), 1);
        assert!(tool_mounts.iter().all(|mount| mount["type"] == "bind"));
    }

    #[test]
    fn host_network_config_omits_network_namespace() {
        let temp = tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        fs::create_dir_all(rootfs.join("bin")).unwrap();
        fs::write(rootfs.join("bin/sh"), b"placeholder").unwrap();
        let mut cfg: SandboxConfig =
            toml::from_str("rootfs_dir = \".\"\nnetwork_mode = \"host\"").unwrap();
        cfg.rootfs_dir = rootfs;
        cfg.managed_bundle_dir = temp.path().join("bundle");
        let bundle =
            prepare_managed_bundle(&cfg, &NetworkAttachment::Host, &BTreeMap::new(), None).unwrap();
        let spec: Value =
            serde_json::from_slice(&fs::read(bundle.join("config.json")).unwrap()).unwrap();
        assert!(spec["linux"]["namespaces"]
            .as_array()
            .unwrap()
            .iter()
            .all(|namespace| namespace["type"] != "network"));
    }
}
