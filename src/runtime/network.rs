use crate::config::{NetworkMode, NetworkSettings};
use anyhow::{bail, Context};
use std::{ffi::OsStr, process::Output};
use tokio::process::Command;

pub(super) async fn configure_network(settings: &NetworkSettings, pid: i32) -> anyhow::Result<()> {
    match settings.mode {
        NetworkMode::Host => {
            cleanup_host_veth().await;
            Ok(())
        }
        NetworkMode::None => {
            cleanup_host_veth().await;
            nsenter_ip(pid, ["link", "set", "lo", "up"]).await?;
            Ok(())
        }
        NetworkMode::Nat => configure_nat(settings, pid).await,
    }
}

async fn configure_nat(settings: &NetworkSettings, pid: i32) -> anyhow::Result<()> {
    ensure_bridge(settings).await?;
    ensure_nat_rules(settings).await?;

    let host_veth = "agentcellv";
    let _ = run_command("ip", &["link", "del", host_veth]).await;
    run_checked(
        "ip",
        &[
            "link", "add", host_veth, "type", "veth", "peer", "name", "eth0",
        ],
        "create sandbox veth pair",
    )
    .await?;
    if let Err(error) = configure_veth(settings, pid, host_veth).await {
        let _ = run_command("ip", &["link", "del", host_veth]).await;
        return Err(error);
    }
    Ok(())
}

async fn cleanup_host_veth() {
    let _ = run_command("ip", &["link", "del", "agentcellv"]).await;
}

async fn ensure_bridge(settings: &NetworkSettings) -> anyhow::Result<()> {
    if !run_command("ip", &["link", "show", "dev", &settings.bridge])
        .await
        .context("check sandbox bridge")?
        .status
        .success()
    {
        run_checked(
            "ip",
            &["link", "add", "name", &settings.bridge, "type", "bridge"],
            "create sandbox bridge",
        )
        .await?;
    }
    run_checked(
        "ip",
        &[
            "addr",
            "replace",
            &settings.subnet.address_with_prefix(settings.gateway),
            "dev",
            &settings.bridge,
        ],
        "assign sandbox bridge address",
    )
    .await?;
    run_checked(
        "ip",
        &["link", "set", "dev", &settings.bridge, "up"],
        "activate sandbox bridge",
    )
    .await
}

async fn ensure_nat_rules(settings: &NetworkSettings) -> anyhow::Result<()> {
    let subnet = settings.subnet.network_with_prefix();
    ensure_iptables_rule(
        "nat",
        "POSTROUTING",
        vec![
            "-s".into(),
            subnet.clone(),
            "!".into(),
            "-o".into(),
            settings.bridge.clone(),
            "-j".into(),
            "MASQUERADE".into(),
        ],
    )
    .await?;
    ensure_iptables_rule(
        "filter",
        "FORWARD",
        vec![
            "-i".into(),
            settings.bridge.clone(),
            "-s".into(),
            subnet.clone(),
            "-j".into(),
            "ACCEPT".into(),
        ],
    )
    .await?;
    ensure_iptables_rule(
        "filter",
        "FORWARD",
        vec![
            "-o".into(),
            settings.bridge.clone(),
            "-d".into(),
            subnet,
            "-m".into(),
            "conntrack".into(),
            "--ctstate".into(),
            "ESTABLISHED,RELATED".into(),
            "-j".into(),
            "ACCEPT".into(),
        ],
    )
    .await
}

async fn ensure_iptables_rule(table: &str, chain: &str, rule: Vec<String>) -> anyhow::Result<()> {
    let mut check = vec!["-t".into(), table.into(), "-C".into(), chain.into()];
    check.extend(rule.iter().cloned());
    if run_command(
        "iptables",
        &check.iter().map(String::as_str).collect::<Vec<_>>(),
    )
    .await?
    .status
    .success()
    {
        return Ok(());
    }
    let mut insert = vec![
        "-t".into(),
        table.into(),
        "-I".into(),
        chain.into(),
        "1".into(),
    ];
    insert.extend(rule);
    run_checked("iptables", &insert, "install sandbox NAT firewall rule").await
}

async fn configure_veth(
    settings: &NetworkSettings,
    pid: i32,
    host_veth: &str,
) -> anyhow::Result<()> {
    run_checked(
        "ip",
        &["link", "set", host_veth, "master", &settings.bridge],
        "attach sandbox veth to bridge",
    )
    .await?;
    run_checked(
        "ip",
        &["link", "set", host_veth, "up"],
        "activate sandbox host veth",
    )
    .await?;
    run_checked(
        "ip",
        &["link", "set", "eth0", "netns", &pid.to_string()],
        "move sandbox veth into network namespace",
    )
    .await?;

    nsenter_ip(pid, ["link", "set", "lo", "up"]).await?;
    nsenter_ip(
        pid,
        [
            "addr",
            "replace",
            &settings.subnet.address_with_prefix(settings.address),
            "dev",
            "eth0",
        ],
    )
    .await?;
    nsenter_ip(pid, ["link", "set", "eth0", "up"]).await?;
    nsenter_ip(
        pid,
        [
            "route",
            "replace",
            "default",
            "via",
            &settings.gateway.to_string(),
            "dev",
            "eth0",
        ],
    )
    .await?;

    if !settings.dns.is_empty() {
        let lines = settings
            .dns
            .iter()
            .map(|dns| format!("'nameserver {dns}'"))
            .collect::<Vec<_>>()
            .join(" ");
        let script = format!("printf '%s\\n' {lines} > /etc/resolv.conf");
        run_checked(
            "nsenter",
            &[
                "-t".into(),
                pid.to_string(),
                "-m".into(),
                "-n".into(),
                "/bin/sh".into(),
                "-c".into(),
                script,
            ],
            "configure sandbox DNS",
        )
        .await?;
    }
    Ok(())
}

async fn nsenter_ip<const N: usize>(pid: i32, args: [&str; N]) -> anyhow::Result<()> {
    let mut command_args = vec!["-t".into(), pid.to_string(), "-n".into(), "ip".into()];
    command_args.extend(args.into_iter().map(str::to_owned));
    run_checked(
        "nsenter",
        &command_args,
        "configure sandbox network namespace",
    )
    .await
}

async fn run_command(program: &str, args: &[&str]) -> anyhow::Result<Output> {
    Command::new(program)
        .args(args)
        .output()
        .await
        .with_context(|| format!("run {program}"))
}

async fn run_checked<I, S>(program: &str, args: I, context: &str) -> anyhow::Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = Command::new(program)
        .args(args)
        .output()
        .await
        .with_context(|| format!("{context}: execute {program}"))?;
    if !output.status.success() {
        bail!(
            "{context}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}
