use crate::config::{CgroupAction, NetworkMode};
use std::path::Path;

use super::process::RuntimePlan;

pub(crate) fn run_args(plan: &RuntimePlan, bundle_dir: &Path, container_id: &str) -> Vec<String> {
    let mut args = Vec::new();
    if plan.backend.is_runsc() {
        if plan.allow_host_uds {
            args.push("--host-uds=open".into());
        }
        if matches!(plan.cgroups, CgroupAction::Ignore) {
            args.push("--ignore-cgroups".into());
        }
        match plan.network_mode {
            NetworkMode::Host => args.push("--network=host".into()),
            NetworkMode::None => args.push("--network=none".into()),
            NetworkMode::Nat => {}
        }
        if plan.persist_rootfs {
            args.push("--overlay2=none".into());
        }
    }
    args.extend([
        "run".into(),
        "--bundle".into(),
        bundle_dir.to_string_lossy().into_owned(),
        container_id.into(),
    ]);
    args
}

pub(crate) fn probe_args(program: &str) -> &'static [&'static str] {
    if program == "ip" {
        &["-V"]
    } else {
        &["--version"]
    }
}
