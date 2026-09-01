use crate::config::NetworkSettings;
use anyhow::bail;
use tracing::{debug, info, warn};

use super::command::{CommandExecutor, CommandOutput, CommandSpec};

pub(super) async fn prepare_nat(
    executor: &dyn CommandExecutor,
    settings: &NetworkSettings,
    host_veth: &str,
    peer_veth: &str,
    netns_name: &str,
) -> anyhow::Result<()> {
    run_checked(
        executor,
        "ip",
        &["netns", "add", netns_name],
        "create sandbox network namespace",
    )
    .await?;
    let result = async {
        run_checked(
            executor,
            "ip",
            &[
                "link", "add", host_veth, "type", "veth", "peer", "name", peer_veth,
            ],
            "create sandbox veth pair",
        )
        .await?;
        configure_veth(executor, settings, host_veth, peer_veth, netns_name).await
    }
    .await;
    if let Err(error) = result {
        cleanup_resources(executor, host_veth, netns_name).await;
        return Err(error);
    }
    Ok(())
}

pub(super) async fn cleanup_resources(
    executor: &dyn CommandExecutor,
    host_veth: &str,
    netns_name: &str,
) {
    match run_command(
        executor,
        "ip",
        ["link", "del", host_veth],
        "remove sandbox host veth",
    )
    .await
    {
        Ok(output) if output.success() => info!("removed sandbox host veth"),
        Ok(_) => debug!("sandbox host veth was already absent"),
        Err(error) => warn!(error = %error, "could not inspect sandbox host veth"),
    }
    match run_command(
        executor,
        "ip",
        ["netns", "del", netns_name],
        "remove sandbox network namespace",
    )
    .await
    {
        Ok(output) if output.success() => info!("removed sandbox network namespace"),
        Ok(_) => debug!("sandbox network namespace was already absent"),
        Err(error) => warn!(error = %error, "could not inspect sandbox network namespace"),
    }
}

pub(super) async fn ensure_bridge(
    executor: &dyn CommandExecutor,
    settings: &NetworkSettings,
) -> anyhow::Result<()> {
    debug!(bridge = %settings.bridge, "ensuring sandbox bridge");
    if !run_command(
        executor,
        "ip",
        ["link", "show", "dev", settings.bridge.as_str()],
        "check sandbox bridge",
    )
    .await?
    .success()
    {
        run_checked(
            executor,
            "ip",
            &["link", "add", "name", &settings.bridge, "type", "bridge"],
            "create sandbox bridge",
        )
        .await?;
    }
    run_checked(
        executor,
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
        executor,
        "ip",
        &["link", "set", "dev", &settings.bridge, "up"],
        "activate sandbox bridge",
    )
    .await
}

pub(super) async fn ensure_nat_rules(
    executor: &dyn CommandExecutor,
    settings: &NetworkSettings,
) -> anyhow::Result<()> {
    let subnet = settings.subnet.network_with_prefix();
    ensure_iptables_rule(
        executor,
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
        executor,
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
        executor,
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

async fn ensure_iptables_rule(
    executor: &dyn CommandExecutor,
    table: &str,
    chain: &str,
    rule: Vec<String>,
) -> anyhow::Result<()> {
    let mut check = vec!["-t".into(), table.into(), "-C".into(), chain.into()];
    check.extend(rule.iter().cloned());
    if run_command(
        executor,
        "iptables",
        check.iter().map(String::as_str),
        "check sandbox NAT firewall rule",
    )
    .await?
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
    run_checked(
        executor,
        "iptables",
        insert.iter().map(String::as_str),
        "install sandbox NAT firewall rule",
    )
    .await
}

async fn configure_veth(
    executor: &dyn CommandExecutor,
    settings: &NetworkSettings,
    host_veth: &str,
    peer_veth: &str,
    netns_name: &str,
) -> anyhow::Result<()> {
    run_checked(
        executor,
        "ip",
        &["link", "set", host_veth, "master", &settings.bridge],
        "attach sandbox veth to bridge",
    )
    .await?;
    run_checked(
        executor,
        "ip",
        &["link", "set", host_veth, "up"],
        "activate sandbox host veth",
    )
    .await?;
    run_checked(
        executor,
        "ip",
        &["link", "set", peer_veth, "netns", netns_name],
        "move sandbox veth into network namespace",
    )
    .await?;
    netns_ip(
        executor,
        netns_name,
        ["link", "set", peer_veth, "name", "eth0"],
    )
    .await?;
    netns_ip(executor, netns_name, ["link", "set", "lo", "up"]).await?;
    netns_ip(
        executor,
        netns_name,
        [
            "addr",
            "replace",
            &settings.subnet.address_with_prefix(settings.address),
            "dev",
            "eth0",
        ],
    )
    .await?;
    netns_ip(executor, netns_name, ["link", "set", "eth0", "up"]).await?;
    netns_ip(
        executor,
        netns_name,
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
    .await
}

pub(super) async fn configure_dns(
    executor: &dyn CommandExecutor,
    settings: &NetworkSettings,
    runtime: &str,
    container_id: &str,
) -> anyhow::Result<()> {
    if !settings.dns.is_empty() {
        let lines = settings
            .dns
            .iter()
            .map(|dns| format!("'nameserver {dns}'"))
            .collect::<Vec<_>>()
            .join(" ");
        let script = format!("printf '%s\\n' {lines} > /etc/resolv.conf");
        run_checked(
            executor,
            runtime,
            &[
                "exec".into(),
                container_id.to_owned(),
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

async fn netns_ip<const N: usize>(
    executor: &dyn CommandExecutor,
    netns_name: &str,
    args: [&str; N],
) -> anyhow::Result<()> {
    let mut command_args = vec![
        "netns".into(),
        "exec".into(),
        netns_name.into(),
        "ip".into(),
    ];
    command_args.extend(args.into_iter().map(str::to_owned));
    run_checked(
        executor,
        "ip",
        command_args.iter().map(String::as_str),
        "configure sandbox network namespace",
    )
    .await
}

pub(super) async fn nsenter_ip<const N: usize>(
    executor: &dyn CommandExecutor,
    pid: i32,
    args: [&str; N],
) -> anyhow::Result<()> {
    let mut command_args = vec!["-t".into(), pid.to_string(), "-n".into(), "ip".into()];
    command_args.extend(args.into_iter().map(str::to_owned));
    run_checked(
        executor,
        "nsenter",
        &command_args,
        "configure sandbox network namespace",
    )
    .await
}

async fn run_command<I, S>(
    executor: &dyn CommandExecutor,
    program: impl Into<String>,
    args: I,
    context: &'static str,
) -> anyhow::Result<CommandOutput>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    executor
        .execute(&CommandSpec::new(
            program,
            args.into_iter().map(|arg| arg.as_ref().to_owned()),
            context,
        ))
        .await
}

async fn run_checked<I, S>(
    executor: &dyn CommandExecutor,
    program: &str,
    args: I,
    context: &'static str,
) -> anyhow::Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let output = run_command(executor, program, args, context).await?;
    if !output.success() {
        bail!("{context}: {}", output.stderr());
    }
    Ok(())
}

pub(super) fn veth_names(instance_id: &str) -> (String, String) {
    let suffix = &instance_id[instance_id.len().saturating_sub(10)..];
    (format!("kc-v-{suffix}"), format!("kc-p-{suffix}"))
}
