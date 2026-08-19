use crate::config;
use anyhow::{bail, ensure, Context};
use flate2::read::GzDecoder;
use futures_util::StreamExt;
use reqwest::Client;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    cmp::Ordering,
    fs,
    io::{self, Read, Write},
    path::{Component, Path},
    time::Duration,
};
use tar::Archive;
use tokio::{fs as async_fs, io::AsyncWriteExt};
use uuid::Uuid;

const ALPINE_BASE_URL: &str = "https://dl-cdn.alpinelinux.org/alpine/latest-stable";

#[derive(Clone)]
struct AlpineRepository {
    client: Client,
    base_url: String,
}

impl AlpineRepository {
    fn official() -> anyhow::Result<Self> {
        Self::new(ALPINE_BASE_URL)
    }

    fn new(base_url: impl Into<String>) -> anyhow::Result<Self> {
        let client = Client::builder()
            .user_agent(concat!("agent-cell/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(20))
            .timeout(Duration::from_secs(300))
            .build()
            .context("create Alpine download client")?;
        Ok(Self {
            client,
            base_url: base_url.into().trim_end_matches('/').to_owned(),
        })
    }

    async fn download_and_extract(
        &self,
        destination: &Path,
        requested_version: Option<&str>,
    ) -> anyhow::Result<String> {
        let architecture = alpine_architecture()?;
        let (version, filename) = match requested_version {
            Some(version) => {
                validate_version(version)?;
                (
                    version.to_owned(),
                    format!("alpine-minirootfs-{version}-{architecture}.tar.gz"),
                )
            }
            None => self.latest_artifact(architecture).await?,
        };
        let release_url = format!("{}/releases/{architecture}/{filename}", self.base_url);
        let checksum_url = format!("{release_url}.sha256");
        let archive_path = destination.join(".alpine-minirootfs.tar.gz");

        self.download_file(&release_url, &archive_path).await?;
        let expected_hash = self.download_checksum(&checksum_url).await?;
        verify_sha256(&archive_path, &expected_hash)
            .with_context(|| format!("verify {filename} SHA256"))?;
        extract_archive(&archive_path, destination)
            .with_context(|| format!("extract {filename}"))?;
        fs::remove_file(&archive_path).context("remove downloaded Alpine archive")?;

        Ok(version)
    }

    async fn latest_artifact(&self, architecture: &str) -> anyhow::Result<(String, String)> {
        let index_url = format!("{}/releases/{architecture}/", self.base_url);
        let index = self
            .client
            .get(&index_url)
            .send()
            .await
            .with_context(|| format!("download Alpine release index {index_url}"))?
            .error_for_status()
            .with_context(|| format!("download Alpine release index {index_url}"))?
            .text()
            .await
            .context("read Alpine release index")?;
        latest_artifact_from_index(&index, architecture)
    }

    async fn download_checksum(&self, url: &str) -> anyhow::Result<String> {
        let text = self
            .client
            .get(url)
            .send()
            .await
            .with_context(|| format!("download checksum {url}"))?
            .error_for_status()
            .with_context(|| format!("download checksum {url}"))?
            .text()
            .await
            .context("read Alpine checksum")?;
        parse_checksum(&text)
    }

    async fn download_file(&self, url: &str, path: &Path) -> anyhow::Result<()> {
        let response = self
            .client
            .get(url)
            .send()
            .await
            .with_context(|| format!("download {url}"))?
            .error_for_status()
            .with_context(|| format!("download {url}"))?;
        let mut stream = response.bytes_stream();
        let mut file = async_fs::File::create(path)
            .await
            .with_context(|| format!("create download file {}", path.display()))?;
        while let Some(chunk) = stream.next().await {
            file.write_all(&chunk.context("read Alpine download")?)
                .await
                .context("write Alpine download")?;
        }
        file.flush().await.context("flush Alpine download")?;
        Ok(())
    }
}

pub(crate) async fn run(config_path: &Path, requested_version: Option<&str>) -> anyhow::Result<()> {
    let working_dir = fs::canonicalize(".").context("resolve current directory")?;
    let repository = AlpineRepository::official()?;
    run_in(&working_dir, config_path, requested_version, &repository).await
}

async fn run_in(
    working_dir: &Path,
    config_path: &Path,
    requested_version: Option<&str>,
    repository: &AlpineRepository,
) -> anyhow::Result<()> {
    let rootfs = working_dir.join("sysroot");
    let workspace = working_dir.join("workspace");
    match fs::symlink_metadata(&rootfs) {
        Ok(_) => bail!(
            "refusing to initialize existing sysroot {}; remove it manually if you want to start over",
            rootfs.display()
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("inspect sysroot {}", rootfs.display())),
    }

    let staging = working_dir.join(format!(".agent-cell-sysroot-{}", Uuid::new_v4()));
    fs::create_dir(&staging)
        .with_context(|| format!("create staging directory {}", staging.display()))?;

    let result = async {
        let version = repository
            .download_and_extract(&staging, requested_version)
            .await?;
        config::prepare_sysroot(&staging, Some(&workspace))
            .with_context(|| format!("prepare Alpine sysroot {}", staging.display()))?;
        fs::rename(&staging, &rootfs)
            .with_context(|| format!("install sysroot {}", rootfs.display()))?;

        let config_created = if config_path.exists() {
            false
        } else {
            let content = generated_config(&rootfs, &workspace)?;
            write_config_if_missing(config_path, &content)?
        };

        Ok::<_, anyhow::Error>((version, config_created))
    }
    .await;

    if result.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    let (version, config_created) = result?;

    println!("initialized Alpine {version} in {}", rootfs.display());
    if config_created {
        println!("created {}", config_path.display());
    } else {
        println!("kept existing configuration {}", config_path.display());
    }
    Ok(())
}

fn alpine_architecture() -> anyhow::Result<&'static str> {
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

fn validate_version(version: &str) -> anyhow::Result<()> {
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

fn latest_artifact_from_index(index: &str, architecture: &str) -> anyhow::Result<(String, String)> {
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

fn parse_checksum(content: &str) -> anyhow::Result<String> {
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

fn verify_sha256(path: &Path, expected: &str) -> anyhow::Result<()> {
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

fn extract_archive(archive_path: &Path, destination: &Path) -> anyhow::Result<()> {
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

fn validate_archive_path(path: &Path) -> anyhow::Result<()> {
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

fn generated_config(rootfs: &Path, workspace: &Path) -> anyhow::Result<String> {
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

fn write_config_if_missing(path: &Path, content: &str) -> anyhow::Result<bool> {
    let Some(parent) = path.parent() else {
        bail!("configuration path has no parent: {}", path.display());
    };
    fs::create_dir_all(parent)
        .with_context(|| format!("create configuration directory {}", parent.display()))?;
    let mut file = match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => return Ok(false),
        Err(error) => {
            return Err(error).with_context(|| format!("create config {}", path.display()))
        }
    };
    if let Err(error) = file.write_all(content.as_bytes()) {
        let _ = fs::remove_file(path);
        return Err(error).with_context(|| format!("write config {}", path.display()));
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, extract::State, http::Uri, response::Response, routing::get, Router};
    use std::sync::Arc;
    use tempfile::tempdir;
    use tokio::net::TcpListener;

    #[test]
    fn latest_index_ignores_release_candidates() {
        let index = r#"
            alpine-minirootfs-3.23.2-x86_64.tar.gz
            alpine-minirootfs-3.24.0-x86_64.tar.gz
            alpine-minirootfs-3.24.1-x86_64.tar.gz
            alpine-minirootfs-3.25.0_rc1-x86_64.tar.gz
        "#;
        assert_eq!(
            latest_artifact_from_index(index, "x86_64").unwrap(),
            (
                "3.24.1".into(),
                "alpine-minirootfs-3.24.1-x86_64.tar.gz".into()
            )
        );
    }

    #[test]
    fn archive_paths_must_stay_inside_destination() {
        assert!(validate_archive_path(Path::new("../outside")).is_err());
        assert!(validate_archive_path(Path::new("/outside")).is_err());
        assert!(validate_archive_path(Path::new("bin/sh")).is_ok());
    }

    #[tokio::test]
    async fn init_downloads_verifies_extracts_and_generates_config() {
        let temp = tempdir().unwrap();
        let archive = test_archive();
        let checksum = format!(
            "{:x}  alpine-minirootfs-3.24.1-{}.tar.gz\n",
            Sha256::digest(&archive),
            alpine_architecture().unwrap()
        );
        let (base_url, server) = test_server(archive, checksum).await;
        let repository = AlpineRepository::new(base_url).unwrap();
        let config_path = temp.path().join("config.toml");

        run_in(temp.path(), &config_path, Some("3.24.1"), &repository)
            .await
            .unwrap();
        server.abort();
        server.await.unwrap_err();

        let config = crate::config::Config::load(&config_path).unwrap();
        assert!(config_path.is_file());
        assert!(config.sandbox.rootfs_dir.join("bin/sh").is_file());
        assert!(crate::config::sysroot_issues(&config.sandbox).is_empty());
        assert!(!config.api.secret.is_empty());
    }

    #[tokio::test]
    async fn init_does_not_overwrite_existing_sysroot_or_config() {
        let temp = tempdir().unwrap();
        let rootfs = temp.path().join("sysroot");
        fs::create_dir(&rootfs).unwrap();
        let config_path = temp.path().join("config.toml");
        fs::write(&config_path, "existing configuration\n").unwrap();
        let repository = AlpineRepository::new("http://127.0.0.1:1").unwrap();

        let error = run_in(temp.path(), &config_path, Some("3.24.1"), &repository)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("existing sysroot"));
        assert_eq!(
            fs::read_to_string(config_path).unwrap(),
            "existing configuration\n"
        );
    }

    #[tokio::test]
    async fn checksum_failure_leaves_no_sysroot_or_staging_directory() {
        let temp = tempdir().unwrap();
        let archive = test_archive();
        let checksum = format!("{}  archive.tar.gz\n", "0".repeat(64));
        let (base_url, server) = test_server(archive, checksum).await;
        let repository = AlpineRepository::new(base_url).unwrap();
        let config_path = temp.path().join("config.toml");

        assert!(
            run_in(temp.path(), &config_path, Some("3.24.1"), &repository)
                .await
                .is_err()
        );
        server.abort();
        server.await.unwrap_err();
        assert!(!temp.path().join("sysroot").exists());
        assert!(!temp.path().read_dir().unwrap().any(|entry| entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".agent-cell-sysroot-")));
    }

    async fn test_server(
        archive: Vec<u8>,
        checksum: String,
    ) -> (String, tokio::task::JoinHandle<()>) {
        #[derive(Clone)]
        struct StateData {
            archive: Arc<Vec<u8>>,
            checksum: Arc<String>,
        }

        async fn handler(uri: Uri, State(state): State<StateData>) -> Response {
            if uri.path().ends_with(".sha256") {
                Response::new(Body::from((*state.checksum).clone()))
            } else {
                Response::new(Body::from((*state.archive).clone()))
            }
        }

        let state = StateData {
            archive: Arc::new(archive),
            checksum: Arc::new(checksum),
        };
        let app = Router::new()
            .route("/*path", get(handler))
            .with_state(state);
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{address}/alpine/latest-stable"), task)
    }

    fn test_archive() -> Vec<u8> {
        let mut tar = tar::Builder::new(Vec::new());
        let contents = b"#!/bin/sh\n";
        let mut header = tar::Header::new_gnu();
        header.set_path("bin/sh").unwrap();
        header.set_size(contents.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        tar.append(&header, &contents[..]).unwrap();
        let tar = tar.into_inner().unwrap();

        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&tar).unwrap();
        encoder.finish().unwrap()
    }
}
