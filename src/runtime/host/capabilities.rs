use super::{cgroups, probe};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostCapabilities {
    pub cgroups: CgroupCapabilities,
    pub network: NetworkCapabilities,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CgroupCapabilities {
    pub memory_controller: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetworkCapabilities {
    net_admin: bool,
    route_netlink: bool,
    netfilter_netlink: bool,
}

impl HostCapabilities {
    pub fn detect() -> Self {
        Self {
            cgroups: CgroupCapabilities {
                memory_controller: cgroups::memory_controller_available(),
            },
            network: NetworkCapabilities {
                net_admin: probe::net_admin_available(),
                route_netlink: probe::route_netlink_available(),
                netfilter_netlink: probe::netfilter_netlink_available(),
            },
        }
    }
    pub fn nat_available(&self) -> bool {
        self.network.nat_available()
    }
    pub fn nat_unavailability_reasons(&self) -> Vec<&'static str> {
        self.network.unavailability_reasons()
    }
    #[cfg(test)]
    pub fn for_test(
        memory_cgroup: bool,
        net_admin: bool,
        route_netlink: bool,
        netfilter_netlink: bool,
    ) -> Self {
        Self {
            cgroups: CgroupCapabilities {
                memory_controller: memory_cgroup,
            },
            network: NetworkCapabilities {
                net_admin,
                route_netlink,
                netfilter_netlink,
            },
        }
    }
}
impl NetworkCapabilities {
    fn nat_available(&self) -> bool {
        self.net_admin && self.route_netlink && self.netfilter_netlink
    }
    fn unavailability_reasons(&self) -> Vec<&'static str> {
        let mut reasons = Vec::new();
        if !self.net_admin {
            reasons.push("CAP_NET_ADMIN");
        }
        if !self.route_netlink {
            reasons.push("NETLINK_ROUTE");
        }
        if !self.netfilter_netlink {
            reasons.push("NETLINK_NETFILTER");
        }
        reasons
    }
}

#[cfg(test)]
mod tests {
    use super::{cgroups, probe, HostCapabilities};
    #[test]
    fn nat_requires_all_network_capabilities() {
        let capabilities = HostCapabilities::for_test(true, true, true, true);
        assert!(capabilities.nat_available());
        let capabilities = HostCapabilities::for_test(true, false, true, true);
        assert!(!capabilities.nat_available());
        assert_eq!(
            capabilities.nat_unavailability_reasons(),
            vec!["CAP_NET_ADMIN"]
        );
    }
    #[test]
    fn parses_cgroup_controller_formats() {
        assert!(cgroups::has_controller("cpuset cpu memory io", "memory"));
        assert!(!cgroups::has_controller("cpuset cpu io", "memory"));
        assert!(cgroups::v1_memory_controller_enabled(
            "#subsys_name hierarchy num_cgroups enabled\nmemory 4 12 1\n"
        ));
        assert!(!cgroups::v1_memory_controller_enabled(
            "#subsys_name hierarchy num_cgroups enabled\nmemory 4 12 0\n"
        ));
    }
    #[test]
    fn parses_effective_net_admin_capability() {
        assert!(probe::has_cap_net_admin(
            "Name:\ttest\nCapEff:\t0000000000001000\n"
        ));
        assert!(!probe::has_cap_net_admin(
            "Name:\ttest\nCapEff:\t0000000000000000\n"
        ));
        assert!(!probe::has_cap_net_admin("Name:\ttest\n"));
    }
}
