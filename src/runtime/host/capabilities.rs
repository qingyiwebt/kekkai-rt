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
    ip: bool,
    nsenter: bool,
    iptables: bool,
}

impl HostCapabilities {
    pub fn detect() -> Self {
        Self {
            cgroups: CgroupCapabilities {
                memory_controller: cgroups::memory_controller_available(),
            },
            network: NetworkCapabilities {
                net_admin: probe::net_admin_available(),
                ip: probe::command_available("ip", "-V"),
                nsenter: probe::command_available("nsenter", "--version"),
                iptables: probe::command_available("iptables", "--version"),
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
        ip: bool,
        nsenter: bool,
        iptables: bool,
    ) -> Self {
        Self {
            cgroups: CgroupCapabilities {
                memory_controller: memory_cgroup,
            },
            network: NetworkCapabilities {
                net_admin,
                ip,
                nsenter,
                iptables,
            },
        }
    }
}
impl NetworkCapabilities {
    fn nat_available(&self) -> bool {
        self.net_admin && self.ip && self.nsenter && self.iptables
    }
    fn unavailability_reasons(&self) -> Vec<&'static str> {
        let mut reasons = Vec::new();
        if !self.net_admin {
            reasons.push("CAP_NET_ADMIN");
        }
        if !self.ip {
            reasons.push("ip");
        }
        if !self.nsenter {
            reasons.push("nsenter");
        }
        if !self.iptables {
            reasons.push("iptables");
        }
        reasons
    }
}

#[cfg(test)]
mod tests {
    use super::{cgroups, probe, HostCapabilities};
    #[test]
    fn nat_requires_all_network_capabilities() {
        let capabilities = HostCapabilities::for_test(true, true, true, true, true);
        assert!(capabilities.nat_available());
        let capabilities = HostCapabilities::for_test(true, false, true, true, true);
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
