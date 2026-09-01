use std::{fs, os::unix::fs::PermissionsExt, process::Command};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HostCapabilities {
    pub(crate) cgroups: CgroupCapabilities,
    pub(crate) network: NetworkCapabilities,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CgroupCapabilities {
    pub(crate) memory_controller: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NetworkCapabilities {
    net_admin: bool,
    ip: bool,
    nsenter: bool,
    iptables: bool,
}

impl HostCapabilities {
    pub(crate) fn detect() -> Self {
        Self {
            cgroups: CgroupCapabilities {
                memory_controller: memory_cgroup_available(),
            },
            network: NetworkCapabilities {
                net_admin: net_admin_available(),
                ip: host_command_available("ip", "-V"),
                nsenter: host_command_available("nsenter", "--version"),
                iptables: host_command_available("iptables", "--version"),
            },
        }
    }

    pub(crate) fn nat_available(&self) -> bool {
        self.network.nat_available()
    }

    pub(crate) fn nat_unavailability_reasons(&self) -> Vec<&'static str> {
        self.network.unavailability_reasons()
    }

    #[cfg(test)]
    pub(crate) fn for_test(
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

fn memory_cgroup_available() -> bool {
    if let Ok(controllers) = fs::read_to_string("/sys/fs/cgroup/cgroup.controllers") {
        return has_controller(&controllers, "memory");
    }

    fs::read_to_string("/proc/cgroups")
        .ok()
        .map(|content| v1_memory_controller_enabled(&content))
        .unwrap_or(false)
}

fn has_controller(controllers: &str, wanted: &str) -> bool {
    controllers
        .split_whitespace()
        .any(|controller| controller == wanted)
}

fn v1_memory_controller_enabled(cgroups: &str) -> bool {
    cgroups.lines().any(|line| {
        let mut fields = line.split_whitespace();
        let name = fields.next();
        let _hierarchy = fields.next();
        let _groups = fields.next();
        let enabled = fields.next();
        name == Some("memory") && enabled == Some("1")
    })
}

fn net_admin_available() -> bool {
    if !cfg!(target_os = "linux") {
        return false;
    }

    let Ok(status) = fs::read_to_string("/proc/self/status") else {
        return false;
    };
    has_cap_net_admin(&status)
}

fn has_cap_net_admin(status: &str) -> bool {
    let Some(value) = status
        .lines()
        .find_map(|line| line.strip_prefix("CapEff:").map(str::trim))
    else {
        return false;
    };
    u64::from_str_radix(value, 16)
        .map(|effective| effective & (1 << 12) != 0)
        .unwrap_or(false)
}

fn host_command_available(program: &str, version_flag: &str) -> bool {
    let Some(path) = std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path).find_map(|directory| {
            let candidate = directory.join(program);
            match fs::metadata(&candidate) {
                Ok(metadata)
                    if metadata.is_file() && metadata.permissions().mode() & 0o111 != 0 =>
                {
                    Some(candidate)
                }
                _ => None,
            }
        })
    }) else {
        return false;
    };

    Command::new(path)
        .arg(version_flag)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::{
        has_cap_net_admin, has_controller, v1_memory_controller_enabled, HostCapabilities,
    };

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
        assert!(has_controller("cpuset cpu memory io", "memory"));
        assert!(!has_controller("cpuset cpu io", "memory"));
        assert!(v1_memory_controller_enabled(
            "#subsys_name hierarchy num_cgroups enabled\nmemory 4 12 1\n"
        ));
        assert!(!v1_memory_controller_enabled(
            "#subsys_name hierarchy num_cgroups enabled\nmemory 4 12 0\n"
        ));
    }

    #[test]
    fn parses_effective_net_admin_capability() {
        assert!(has_cap_net_admin(
            "Name:\ttest\nCapEff:\t0000000000001000\n"
        ));
        assert!(!has_cap_net_admin(
            "Name:\ttest\nCapEff:\t0000000000000000\n"
        ));
        assert!(!has_cap_net_admin("Name:\ttest\n"));
    }
}
