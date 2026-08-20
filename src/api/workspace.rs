use super::AppState;
use crate::workspace::{WorkspaceError, WorkspaceItem};
use axum::{
    body::{Body, Bytes},
    extract::{Path, State},
    http::{header, HeaderValue, StatusCode},
    response::IntoResponse,
    Json,
};
use serde_json::json;

pub(super) async fn root(State(state): State<AppState>) -> impl IntoResponse {
    get(State(state), Path(String::new())).await
}

pub(super) async fn delete_root() -> impl IntoResponse {
    WorkspaceError::InvalidPath("workspace root cannot be deleted".into()).into_response()
}

pub(super) async fn get(
    State(state): State<AppState>,
    Path(path): Path<String>,
) -> impl IntoResponse {
    match state.workspace.get(&path).await {
        Ok(WorkspaceItem::Directory(directory)) => Json(directory).into_response(),
        Ok(WorkspaceItem::File(contents)) => (
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

pub(super) async fn put(
    State(state): State<AppState>,
    Path(path): Path<String>,
    body: Bytes,
) -> impl IntoResponse {
    match state.workspace.put(&path, &body).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => error.into_response(),
    }
}

pub(super) async fn delete(
    State(state): State<AppState>,
    Path(path): Path<String>,
) -> impl IntoResponse {
    match state.workspace.delete(&path).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => error.into_response(),
    }
}

impl IntoResponse for WorkspaceError {
    fn into_response(self) -> axum::response::Response {
        let status = match self {
            WorkspaceError::Disabled | WorkspaceError::NotFound => StatusCode::NOT_FOUND,
            WorkspaceError::InvalidPath(_) => StatusCode::BAD_REQUEST,
            WorkspaceError::Conflict(_) => StatusCode::CONFLICT,
            WorkspaceError::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, Json(json!({"error": self.message()}))).into_response()
    }
}
