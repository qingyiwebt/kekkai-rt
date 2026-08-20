mod network;
mod runtime;

use serde::Deserialize;
use std::{
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
}

#[derive(Clone, Debug, Deserialize)]
pub struct ApiConfig {
    pub listen_addr: SocketAddr,
    pub secret: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxConfig {
    pub rootfs_dir: PathBuf,
    #[serde(default)]
    pub workspace_dir: Option<PathBuf>,
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
    "agentcell0".into()
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
    #[error("api.secret must not be empty")]
    EmptySecret,
    #[error("sandbox.backend must be runsc or runc")]
    InvalidBackend,
    #[error("sandbox.rootfs_dir does not exist or is not a directory: {0}")]
    MissingRootfs(PathBuf),
    #[error("invalid sandbox network configuration: {0}")]
    InvalidNetwork(String),
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let mut config: Self = toml::from_str(&fs::read_to_string(path)?)?;
        if config.api.secret.trim().is_empty() {
            return Err(ConfigError::EmptySecret);
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
        config.sandbox.workspace_dir = config
            .sandbox
            .workspace_dir
            .take()
            .map(|dir| resolve_path(&config_dir, dir));
        config.sandbox.managed_bundle_dir = config_dir.join("bundle");

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
    fn workspace_defaults_to_none() {
        let config: SandboxConfig = toml::from_str("rootfs_dir = \".\"").unwrap();
        assert!(config.workspace_dir.is_none());
    }

    #[test]
    fn config_file_without_parent_uses_current_directory() {
        let current_dir = fs::canonicalize(".").unwrap();
        assert_eq!(
            config_directory(Path::new("config.toml")).unwrap(),
            current_dir
        );
    }

    #[test]
    fn load_resolves_paths_relative_to_config_file_without_preparing_workspace() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("rootfs")).unwrap();
        let config_path = temp.path().join("config.toml");
        fs::write(
            &config_path,
            r#"
[api]
listen_addr = "127.0.0.1:0"
secret = "secret"

[sandbox]
rootfs_dir = "rootfs"
workspace_dir = "workspace"
"#,
        )
        .unwrap();

        let config = Config::load(&config_path).unwrap();
        let config_dir = fs::canonicalize(temp.path()).unwrap();
        assert_eq!(config.sandbox.rootfs_dir, config_dir.join("rootfs"));
        assert_eq!(
            config.sandbox.workspace_dir,
            Some(config_dir.join("workspace"))
        );
        assert_eq!(config.sandbox.managed_bundle_dir, config_dir.join("bundle"));
        assert!(!temp.path().join("workspace").exists());
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
secret = "secret"

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
}
