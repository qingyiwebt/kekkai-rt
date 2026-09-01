use anyhow::{bail, ensure, Context};
use flate2::read::GzDecoder;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{self, Read},
    path::{Path, PathBuf},
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
        super::files::validate_archive_path(&path)?;
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
        super::files::validate_archive_path(&path)?;
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
