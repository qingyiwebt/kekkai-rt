use anyhow::{bail, Context};
use serde::Deserialize;
use tokio::process::Command;
use tracing::{debug, info, warn};

#[derive(Debug, Deserialize)]
pub struct RuncState {
    pub pid: i32,
    pub status: String,
}

pub async fn read_state(runtime: &str, id: &str) -> anyhow::Result<Option<RuncState>> {
    let output = Command::new(runtime)
        .args(["state", id])
        .output()
        .await
        .with_context(|| format!("read {runtime} state for {id}"))?;
    if !output.status.success() {
        debug!(runtime, container_id = id, "container state is unavailable");
        return Ok(None);
    }

    let state: RuncState = serde_json::from_slice(&output.stdout)
        .with_context(|| format!("parse {runtime} state for {id}"))?;
    if state.status == "running" {
        Ok(Some(state))
    } else {
        debug!(
            runtime,
            container_id = id,
            status = %state.status,
            "container is not running yet"
        );
        Ok(None)
    }
}

pub async fn remove(runtime: &str, id: &str) -> anyhow::Result<()> {
    debug!(runtime, container_id = id, "removing container state");
    match Command::new(runtime)
        .args(["kill", id, "KILL"])
        .output()
        .await
    {
        Ok(output) if !output.status.success() => debug!(
            runtime,
            container_id = id,
            error = %String::from_utf8_lossy(&output.stderr).trim(),
            "container kill was not needed or failed"
        ),
        Err(error) => debug!(runtime, container_id = id, %error, "container kill command failed"),
        _ => {}
    }

    let output = Command::new(runtime)
        .args(["delete", "--force", id])
        .output()
        .await
        .with_context(|| format!("delete {runtime} container {id}"))?;
    if !output.status.success() && !is_missing_error(&output.stderr) {
        bail!(
            "delete {runtime} container {id}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn is_missing_error(stderr: &[u8]) -> bool {
    let details = String::from_utf8_lossy(stderr).to_ascii_lowercase();
    details.contains("does not exist")
        || details.contains("not found")
        || details.contains("no such container")
}

#[cfg(target_os = "linux")]
pub fn configure_parent_death_signal(command: &mut Command) {
    use std::io;

    // The parent-death signal is inherited by the runtime process only. The
    // foreground runtime then owns the container process and will terminate
    // it when the runtime itself is killed.
    unsafe {
        command.pre_exec(|| {
            let parent_pid = libc::getppid();
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::getppid() != parent_pid {
                libc::kill(libc::getpid(), libc::SIGKILL);
            }
            Ok(())
        });
    }
}

#[cfg(not(target_os = "linux"))]
pub fn configure_parent_death_signal(_command: &mut Command) {}

pub fn log_runtime_exit(runtime: &str, id: &str, status: std::process::ExitStatus) {
    if status.success() {
        info!(runtime, container_id = id, "foreground runtime exited");
    } else {
        warn!(
            runtime,
            container_id = id,
            status = ?status,
            "foreground runtime exited unexpectedly"
        );
    }
}
