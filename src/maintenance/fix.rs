use super::{check, sysroot};
use crate::config::Config;
use anyhow::{bail, Context};
use std::path::Path;

pub(crate) async fn run(config_path: &Path) -> anyhow::Result<()> {
    let config = Config::load(config_path).context("load configuration")?;
    println!("Repairing {}", config_path.display());
    let changed = sysroot::fix_sysroot(&config.sandbox).context("repair sandbox directories")?;
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
