use super::{check, sysroot};
use crate::{config::Config, runtime::host::HostCapabilities};
use anyhow::{bail, Context};
use std::path::Path;

pub async fn run(config_path: &Path) -> anyhow::Result<()> {
    let config = Config::load(config_path).context("load configuration")?;
    println!("Repairing {}", config_path.display());
    let capabilities = HostCapabilities::detect();
    let features = config
        .features
        .resolve(&capabilities)
        .map_err(|error| anyhow::anyhow!("resolve runtime features: {error}"))?;
    let changed = sysroot::fix_sysroot_with_identity(
        &config.sandbox,
        &config.mounts,
        features.user_namespace,
    )
    .context("repair sandbox directories")?;
    if changed.is_empty() {
        println!("No sandbox directories needed repair");
    } else {
        for path in changed {
            println!("created {}", path.display());
        }
    }

    let report = check::inspect(&config).await;
    report.print();
    if report.failures() > 0 {
        bail!(
            "sandbox fix completed with {} unresolved issue(s)",
            report.failures()
        );
    }
    println!("Sandbox fix completed");
    Ok(())
}
