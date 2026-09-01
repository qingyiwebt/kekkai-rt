use anyhow::{bail, Context};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    os::unix::io::AsRawFd,
};

use crate::config::SandboxConfig;

pub(crate) fn id(cfg: &SandboxConfig) -> String {
    let source = cfg.managed_bundle_dir.to_string_lossy();
    let digest = Sha256::digest(source.as_bytes());
    let suffix = digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("kekkai-rt-{suffix}")
}

pub(crate) fn acquire_lock(cfg: &SandboxConfig) -> anyhow::Result<File> {
    fs::create_dir_all(&cfg.managed_bundle_dir).with_context(|| {
        format!(
            "create managed bundle directory {}",
            cfg.managed_bundle_dir.display()
        )
    })?;
    let path = cfg.managed_bundle_dir.join(".lock");
    let file = File::options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("open sandbox instance lock {}", path.display()))?;
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result != 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::WouldBlock {
            bail!(
                "sandbox instance is already running for {}",
                cfg.managed_bundle_dir.display()
            );
        }
        return Err(error).context("acquire sandbox instance lock");
    }
    Ok(file)
}
