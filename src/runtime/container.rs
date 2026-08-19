use super::CONFIG_HASH_ANNOTATION;
use crate::config::NetworkMode;
use anyhow::{bail, Context};
use serde::Deserialize;
use std::collections::HashMap;
use tokio::process::Command;

#[derive(Debug, Deserialize)]
pub(super) struct RuncState {
    pub(super) pid: i32,
    pub(super) status: String,
    #[serde(default)]
    pub(super) annotations: HashMap<String, String>,
}

pub(super) async fn read_state(runtime: &str, id: &str) -> anyhow::Result<Option<RuncState>> {
    let output = Command::new(runtime)
        .args(["state", id])
        .output()
        .await
        .with_context(|| format!("read {runtime} state for {id}"))?;
    if !output.status.success() {
        return Ok(None);
    }
    let state: RuncState = serde_json::from_slice(&output.stdout)
        .with_context(|| format!("parse {runtime} state for {id}"))?;
    if state.status == "running" {
        Ok(Some(state))
    } else {
        let _ = recreate_container(runtime, id).await;
        Ok(None)
    }
}

pub(super) async fn recreate_container(runtime: &str, id: &str) -> anyhow::Result<()> {
    let _ = Command::new(runtime)
        .args(["kill", id, "KILL"])
        .status()
        .await;
    let output = Command::new(runtime)
        .args(["delete", "-f", id])
        .output()
        .await
        .with_context(|| format!("delete {runtime} container {id}"))?;
    if !output.status.success()
        && !String::from_utf8_lossy(&output.stderr).contains("does not exist")
    {
        bail!(
            "delete {runtime} container {id}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

pub(super) fn container_config_matches(
    state: &RuncState,
    mode: &NetworkMode,
    config_hash: &str,
) -> anyhow::Result<bool> {
    let container_namespace = std::fs::read_link(format!("/proc/{}/ns/net", state.pid))
        .with_context(|| format!("inspect network namespace for container pid {}", state.pid))?;
    let host_namespace =
        std::fs::read_link("/proc/self/ns/net").context("inspect host network namespace")?;
    let host_network = container_namespace == host_namespace;
    Ok(state
        .annotations
        .get(CONFIG_HASH_ANNOTATION)
        .map(String::as_str)
        == Some(config_hash)
        && match mode {
            NetworkMode::Host => host_network,
            NetworkMode::Nat | NetworkMode::None => {
                !host_network
                    && state
                        .annotations
                        .get("io.agentcell.network-mode")
                        .map(String::as_str)
                        == Some(mode.as_str())
            }
        })
}
