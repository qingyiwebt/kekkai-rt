#[cfg(target_os = "linux")]
use std::{fs, path::Path};

#[cfg(any(target_os = "linux", test))]
pub const ID_MAP_SIZE: u32 = 65_536;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IdMapping {
    pub container_id: u32,
    pub host_id: u32,
    pub size: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UserNamespaceMapping {
    pub uid: IdMapping,
    pub gid: IdMapping,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserNamespaceCapabilities {
    pub namespace: bool,
    pub setuid: bool,
    pub setgid: bool,
    pub mapping: Option<UserNamespaceMapping>,
}

impl UserNamespaceCapabilities {
    pub fn available(&self) -> bool {
        self.namespace && self.setuid && self.setgid && self.mapping.is_some()
    }

    pub fn unavailability_reasons(&self) -> Vec<&'static str> {
        let mut reasons = Vec::new();
        if !self.namespace {
            reasons.push("user namespace support");
        }
        if !self.setuid {
            reasons.push("CAP_SETUID");
        }
        if !self.setgid {
            reasons.push("CAP_SETGID");
        }
        if self.mapping.is_none() {
            reasons.push("/etc/subuid and /etc/subgid mappings");
        }
        reasons
    }
}

pub fn detect() -> UserNamespaceCapabilities {
    #[cfg(target_os = "linux")]
    {
        let status = fs::read_to_string("/proc/self/status").unwrap_or_default();
        let uid = unsafe { libc::geteuid() };
        let passwd_owner = fs::read_to_string("/etc/passwd").ok().and_then(|content| {
            content.lines().find_map(|line| {
                let fields = line.split(':').collect::<Vec<_>>();
                (fields.get(2)?.parse::<u32>().ok()? == uid).then(|| fields[0].to_owned())
            })
        });
        let owners = [
            std::env::var("USER").ok(),
            std::env::var("LOGNAME").ok(),
            passwd_owner,
            (uid == 0).then(|| "root".to_owned()),
            Some(uid.to_string()),
        ];
        let owners = owners
            .iter()
            .filter_map(Option::as_deref)
            .collect::<Vec<_>>();
        let mapping = match (
            fs::read_to_string("/etc/subuid"),
            fs::read_to_string("/etc/subgid"),
        ) {
            (Ok(uid_map), Ok(gid_map)) => {
                let uid_start = first_range(&uid_map, &owners);
                let gid_start = first_range(&gid_map, &owners);
                uid_start
                    .zip(gid_start)
                    .map(|(uid_start, gid_start)| UserNamespaceMapping {
                        uid: IdMapping {
                            container_id: 0,
                            host_id: uid_start,
                            size: ID_MAP_SIZE,
                        },
                        gid: IdMapping {
                            container_id: 0,
                            host_id: gid_start,
                            size: ID_MAP_SIZE,
                        },
                    })
            }
            _ => None,
        };
        return UserNamespaceCapabilities {
            namespace: Path::new("/proc/self/ns/user").exists()
                && fs::read_to_string("/proc/sys/user/max_user_namespaces")
                    .ok()
                    .and_then(|value| value.trim().parse::<u64>().ok())
                    .is_some_and(|value| value > 0),
            setuid: super::probe::capability_available(&status, 7),
            setgid: super::probe::capability_available(&status, 6),
            mapping,
        };
    }

    #[cfg(not(target_os = "linux"))]
    {
        UserNamespaceCapabilities {
            namespace: false,
            setuid: false,
            setgid: false,
            mapping: None,
        }
    }
}

#[cfg(any(target_os = "linux", test))]
fn first_range(contents: &str, owners: &[&str]) -> Option<u32> {
    contents.lines().find_map(|line| {
        let mut fields = line.split(':');
        let owner = fields.next()?.trim();
        if !owners.iter().any(|candidate| *candidate == owner) {
            return None;
        }
        let start = fields.next()?.trim().parse::<u32>().ok()?;
        let size = fields.next()?.trim().parse::<u32>().ok()?;
        (size >= ID_MAP_SIZE && start.checked_add(ID_MAP_SIZE - 1).is_some()).then_some(start)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_first_sufficient_subordinate_range() {
        assert_eq!(
            first_range("alice:2000:100\nalice:100000:65536\n", &["alice"]),
            Some(100000)
        );
    }

    #[test]
    fn rejects_short_or_overflowing_ranges() {
        assert_eq!(first_range("alice:1000:65535\n", &["alice"]), None);
        assert_eq!(first_range("alice:4294967295:65536\n", &["alice"]), None);
    }
}
