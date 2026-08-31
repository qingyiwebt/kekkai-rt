mod network;
mod runtime;

use serde::Deserialize;
use std::os::unix::fs::PermissionsExt;
use std::{
    collections::{BTreeMap, HashMap},
    fs,
    net::SocketAddr,
    path::{Path, PathBuf},
};
use thiserror::Error;

pub use network::{NetworkMode, NetworkSettings};

#[derive(Clone, Debug, Deserialize)]
pub struct Config {
    pub api: ApiConfig,
    pub sandbox: SandboxConfig,
    #[serde(default)]
    pub mounts: BTreeMap<PathBuf, PathBuf>,
    #[serde(default)]
    pub tools: HashMap<String, ToolConfig>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ApiConfig {
    pub listen_addr: SocketAddr,
    pub token: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ToolConfig {
    pub path: PathBuf,
    #[serde(default)]
    pub env: Option<PathBuf>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxConfig {
    pub rootfs_dir: PathBuf,
    #[serde(default = "default_backend")]
    pub backend: String,
    #[serde(default = "default_timeout")]
    pub max_timeout_seconds: u64,
    #[serde(default = "default_network_mode")]
    pub network_mode: String,
    #[serde(default = "default_network_bridge")]
    pub network_bridge: String,
    #[serde(default = "default_network_subnet")]
    pub network_subnet: String,
    #[serde(default = "default_network_gateway")]
    pub network_gateway: String,
    #[serde(default = "default_network_ip")]
    pub network_ip: String,
    #[serde(default = "default_network_dns")]
    pub network_dns: Vec<String>,
    #[serde(skip)]
    pub managed_bundle_dir: PathBuf,
}

fn default_backend() -> String {
    "runsc".into()
}

fn default_timeout() -> u64 {
    300
}

fn default_network_mode() -> String {
    "nat".into()
}

fn default_network_bridge() -> String {
    "kekkai-rt0".into()
}

fn default_network_subnet() -> String {
    "10.200.0.0/24".into()
}

fn default_network_gateway() -> String {
    "10.200.0.1".into()
}

fn default_network_ip() -> String {
    "10.200.0.2".into()
}

fn default_network_dns() -> Vec<String> {
    vec!["1.1.1.1".into(), "8.8.8.8".into()]
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("read config: {0}")]
    Io(#[from] std::io::Error),
    #[error("parse config: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("api.token must not be empty")]
    EmptyToken,
    #[error("sandbox.backend must be runsc or runc")]
    InvalidBackend,
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
        if config.sandbox.backend != "runsc" && config.sandbox.backend != "runc" {
            return Err(ConfigError::InvalidBackend);
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

fn validate_mount_destination(path: &Path) -> Result<(), ConfigError> {
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

fn resolve_path(base: &Path, path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        base.join(path)
    }
}

fn config_directory(path: &Path) -> std::io::Result<PathBuf> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::canonicalize(parent)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_file_without_parent_uses_current_directory() {
        let current_dir = fs::canonicalize(".").unwrap();
        assert_eq!(
            config_directory(Path::new("config.toml")).unwrap(),
            current_dir
        );
    }

    #[test]
    fn load_resolves_mount_paths_relative_to_config_file_without_preparing_sources() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("rootfs")).unwrap();
        let config_path = temp.path().join("config.toml");
        fs::write(
            &config_path,
            r#"
[api]
listen_addr = "127.0.0.1:0"
token = "secret"

[sandbox]
rootfs_dir = "rootfs"

[mounts]
"/workspace" = "workspace"
"#,
        )
        .unwrap();

        let config = Config::load(&config_path).unwrap();
        let config_dir = fs::canonicalize(temp.path()).unwrap();
        assert_eq!(config.sandbox.rootfs_dir, config_dir.join("rootfs"));
        assert_eq!(
            config.mounts.get(&PathBuf::from("/workspace")),
            Some(&config_dir.join("workspace"))
        );
        assert_eq!(config.sandbox.managed_bundle_dir, config_dir.join("bundle"));
        assert!(!temp.path().join("workspace").exists());
        assert!(config.tools.is_empty());
    }

    #[test]
    fn load_resolves_and_parses_tools() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("rootfs")).unwrap();
        let tool = temp.path().join("tool");
        fs::write(&tool, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(
            temp.path().join("tool.env"),
            "# secret\nKEY=VALUE\nQUOTED='hello'\n",
        )
        .unwrap();
        let config_path = temp.path().join("config.toml");
        fs::write(
            &config_path,
            r#"
[api]
listen_addr = "127.0.0.1:0"
token = "secret"

[sandbox]
rootfs_dir = "rootfs"

[tools.'something-cli']
path = "tool"
env = "tool.env"
"#,
        )
        .unwrap();

        let config = Config::load(&config_path).unwrap();
        let tool = config.tools.get("something-cli").unwrap();
        let config_dir = fs::canonicalize(temp.path()).unwrap();
        assert_eq!(tool.path, config_dir.join("tool"));
        assert_eq!(tool.env, Some(config_dir.join("tool.env")));
    }

    #[test]
    fn load_allows_tools_without_env_file() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("rootfs")).unwrap();
        let tool = temp.path().join("tool");
        fs::write(&tool, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
        let config_path = temp.path().join("config.toml");
        fs::write(
            &config_path,
            r#"
[api]
listen_addr = "127.0.0.1:0"
token = "token"

[sandbox]
rootfs_dir = "rootfs"

[tools.'something-cli']
path = "tool"
"#,
        )
        .unwrap();

        let config = Config::load(&config_path).unwrap();
        assert_eq!(config.tools["something-cli"].env, None);
    }

    #[test]
    fn old_bundle_configuration_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("config.toml");
        fs::write(
            &config_path,
            r#"
[api]
listen_addr = "127.0.0.1:0"
token = "secret"

[sandbox]
bundle_dir = "."
"#,
        )
        .unwrap();
        assert!(matches!(
            Config::load(&config_path),
            Err(ConfigError::Parse(_))
        ));
    }

    #[test]
    fn old_workspace_configuration_is_rejected() {
        let parsed: Result<SandboxConfig, _> = toml::from_str(
            r#"
rootfs_dir = "."
workspace_dir = "./workspace"
"#,
        );
        assert!(parsed.is_err());
    }

    #[test]
    fn mount_destinations_must_be_safe_absolute_paths() {
        let mut config: Config = toml::from_str(
            r#"
[api]
listen_addr = "127.0.0.1:0"
token = "secret"

[sandbox]
rootfs_dir = "."

[mounts]
"relative" = "/tmp/source"
"#,
        )
        .unwrap();
        let error = config
            .mounts
            .keys()
            .next()
            .map(|path| validate_mount_destination(path))
            .unwrap();
        assert!(error.is_err());
        config.mounts.clear();
        assert!(validate_mount_destination(Path::new("/proc")).is_err());
        assert!(validate_mount_destination(Path::new("/safe/path")).is_ok());
    }
}
