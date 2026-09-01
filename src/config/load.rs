use super::Config;
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("read config: {0}")]
    Io(#[from] std::io::Error),
    #[error("parse config: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("api.token must not be empty")]
    EmptyToken,
    #[error("sandbox.rootfs_dir does not exist or is not a directory: {0}")]
    MissingRootfs(PathBuf),
    #[error("tool {name} executable does not exist or is not a regular file: {path}")]
    InvalidToolPath { name: String, path: PathBuf },
    #[error("tool {name} executable is not executable: {path}")]
    ToolNotExecutable { name: String, path: PathBuf },
    #[error("tool {name} env file does not exist or is not a regular file: {path}")]
    InvalidToolEnvPath { name: String, path: PathBuf },
    #[error("tool name must not be empty or contain NUL: {name:?}")]
    InvalidToolName { name: String },
    #[error("invalid sandbox network configuration: {0}")]
    InvalidNetwork(String),
    #[error("mount destination must be an absolute normal path: {0}")]
    InvalidMountDestination(PathBuf),
    #[error("mount destination conflicts with a runtime mount: {0}")]
    ReservedMountDestination(PathBuf),
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let mut config: Self = toml::from_str(&fs::read_to_string(path)?)?;
        if config.api.token.trim().is_empty() {
            return Err(ConfigError::EmptyToken);
        }
        let config_dir = config_directory(path)?;
        let rootfs_candidate = resolve_path(&config_dir, config.sandbox.rootfs_dir);
        config.sandbox.rootfs_dir = match fs::canonicalize(&rootfs_candidate) {
            Ok(path) => path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(ConfigError::MissingRootfs(rootfs_candidate))
            }
            Err(error) => return Err(ConfigError::Io(error)),
        };
        config.mounts = config
            .mounts
            .into_iter()
            .map(|(destination, source)| {
                validate_mount_destination(&destination)?;
                Ok((destination, resolve_path(&config_dir, source)))
            })
            .collect::<Result<_, ConfigError>>()?;
        config.sandbox.managed_bundle_dir = config_dir.join("bundle");

        for (name, tool) in &mut config.tools {
            if name.is_empty() || name.contains('\0') {
                return Err(ConfigError::InvalidToolName { name: name.clone() });
            }
            tool.path = resolve_path(&config_dir, tool.path.clone());
            tool.env = tool.env.take().map(|env| resolve_path(&config_dir, env));
            let metadata = fs::metadata(&tool.path).map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    ConfigError::InvalidToolPath {
                        name: name.clone(),
                        path: tool.path.clone(),
                    }
                } else {
                    ConfigError::Io(error)
                }
            })?;
            if !metadata.is_file() {
                return Err(ConfigError::InvalidToolPath {
                    name: name.clone(),
                    path: tool.path.clone(),
                });
            }
            if metadata.permissions().mode() & 0o111 == 0 {
                return Err(ConfigError::ToolNotExecutable {
                    name: name.clone(),
                    path: tool.path.clone(),
                });
            }
            if let Some(env) = &tool.env {
                let env_metadata = fs::metadata(env).map_err(|error| {
                    if error.kind() == std::io::ErrorKind::NotFound {
                        ConfigError::InvalidToolEnvPath {
                            name: name.clone(),
                            path: env.clone(),
                        }
                    } else {
                        ConfigError::Io(error)
                    }
                })?;
                if !env_metadata.is_file() {
                    return Err(ConfigError::InvalidToolEnvPath {
                        name: name.clone(),
                        path: env.clone(),
                    });
                }
            }
        }
        if !config.sandbox.rootfs_dir.is_dir() {
            return Err(ConfigError::MissingRootfs(config.sandbox.rootfs_dir));
        }
        config
            .sandbox
            .resolved()
            .map_err(ConfigError::InvalidNetwork)?;
        Ok(config)
    }
}

pub(crate) fn validate_mount_destination(path: &Path) -> Result<(), ConfigError> {
    let valid = path.is_absolute()
        && path != Path::new("/")
        && path
            .components()
            .skip(1)
            .all(|component| matches!(component, std::path::Component::Normal(_)));
    if !valid {
        return Err(ConfigError::InvalidMountDestination(path.to_path_buf()));
    }
    const RESERVED: &[&str] = &[
        "/proc",
        "/sys",
        "/dev",
        "/dev/pts",
        "/dev/shm",
        "/dev/mqueue",
        "/run",
        "/sys/fs/cgroup",
    ];
    if RESERVED.iter().any(|reserved| {
        let reserved = Path::new(reserved);
        path == reserved || path.starts_with(reserved)
    }) {
        return Err(ConfigError::ReservedMountDestination(path.to_path_buf()));
    }
    Ok(())
}

pub(crate) fn resolve_path(base: &Path, path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        base.join(path)
    }
}

pub(crate) fn config_directory(path: &Path) -> std::io::Result<PathBuf> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::canonicalize(parent)
}
