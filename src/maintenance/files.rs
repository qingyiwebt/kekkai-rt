use anyhow::{bail, ensure};
use std::{
    fs, io,
    path::{Component, Path},
};

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
