use std::{fs, os::unix::fs::PermissionsExt, process::Command};

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
pub(super) fn command_available(program: &str, version_flag: &str) -> bool {
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
