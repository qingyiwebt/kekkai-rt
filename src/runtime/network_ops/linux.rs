use super::NetworkBackend;
use crate::config::NetworkSettings;
use anyhow::{anyhow, Context};
use async_trait::async_trait;
use futures_util::StreamExt;
use netlink_bindings::nftables::{
    self, CmpOps, CtKeys, MetaKeys, Nfgenmsg, PayloadBase, VerdictCode,
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
const NFT_TABLE: &[u8] = b"kekkai_rt";
const NFT_FORWARD_CHAIN: &[u8] = b"kekkai_rt_forward";
const NFT_POSTROUTING_CHAIN: &[u8] = b"kekkai_rt_postrouting";
const NFT_RULE_FORWARD_INGRESS: &[u8] = b"kekkai_rt:forward:ingress";
const NFT_RULE_FORWARD_REPLY: &[u8] = b"kekkai_rt:forward:reply";
const NFT_RULE_POSTROUTING_MASQUERADE: &[u8] = b"kekkai_rt:postrouting:masquerade";

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
            let (connection, handle, _) =
                new_connection().context("open NETLINK_ROUTE connection")?;
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
        let (connection, handle, _) = new_connection().context("open NETLINK_ROUTE connection")?;
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

async fn link_index(handle: &rtnetlink::Handle, name: &str) -> anyhow::Result<u32> {
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
                    .set(LinkUnspec::new_with_index(peer_index).name("eth0").build())
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
                    .add(eth0, IpAddr::V4(settings.address), settings.subnet.prefix)
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

fn with_namespace<T>(
    path: &str,
    operation: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let mut guard = NetworkNamespaceGuard::enter(path)?;
    let operation_result = operation();
    let restore_result = guard.restore();
    if let Err(restore_error) = restore_result {
        return match operation_result {
            Ok(_) => Err(restore_error).context("restore current network namespace"),
            Err(error) => Err(error).context(format!(
                "restore current network namespace: {restore_error}"
            )),
        };
    }
    operation_result
}

struct NetworkNamespaceGuard {
    original: File,
    restored: bool,
}

impl NetworkNamespaceGuard {
    fn enter(path: &str) -> anyhow::Result<Self> {
        let original = File::open("/proc/self/ns/net").context("open current network namespace")?;
        let namespace =
            File::open(path).with_context(|| format!("open network namespace {path}"))?;
        let result = unsafe { libc::setns(namespace.as_raw_fd(), libc::CLONE_NEWNET) };
        if result != 0 {
            return Err(io::Error::last_os_error()).context("enter network namespace");
        }

        Ok(Self {
            original,
            restored: false,
        })
    }

    fn restore(&mut self) -> io::Result<()> {
        if self.restored {
            return Ok(());
        }
        let result = unsafe { libc::setns(self.original.as_raw_fd(), libc::CLONE_NEWNET) };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        self.restored = true;
        Ok(())
    }
}

impl Drop for NetworkNamespaceGuard {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

fn install_nft_rules(settings: &NetworkSettings) -> anyhow::Result<()> {
    // A concurrent start can observe the table as absent while another start
    // is creating it. Re-read the kernel state after EEXIST/ERESTART instead
    // of surfacing a transient race as a sandbox startup failure.
    for attempt in 0..3 {
        match install_nft_rules_once(settings) {
            Ok(()) => return Ok(()),
            Err(error) if attempt < 2 && is_retryable_nft_error(&error) => {
                warn!(attempt = attempt + 1, error = %error, "retrying nftables setup after concurrent change");
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!("nftables retry loop always returns")
}

fn install_nft_rules_once(settings: &NetworkSettings) -> anyhow::Result<()> {
    let mut socket = NetlinkSocket::new();
    let header = Nfgenmsg {
        nfgen_family: libc::AF_INET as u8,
        version: 0,
        _res_id_be: 0,
    };
    let mut batch_header = Nfgenmsg::new();
    batch_header.set_res_id(10); // NFNL_SUBSYS_NFTABLES

    let table_exists = nft_table_exists(&mut socket, &header)?;
    let forward_exists = table_exists && nft_chain_exists(&mut socket, &header, NFT_FORWARD_CHAIN)?;
    let postrouting_exists =
        table_exists && nft_chain_exists(&mut socket, &header, NFT_POSTROUTING_CHAIN)?;
    let forward_rules = if forward_exists {
        nft_rule_state(&mut socket, &header, NFT_FORWARD_CHAIN)?
    } else {
        RuleState::default()
    };
    let postrouting_rules = if postrouting_exists {
        nft_rule_state(&mut socket, &header, NFT_POSTROUTING_CHAIN)?
    } else {
        RuleState::default()
    };

    let add_forward_ingress = should_add_rule(&forward_rules, 0);
    let add_forward_reply = should_add_rule(&forward_rules, 1);
    let add_postrouting_masquerade = should_add_rule(&postrouting_rules, 2);

    if table_exists
        && forward_exists
        && postrouting_exists
        && !add_forward_ingress
        && !add_forward_reply
        && !add_postrouting_masquerade
    {
        return Ok(());
    }

    let generation_id = latest_nft_generation_id(&mut socket)?;

    // nftables requires related changes to be enclosed in one netlink batch.
    let mut batch = nftables::Chained::new(socket.reserve_seq(8));
    batch
        .request()
        .op_batch_begin_do(&batch_header)
        .encode()
        .push_genid(generation_id);

    if !table_exists {
        let mut table = batch
            .request()
            .set_create()
            .set_excl()
            .op_newtable_do(&header);
        table.encode().push_name_bytes(NFT_TABLE);
    }

    if !forward_exists {
        let mut forward_chain = batch
            .request()
            .set_create()
            .set_excl()
            .op_newchain_do(&header);
        forward_chain
            .encode()
            .push_table_bytes(NFT_TABLE)
            .push_name_bytes(NFT_FORWARD_CHAIN)
            .push_type_bytes(b"filter")
            .nested_hook()
            .push_num(2)
            .push_priority(0)
            .end_nested()
            .push_policy(VerdictCode::Accept as u32);
    }

    if !postrouting_exists {
        let mut postrouting_chain = batch
            .request()
            .set_create()
            .set_excl()
            .op_newchain_do(&header);
        postrouting_chain
            .encode()
            .push_table_bytes(NFT_TABLE)
            .push_name_bytes(NFT_POSTROUTING_CHAIN)
            .push_type_bytes(b"nat")
            .nested_hook()
            .push_num(4)
            .push_priority(100)
            .end_nested()
            .push_policy(VerdictCode::Accept as u32);
    }

    let subnet = u32::from(settings.subnet.address).to_be_bytes();
    let mask = prefix_mask(settings.subnet.prefix).to_be_bytes();
    let bridge = nul_terminated(settings.bridge.as_bytes());

    if add_forward_ingress {
        add_rule(
            &mut batch,
            NFT_FORWARD_CHAIN,
            NFT_RULE_FORWARD_INGRESS,
            |mut exprs| {
                exprs = meta_cmp(exprs, MetaKeys::Iifname as u32, CmpOps::Eq as u32, &bridge);
                exprs = ip_cmp(exprs, 12, &subnet, &mask, CmpOps::Eq as u32);
                verdict(exprs, VerdictCode::Accept as u32)
            },
            &header,
        );
    }

    if add_forward_reply {
        add_rule(
            &mut batch,
            NFT_FORWARD_CHAIN,
            NFT_RULE_FORWARD_REPLY,
            |mut exprs| {
                exprs = meta_cmp(exprs, MetaKeys::Oifname as u32, CmpOps::Eq as u32, &bridge);
                exprs = ip_cmp(exprs, 16, &subnet, &mask, CmpOps::Eq as u32);
                exprs = ct_state(exprs);
                verdict(exprs, VerdictCode::Accept as u32)
            },
            &header,
        );
    }

    if add_postrouting_masquerade {
        add_rule(
            &mut batch,
            NFT_POSTROUTING_CHAIN,
            NFT_RULE_POSTROUTING_MASQUERADE,
            |mut exprs| {
                exprs = ip_cmp(exprs, 12, &subnet, &mask, CmpOps::Eq as u32);
                exprs = meta_cmp(exprs, MetaKeys::Oifname as u32, CmpOps::Neq as u32, &bridge);
                exprs.nested_elem().push_name(c"masq").end_nested()
            },
            &header,
        );
    }
    batch.request().op_batch_end_do(&batch_header);
    let batch = batch.finalize();
    let mut reply = socket
        .request_chained(&batch)
        .context("send Kekkai Runtime nftables transaction")?;
    while let Some(result) = reply.recv() {
        result.map_err(|error| {
            anyhow::Error::new(error).context("Kekkai Runtime nftables transaction failed")
        })?;
    }
    Ok(())
}

fn latest_nft_generation_id(socket: &mut NetlinkSocket) -> anyhow::Result<u32> {
    let request = nftables::Request::new().op_getgen_do(&Nfgenmsg::new());
    let mut reply = socket
        .request(&request)
        .context("request nftables generation ID")?;
    let (_, attrs) = reply
        .recv_one()
        .map_err(|error| anyhow::Error::new(error).context("read nftables generation ID"))?;
    attrs.get_id().context("nftables generation ID is missing")
}

fn nft_table_exists(socket: &mut NetlinkSocket, header: &Nfgenmsg) -> anyhow::Result<bool> {
    let mut request = nftables::Request::new().op_gettable_dump(header);
    {
        let _ = request.encode().push_name_bytes(NFT_TABLE);
    }
    let mut reply = socket
        .request(&request)
        .context("query Kekkai Runtime nftables table")?;
    while let Some(result) = reply.recv() {
        let (_, attrs) = result
            .map_err(|error| anyhow::Error::new(error).context("read Kekkai Runtime table"))?;
        if attrs
            .get_name()
            .map(|name| name.to_bytes() == NFT_TABLE)
            .unwrap_or(false)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn nft_chain_exists(
    socket: &mut NetlinkSocket,
    header: &Nfgenmsg,
    chain: &[u8],
) -> anyhow::Result<bool> {
    let mut request = nftables::Request::new().op_getchain_dump(header);
    {
        let _ = request.encode().push_table_bytes(NFT_TABLE);
    }
    let mut reply = socket
        .request(&request)
        .context("query Kekkai Runtime nftables chains")?;
    while let Some(result) = reply.recv() {
        let (_, attrs) = result
            .map_err(|error| anyhow::Error::new(error).context("read Kekkai Runtime chain"))?;
        if attrs
            .get_table()
            .map(|table| table.to_bytes() == NFT_TABLE)
            .unwrap_or(false)
            && attrs
                .get_name()
                .map(|name| name.to_bytes() == chain)
                .unwrap_or(false)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

#[derive(Default)]
struct RuleState {
    count: usize,
    managed: [bool; 3],
}

fn nft_rule_state(
    socket: &mut NetlinkSocket,
    header: &Nfgenmsg,
    chain: &[u8],
) -> anyhow::Result<RuleState> {
    let mut request = nftables::Request::new().op_getrule_dump(header);
    {
        let _ = request
            .encode()
            .push_table_bytes(NFT_TABLE)
            .push_chain_bytes(chain);
    }
    let mut reply = socket
        .request(&request)
        .context("query Kekkai Runtime nftables rules")?;
    let mut state = RuleState::default();
    while let Some(result) = reply.recv() {
        let (_, attrs) = result
            .map_err(|error| anyhow::Error::new(error).context("read Kekkai Runtime rule"))?;
        state.count += 1;
        if let Ok(userdata) = attrs.get_userdata() {
            if userdata == NFT_RULE_FORWARD_INGRESS {
                state.managed[0] = true;
            } else if userdata == NFT_RULE_FORWARD_REPLY {
                state.managed[1] = true;
            } else if userdata == NFT_RULE_POSTROUTING_MASQUERADE {
                state.managed[2] = true;
            }
        }
    }
    Ok(state)
}

fn should_add_rule(state: &RuleState, managed_index: usize) -> bool {
    if state.managed[managed_index] {
        return false;
    }

    // Rules created by versions before stable userdata was added are still
    // valid. Treat an already populated dedicated chain as owning its legacy
    // rule instead of appending duplicates on every restart.
    if state.managed.iter().all(|managed| !managed) {
        return match managed_index {
            // The legacy forward rules were appended in this order.
            0 => state.count == 0,
            1 => state.count < 2,
            2 => state.count == 0,
            _ => unreachable!(),
        };
    }

    let legacy_rule_count = match managed_index {
        0 | 1 => 2,
        2 => 1,
        _ => unreachable!(),
    };
    state.count < legacy_rule_count
}

fn is_retryable_nft_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<netlink_socket2::ReplyError>()
            .and_then(|reply| reply.as_io_error().raw_os_error())
            .is_some_and(|code| code == libc::EEXIST || code == libc::ERESTART)
    })
}

fn add_rule<F>(
    batch: &mut nftables::Chained<'static>,
    chain: &[u8],
    userdata: &[u8],
    build: F,
    header: &Nfgenmsg,
) where
    F: for<'a> FnOnce(
        nftables::PushExprListAttrs<nftables::PushRuleAttrs<&'a mut Vec<u8>>>,
    ) -> nftables::PushExprListAttrs<nftables::PushRuleAttrs<&'a mut Vec<u8>>>,
{
    let mut request = batch
        .request()
        .set_create()
        .set_append()
        .op_newrule_do(header);
    {
        let root = request
            .encode()
            .push_table_bytes(NFT_TABLE)
            .push_chain_bytes(chain);
        let expressions = build(root.nested_expressions());
        expressions.end_nested().push_userdata(userdata);
    }
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
        // nftables rejects overlapping source and destination registers for
        // bitwise expressions. Keep the payload in register 2 and write the
        // masked value to a separate register.
        .push_dreg(3)
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
        .push_sreg(3)
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
        .push_value(&6u32.to_be_bytes())
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
    // meta iifname/oifname always produce IFNAMSIZ bytes in the kernel. Use
    // the same fixed-width representation for the comparison value so the
    // rule does not depend on the interface name's current length.
    let mut result = vec![0; libc::IFNAMSIZ];
    let copy_len = value.len().min(libc::IFNAMSIZ.saturating_sub(1));
    result[..copy_len].copy_from_slice(&value[..copy_len]);
    result
}
