use std::fs;

pub(super) fn net_admin_available() -> bool {
    if !cfg!(target_os = "linux") {
        return false;
    }
    let Ok(status) = fs::read_to_string("/proc/self/status") else {
        return false;
    };
    has_cap_net_admin(&status)
}
pub(super) fn has_cap_net_admin(status: &str) -> bool {
    capability_available(status, 12)
}
pub(super) fn capability_available(status: &str, bit: u8) -> bool {
    let Some(value) = status
        .lines()
        .find_map(|line| line.strip_prefix("CapEff:").map(str::trim))
    else {
        return false;
    };
    u64::from_str_radix(value, 16)
        .map(|effective| effective & (1u64 << bit) != 0)
        .unwrap_or(false)
}
#[cfg(target_os = "linux")]
fn netlink_available(protocol: i32) -> bool {
    let fd = unsafe {
        libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_RAW | libc::SOCK_CLOEXEC,
            protocol,
        )
    };
    if fd < 0 {
        return false;
    }
    (unsafe { libc::close(fd) }) == 0
}

#[cfg(target_os = "linux")]
pub(super) fn route_netlink_available() -> bool {
    netlink_available(libc::NETLINK_ROUTE)
}

#[cfg(not(target_os = "linux"))]
pub(super) fn route_netlink_available() -> bool {
    false
}

#[cfg(target_os = "linux")]
pub(super) fn netfilter_netlink_available() -> bool {
    netlink_available(libc::NETLINK_NETFILTER)
}

#[cfg(not(target_os = "linux"))]
pub(super) fn netfilter_netlink_available() -> bool {
    false
}
