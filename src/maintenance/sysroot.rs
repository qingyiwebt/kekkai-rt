use crate::config::SandboxConfig;
use std::{
    collections::BTreeMap,
    fs,
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
}
