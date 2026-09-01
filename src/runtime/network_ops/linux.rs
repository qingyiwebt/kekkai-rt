use super::NetworkBackend;
use crate::config::NetworkSettings;
use anyhow::{anyhow, Context};
use async_trait::async_trait;
use futures_util::StreamExt;
use netlink_bindings::{
    nftables::{self, CmpOps, CtKeys, MetaKeys, Nfgenmsg, PayloadBase, VerdictCode},
    traits::NetlinkRequest,
};
use netlink_socket2::NetlinkSocket;
use rtnetlink::{
    new_connection, LinkBridge, LinkUnspec, LinkVeth, NetworkNamespace, RouteMessageBuilder,
};
use std::{
    fs::File,
    io,
    net::{IpAddr, Ipv4Addr},
    os::fd::AsRawFd,
    path::Path,
};
use tokio::task;
use tracing::{debug, info, warn};

pub(super) struct LinuxNetworkBackend;

const NETNS_PREFIX: &str = "/run/netns/";
const NFT_TABLE: &[u8] = b"agentcell";
const NFT_FORWARD_CHAIN: &[u8] = b"agentcell_forward";
const NFT_POSTROUTING_CHAIN: &[u8] = b"agentcell_postrouting";

#[async_trait]
impl NetworkBackend for LinuxNetworkBackend {
    async fn prepare_nat(
        &self,
        settings: &NetworkSettings,
        host_veth: &str,
        peer_veth: &str,
        netns_name: &str,
    ) -> anyhow::Result<()> {
        NetworkNamespace::add(netns_name)
            .await
            .map_err(|error| anyhow!("create sandbox network namespace: {error}"))?;

        let result = async {
            let (connection, handle, _) = new_connection()
                .context("open NETLINK_ROUTE connection")?;
            tokio::spawn(connection);

            handle
                .link()
                .add(LinkVeth::new(host_veth, peer_veth).build())
                .execute()
                .await
                .map_err(|error| anyhow!("create sandbox veth pair: {error}"))?;

            let bridge_index = link_index(&handle, &settings.bridge).await?;
            let host_index = link_index(&handle, host_veth).await?;
            handle
                .link()
                .set(
                    LinkUnspec::new_with_index(host_index)
                        .controller(bridge_index)
                        .up()
                        .build(),
                )
                .execute()
                .await
                .map_err(|error| anyhow!("attach sandbox veth to bridge: {error}"))?;

            let netns_path = format!("{NETNS_PREFIX}{netns_name}");
            let namespace = File::open(&netns_path)
                .with_context(|| format!("open network namespace {netns_path}"))?;
            handle
                .link()
                .set(
                    LinkUnspec::new_with_index(link_index(&handle, peer_veth).await?)
                        .setns_by_fd(namespace.as_raw_fd())
                        .build(),
                )
                .execute()
                .await
                .map_err(|error| anyhow!("move sandbox veth into network namespace: {error}"))?;

            configure_namespace(&netns_path, settings, peer_veth.to_owned()).await
        }
        .await;

        if let Err(error) = result {
            self.cleanup_resources(host_veth, netns_name).await;
            return Err(error);
        }
        Ok(())
    }

    async fn cleanup_resources(&self, host_veth: &str, netns_name: &str) {
        if let Ok((connection, handle, _)) = new_connection() {
            tokio::spawn(connection);
            match link_index(&handle, host_veth).await {
                Ok(index) => match handle.link().del(index).execute().await {
                    Ok(()) => info!(interface = host_veth, "removed sandbox host veth"),
                    Err(error) => warn!(error = %error, "could not remove sandbox host veth"),
                },
                Err(error) => debug!(error = %error, "sandbox host veth was already absent"),
            }
        }

        if Path::new(NETNS_PREFIX).join(netns_name).exists() {
            match NetworkNamespace::del(netns_name).await {
                Ok(()) => info!(namespace = netns_name, "removed sandbox network namespace"),
                Err(error) => warn!(error = %error, "could not remove sandbox network namespace"),
            }
        }
    }

    async fn ensure_bridge(&self, settings: &NetworkSettings) -> anyhow::Result<()> {
        let (connection, handle, _) =
            new_connection().context("open NETLINK_ROUTE connection")?;
        tokio::spawn(connection);
        let bridge_index = match link_index(&handle, &settings.bridge).await {
            Ok(index) => index,
            Err(_) => {
                handle
                    .link()
                    .add(LinkBridge::new(&settings.bridge).build())
                    .execute()
                    .await
                    .map_err(|error| anyhow!("create sandbox bridge: {error}"))?;
                link_index(&handle, &settings.bridge).await?
            }
        };
        handle
            .address()
            .add(
                bridge_index,
                IpAddr::V4(settings.gateway),
                settings.subnet.prefix,
            )
            .replace()
            .execute()
            .await
            .map_err(|error| anyhow!("assign sandbox bridge address: {error}"))?;
        handle
            .link()
            .set(LinkUnspec::new_with_index(bridge_index).up().build())
            .execute()
            .await
            .map_err(|error| anyhow!("activate sandbox bridge: {error}"))?;
        Ok(())
    }

    async fn ensure_nat_rules(&self, settings: &NetworkSettings) -> anyhow::Result<()> {
        let settings = settings.clone();
        task::spawn_blocking(move || install_nft_rules(&settings))
            .await
            .context("join nftables setup task")??;
        Ok(())
    }

    async fn configure_none(&self, pid: i32) -> anyhow::Result<()> {
        let path = format!("/proc/{pid}/ns/net");
        task::spawn_blocking(move || {
            with_namespace(&path, || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .context("build network namespace runtime")?;
                runtime.block_on(async {
                    let (connection, handle, _) =
                        new_connection().context("open NETLINK_ROUTE connection")?;
                    tokio::spawn(connection);
                    let lo = link_index(&handle, "lo").await?;
                    handle
                        .link()
                        .set(LinkUnspec::new_with_index(lo).up().build())
                        .execute()
                        .await
                        .map_err(|error| anyhow!("activate sandbox loopback: {error}"))
                })
            })
        })
        .await
        .context("join network namespace configuration task")??;
        Ok(())
    }
}

async fn link_index(
    handle: &rtnetlink::Handle,
    name: &str,
) -> anyhow::Result<u32> {
    handle
        .link()
        .get()
        .match_name(name)
        .execute()
        .next()
        .await
        .transpose()
        .map_err(|error| anyhow!("lookup interface {name}: {error}"))?
        .map(|message| message.header.index)
        .ok_or_else(|| anyhow!("interface {name} not found"))
}

async fn configure_namespace(
    path: &str,
    settings: &NetworkSettings,
    peer_name: String,
) -> anyhow::Result<()> {
    let path = path.to_owned();
    let settings = settings.clone();
    task::spawn_blocking(move || {
        with_namespace(&path, || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .context("build network namespace runtime")?;
            runtime.block_on(async move {
                let (connection, handle, _) =
                    new_connection().context("open namespace NETLINK_ROUTE connection")?;
                tokio::spawn(connection);
                let peer_index = link_index(&handle, &peer_name).await?;
                handle
                    .link()
                    .set(
                        LinkUnspec::new_with_index(peer_index)
                            .name("eth0")
                            .build(),
                    )
                    .execute()
                    .await
                    .map_err(|error| anyhow!("rename sandbox veth: {error}"))?;
                let loopback = link_index(&handle, "lo").await?;
                handle
                    .link()
                    .set(LinkUnspec::new_with_index(loopback).up().build())
                    .execute()
                    .await
                    .map_err(|error| anyhow!("activate sandbox loopback: {error}"))?;
                let eth0 = link_index(&handle, "eth0").await?;
                handle
                    .address()
                    .add(
                        eth0,
                        IpAddr::V4(settings.address),
                        settings.subnet.prefix,
                    )
                    .replace()
                    .execute()
                    .await
                    .map_err(|error| anyhow!("assign sandbox address: {error}"))?;
                handle
                    .link()
                    .set(LinkUnspec::new_with_index(eth0).up().build())
                    .execute()
                    .await
                    .map_err(|error| anyhow!("activate sandbox veth: {error}"))?;
                let route = RouteMessageBuilder::<Ipv4Addr>::new()
                    .output_interface(eth0)
                    .gateway(settings.gateway)
                    .destination_prefix(Ipv4Addr::UNSPECIFIED, 0)
                    .build();
                handle
                    .route()
                    .add(route)
                    .replace()
                    .execute()
                    .await
                    .map_err(|error| anyhow!("configure sandbox default route: {error}"))?;
                Ok(())
            })
        })
    })
    .await
    .context("join network namespace setup task")??;
    Ok(())
}

fn with_namespace<T>(path: &str, operation: impl FnOnce() -> anyhow::Result<T>) -> anyhow::Result<T> {
    let namespace = File::open(path).with_context(|| format!("open network namespace {path}"))?;
    let result = unsafe { libc::setns(namespace.as_raw_fd(), libc::CLONE_NEWNET) };
    if result != 0 {
        return Err(io::Error::last_os_error()).context("enter network namespace");
    }
    operation()
}

fn install_nft_rules(settings: &NetworkSettings) -> anyhow::Result<()> {
    let mut socket = NetlinkSocket::new();
    let header = Nfgenmsg {
        nfgen_family: libc::AF_INET as u8,
        version: 0,
        _res_id_be: 0,
    };

    let mut table = nftables::Request::new()
        .set_create()
        .set_excl()
        .op_newtable_do(&header);
    table.encode().push_name_bytes(NFT_TABLE);
    send_ack(&mut socket, &table, true)?;

    let mut forward_chain = nftables::Request::new()
        .set_create()
        .set_excl()
        .op_newchain_do(&header);
    forward_chain
        .encode()
        .push_table_bytes(NFT_TABLE)
        .push_name_bytes(NFT_FORWARD_CHAIN)
        .nested_hook()
        .push_num(2)
        .push_priority(0)
        .end_nested()
        .push_policy(VerdictCode::Accept as u32);
    send_ack(
        &mut socket,
        &forward_chain,
        true,
    )?;

    let mut postrouting_chain = nftables::Request::new()
        .set_create()
        .set_excl()
        .op_newchain_do(&header);
    postrouting_chain
        .encode()
        .push_table_bytes(NFT_TABLE)
        .push_name_bytes(NFT_POSTROUTING_CHAIN)
        .nested_hook()
        .push_num(4)
        .push_priority(100)
        .end_nested()
        .push_policy(VerdictCode::Accept as u32);
    send_ack(
        &mut socket,
        &postrouting_chain,
        true,
    )?;

    let subnet = u32::from(settings.subnet.address).to_be_bytes();
    let mask = prefix_mask(settings.subnet.prefix).to_be_bytes();
    let bridge = nul_terminated(settings.bridge.as_bytes());

    let ingress = rule_request(NFT_FORWARD_CHAIN, |mut exprs| {
        exprs = meta_cmp(exprs, MetaKeys::Iifname as u32, CmpOps::Eq as u32, &bridge);
        exprs = ip_cmp(exprs, 12, &subnet, &mask, CmpOps::Eq as u32);
        verdict(exprs, VerdictCode::Accept as u32)
    }, &header);
    send_ack(&mut socket, &ingress, false)?;

    let reply = rule_request(NFT_FORWARD_CHAIN, |mut exprs| {
        exprs = meta_cmp(exprs, MetaKeys::Oifname as u32, CmpOps::Eq as u32, &bridge);
        exprs = ip_cmp(exprs, 16, &subnet, &mask, CmpOps::Eq as u32);
        exprs = ct_state(exprs);
        verdict(exprs, VerdictCode::Accept as u32)
    }, &header);
    send_ack(&mut socket, &reply, false)?;

    let masquerade = rule_request(NFT_POSTROUTING_CHAIN, |mut exprs| {
        exprs = ip_cmp(exprs, 12, &subnet, &mask, CmpOps::Eq as u32);
        exprs = meta_cmp(exprs, MetaKeys::Oifname as u32, CmpOps::Neq as u32, &bridge);
        exprs
            .nested_elem()
            .push_name(c"masq")
            .end_nested()
    }, &header);
    send_ack(&mut socket, &masquerade, false)?;
    Ok(())
}

fn send_ack<R: NetlinkRequest>(
    socket: &mut NetlinkSocket,
    request: &R,
    ignore_exists: bool,
) -> anyhow::Result<()> {
    let mut reply = socket.request(request).context("send Netlink request")?;
    match reply.recv_ack() {
        Ok(()) => Ok(()),
        Err(error) if ignore_exists && error.to_string().contains("File exists") => Ok(()),
        Err(error) => Err(anyhow!("Netlink request failed: {error}")),
    }
}

fn rule_request<F>(
    chain: &[u8],
    build: F,
    header: &Nfgenmsg,
) -> nftables::OpNewruleDo<'static>
where
    F: for<'a> FnOnce(
        nftables::PushExprListAttrs<nftables::PushRuleAttrs<&'a mut Vec<u8>>>,
    ) -> nftables::PushExprListAttrs<nftables::PushRuleAttrs<&'a mut Vec<u8>>>,
{
    let mut request = nftables::Request::new().op_newrule_do(header);
    {
        let root = request
            .encode()
            .push_table_bytes(NFT_TABLE)
            .push_chain_bytes(chain);
        let expressions = build(root.nested_expressions());
        let _root = expressions.end_nested();
    }
    request
}

fn meta_cmp<'a>(
    exprs: nftables::PushExprListAttrs<nftables::PushRuleAttrs<&'a mut Vec<u8>>>,
    key: u32,
    op: u32,
    value: &[u8],
) -> nftables::PushExprListAttrs<nftables::PushRuleAttrs<&'a mut Vec<u8>>> {
    exprs
        .nested_elem()
        .nested_data_meta()
        .push_dreg(1)
        .push_key(key)
        .end_nested()
        .end_nested()
        .nested_elem()
        .nested_data_cmp()
        .push_sreg(1)
        .push_op(op)
        .nested_data()
        .push_value(value)
        .end_nested()
        .end_nested()
        .end_nested()
}

fn ip_cmp<'a>(
    exprs: nftables::PushExprListAttrs<nftables::PushRuleAttrs<&'a mut Vec<u8>>>,
    offset: u32,
    value: &[u8; 4],
    mask: &[u8; 4],
    op: u32,
) -> nftables::PushExprListAttrs<nftables::PushRuleAttrs<&'a mut Vec<u8>>> {
    exprs
        .nested_elem()
        .nested_data_payload()
        .push_dreg(2)
        .push_base(PayloadBase::NetworkHeader as u32)
        .push_offset(offset)
        .push_len(4)
        .end_nested()
        .end_nested()
        .nested_elem()
        .nested_data_bitwise()
        .push_sreg(2)
        .push_dreg(2)
        .push_len(4)
        .nested_mask()
        .push_value(mask)
        .end_nested()
        .nested_xor()
        .push_value(&[0; 4])
        .end_nested()
        .end_nested()
        .end_nested()
        .nested_elem()
        .nested_data_cmp()
        .push_sreg(2)
        .push_op(op)
        .nested_data()
        .push_value(value)
        .end_nested()
        .end_nested()
        .end_nested()
}

fn ct_state(
    exprs: nftables::PushExprListAttrs<nftables::PushRuleAttrs<&mut Vec<u8>>>,
) -> nftables::PushExprListAttrs<nftables::PushRuleAttrs<&mut Vec<u8>>> {
    exprs
        .nested_elem()
        .nested_data_ct()
        .push_dreg(3)
        .push_key(CtKeys::State as u32)
        .end_nested()
        .end_nested()
        .nested_elem()
        .nested_data_bitwise()
        .push_sreg(3)
        .push_dreg(3)
        .push_len(4)
        .nested_mask()
        .push_value(&6u32.to_ne_bytes())
        .end_nested()
        .nested_xor()
        .push_value(&[0; 4])
        .end_nested()
        .end_nested()
        .end_nested()
        .nested_elem()
        .nested_data_cmp()
        .push_sreg(3)
        .push_op(CmpOps::Neq as u32)
        .nested_data()
        .push_value(&[0; 4])
        .end_nested()
        .end_nested()
        .end_nested()
}

fn verdict(
    exprs: nftables::PushExprListAttrs<nftables::PushRuleAttrs<&mut Vec<u8>>>,
    code: u32,
) -> nftables::PushExprListAttrs<nftables::PushRuleAttrs<&mut Vec<u8>>> {
    exprs
        .nested_elem()
        .nested_data_immediate()
        .push_dreg(0)
        .nested_data()
        .nested_verdict()
        .push_code(code)
        .end_nested()
        .end_nested()
        .end_nested()
        .end_nested()
}

fn prefix_mask(prefix: u8) -> u32 {
    if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    }
}

fn nul_terminated(value: &[u8]) -> Vec<u8> {
    let mut result = value.to_vec();
    result.push(0);
    result
}
