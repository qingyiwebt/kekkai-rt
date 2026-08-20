use anyhow::{bail, ensure};
use flate2::read::GzDecoder;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    cmp::Ordering,
    fs,
    io::{self, Read, Write},
    path::{Component, Path},
};
use tar::Archive;
use uuid::Uuid;

pub(crate) fn alpine_architecture() -> anyhow::Result<&'static str> {
    match std::env::consts::ARCH {
        "x86_64" => Ok("x86_64"),
        "x86" => Ok("x86"),
        "aarch64" => Ok("aarch64"),
        "arm" => Ok("armv7"),
        "riscv64" => Ok("riscv64"),
        "s390x" => Ok("s390x"),
        "powerpc64le" => Ok("ppc64le"),
        "loongarch64" => Ok("loongarch64"),
        architecture => bail!(
            "unsupported host architecture {architecture}; Alpine init has no matching minirootfs"
        ),
    }
}

pub(crate) fn validate_version(version: &str) -> anyhow::Result<()> {
    ensure!(
        !version.is_empty()
            && version
                .split('.')
                .all(|part| !part.is_empty()
                    && part.chars().all(|character| character.is_ascii_digit())),
        "invalid Alpine version {version:?}; expected a stable version such as 3.24.1"
    );
    Ok(())
}

pub(crate) fn latest_artifact_from_index(
    index: &str,
    architecture: &str,
) -> anyhow::Result<(String, String)> {
    let suffix = format!("-{architecture}.tar.gz");
    let mut candidates = index
        .split(|character: char| character.is_whitespace() || character == '"' || character == '\'')
        .filter_map(|token| {
            let version = token
                .strip_prefix("alpine-minirootfs-")?
                .strip_suffix(&suffix)?;
            if validate_version(version).is_err() {
                return None;
            }
            Some((version.to_owned(), token.to_owned()))
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| compare_versions(&left.0, &right.0));
    candidates.pop().ok_or_else(|| {
        anyhow::anyhow!("Alpine release index contains no stable minirootfs for {architecture}")
    })
}

fn compare_versions(left: &str, right: &str) -> Ordering {
    let left = left
        .split('.')
        .map(|part| part.parse::<u64>().unwrap_or(0))
        .collect::<Vec<_>>();
    let right = right
        .split('.')
        .map(|part| part.parse::<u64>().unwrap_or(0))
        .collect::<Vec<_>>();
    (0..left.len().max(right.len()))
        .map(|index| {
            (
                left.get(index).copied().unwrap_or(0),
                right.get(index).copied().unwrap_or(0),
            )
        })
        .find_map(|(left, right)| match left.cmp(&right) {
            Ordering::Equal => None,
            ordering => Some(ordering),
        })
        .unwrap_or(Ordering::Equal)
}

pub(crate) fn parse_checksum(content: &str) -> anyhow::Result<String> {
    let checksum = content
        .split_whitespace()
        .next()
        .ok_or_else(|| anyhow::anyhow!("checksum file is empty"))?;
    ensure!(
        checksum.len() == 64
            && checksum
                .chars()
                .all(|character| character.is_ascii_hexdigit()),
        "checksum file does not contain a SHA256 digest"
    );
    Ok(checksum.to_ascii_lowercase())
}

pub(crate) fn verify_sha256(path: &Path, expected: &str) -> anyhow::Result<()> {
    let mut file = fs::File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let length = file.read(&mut buffer)?;
        if length == 0 {
            break;
        }
        digest.update(&buffer[..length]);
    }
    let actual = format!("{:x}", digest.finalize());
    ensure!(
        actual == expected,
        "SHA256 mismatch: expected {expected}, got {actual}"
    );
    Ok(())
}

pub(crate) fn extract_archive(archive_path: &Path, destination: &Path) -> anyhow::Result<()> {
    let archive = fs::File::open(archive_path)?;
    let decoder = GzDecoder::new(archive);
    let mut archive = Archive::new(decoder);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        validate_archive_path(&path)?;
        entry.unpack_in(destination)?;
    }
    Ok(())
}

pub(crate) fn validate_archive_path(path: &Path) -> anyhow::Result<()> {
    ensure!(
        !path.is_absolute(),
        "archive contains absolute path {}",
        path.display()
    );
    ensure!(
        !path
            .components()
            .any(|component| matches!(component, Component::ParentDir)),
        "archive contains path traversal {}",
        path.display()
    );
    Ok(())
}

#[derive(Serialize)]
struct GeneratedConfig {
    api: GeneratedApiConfig,
    sandbox: GeneratedSandboxConfig,
}

#[derive(Serialize)]
struct GeneratedApiConfig {
    listen_addr: String,
    secret: String,
}

#[derive(Serialize)]
struct GeneratedSandboxConfig {
    rootfs_dir: String,
    workspace_dir: String,
    backend: String,
    max_timeout_seconds: u64,
    network_mode: String,
    network_bridge: String,
    network_subnet: String,
    network_gateway: String,
    network_ip: String,
    network_dns: Vec<String>,
}

pub(crate) fn generated_config(rootfs: &Path, workspace: &Path) -> anyhow::Result<String> {
    let config = GeneratedConfig {
        api: GeneratedApiConfig {
            listen_addr: "0.0.0.0:8080".into(),
            secret: Uuid::new_v4().as_simple().to_string(),
        },
        sandbox: GeneratedSandboxConfig {
            rootfs_dir: rootfs.to_string_lossy().into_owned(),
            workspace_dir: workspace.to_string_lossy().into_owned(),
            backend: "runsc".into(),
            max_timeout_seconds: 300,
            network_mode: "nat".into(),
            network_bridge: "agentcell0".into(),
            network_subnet: "10.200.0.0/24".into(),
            network_gateway: "10.200.0.1".into(),
            network_ip: "10.200.0.2".into(),
            network_dns: vec!["1.1.1.1".into(), "8.8.8.8".into()],
        },
    };
    Ok(format!("{}\n", toml::to_string_pretty(&config)?))
}

pub(crate) fn write_config_if_missing(path: &Path, content: &str) -> anyhow::Result<bool> {
    let Some(parent) = path.parent() else {
        bail!("configuration path has no parent: {}", path.display());
    };
    fs::create_dir_all(parent)?;
    let mut file = match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    if let Err(error) = file.write_all(content.as_bytes()) {
        let _ = fs::remove_file(path);
        return Err(error.into());
    }
    Ok(true)
}
