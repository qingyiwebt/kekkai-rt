use super::{workspace_service, AppState};
use axum::{
    body::{Body, Bytes},
    extract::{Path as PathParam, State},
    http::{header, HeaderValue, StatusCode},
    response::IntoResponse,
    Json,
};
use serde_json::json;

pub async fn root(State(state): State<AppState>) -> impl IntoResponse {
    get(State(state), PathParam(String::new())).await
}
pub async fn delete_root() -> impl IntoResponse {
    workspace_service::WorkspaceError::InvalidPath("workspace root cannot be deleted".into())
        .into_response()
}

pub async fn get(
    State(state): State<AppState>,
    PathParam(path): PathParam<String>,
) -> impl IntoResponse {
    match workspace_service::get(state.workspace_root.as_deref(), &path).await {
        Ok(workspace_service::WorkspaceItem::Directory(directory)) => {
            Json(directory).into_response()
        }
        Ok(workspace_service::WorkspaceItem::File(contents)) => (
            [(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/octet-stream"),
            )],
            Body::from(contents),
        )
            .into_response(),
        Err(error) => error.into_response(),
    }
}
pub async fn put(
    State(state): State<AppState>,
    PathParam(path): PathParam<String>,
    body: Bytes,
) -> impl IntoResponse {
    match workspace_service::put(state.workspace_root.as_deref(), &path, &body).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => error.into_response(),
    }
}
pub async fn delete(
    State(state): State<AppState>,
    PathParam(path): PathParam<String>,
) -> impl IntoResponse {
    match workspace_service::delete(state.workspace_root.as_deref(), &path).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => error.into_response(),
    }
}

impl IntoResponse for workspace_service::WorkspaceError {
    fn into_response(self) -> axum::response::Response {
        let status = match &self {
            workspace_service::WorkspaceError::Disabled
            | workspace_service::WorkspaceError::NotFound => StatusCode::NOT_FOUND,
            workspace_service::WorkspaceError::InvalidPath(_) => StatusCode::BAD_REQUEST,
            workspace_service::WorkspaceError::Conflict(_) => StatusCode::CONFLICT,
            workspace_service::WorkspaceError::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, Json(json!({"error": self.to_string()}))).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::workspace_service::*;
    use tempfile::tempdir;
    #[test]
    fn paths_reject_traversal_and_empty_components() {
        assert!(parse_relative_path("../outside").is_err());
        assert!(parse_relative_path("a//b").is_err());
        assert!(parse_relative_path("/absolute").is_err());
        assert_eq!(
            parse_relative_path("a/b").unwrap(),
            std::path::PathBuf::from("a/b")
        );
    }
    #[tokio::test]
    async fn workspace_supports_binary_crud_and_sorted_directory_listing() {
        let temp = tempdir().unwrap();
        let root = temp.path();
        put(Some(root), "nested/z.bin", &[0, 255, 2]).await.unwrap();
        put(Some(root), "nested/a.txt", b"hello").await.unwrap();
        let WorkspaceItem::Directory(directory) = get(Some(root), "nested").await.unwrap() else {
            panic!("expected directory")
        };
        assert_eq!(directory.path, "nested");
        assert_eq!(directory.entries[0].name, "a.txt");
        assert_eq!(directory.entries[1].name, "z.bin");
        let WorkspaceItem::File(contents) = get(Some(root), "nested/z.bin").await.unwrap() else {
            panic!("expected file")
        };
        assert_eq!(contents, vec![0, 255, 2]);
        delete(Some(root), "nested").await.unwrap();
        assert!(!root.join("nested").exists());
    }
    #[tokio::test]
    async fn workspace_rejects_symlink_paths_that_escape_the_root() {
        use std::os::unix::fs::symlink;
        let root = tempdir().unwrap();
        let outside = tempdir().unwrap();
        tokio::fs::write(outside.path().join("secret"), b"secret")
            .await
            .unwrap();
        symlink(outside.path(), root.path().join("escape")).unwrap();
        assert!(matches!(
            get(Some(root.path()), "escape/secret").await,
            Err(WorkspaceError::InvalidPath(_))
        ));
        assert!(matches!(
            put(Some(root.path()), "escape/new", b"blocked").await,
            Err(WorkspaceError::InvalidPath(_))
        ));
    }
}
