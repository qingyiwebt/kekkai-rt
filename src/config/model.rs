use super::{features::FeaturesConfig, network::NetworkMode, runtime::RuntimeBackend};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap},
    net::SocketAddr,
    path::PathBuf,
};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Config {
    pub api: ApiConfig,
    pub sandbox: SandboxConfig,
    #[serde(default)]
    pub features: FeaturesConfig,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub mounts: BTreeMap<PathBuf, PathBuf>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub tools: HashMap<String, ToolConfig>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ApiConfig {
    pub listen_addr: SocketAddr,
    pub token: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ToolConfig {
    pub path: PathBuf,
    #[serde(default)]
    pub env: Option<PathBuf>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxConfig {
    pub rootfs_dir: PathBuf,
    #[serde(default = "default_backend")]
    pub backend: RuntimeBackend,
    #[serde(default = "default_timeout")]
    pub max_timeout_seconds: u64,
    #[serde(default = "default_network_mode")]
    pub network_mode: NetworkMode,
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

fn default_backend() -> RuntimeBackend {
    RuntimeBackend::Runsc
}
fn default_timeout() -> u64 {
    300
}
fn default_network_mode() -> NetworkMode {
    NetworkMode::Nat
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
