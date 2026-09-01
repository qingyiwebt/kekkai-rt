use crate::{
    config::{SandboxConfig, UserNamespaceAction},
    runtime::host::{IdMapping, UserNamespaceMapping},
};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

const MOUNTPOINTS: &[&str] = &[
    "/proc",
    "/sys",
    "/dev",
    "/dev/pts",
    "/dev/shm",
    "/dev/mqueue",
    "/run",
    "/sys/fs/cgroup",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SysrootIssue {
    pub path: PathBuf,
    pub reason: &'static str,
}

impl std::fmt::Display for SysrootIssue {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{} ({})", self.path.display(), self.reason)
    }
}

pub fn sysroot_issues(
    config: &SandboxConfig,
    mounts: &BTreeMap<PathBuf, PathBuf>,
) -> Vec<SysrootIssue> {
    let mut issues = Vec::new();
    for mountpoint in MOUNTPOINTS {
        check_directory(
            &config.rootfs_dir.join(mountpoint.trim_start_matches('/')),
            &mut issues,
        );
    }

    let shell = config.rootfs_dir.join("bin/sh");
    match fs::metadata(&shell) {
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => issues.push(SysrootIssue {
            path: shell,
            reason: "not a regular file",
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => issues.push(SysrootIssue {
            path: shell,
            reason: "missing",
        }),
        Err(_) => issues.push(SysrootIssue {
            path: shell,
            reason: "cannot be inspected",
        }),
    }

    for (destination, source) in mounts {
        match fs::metadata(source) {
            Ok(source_metadata) => {
                let target = config
                    .rootfs_dir
                    .join(destination.strip_prefix("/").unwrap_or(destination));
                if source_metadata.is_dir() {
                    check_directory(&target, &mut issues);
                } else if source_metadata.is_file() {
                    check_file(&target, &mut issues);
                } else {
                    issues.push(SysrootIssue {
                        path: source.clone(),
                        reason: "mount source is not a regular file or directory",
                    });
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                issues.push(SysrootIssue {
                    path: source.clone(),
                    reason: "mount source is missing",
                })
            }
            Err(_) => issues.push(SysrootIssue {
                path: source.clone(),
                reason: "mount source cannot be inspected",
            }),
        }
    }

    issues
}

pub fn fix_sysroot(
    config: &SandboxConfig,
    mounts: &BTreeMap<PathBuf, PathBuf>,
) -> std::io::Result<Vec<PathBuf>> {
    prepare_sysroot(&config.rootfs_dir, mounts)
}

pub fn identity_issues(config: &SandboxConfig, action: UserNamespaceAction) -> Vec<SysrootIssue> {
    let UserNamespaceAction::Use(mapping) = action else {
        return Vec::new();
    };

    let marker = identity_marker(config);
    let expected = mapping_marker(mapping);
    match fs::read_to_string(&marker) {
        Ok(value) if value == expected => {}
        Ok(_) => {
            return vec![identity_issue(
                marker,
                "identity mapping marker does not match",
            )]
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return vec![identity_issue(
                marker,
                "identity mapping has not been prepared",
            )]
        }
        Err(_) => {
            return vec![identity_issue(
                marker,
                "identity mapping marker cannot be read",
            )]
        }
    }

    if let Err(reason) = validate_tree(&config.rootfs_dir, mapping) {
        return vec![identity_issue(config.rootfs_dir.clone(), reason)];
    }
    Vec::new()
}

pub fn fix_sysroot_with_identity(
    config: &SandboxConfig,
    mounts: &BTreeMap<PathBuf, PathBuf>,
    action: UserNamespaceAction,
) -> std::io::Result<Vec<PathBuf>> {
    let mut changed = fix_sysroot(config, mounts)?;
    let UserNamespaceAction::Use(mapping) = action else {
        return Ok(changed);
    };

    let marker = identity_marker(config);
    match fs::read_to_string(&marker) {
        Ok(value) if value == mapping_marker(mapping) => return Ok(changed),
        Ok(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "rootfs was prepared for a different user namespace mapping: {}",
                    marker.display()
                ),
            ))
        }
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error),
        Err(_) => {}
    }

    shift_tree(&config.rootfs_dir, mapping)?;
    write_marker_atomically(&marker, mapping_marker(mapping).as_bytes())?;
    changed.push(marker);
    Ok(changed)
}

fn identity_marker(config: &SandboxConfig) -> PathBuf {
    if config.managed_bundle_dir.as_os_str().is_empty() {
        config
            .rootfs_dir
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("bundle")
            .join("rootfs-userns.mapping")
    } else {
        config.managed_bundle_dir.join("rootfs-userns.mapping")
    }
}

fn identity_issue(path: PathBuf, reason: &'static str) -> SysrootIssue {
    SysrootIssue { path, reason }
}

fn mapping_marker(mapping: UserNamespaceMapping) -> String {
    format!(
        "uid={}:{}:{}\ngid={}:{}:{}\n",
        mapping.uid.container_id,
        mapping.uid.host_id,
        mapping.uid.size,
        mapping.gid.container_id,
        mapping.gid.host_id,
        mapping.gid.size
    )
}

fn validate_tree(root: &Path, mapping: UserNamespaceMapping) -> Result<(), &'static str> {
    validate_entry(root, mapping)
}

fn validate_entry(path: &Path, mapping: UserNamespaceMapping) -> Result<(), &'static str> {
    let metadata = fs::symlink_metadata(path).map_err(|_| "rootfs identity cannot be inspected")?;
    if !mapped_id(metadata.uid(), mapping.uid) || !mapped_id(metadata.gid(), mapping.gid) {
        return Err("rootfs contains ownership outside the configured user namespace mapping");
    }
    if metadata.is_dir() {
        for entry in fs::read_dir(path).map_err(|_| "rootfs directory cannot be read")? {
            validate_entry(
                &entry
                    .map_err(|_| "rootfs directory entry cannot be read")?
                    .path(),
                mapping,
            )?;
        }
    }
    Ok(())
}

fn mapped_id(value: u32, mapping: IdMapping) -> bool {
    value >= mapping.host_id && value < mapping.host_id.saturating_add(mapping.size)
}

#[cfg(unix)]
fn shift_tree(root: &Path, mapping: UserNamespaceMapping) -> std::io::Result<()> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};

    fn shift_entry(path: &Path, mapping: UserNamespaceMapping) -> std::io::Result<()> {
        let metadata = fs::symlink_metadata(path)?;
        let uid = shifted_id(metadata.uid(), mapping.uid)?;
        let gid = shifted_id(metadata.gid(), mapping.gid)?;
        if uid != metadata.uid() || gid != metadata.gid() {
            let path_string = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "rootfs path contains NUL")
            })?;
            let result = unsafe { libc::lchown(path_string.as_ptr(), uid, gid) };
            if result != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        if metadata.is_dir() {
            for entry in fs::read_dir(path)? {
                shift_entry(&entry?.path(), mapping)?;
            }
        }
        Ok(())
    }

    shift_entry(root, mapping)
}

#[cfg(not(unix))]
fn shift_tree(_root: &Path, _mapping: UserNamespaceMapping) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "user namespace rootfs ownership preparation is only supported on Unix",
    ))
}

#[cfg(unix)]
fn shifted_id(value: u32, mapping: IdMapping) -> std::io::Result<u32> {
    if value >= mapping.host_id && value < mapping.host_id.saturating_add(mapping.size) {
        return Ok(value);
    }
    if value >= mapping.size {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("rootfs ownership id {value} is outside the mapping"),
        ));
    }
    mapping.host_id.checked_add(value).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "rootfs ownership mapping overflows",
        )
    })
}

fn write_marker_atomically(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    fs::write(&temporary, contents)?;
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(())
}

pub fn prepare_sysroot(
    rootfs_dir: &Path,
    mounts: &BTreeMap<PathBuf, PathBuf>,
) -> std::io::Result<Vec<PathBuf>> {
    let mut changed = Vec::new();
    for mountpoint in MOUNTPOINTS {
        let path = rootfs_dir.join(mountpoint.trim_start_matches('/'));
        if ensure_directory(&path)? {
            changed.push(path);
        }
    }

    for (destination, source) in mounts {
        let source_metadata = fs::metadata(source)?;
        let target = rootfs_dir.join(destination.strip_prefix("/").unwrap_or(destination));
        if source_metadata.is_dir() {
            if ensure_directory(&target)? {
                changed.push(target);
            }
        } else if source_metadata.is_file() {
            if let Some(parent) = target.parent() {
                if ensure_directory(parent)? {
                    changed.push(parent.to_path_buf());
                }
            }
            if !target.exists() {
                fs::File::create(&target)?;
                changed.push(target);
            }
        } else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "mount source is not a regular file or directory: {}",
                    source.display()
                ),
            ));
        }
    }

    Ok(changed)
}

fn check_directory(path: &Path, issues: &mut Vec<SysrootIssue>) {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => issues.push(SysrootIssue {
            path: path.to_path_buf(),
            reason: "not a directory",
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => issues.push(SysrootIssue {
            path: path.to_path_buf(),
            reason: "missing",
        }),
        Err(_) => issues.push(SysrootIssue {
            path: path.to_path_buf(),
            reason: "cannot be inspected",
        }),
    }
}

fn check_file(path: &Path, issues: &mut Vec<SysrootIssue>) {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => issues.push(SysrootIssue {
            path: path.to_path_buf(),
            reason: "mount target is not a file",
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => issues.push(SysrootIssue {
            path: path.to_path_buf(),
            reason: "mount target is missing",
        }),
        Err(_) => issues.push(SysrootIssue {
            path: path.to_path_buf(),
            reason: "mount target cannot be inspected",
        }),
    }
}

fn ensure_directory(path: &Path) -> std::io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => Ok(false),
        Ok(_) => Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("{} exists and is not a directory", path.display()),
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(path)?;
            Ok(true)
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn config(rootfs_dir: PathBuf) -> SandboxConfig {
        let mut config: SandboxConfig = toml::from_str("rootfs_dir = \".\"").unwrap();
        config.rootfs_dir = rootfs_dir;
        config
    }

    #[test]
    fn reports_missing_mountpoints_shell_and_workspace() {
        let temp = tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        fs::create_dir(&rootfs).unwrap();
        let mut mounts = BTreeMap::new();
        mounts.insert(PathBuf::from("/workspace"), temp.path().join("workspace"));
        let issues = sysroot_issues(&config(rootfs), &mounts);

        assert!(issues.iter().any(|issue| issue.path.ends_with("proc")));
        assert!(issues.iter().any(|issue| issue.path.ends_with("bin/sh")));
        assert!(issues
            .iter()
            .any(|issue| issue.reason == "mount source is missing"));
    }

    #[test]
    fn fix_is_idempotent_and_does_not_replace_files() {
        let temp = tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        fs::create_dir_all(rootfs.join("bin")).unwrap();
        fs::write(rootfs.join("bin/sh"), b"shell").unwrap();
        let workspace = temp.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        let mut mounts = BTreeMap::new();
        mounts.insert(PathBuf::from("/workspace"), workspace.clone());
        let config = config(rootfs.clone());

        let changed = fix_sysroot(&config, &mounts).unwrap();
        assert_eq!(changed.len(), 9);
        assert!(fix_sysroot(&config, &mounts).unwrap().is_empty());
        assert!(sysroot_issues(&config, &mounts).is_empty());
        assert_eq!(fs::read(rootfs.join("bin/sh")).unwrap(), b"shell");
    }

    #[test]
    fn fix_rejects_file_mountpoint() {
        let temp = tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        fs::create_dir_all(rootfs.join("bin")).unwrap();
        fs::write(rootfs.join("proc"), b"not a directory").unwrap();
        let error = fix_sysroot(&config(rootfs), &BTreeMap::new()).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
    }

    #[test]
    fn fix_without_workspace_does_not_create_workspace_mountpoint() {
        let temp = tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        fs::create_dir_all(rootfs.join("bin")).unwrap();
        fs::write(rootfs.join("bin/sh"), b"shell").unwrap();

        let mounts = BTreeMap::new();
        let changed = fix_sysroot(&config(rootfs.clone()), &mounts).unwrap();
        assert_eq!(changed.len(), 8);
        assert!(!rootfs.join("workspace").exists());
        assert!(sysroot_issues(&config(rootfs), &mounts).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn shifted_ids_map_container_ownership_into_host_range() {
        let mapping = IdMapping {
            container_id: 0,
            host_id: 100_000,
            size: 65_536,
        };
        assert_eq!(shifted_id(0, mapping).unwrap(), 100_000);
        assert_eq!(shifted_id(42, mapping).unwrap(), 100_042);
        assert_eq!(shifted_id(100_042, mapping).unwrap(), 100_042);
        assert!(shifted_id(65_536, mapping).is_err());
    }
}
