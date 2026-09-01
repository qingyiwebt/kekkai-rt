use crate::{
    config::{ResolvedFeatures, SandboxConfig, UserNamespaceAction},
    proxy::ToolSocketMount,
};
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
    #[serde(skip_serializing_if = "Option::is_none")]
    user: Option<OciProcessUser>,
    #[serde(skip_serializing_if = "Option::is_none")]
    capabilities: Option<OciCapabilities>,
}

#[derive(Serialize)]
struct OciProcessUser {
    uid: u32,
    gid: u32,
    #[serde(rename = "additionalGids")]
    additional_gids: Vec<u32>,
}

#[derive(Serialize)]
struct OciCapabilities {
    bounding: Vec<&'static str>,
    effective: Vec<&'static str>,
    inheritable: Vec<&'static str>,
    permitted: Vec<&'static str>,
    ambient: Vec<&'static str>,
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
    #[serde(rename = "uidMappings", skip_serializing_if = "Option::is_none")]
    uid_mappings: Option<Vec<OciIdMapping>>,
    #[serde(rename = "gidMappings", skip_serializing_if = "Option::is_none")]
    gid_mappings: Option<Vec<OciIdMapping>>,
}

#[derive(Serialize)]
struct OciNamespace {
    #[serde(rename = "type")]
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
}

#[derive(Serialize)]
struct OciIdMapping {
    #[serde(rename = "containerID")]
    container_id: u32,
    #[serde(rename = "hostID")]
    host_id: u32,
    size: u32,
}

const DEFAULT_CAPABILITIES: &[&str] = &[
    "CAP_AUDIT_WRITE",
    "CAP_CHOWN",
    "CAP_DAC_OVERRIDE",
    "CAP_FOWNER",
    "CAP_FSETID",
    "CAP_KILL",
    "CAP_MKNOD",
    "CAP_NET_BIND_SERVICE",
    "CAP_NET_RAW",
    "CAP_SETFCAP",
    "CAP_SETGID",
    "CAP_SETPCAP",
    "CAP_SETUID",
    "CAP_SYS_CHROOT",
];

pub fn prepare_managed_bundle(
    cfg: &SandboxConfig,
    features: &ResolvedFeatures,
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

    let mut namespaces = standard_namespaces(features);
    if let NetworkAttachment::Isolated { namespace_path } = attachment {
        namespaces.push(OciNamespace {
            kind: "network",
            path: namespace_path.clone(),
        });
    }

    let (user, capabilities, uid_mappings, gid_mappings) = match features.user_namespace {
        UserNamespaceAction::Use(mapping) => (
            Some(OciProcessUser {
                uid: 0,
                gid: 0,
                additional_gids: vec![0],
            }),
            Some(OciCapabilities {
                bounding: DEFAULT_CAPABILITIES.to_vec(),
                effective: DEFAULT_CAPABILITIES.to_vec(),
                inheritable: DEFAULT_CAPABILITIES.to_vec(),
                permitted: DEFAULT_CAPABILITIES.to_vec(),
                ambient: DEFAULT_CAPABILITIES.to_vec(),
            }),
            Some(vec![OciIdMapping {
                container_id: mapping.uid.container_id,
                host_id: mapping.uid.host_id,
                size: mapping.uid.size,
            }]),
            Some(vec![OciIdMapping {
                container_id: mapping.gid.container_id,
                host_id: mapping.gid.host_id,
                size: mapping.gid.size,
            }]),
        ),
        UserNamespaceAction::Ignore => (None, None, None, None),
    };

    let spec = OciSpec {
        oci_version: "1.0.2",
        process: OciProcess {
            terminal: false,
            args: ["/bin/sh", "-c", "while :; do sleep 3600; done"],
            env: ["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"],
            cwd: "/",
            user,
            capabilities,
        },
        root: OciRoot {
            path: cfg.rootfs_dir.to_string_lossy().into_owned(),
            readonly: false,
        },
        hostname: "kekkai-rt",
        mounts,
        linux: OciLinux {
            namespaces,
            uid_mappings,
            gid_mappings,
        },
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

fn standard_namespaces(features: &ResolvedFeatures) -> Vec<OciNamespace> {
    let mut kinds = vec!["pid", "mount", "ipc", "uts", "cgroup"];
    if matches!(features.user_namespace, UserNamespaceAction::Use(_)) {
        kinds.push("user");
    }
    kinds
        .into_iter()
        .map(|kind| OciNamespace { kind, path: None })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CgroupAction, ResolvedFeatures, UserNamespaceAction};
    use serde_json::Value;
    use tempfile::tempdir;

    fn features() -> ResolvedFeatures {
        ResolvedFeatures {
            cgroups: CgroupAction::Ignore,
            user_namespace: UserNamespaceAction::Ignore,
        }
    }

    fn user_namespace_features() -> ResolvedFeatures {
        use crate::runtime::host::{IdMapping, UserNamespaceMapping};
        ResolvedFeatures {
            cgroups: CgroupAction::Ignore,
            user_namespace: UserNamespaceAction::Use(UserNamespaceMapping {
                uid: IdMapping {
                    container_id: 0,
                    host_id: 100_000,
                    size: 65_536,
                },
                gid: IdMapping {
                    container_id: 0,
                    host_id: 100_000,
                    size: 65_536,
                },
            }),
        }
    }

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
            &features(),
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
            &features(),
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
        let bundle = prepare_managed_bundle(
            &cfg,
            &features(),
            &NetworkAttachment::Host,
            &BTreeMap::new(),
            None,
        )
        .unwrap();
        let spec: Value =
            serde_json::from_slice(&fs::read(bundle.join("config.json")).unwrap()).unwrap();
        assert!(spec["linux"]["namespaces"]
            .as_array()
            .unwrap()
            .iter()
            .all(|namespace| namespace["type"] != "network"));
    }

    #[test]
    fn generated_config_contains_user_namespace_mappings_and_capability_allowlist() {
        let temp = tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        fs::create_dir_all(rootfs.join("bin")).unwrap();
        fs::write(rootfs.join("bin/sh"), b"placeholder").unwrap();
        let mut cfg: SandboxConfig = toml::from_str("rootfs_dir = \".\"").unwrap();
        cfg.rootfs_dir = rootfs;
        cfg.managed_bundle_dir = temp.path().join("bundle");

        let bundle = prepare_managed_bundle(
            &cfg,
            &user_namespace_features(),
            &NetworkAttachment::Host,
            &BTreeMap::new(),
            None,
        )
        .unwrap();
        let spec: Value =
            serde_json::from_slice(&fs::read(bundle.join("config.json")).unwrap()).unwrap();
        assert!(spec["linux"]["namespaces"]
            .as_array()
            .unwrap()
            .iter()
            .any(|namespace| namespace["type"] == "user"));
        assert_eq!(spec["linux"]["uidMappings"][0]["containerID"], 0);
        assert_eq!(spec["linux"]["uidMappings"][0]["hostID"], 100_000);
        assert_eq!(spec["process"]["user"]["uid"], 0);
        assert_eq!(spec["process"]["user"]["additionalGids"][0], 0);
        let capabilities = spec["process"]["capabilities"].as_object().unwrap();
        let expected = capabilities["effective"].as_array().unwrap();
        assert_eq!(capabilities["bounding"].as_array().unwrap(), expected);
        assert_eq!(capabilities["inheritable"].as_array().unwrap(), expected);
        assert_eq!(capabilities["permitted"].as_array().unwrap(), expected);
        assert_eq!(capabilities["ambient"].as_array().unwrap(), expected);
        assert!(!expected
            .iter()
            .any(|capability| capability == "CAP_NET_ADMIN"));
        assert!(!expected
            .iter()
            .any(|capability| capability == "CAP_SYS_ADMIN"));
    }
}
