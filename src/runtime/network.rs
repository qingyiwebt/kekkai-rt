use crate::config::{NetworkMode, NetworkSettings};
use anyhow::{bail, Context};
use std::{ffi::OsStr, process::Output};
use tokio::process::Command;
use tracing::{debug, info, warn};

const HOST_VETH: &str = "kekkai-rtv";
const NETNS_NAME: &str = "kekkai-rtns";
const NETNS_PATH: &str = "/run/netns/kekkai-rtns";

pub(super) struct NetworkSession {
    namespace_path: Option<String>,
}

impl NetworkSession {
    pub(super) async fn prepare(settings: &NetworkSettings) -> anyhow::Result<Self> {
        cleanup_session().await;
        if !matches!(settings.mode, NetworkMode::Nat) {
            return Ok(Self {
                namespace_path: None,
            });
        }

        ensure_bridge(settings).await?;
        ensure_nat_rules(settings).await?;
        prepare_nat(settings).await?;
        Ok(Self {
            namespace_path: Some(NETNS_PATH.into()),
        })
    }

    pub(super) fn namespace_path(&self) -> Option<&str> {
        self.namespace_path.as_deref()
    }

    pub(super) async fn configure(
        &self,
        settings: &NetworkSettings,
        pid: i32,
        runtime: &str,
        container_id: &str,
    ) -> anyhow::Result<()> {
        info!(mode = %settings.mode.as_str(), pid, "configuring sandbox network");
        match settings.mode {
            NetworkMode::Host => Ok(()),
            NetworkMode::None => nsenter_ip(pid, ["link", "set", "lo", "up"]).await,
            NetworkMode::Nat => configure_dns(settings, runtime, container_id).await,
        }
    }

    pub(super) async fn cleanup(self) -> anyhow::Result<()> {
        cleanup_session().await;
        Ok(())
    }
}

pub(super) async fn prepare_network(settings: &NetworkSettings) -> anyhow::Result<NetworkSession> {
    NetworkSession::prepare(settings).await
}

pub(super) async fn configure_network(
    session: &NetworkSession,
    settings: &NetworkSettings,
    pid: i32,
    runtime: &str,
    container_id: &str,
) -> anyhow::Result<()> {
    session
        .configure(settings, pid, runtime, container_id)
        .await
}

/*
 * Keep the old free-function cleanup entry point for startup recovery and
 * maintenance of a stale session left by an interrupted process.
 */
async fn prepare_nat(settings: &NetworkSettings) -> anyhow::Result<()> {
    run_checked(
        "ip",
        &["netns", "add", NETNS_NAME],
        "create sandbox network namespace",
    )
    .await?;
    let result = async {
        run_checked(
            "ip",
            &[
                "link", "add", HOST_VETH, "type", "veth", "peer", "name", "eth0",
            ],
            "create sandbox veth pair",
        )
        .await?;
        configure_veth(settings).await
    }
    .await;
    if let Err(error) = result {
        cleanup_session().await;
        return Err(error);
    }
    Ok(())
}

pub(super) async fn cleanup_session() {
    match run_command("ip", &["link", "del", HOST_VETH]).await {
        Ok(output) if output.status.success() => {
            info!("removed sandbox host veth");
        }
        Ok(_) => {
            debug!("sandbox host veth was already absent");
        }
        Err(error) => {
            warn!(error = %error, "could not inspect sandbox host veth");
        }
    }
    match run_command("ip", &["netns", "del", NETNS_NAME]).await {
        Ok(output) if output.status.success() => {
            info!("removed sandbox network namespace");
        }
        Ok(_) => {
            debug!("sandbox network namespace was already absent");
        }
        Err(error) => {
            warn!(error = %error, "could not inspect sandbox network namespace");
        }
    }
}

async fn ensure_bridge(settings: &NetworkSettings) -> anyhow::Result<()> {
    debug!(bridge = %settings.bridge, "ensuring sandbox bridge");
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

async fn configure_veth(settings: &NetworkSettings) -> anyhow::Result<()> {
    run_checked(
        "ip",
        &["link", "set", HOST_VETH, "master", &settings.bridge],
        "attach sandbox veth to bridge",
    )
    .await?;
    run_checked(
        "ip",
        &["link", "set", HOST_VETH, "up"],
        "activate sandbox host veth",
    )
    .await?;
    run_checked(
        "ip",
        &["link", "set", "eth0", "netns", NETNS_NAME],
        "move sandbox veth into network namespace",
    )
    .await?;

    netns_ip(["link", "set", "lo", "up"]).await?;
    netns_ip([
        "addr",
        "replace",
        &settings.subnet.address_with_prefix(settings.address),
        "dev",
        "eth0",
    ])
    .await?;
    netns_ip(["link", "set", "eth0", "up"]).await?;
    netns_ip([
        "route",
        "replace",
        "default",
        "via",
        &settings.gateway.to_string(),
        "dev",
        "eth0",
    ])
    .await?;
    Ok(())
}

async fn configure_dns(
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
        // runsc's mount namespace is intentionally empty: the container rootfs
        // is served by its gofer process and is not available to host-side
        // nsenter. Execute the write inside the container instead.
        run_checked(
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

async fn netns_ip<const N: usize>(args: [&str; N]) -> anyhow::Result<()> {
    let mut command_args = vec![
        "netns".into(),
        "exec".into(),
        NETNS_NAME.into(),
        "ip".into(),
    ];
    command_args.extend(args.into_iter().map(str::to_owned));
    run_checked("ip", &command_args, "configure sandbox network namespace").await
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
    debug!(program, args = ?args, "running host network command");
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
    debug!(program, context, "running checked host command");
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
