use crate::{
    config::{self, Config, NetworkMode},
    runtime::Sandbox,
};
use anyhow::{bail, Context};
use std::path::Path;

pub(crate) async fn check(config_path: &Path) -> anyhow::Result<()> {
    let config = Config::load(config_path).context("load configuration")?;
    println!("Checking {}", config_path.display());
    let failures = check_loaded(&config).await;
    if failures > 0 {
        bail!("sandbox check failed with {failures} issue(s)");
    }
    println!("Sandbox check passed");
    Ok(())
}

pub(crate) async fn fix(config_path: &Path) -> anyhow::Result<()> {
    let config = Config::load(config_path).context("load configuration")?;
    println!("Repairing {}", config_path.display());
    let changed = config::fix_sysroot(&config.sandbox).context("repair sandbox directories")?;
    if changed.is_empty() {
        println!("No sandbox directories needed repair");
    } else {
        for path in changed {
            println!("created {}", path.display());
        }
    }

    let failures = check_loaded(&config).await;
    if failures > 0 {
        bail!("sandbox fix completed with {failures} unresolved issue(s)");
    }
    println!("Sandbox fix completed");
    Ok(())
}

async fn check_loaded(config: &Config) -> usize {
    let mut failures = 0;
    println!("[ok] configuration");
    println!("[ok] rootfs {}", config.sandbox.rootfs_dir.display());

    let sysroot_issues = config::sysroot_issues(&config.sandbox);
    if sysroot_issues.is_empty() {
        println!("[ok] sysroot mountpoints and /bin/sh");
    } else {
        for issue in sysroot_issues {
            println!("[error] sysroot: {issue}");
            failures += 1;
        }
    }

    match Sandbox::probe_program(&config.sandbox.backend).await {
        Ok(()) => println!("[ok] backend {}", config.sandbox.backend),
        Err(error) => {
            println!("[error] backend {}: {error}", config.sandbox.backend);
            failures += 1;
        }
    }

    if let Ok(settings) = config.sandbox.network_settings() {
        if matches!(settings.mode, NetworkMode::Nat) {
            for program in ["ip", "nsenter", "iptables"] {
                match Sandbox::probe_program(program).await {
                    Ok(()) => println!("[ok] NAT dependency {program}"),
                    Err(error) => {
                        println!("[error] NAT dependency {program}: {error}");
                        failures += 1;
                    }
                }
            }
        } else {
            println!("[ok] network mode {}", settings.mode.as_str());
        }
    }

    failures
}
