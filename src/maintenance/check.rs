use super::sysroot;
use crate::{
    config::{Config, NetworkMode},
    runtime::Sandbox,
};
use anyhow::{bail, Context};
use std::path::Path;

pub(crate) async fn run(config_path: &Path) -> anyhow::Result<()> {
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

pub(crate) struct CheckReport {
    lines: Vec<String>,
    failures: usize,
}

impl CheckReport {
    pub(crate) fn print(&self) {
        for line in &self.lines {
            println!("{line}");
        }
    }

    pub(crate) fn failures(&self) -> usize {
        self.failures
    }
}

pub(crate) async fn inspect(config: &Config) -> CheckReport {
    let mut report = CheckReport {
        lines: vec![
            "[ok] configuration".into(),
            format!("[ok] rootfs {}", config.sandbox.rootfs_dir.display()),
        ],
        failures: 0,
    };

    let sysroot_issues = sysroot::sysroot_issues(&config.sandbox);
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

    match Sandbox::probe_program(&config.sandbox.backend).await {
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
            for program in ["ip", "nsenter", "iptables"] {
                match Sandbox::probe_program(program).await {
                    Ok(()) => report.lines.push(format!("[ok] NAT dependency {program}")),
                    Err(error) => {
                        report
                            .lines
                            .push(format!("[error] NAT dependency {program}: {error}"));
                        report.failures += 1;
                    }
                }
            }
        } else {
            report
                .lines
                .push(format!("[ok] network mode {}", settings.mode.as_str()));
        }
    }

    report
}
