use serde::{Deserialize, Serialize};
use std::net::Ipv4Addr;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NetworkMode {
    Nat,
    Host,
    None,
}

impl NetworkMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Nat => "nat",
            Self::Host => "host",
            Self::None => "none",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ipv4Cidr {
    pub address: Ipv4Addr,
    pub prefix: u8,
}

impl Ipv4Cidr {
    fn parse(value: &str, field: &str) -> Result<Self, String> {
        let (address, prefix) = value
            .split_once('/')
            .ok_or_else(|| format!("{field} must be an IPv4 CIDR"))?;
        let address = address
            .parse::<Ipv4Addr>()
            .map_err(|_| format!("{field} has an invalid IPv4 address"))?;
        let prefix = prefix
            .parse::<u8>()
            .map_err(|_| format!("{field} has an invalid prefix length"))?;
        if prefix > 32 {
            return Err(format!("{field} prefix length must be between 0 and 32"));
        }
        Ok(Self { address, prefix })
    }

    fn mask(&self) -> u32 {
        if self.prefix == 0 {
            0
        } else {
            u32::MAX << (32 - self.prefix)
        }
    }

    fn network(&self) -> u32 {
        u32::from(self.address) & self.mask()
    }

    pub fn contains(&self, address: Ipv4Addr) -> bool {
        u32::from(address) & self.mask() == self.network()
    }

    pub fn address_with_prefix(&self, address: Ipv4Addr) -> String {
        format!("{address}/{}", self.prefix)
    }

    pub fn network_with_prefix(&self) -> String {
        self.address_with_prefix(Ipv4Addr::from(self.network()))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetworkSettings {
    pub mode: NetworkMode,
    pub bridge: String,
    pub subnet: Ipv4Cidr,
    pub gateway: Ipv4Addr,
    pub address: Ipv4Addr,
    pub dns: Vec<Ipv4Addr>,
}

impl super::SandboxConfig {
    pub fn network_settings(&self) -> Result<NetworkSettings, String> {
        if self.network_bridge.is_empty()
            || self.network_bridge.len() > 15
            || self
                .network_bridge
                .chars()
                .any(|c| c.is_whitespace() || c == '/')
        {
            return Err(
                "network_bridge must be a non-empty Linux interface name of at most 15 characters"
                    .into(),
            );
        }

        let subnet = Ipv4Cidr::parse(&self.network_subnet, "network_subnet")?;
        if subnet.address != Ipv4Addr::from(subnet.network()) {
            return Err("network_subnet must use the network address for its prefix".into());
        }
        let gateway = self
            .network_gateway
            .parse::<Ipv4Addr>()
            .map_err(|_| "network_gateway has an invalid IPv4 address".to_string())?;
        let address = self
            .network_ip
            .parse::<Ipv4Addr>()
            .map_err(|_| "network_ip has an invalid IPv4 address".to_string())?;
        if !subnet.contains(gateway) {
            return Err("network_gateway must belong to network_subnet".into());
        }
        if !subnet.contains(address) {
            return Err("network_ip must belong to network_subnet".into());
        }
        if gateway == address {
            return Err("network_gateway and network_ip must be different".into());
        }
        if subnet.prefix <= 30 {
            let broadcast = subnet.network() | !subnet.mask();
            if u32::from(gateway) == subnet.network() || u32::from(gateway) == broadcast {
                return Err("network_gateway must not be the network or broadcast address".into());
            }
            if u32::from(address) == subnet.network() || u32::from(address) == broadcast {
                return Err("network_ip must not be the network or broadcast address".into());
            }
        }

        let mut dns = Vec::with_capacity(self.network_dns.len());
        for value in &self.network_dns {
            dns.push(
                value
                    .parse::<Ipv4Addr>()
                    .map_err(|_| format!("network_dns contains invalid IPv4 address: {value}"))?,
            );
        }

        Ok(NetworkSettings {
            mode: self.network_mode,
            bridge: self.network_bridge.clone(),
            subnet,
            gateway,
            address,
            dns,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> super::super::SandboxConfig {
        toml::from_str("rootfs_dir = \".\"").unwrap()
    }

    #[test]
    fn network_defaults_to_nat() {
        let settings = config().network_settings().unwrap();
        assert_eq!(settings.mode, NetworkMode::Nat);
        assert_eq!(settings.bridge, "kekkai-rt0");
        assert_eq!(settings.subnet.prefix, 24);
        assert_eq!(settings.gateway, Ipv4Addr::new(10, 200, 0, 1));
        assert_eq!(settings.address, Ipv4Addr::new(10, 200, 0, 2));
    }

    #[test]
    fn network_modes_are_supported() {
        for mode in [NetworkMode::Nat, NetworkMode::Host, NetworkMode::None] {
            let mut config = config();
            config.network_mode = mode;
            assert!(config.network_settings().is_ok(), "mode={mode:?}");
        }
    }

    #[test]
    fn invalid_network_values_are_rejected() {
        let parsed: Result<super::super::SandboxConfig, _> =
            toml::from_str("rootfs_dir = \".\"\nnetwork_mode = \"bridge\"");
        assert!(parsed.is_err());

        let mut config = config();
        config.network_mode = NetworkMode::Nat;
        config.network_ip = "10.201.0.2".into();
        assert!(config.network_settings().is_err());
    }
}
