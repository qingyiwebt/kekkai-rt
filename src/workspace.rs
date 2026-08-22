use serde::Serialize;
use std::{
    os::unix::fs::PermissionsExt,
    path::{Component, Path, PathBuf},
};
use tokio::{fs, io::AsyncWriteExt};
use uuid::Uuid;

#[derive(Debug)]
pub(crate) enum WorkspaceError {
    Disabled,
    InvalidPath(String),
    NotFound,
    Conflict(String),
    Io(std::io::Error),
}

impl WorkspaceError {
    pub(crate) fn message(&self) -> String {
        match self {
            Self::Disabled => "workspace is not configured".into(),
            Self::InvalidPath(message) => message.clone(),
            Self::NotFound => "workspace path does not exist".into(),
            Self::Conflict(message) => message.clone(),
            Self::Io(error) => format!("workspace operation failed: {error}"),
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct WorkspaceDirectory {
    pub(crate) path: String,
    #[serde(rename = "type")]
    pub(crate) kind: &'static str,
    pub(crate) entries: Vec<WorkspaceEntry>,
}

#[derive(Debug, Serialize)]
pub(crate) struct WorkspaceEntry {
    pub(crate) name: String,
    #[serde(rename = "type")]
    pub(crate) kind: &'static str,
    pub(crate) size: u64,
}

pub(crate) enum WorkspaceItem {
    Directory(WorkspaceDirectory),
    File(Vec<u8>),
}

#[derive(Clone, Default)]
pub(crate) struct WorkspaceService {
    root: Option<PathBuf>,
}

impl WorkspaceService {
    pub(crate) fn new(root: Option<PathBuf>) -> Self {
        Self { root }
    }

    pub(crate) async fn get(&self, raw_path: &str) -> Result<WorkspaceItem, WorkspaceError> {
        let path = self.resolve_path(raw_path, false).await?;
        let metadata = fs::symlink_metadata(&path).await.map_err(map_not_found)?;
        if metadata.is_dir() {
            return self
                .read_directory(raw_path, &path)
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

    pub(crate) async fn put(&self, raw_path: &str, contents: &[u8]) -> Result<(), WorkspaceError> {
        let path = self.resolve_path(raw_path, true).await?;
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

    pub(crate) async fn delete(&self, raw_path: &str) -> Result<(), WorkspaceError> {
        if raw_path.is_empty() {
            return Err(WorkspaceError::InvalidPath(
                "workspace root cannot be deleted".into(),
            ));
        }
        let path = self.resolve_path(raw_path, false).await?;
        let metadata = fs::symlink_metadata(&path).await.map_err(map_not_found)?;
        let result = if metadata.is_dir() {
            fs::remove_dir_all(path).await
        } else {
            fs::remove_file(path).await
        };
        result.map_err(WorkspaceError::Io)
    }

    async fn read_directory(
        &self,
        raw_path: &str,
        path: &Path,
    ) -> Result<WorkspaceDirectory, WorkspaceError> {
        let mut reader = fs::read_dir(path).await.map_err(WorkspaceError::Io)?;
        let mut entries = Vec::new();
        while let Some(entry) = reader.next_entry().await.map_err(WorkspaceError::Io)? {
            let metadata = fs::symlink_metadata(entry.path())
                .await
                .map_err(WorkspaceError::Io)?;
            let kind = if metadata.is_dir() {
                "directory"
            } else if metadata.is_file() {
                "file"
            } else if metadata.file_type().is_symlink() {
                "symlink"
            } else {
                "other"
            };
            entries.push(WorkspaceEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                kind,
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

    async fn resolve_path(
        &self,
        raw_path: &str,
        allow_missing: bool,
    ) -> Result<PathBuf, WorkspaceError> {
        let root = self.root.as_deref().ok_or(WorkspaceError::Disabled)?;
        let relative = parse_relative_path(raw_path)?;
        let root = fs::canonicalize(root).await.map_err(WorkspaceError::Io)?;
        let path = root.join(&relative);
        let canonical = match fs::canonicalize(&path).await {
            Ok(canonical) => canonical,
            Err(error) if allow_missing && error.kind() == std::io::ErrorKind::NotFound => {
                let mut existing = path.clone();
                loop {
                    match fs::symlink_metadata(&existing).await {
                        Ok(_) => break,
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                            existing.pop();
                            if existing.as_os_str().is_empty() {
                                return Err(WorkspaceError::NotFound);
                            }
                        }
                        Err(error) => return Err(WorkspaceError::Io(error)),
                    }
                }
                let canonical_existing = fs::canonicalize(&existing)
                    .await
                    .map_err(WorkspaceError::Io)?;
                if !canonical_existing.starts_with(&root) {
                    return Err(WorkspaceError::InvalidPath(
                        "workspace path escapes workspace root".into(),
                    ));
                }
                path.clone()
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(WorkspaceError::NotFound)
            }
            Err(error) => return Err(WorkspaceError::Io(error)),
        };
        if !canonical.starts_with(&root) {
            return Err(WorkspaceError::InvalidPath(
                "workspace path escapes workspace root".into(),
            ));
        }
        Ok(path)
    }
}

fn map_not_found(error: std::io::Error) -> WorkspaceError {
    if error.kind() == std::io::ErrorKind::NotFound {
        WorkspaceError::NotFound
    } else {
        WorkspaceError::Io(error)
    }
}

pub(crate) fn parse_relative_path(raw_path: &str) -> Result<PathBuf, WorkspaceError> {
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

async fn ensure_directory(path: &Path) -> std::io::Result<()> {
    fs::create_dir_all(path).await?;
    fs::set_permissions(path, std::fs::Permissions::from_mode(0o777)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn paths_reject_traversal_and_empty_components() {
        assert!(parse_relative_path("../outside").is_err());
        assert!(parse_relative_path("a//b").is_err());
        assert!(parse_relative_path("/absolute").is_err());
        assert_eq!(parse_relative_path("a/b").unwrap(), PathBuf::from("a/b"));
    }

    #[tokio::test]
    async fn service_supports_binary_crud_and_sorted_directory_listing() {
        let temp = tempdir().unwrap();
        let service = WorkspaceService::new(Some(temp.path().to_path_buf()));
        service.put("nested/z.bin", &[0, 255, 2]).await.unwrap();
        service.put("nested/a.txt", b"hello").await.unwrap();

        let WorkspaceItem::Directory(directory) = service.get("nested").await.unwrap() else {
            panic!("expected directory");
        };
        assert_eq!(directory.path, "nested");
        assert_eq!(directory.entries[0].name, "a.txt");
        assert_eq!(directory.entries[1].name, "z.bin");

        let WorkspaceItem::File(contents) = service.get("nested/z.bin").await.unwrap() else {
            panic!("expected file");
        };
        assert_eq!(contents, vec![0, 255, 2]);

        service.delete("nested").await.unwrap();
        assert!(!temp.path().join("nested").exists());
    }

    #[tokio::test]
    async fn service_rejects_symlink_paths_that_escape_the_root() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        let outside = tempdir().unwrap();
        fs::write(outside.path().join("secret"), b"secret")
            .await
            .unwrap();
        symlink(outside.path(), root.path().join("escape")).unwrap();

        let service = WorkspaceService::new(Some(root.path().to_path_buf()));
        assert!(matches!(
            service.get("escape/secret").await,
            Err(WorkspaceError::InvalidPath(_))
        ));
        assert!(matches!(
            service.put("escape/new", b"blocked").await,
            Err(WorkspaceError::InvalidPath(_))
        ));
    }
}
