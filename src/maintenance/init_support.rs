use anyhow::{bail, ensure, Context};
use flate2::read::GzDecoder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{self, Read},
    path::{Component, Path, PathBuf},
};
use tar::Archive;
use uuid::Uuid;

#[derive(Debug, Deserialize)]
struct OciIndex {
    manifests: Vec<Descriptor>,
}

#[derive(Debug, Deserialize)]
struct OciManifest {
    config: Descriptor,
    layers: Vec<Descriptor>,
}

#[derive(Debug, Deserialize)]
struct Descriptor {
    #[serde(rename = "mediaType")]
    media_type: Option<String>,
    digest: String,
    #[serde(default)]
    platform: Option<Platform>,
}

#[derive(Debug, Deserialize)]
struct Platform {
    os: String,
    architecture: String,
}

#[derive(Serialize)]
struct GeneratedConfig {
    api: GeneratedApiConfig,
    sandbox: GeneratedSandboxConfig,
    mounts: BTreeMap<String, String>,
}

#[derive(Serialize)]
struct GeneratedApiConfig {
    listen_addr: String,
    token: String,
}

#[derive(Serialize)]
struct GeneratedSandboxConfig {
    rootfs_dir: String,
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
    let mut mounts = BTreeMap::new();
    mounts.insert(
        "/workspace".into(),
        workspace.to_string_lossy().into_owned(),
    );
    let config = GeneratedConfig {
        api: GeneratedApiConfig {
            listen_addr: "0.0.0.0:8080".into(),
            token: Uuid::new_v4().as_simple().to_string(),
        },
        sandbox: GeneratedSandboxConfig {
            rootfs_dir: rootfs.to_string_lossy().into_owned(),
            backend: "runsc".into(),
            max_timeout_seconds: 300,
            network_mode: "nat".into(),
            network_bridge: "kekkai-rt0".into(),
            network_subnet: "10.200.0.0/24".into(),
            network_gateway: "10.200.0.1".into(),
            network_ip: "10.200.0.2".into(),
            network_dns: vec!["1.1.1.1".into(), "8.8.8.8".into()],
        },
        mounts,
    };
    Ok(format!("{}\n", toml::to_string_pretty(&config)?))
}

pub(crate) fn extract_oci_image(image: &Path, destination: &Path) -> anyhow::Result<()> {
    if image.is_dir() {
        return apply_layout(image, destination);
    }

    let layout = destination
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!(".kekkai-rt-oci-{}", Uuid::new_v4()));
    fs::create_dir(&layout)
        .with_context(|| format!("create OCI archive staging directory {}", layout.display()))?;
    let result = extract_archive(image, &layout).and_then(|_| apply_layout(&layout, destination));
    let cleanup_result = fs::remove_dir_all(&layout);
    if let Err(error) = result {
        let _ = cleanup_result;
        return Err(error);
    }
    cleanup_result.context("remove OCI archive staging directory")?;
    Ok(())
}

fn apply_layout(layout: &Path, destination: &Path) -> anyhow::Result<()> {
    let index: OciIndex = read_json(&layout.join("index.json"))
        .with_context(|| format!("read OCI image index {}", layout.display()))?;
    let descriptor = select_platform(&index.manifests)?;
    ensure!(
        descriptor
            .media_type
            .as_deref()
            .map(|value| value.contains("image.manifest"))
            .unwrap_or(true),
        "selected OCI descriptor is not an image manifest"
    );
    let manifest: OciManifest =
        read_blob_json(layout, &descriptor.digest).context("read OCI image manifest")?;
    ensure!(
        manifest
            .config
            .media_type
            .as_deref()
            .map(|value| value.contains("image.config"))
            .unwrap_or(true),
        "OCI manifest config descriptor is invalid"
    );
    verify_blob(layout, &manifest.config.digest).context("verify OCI image config")?;

    for layer in manifest.layers {
        verify_blob(layout, &layer.digest)
            .with_context(|| format!("verify OCI layer {}", layer.digest))?;
        apply_layer(layout, &layer, destination)
            .with_context(|| format!("apply OCI layer {}", layer.digest))?;
    }
    Ok(())
}

fn select_platform(manifests: &[Descriptor]) -> anyhow::Result<&Descriptor> {
    let architecture = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "x86" => "386",
        "aarch64" => "arm64",
        "arm" => "arm",
        other => other,
    };
    if let Some(descriptor) = manifests.iter().find(|descriptor| {
        descriptor
            .platform
            .as_ref()
            .is_some_and(|platform| platform.os == "linux" && platform.architecture == architecture)
    }) {
        return Ok(descriptor);
    }
    if manifests.len() == 1 && manifests[0].platform.is_none() {
        return Ok(&manifests[0]);
    }
    bail!("OCI image has no linux/{architecture} manifest")
}

fn apply_layer(layout: &Path, descriptor: &Descriptor, destination: &Path) -> anyhow::Result<()> {
    let path = blob_path(layout, &descriptor.digest)?;
    let file = File::open(&path)?;
    let reader: Box<dyn Read> = match descriptor.media_type.as_deref() {
        Some("application/vnd.oci.image.layer.v1.tar+gzip")
        | Some("application/vnd.docker.image.rootfs.diff.tar.gzip") => {
            Box::new(GzDecoder::new(file))
        }
        Some("application/vnd.oci.image.layer.v1.tar+zstd") => {
            Box::new(zstd::stream::read::Decoder::new(file).context("create zstd layer decoder")?)
        }
        Some("application/vnd.oci.image.layer.v1.tar")
        | Some("application/vnd.docker.image.rootfs.diff.tar")
        | None => Box::new(file),
        Some(media_type) => bail!("unsupported OCI layer media type {media_type}"),
    };
    let mut archive = Archive::new(reader);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        validate_archive_path(&path)?;
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        let parent = path.parent().unwrap_or_else(|| Path::new(""));
        let target_parent = destination.join(parent);
        if file_name == ".wh..wh..opq" {
            remove_children(&target_parent)?;
        } else if let Some(name) = file_name.strip_prefix(".wh.") {
            ensure!(!name.is_empty(), "OCI whiteout has an empty target");
            remove_path(&target_parent.join(name))?;
        } else {
            entry.unpack_in(destination)?;
        }
    }
    Ok(())
}

fn extract_archive(archive_path: &Path, destination: &Path) -> anyhow::Result<()> {
    let archive = File::open(archive_path)
        .with_context(|| format!("open OCI image archive {}", archive_path.display()))?;
    let mut archive = Archive::new(archive);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        validate_archive_path(&path)?;
        entry.unpack_in(destination)?;
    }
    Ok(())
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> anyhow::Result<T> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}

fn read_blob_json<T: for<'de> Deserialize<'de>>(layout: &Path, digest: &str) -> anyhow::Result<T> {
    let path = blob_path(layout, digest)?;
    verify_blob(layout, digest)?;
    read_json(&path)
}

fn blob_path(layout: &Path, digest: &str) -> anyhow::Result<PathBuf> {
    let (algorithm, value) = digest
        .split_once(':')
        .ok_or_else(|| anyhow::anyhow!("OCI digest is missing its algorithm: {digest}"))?;
    ensure!(
        algorithm == "sha256",
        "unsupported OCI digest algorithm {algorithm}"
    );
    ensure!(
        value.len() == 64 && value.chars().all(|character| character.is_ascii_hexdigit()),
        "invalid OCI SHA256 digest {digest}"
    );
    Ok(layout.join("blobs").join(algorithm).join(value))
}

fn verify_blob(layout: &Path, digest: &str) -> anyhow::Result<()> {
    let path = blob_path(layout, digest)?;
    let mut file =
        File::open(&path).with_context(|| format!("open OCI blob {}", path.display()))?;
    let mut hash = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let length = file.read(&mut buffer)?;
        if length == 0 {
            break;
        }
        hash.update(&buffer[..length]);
    }
    let actual = format!("sha256:{:x}", hash.finalize());
    ensure!(
        actual == digest,
        "OCI blob digest mismatch: expected {digest}, got {actual}"
    );
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

fn remove_path(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn remove_children(path: &Path) -> io::Result<()> {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    for entry in entries {
        remove_path(&entry?.path())?;
    }
    Ok(())
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
    if let Err(error) = io::Write::write_all(&mut file, content.as_bytes()) {
        let _ = fs::remove_file(path);
        return Err(error.into());
    }
    Ok(true)
}
