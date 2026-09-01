use std::fs;

pub(super) fn memory_controller_available() -> bool {
    if let Ok(controllers) = fs::read_to_string("/sys/fs/cgroup/cgroup.controllers") {
        return has_controller(&controllers, "memory");
    }
    fs::read_to_string("/proc/cgroups")
        .ok()
        .map(|content| v1_memory_controller_enabled(&content))
        .unwrap_or(false)
}
pub(super) fn has_controller(controllers: &str, wanted: &str) -> bool {
    controllers
        .split_whitespace()
        .any(|controller| controller == wanted)
}
pub(super) fn v1_memory_controller_enabled(cgroups: &str) -> bool {
    cgroups.lines().any(|line| {
        let mut fields = line.split_whitespace();
        let name = fields.next();
        let _hierarchy = fields.next();
        let _groups = fields.next();
        let enabled = fields.next();
        name == Some("memory") && enabled == Some("1")
    })
}
