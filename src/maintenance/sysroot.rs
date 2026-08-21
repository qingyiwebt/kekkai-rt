use crate::config::SandboxConfig;
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

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
pub(crate) struct SysrootIssue {
    pub(crate) path: PathBuf,
    pub(crate) reason: &'static str,
}

impl std::fmt::Display for SysrootIssue {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{} ({})", self.path.display(), self.reason)
    }
}

pub(crate) fn sysroot_issues(config: &SandboxConfig) -> Vec<SysrootIssue> {
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

    if let Some(workspace_dir) = &config.workspace_dir {
        match fs::metadata(workspace_dir) {
            Ok(metadata) if !metadata.is_dir() => issues.push(SysrootIssue {
                path: workspace_dir.clone(),
                reason: "workspace is not a directory",
            }),
            Ok(metadata) if metadata.permissions().mode() & 0o222 == 0 => {
                issues.push(SysrootIssue {
                    path: workspace_dir.clone(),
                    reason: "workspace is not writable",
                })
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                issues.push(SysrootIssue {
                    path: workspace_dir.clone(),
                    reason: "workspace is missing",
                })
            }
            Err(_) => issues.push(SysrootIssue {
                path: workspace_dir.clone(),
                reason: "workspace cannot be inspected",
            }),
        }

        check_directory(&config.rootfs_dir.join("workspace"), &mut issues);
    }

    issues
}

pub(crate) fn fix_sysroot(config: &SandboxConfig) -> std::io::Result<Vec<PathBuf>> {
    prepare_sysroot(&config.rootfs_dir, config.workspace_dir.as_deref())
}

pub(crate) fn prepare_sysroot(
    rootfs_dir: &Path,
    workspace_dir: Option<&Path>,
) -> std::io::Result<Vec<PathBuf>> {
    let mut changed = Vec::new();
    for mountpoint in MOUNTPOINTS {
        let path = rootfs_dir.join(mountpoint.trim_start_matches('/'));
        if ensure_directory(&path)? {
            changed.push(path);
        }
    }

    if workspace_dir.is_some() {
        let path = rootfs_dir.join("workspace");
        if ensure_directory(&path)? {
            changed.push(path);
        }
    }

    if let Some(workspace_dir) = workspace_dir {
        ensure_directory(workspace_dir)?;
        fs::set_permissions(workspace_dir, fs::Permissions::from_mode(0o777))?;
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

    fn config(rootfs_dir: PathBuf, workspace_dir: Option<PathBuf>) -> SandboxConfig {
        let mut config: SandboxConfig = toml::from_str("rootfs_dir = \".\"").unwrap();
        config.rootfs_dir = rootfs_dir;
        config.workspace_dir = workspace_dir;
        config
    }

    #[test]
    fn reports_missing_mountpoints_shell_and_workspace() {
        let temp = tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        fs::create_dir(&rootfs).unwrap();
        let issues = sysroot_issues(&config(rootfs, Some(temp.path().join("workspace"))));

        assert!(issues.iter().any(|issue| issue.path.ends_with("proc")));
        assert!(issues.iter().any(|issue| issue.path.ends_with("bin/sh")));
        assert!(issues
            .iter()
            .any(|issue| issue.reason == "workspace is missing"));
    }

    #[test]
    fn fix_is_idempotent_and_does_not_replace_files() {
        let temp = tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        fs::create_dir_all(rootfs.join("bin")).unwrap();
        fs::write(rootfs.join("bin/sh"), b"shell").unwrap();
        let workspace = temp.path().join("workspace");
        let config = config(rootfs.clone(), Some(workspace.clone()));

        let changed = fix_sysroot(&config).unwrap();
        assert_eq!(changed.len(), 9);
        assert!(fix_sysroot(&config).unwrap().is_empty());
        assert!(sysroot_issues(&config).is_empty());
        assert_eq!(fs::read(rootfs.join("bin/sh")).unwrap(), b"shell");
        assert_eq!(
            fs::metadata(workspace).unwrap().permissions().mode() & 0o777,
            0o777
        );
    }

    #[test]
    fn fix_rejects_file_mountpoint() {
        let temp = tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        fs::create_dir_all(rootfs.join("bin")).unwrap();
        fs::write(rootfs.join("proc"), b"not a directory").unwrap();
        let error = fix_sysroot(&config(rootfs, None)).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
    }

    #[test]
    fn fix_without_workspace_does_not_create_workspace_mountpoint() {
        let temp = tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        fs::create_dir_all(rootfs.join("bin")).unwrap();
        fs::write(rootfs.join("bin/sh"), b"shell").unwrap();

        let changed = fix_sysroot(&config(rootfs.clone(), None)).unwrap();
        assert_eq!(changed.len(), 8);
        assert!(!rootfs.join("workspace").exists());
        assert!(sysroot_issues(&config(rootfs, None)).is_empty());
    }
}
