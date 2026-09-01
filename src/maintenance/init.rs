use super::{
    config_template::{detect_init_features, generated_config},
    init_support::{extract_oci_image, write_config_if_missing},
};
use crate::{
    config::{CgroupMode, NetworkMode},
    runtime::host::HostCapabilities,
};
use anyhow::{bail, Context};
use std::{
    fs, io,
    path::{Path, PathBuf},
};
use uuid::Uuid;

pub async fn run(config_path: &Path, image: &Path) -> anyhow::Result<()> {
    let working_dir = fs::canonicalize(".").context("resolve current directory")?;
    run_in(&working_dir, config_path, image)
}

fn run_in(working_dir: &Path, config_path: &Path, image: &Path) -> anyhow::Result<()> {
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
    if !image.exists() {
        bail!("OCI image does not exist: {}", image.display());
    }

    let staging = working_dir.join(format!(".kekkai-rt-sysroot-{}", Uuid::new_v4()));
    fs::create_dir(&staging)
        .with_context(|| format!("create staging directory {}", staging.display()))?;
    fs::create_dir_all(&workspace)
        .with_context(|| format!("create workspace directory {}", workspace.display()))?;
    let result = (|| {
        extract_oci_image(image, &staging).context("extract OCI image")?;
        let mut mounts = std::collections::BTreeMap::new();
        mounts.insert(PathBuf::from("/workspace"), workspace.clone());
        super::sysroot::prepare_sysroot(&staging, &mounts)
            .with_context(|| format!("prepare OCI sysroot {}", staging.display()))?;
        fs::rename(&staging, &rootfs)
            .with_context(|| format!("install sysroot {}", rootfs.display()))?;

        let config_created = if config_path.exists() {
            false
        } else {
            let capabilities = HostCapabilities::detect();
            let features = detect_init_features(&capabilities);
            if matches!(features.network_mode, NetworkMode::Host) {
                let reasons = capabilities.nat_unavailability_reasons().join(", ");
                eprintln!("warning: NAT is unavailable ({reasons}); generating host network mode");
            }
            if matches!(features.cgroups, CgroupMode::Disabled) {
                eprintln!(
                    "warning: memory cgroup controller is unavailable; generating disabled cgroups"
                );
            }
            let content =
                generated_config(&rootfs, &workspace, features.network_mode, features.cgroups)?;
            write_config_if_missing(config_path, &content)?
        };
        let installed_config = crate::config::Config::load(config_path)
            .context("load installed configuration for rootfs identity preparation")?;
        let capabilities = HostCapabilities::detect();
        let resolved_features = installed_config
            .features
            .resolve(&capabilities)
            .map_err(|error| anyhow::anyhow!("resolve runtime features: {error}"))?;
        super::sysroot::fix_sysroot_with_identity(
            &installed_config.sandbox,
            &installed_config.mounts,
            resolved_features.user_namespace,
        )
        .context("prepare rootfs ownership for user namespace")?;
        Ok::<_, anyhow::Error>(config_created)
    })();

    if result.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    let config_created = result?;
    println!("initialized OCI image in {}", rootfs.display());
    if config_created {
        println!("created {}", config_path.display());
    } else {
        println!("kept existing configuration {}", config_path.display());
    }
    Ok(())
}
