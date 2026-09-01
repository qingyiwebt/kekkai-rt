use super::sysroot;
use crate::{
    config::{CgroupAction, CgroupMode, Config, NetworkMode},
    runtime::host::HostCapabilities,
    runtime::Sandbox,
};
use anyhow::{bail, Context};
use std::path::Path;

pub async fn run(config_path: &Path) -> anyhow::Result<()> {
    let config = Config::load(config_path).context("load configuration")?;
    println!("Checking {}", config_path.display());
    let report = inspect(&config).await;
    report.print();
    if report.failures > 0 {
        bail!("sandbox check failed with {} issue(s)", report.failures);
    }
    println!("Sandbox check passed");
    Ok(())
}

pub struct CheckReport {
    lines: Vec<String>,
    failures: usize,
}

impl CheckReport {
    pub fn print(&self) {
        for line in &self.lines {
            println!("{line}");
        }
    }

    pub fn failures(&self) -> usize {
        self.failures
    }
}

pub async fn inspect(config: &Config) -> CheckReport {
    let mut report = CheckReport {
        lines: vec![
            "[ok] configuration".into(),
            format!("[ok] rootfs {}", config.sandbox.rootfs_dir.display()),
        ],
        failures: 0,
    };
    let capabilities = HostCapabilities::detect();

    match config.features.resolve(&capabilities) {
        Ok(resolved) if matches!(resolved.cgroups, CgroupAction::Use) => {
            report.lines.push(format!(
                "[ok] cgroups enabled ({})",
                config.features.cgroups.as_str()
            ))
        }
        Ok(_) if matches!(config.features.cgroups, CgroupMode::Disabled) => report
            .lines
            .push("[ok] cgroups disabled by configuration".into()),
        Ok(_) => report.lines.push(
            "[warning] cgroups disabled: memory controller is unavailable (auto mode)".into(),
        ),
        Err(error) => {
            report.lines.push(format!("[error] cgroups: {error}"));
            report.failures += 1;
        }
    }

    let sysroot_issues = sysroot::sysroot_issues(&config.sandbox, &config.mounts);
    if sysroot_issues.is_empty() {
        report
            .lines
            .push("[ok] sysroot mountpoints and /bin/sh".into());
    } else {
        for issue in sysroot_issues {
            report.lines.push(format!("[error] sysroot: {issue}"));
            report.failures += 1;
        }
    }

    match Sandbox::probe_program(config.sandbox.backend.as_str()).await {
        Ok(()) => report
            .lines
            .push(format!("[ok] backend {}", config.sandbox.backend)),
        Err(error) => {
            report.lines.push(format!(
                "[error] backend {}: {error}",
                config.sandbox.backend
            ));
            report.failures += 1;
        }
    }

    if let Ok(settings) = config.sandbox.network_settings() {
        if matches!(settings.mode, NetworkMode::Nat) {
            if capabilities.nat_available() {
                report.lines.push("[ok] NAT network capabilities".into());
            } else {
                let reasons = capabilities.nat_unavailability_reasons().join(", ");
                report.lines.push(format!(
                    "[error] NAT network capabilities: unavailable {reasons}"
                ));
                report.failures += 1;
            }
        } else if matches!(settings.mode, NetworkMode::Host) {
            report
                .lines
                .push("[warning] network mode host: sandbox network isolation is reduced".into());
        } else {
            report
                .lines
                .push(format!("[ok] network mode {}", settings.mode.as_str()));
        }
    }

    report
}
