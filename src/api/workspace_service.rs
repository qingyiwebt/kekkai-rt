use serde::Serialize;
use std::{
    os::unix::fs::PermissionsExt,
    path::{Component, Path, PathBuf},
};
use thiserror::Error;
use tokio::{fs, io::AsyncWriteExt};
use uuid::Uuid;

pub(super) async fn get(
    root: Option<&Path>,
    raw_path: &str,
) -> Result<WorkspaceItem, WorkspaceError> {
    let path = resolve_existing_path(root, raw_path).await?;
    let metadata = fs::symlink_metadata(&path).await.map_err(map_not_found)?;
    if metadata.is_dir() {
        return read_directory(raw_path, &path)
            .await
            .map(WorkspaceItem::Directory);
    }
    if metadata.is_file() {
        return fs::read(path)
            .await
            .map(WorkspaceItem::File)
            .map_err(WorkspaceError::Io);
    }
    Err(WorkspaceError::Conflict(
        "workspace path is not a regular file or directory".into(),
    ))
}

pub(super) async fn put(
    root: Option<&Path>,
    raw_path: &str,
    contents: &[u8],
) -> Result<(), WorkspaceError> {
    let path = resolve_write_path(root, raw_path).await?;
    if let Ok(metadata) = fs::symlink_metadata(&path).await {
        if metadata.is_dir() {
            return Err(WorkspaceError::Conflict(
                "cannot write a directory as a file".into(),
            ));
        }
    }
    let parent = path
        .parent()
        .ok_or_else(|| WorkspaceError::InvalidPath("invalid workspace path".into()))?;
    ensure_directory(parent).await.map_err(WorkspaceError::Io)?;
    let temporary = parent.join(format!(".kekkai-rt-{}.tmp", Uuid::new_v4()));
    let result = async {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .await?;
        file.write_all(contents).await?;
        file.flush().await?;
        file.set_permissions(std::fs::Permissions::from_mode(0o666))
            .await?;
        fs::rename(&temporary, &path).await
    }
    .await;
    if let Err(error) = result {
        let _ = fs::remove_file(&temporary).await;
        return Err(WorkspaceError::Io(error));
    }
    Ok(())
}

pub(super) async fn delete(root: Option<&Path>, raw_path: &str) -> Result<(), WorkspaceError> {
    if raw_path.is_empty() {
        return Err(WorkspaceError::InvalidPath(
            "workspace root cannot be deleted".into(),
        ));
    }
    let path = resolve_existing_path(root, raw_path).await?;
    let metadata = fs::symlink_metadata(&path).await.map_err(map_not_found)?;
    let result = if metadata.is_dir() {
        fs::remove_dir_all(path).await
    } else {
        fs::remove_file(path).await
    };
    result.map_err(WorkspaceError::Io)
}

async fn read_directory(raw_path: &str, path: &Path) -> Result<WorkspaceDirectory, WorkspaceError> {
    let mut reader = fs::read_dir(path).await.map_err(WorkspaceError::Io)?;
    let mut entries = Vec::new();
    while let Some(entry) = reader.next_entry().await.map_err(WorkspaceError::Io)? {
        let metadata = fs::symlink_metadata(entry.path())
            .await
            .map_err(WorkspaceError::Io)?;
        entries.push(WorkspaceEntry {
            name: entry.file_name().to_string_lossy().into_owned(),
            kind: entry_kind(&metadata),
            size: metadata.len(),
        });
    }
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(WorkspaceDirectory {
        path: raw_path.into(),
        kind: "directory",
        entries,
    })
}

async fn resolve_existing_path(
    root: Option<&Path>,
    raw_path: &str,
) -> Result<PathBuf, WorkspaceError> {
    let (root, path) = resolve_input(root, raw_path).await?;
    let canonical = fs::canonicalize(&path).await.map_err(map_not_found)?;
    ensure_within_root(&root, &canonical)?;
    Ok(path)
}

async fn resolve_write_path(
    root: Option<&Path>,
    raw_path: &str,
) -> Result<PathBuf, WorkspaceError> {
    let (root, path) = resolve_input(root, raw_path).await?;
    match fs::canonicalize(&path).await {
        Ok(canonical) => {
            ensure_within_root(&root, &canonical)?;
            Ok(path)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let existing = nearest_existing_ancestor(&path).await?;
            let canonical_existing = fs::canonicalize(existing)
                .await
                .map_err(WorkspaceError::Io)?;
            ensure_within_root(&root, &canonical_existing)?;
            Ok(path)
        }
        Err(error) => Err(WorkspaceError::Io(error)),
    }
}

async fn resolve_input(
    root: Option<&Path>,
    raw_path: &str,
) -> Result<(PathBuf, PathBuf), WorkspaceError> {
    let root = root.ok_or(WorkspaceError::Disabled)?;
    let relative = parse_relative_path(raw_path)?;
    let root = fs::canonicalize(root).await.map_err(WorkspaceError::Io)?;
    Ok((root.clone(), root.join(relative)))
}

pub(super) fn parse_relative_path(raw_path: &str) -> Result<PathBuf, WorkspaceError> {
    if raw_path.is_empty() {
        return Ok(PathBuf::new());
    }
    if raw_path.split('/').any(str::is_empty) {
        return Err(WorkspaceError::InvalidPath(
            "workspace path contains an empty component".into(),
        ));
    }
    let mut relative = PathBuf::new();
    for component in Path::new(raw_path).components() {
        match component {
            Component::Normal(component) => relative.push(component),
            _ => {
                return Err(WorkspaceError::InvalidPath(
                    "workspace path must contain only normal relative components".into(),
                ))
            }
        }
    }
    if relative.as_os_str().is_empty() {
        return Err(WorkspaceError::InvalidPath("invalid workspace path".into()));
    }
    Ok(relative)
}

fn ensure_within_root(root: &Path, path: &Path) -> Result<(), WorkspaceError> {
    if path.starts_with(root) {
        Ok(())
    } else {
        Err(WorkspaceError::InvalidPath(
            "workspace path escapes workspace root".into(),
        ))
    }
}
async fn nearest_existing_ancestor(path: &Path) -> Result<PathBuf, WorkspaceError> {
    let mut candidate = path.to_path_buf();
    loop {
        match fs::symlink_metadata(&candidate).await {
            Ok(_) => return Ok(candidate),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if !candidate.pop() {
                    return Err(WorkspaceError::NotFound);
                }
            }
            Err(error) => return Err(WorkspaceError::Io(error)),
        }
    }
}
fn map_not_found(error: std::io::Error) -> WorkspaceError {
    if error.kind() == std::io::ErrorKind::NotFound {
        WorkspaceError::NotFound
    } else {
        WorkspaceError::Io(error)
    }
}
fn entry_kind(metadata: &std::fs::Metadata) -> &'static str {
    if metadata.is_dir() {
        "directory"
    } else if metadata.is_file() {
        "file"
    } else if metadata.file_type().is_symlink() {
        "symlink"
    } else {
        "other"
    }
}
async fn ensure_directory(path: &Path) -> std::io::Result<()> {
    fs::create_dir_all(path).await?;
    fs::set_permissions(path, std::fs::Permissions::from_mode(0o777)).await
}

#[derive(Debug, Error)]
pub(super) enum WorkspaceError {
    #[error("workspace is not configured")]
    Disabled,
    #[error("{0}")]
    InvalidPath(String),
    #[error("workspace path does not exist")]
    NotFound,
    #[error("{0}")]
    Conflict(String),
    #[error("workspace operation failed: {0}")]
    Io(#[source] std::io::Error),
}
#[derive(Debug, Serialize)]
pub(super) struct WorkspaceDirectory {
    pub(super) path: String,
    #[serde(rename = "type")]
    pub(super) kind: &'static str,
    pub(super) entries: Vec<WorkspaceEntry>,
}
#[derive(Debug, Serialize)]
pub(super) struct WorkspaceEntry {
    pub(super) name: String,
    #[serde(rename = "type")]
    pub(super) kind: &'static str,
    pub(super) size: u64,
}
pub(super) enum WorkspaceItem {
    Directory(WorkspaceDirectory),
    File(Vec<u8>),
}
