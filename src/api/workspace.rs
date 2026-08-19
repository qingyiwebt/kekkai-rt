use super::AppState;
use axum::{
    body::{Body, Bytes},
    extract::{Path, State},
    http::{header, HeaderValue, StatusCode},
    response::IntoResponse,
    Json,
};
use serde_json::json;
use std::{
    os::unix::fs::PermissionsExt,
    path::{Component, Path as FsPath, PathBuf},
};
use tokio::fs;
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

#[derive(Debug)]
enum WorkspaceError {
    Disabled,
    InvalidPath(String),
    NotFound,
    Conflict(String),
    Io(std::io::Error),
}

impl WorkspaceError {
    fn status(&self) -> StatusCode {
        match self {
            Self::Disabled | Self::NotFound => StatusCode::NOT_FOUND,
            Self::InvalidPath(_) => StatusCode::BAD_REQUEST,
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    fn message(&self) -> String {
        match self {
            Self::Disabled => "workspace is not configured".into(),
            Self::InvalidPath(message) => message.clone(),
            Self::NotFound => "workspace path does not exist".into(),
            Self::Conflict(message) => message.clone(),
            Self::Io(error) => format!("workspace operation failed: {error}"),
        }
    }
}

impl IntoResponse for WorkspaceError {
    fn into_response(self) -> axum::response::Response {
        (self.status(), Json(json!({"error": self.message()}))).into_response()
    }
}

pub(super) async fn root(State(state): State<AppState>) -> impl IntoResponse {
    get_path(state, String::new()).await
}

pub(super) async fn delete_root() -> impl IntoResponse {
    WorkspaceError::InvalidPath("workspace root cannot be deleted".into())
}

pub(super) async fn get(
    State(state): State<AppState>,
    Path(path): Path<String>,
) -> impl IntoResponse {
    get_path(state, path).await
}

async fn get_path(state: AppState, raw_path: String) -> axum::response::Response {
    let path = match resolve_path(&state, &raw_path, false).await {
        Ok(path) => path,
        Err(error) => return error.into_response(),
    };
    let metadata = match fs::symlink_metadata(&path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return WorkspaceError::NotFound.into_response()
        }
        Err(error) => return WorkspaceError::Io(error).into_response(),
    };

    if metadata.is_dir() {
        let mut entries = Vec::new();
        let mut reader = match fs::read_dir(&path).await {
            Ok(reader) => reader,
            Err(error) => return WorkspaceError::Io(error).into_response(),
        };
        loop {
            let Some(entry) = (match reader.next_entry().await {
                Ok(entry) => entry,
                Err(error) => return WorkspaceError::Io(error).into_response(),
            }) else {
                break;
            };
            let metadata = match fs::symlink_metadata(entry.path()).await {
                Ok(metadata) => metadata,
                Err(error) => return WorkspaceError::Io(error).into_response(),
            };
            let entry_type = if metadata.is_dir() {
                "directory"
            } else if metadata.is_file() {
                "file"
            } else if metadata.file_type().is_symlink() {
                "symlink"
            } else {
                "other"
            };
            entries.push(json!({
                "name": entry.file_name().to_string_lossy(),
                "type": entry_type,
                "size": metadata.len(),
            }));
        }
        entries.sort_by(|a, b| {
            a.get("name")
                .and_then(|value| value.as_str())
                .cmp(&b.get("name").and_then(|value| value.as_str()))
        });
        return Json(json!({
            "path": raw_path,
            "type": "directory",
            "entries": entries,
        }))
        .into_response();
    }

    if metadata.is_file() {
        let contents = match fs::read(&path).await {
            Ok(contents) => contents,
            Err(error) => return WorkspaceError::Io(error).into_response(),
        };
        return (
            [(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/octet-stream"),
            )],
            Body::from(contents),
        )
            .into_response();
    }

    WorkspaceError::Conflict("workspace path is not a regular file or directory".into())
        .into_response()
}

pub(super) async fn put(
    State(state): State<AppState>,
    Path(raw_path): Path<String>,
    body: Bytes,
) -> impl IntoResponse {
    let path = match resolve_path(&state, &raw_path, true).await {
        Ok(path) => path,
        Err(error) => return error.into_response(),
    };
    if let Ok(metadata) = fs::symlink_metadata(&path).await {
        if metadata.is_dir() {
            return WorkspaceError::Conflict("cannot write a directory as a file".into())
                .into_response();
        }
    }

    let parent = path.parent().expect("workspace path always has a parent");
    if let Err(error) = ensure_directory(parent).await {
        return WorkspaceError::Io(error).into_response();
    }
    let temporary = parent.join(format!(".agentcell-{}.tmp", Uuid::new_v4()));
    let result = async {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .await?;
        file.write_all(&body).await?;
        file.flush().await?;
        file.set_permissions(std::fs::Permissions::from_mode(0o666))
            .await?;
        fs::rename(&temporary, &path).await
    }
    .await;
    if let Err(error) = result {
        let _ = fs::remove_file(&temporary).await;
        return WorkspaceError::Io(error).into_response();
    }

    StatusCode::NO_CONTENT.into_response()
}

pub(super) async fn delete(
    State(state): State<AppState>,
    Path(raw_path): Path<String>,
) -> impl IntoResponse {
    if raw_path.is_empty() {
        return WorkspaceError::InvalidPath("workspace root cannot be deleted".into())
            .into_response();
    }
    let path = match resolve_path(&state, &raw_path, false).await {
        Ok(path) => path,
        Err(error) => return error.into_response(),
    };
    let metadata = match fs::symlink_metadata(&path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return WorkspaceError::NotFound.into_response()
        }
        Err(error) => return WorkspaceError::Io(error).into_response(),
    };
    let result = if metadata.is_dir() {
        fs::remove_dir_all(&path).await
    } else {
        fs::remove_file(&path).await
    };
    match result {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => WorkspaceError::Io(error).into_response(),
    }
}

async fn resolve_path(
    state: &AppState,
    raw_path: &str,
    allow_missing: bool,
) -> Result<PathBuf, WorkspaceError> {
    let root = state
        .config
        .sandbox
        .workspace_dir
        .as_deref()
        .ok_or(WorkspaceError::Disabled)?;
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

fn parse_relative_path(raw_path: &str) -> Result<PathBuf, WorkspaceError> {
    if raw_path.is_empty() {
        return Ok(PathBuf::new());
    }
    if raw_path.split('/').any(str::is_empty) {
        return Err(WorkspaceError::InvalidPath(
            "workspace path contains an empty component".into(),
        ));
    }
    let mut relative = PathBuf::new();
    for component in FsPath::new(raw_path).components() {
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

async fn ensure_directory(path: &FsPath) -> std::io::Result<()> {
    fs::create_dir_all(path).await?;
    fs::set_permissions(path, std::fs::Permissions::from_mode(0o777)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Config, runtime::Sandbox};
    use axum::body::Body;
    use http::Request;
    use std::sync::Arc;
    use tempfile::tempdir;
    use tower::ServiceExt;

    fn test_state(workspace_dir: Option<PathBuf>) -> AppState {
        let mut sandbox: crate::config::SandboxConfig = toml::from_str(
            r#"
rootfs_dir = "."
"#,
        )
        .unwrap();
        sandbox.workspace_dir = workspace_dir;
        let config = Config {
            api: crate::config::ApiConfig {
                listen_addr: "127.0.0.1:0".parse().unwrap(),
                secret: "secret".into(),
            },
            sandbox,
        };
        AppState::new(config, Arc::new(Sandbox::test_instance()))
    }

    #[test]
    fn workspace_paths_reject_traversal_and_empty_components() {
        assert!(parse_relative_path("../outside").is_err());
        assert!(parse_relative_path("a//b").is_err());
        assert!(parse_relative_path("/absolute").is_err());
        assert_eq!(parse_relative_path("a/b").unwrap(), PathBuf::from("a/b"));
    }

    #[tokio::test]
    async fn workspace_api_requires_auth_and_supports_recursive_crud() {
        let temp = tempdir().unwrap();
        let state = test_state(Some(temp.path().to_path_buf()));
        let app = crate::api::router(state);

        let unauthorized = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/workspace")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

        let put = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/v1/workspace/nested/hello.txt")
                    .header(header::AUTHORIZATION, "Bearer secret")
                    .body(Body::from("hello"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(put.status(), StatusCode::NO_CONTENT);

        let get = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/workspace/nested/hello.txt")
                    .header(header::AUTHORIZATION, "Bearer secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(get.status(), StatusCode::OK);

        let delete = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/v1/workspace/nested")
                    .header(header::AUTHORIZATION, "Bearer secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(delete.status(), StatusCode::NO_CONTENT);
        assert!(!temp.path().join("nested").exists());
    }
}
