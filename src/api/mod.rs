mod auth;
mod exec;
mod workspace;

use crate::{
    config::Config, execution::ExecutionService, runtime::Sandbox, workspace::WorkspaceService,
};
use axum::{
    middleware,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde_json::json;
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub(crate) auth_secret: Arc<str>,
    pub(crate) execution: Arc<ExecutionService>,
    pub(crate) workspace: WorkspaceService,
}

impl AppState {
    pub fn new(config: Config, sandbox: Arc<Sandbox>) -> Self {
        let max_timeout = config
            .sandbox
            .resolved()
            .map(|settings| settings.max_timeout)
            .unwrap_or_else(|_| std::time::Duration::from_secs(config.sandbox.max_timeout_seconds));
        Self {
            auth_secret: Arc::from(config.api.secret),
            execution: Arc::new(ExecutionService::new(sandbox, max_timeout)),
            workspace: WorkspaceService::new(config.sandbox.workspace_dir),
        }
    }

    pub async fn shutdown(&self) -> anyhow::Result<()> {
        self.execution.shutdown().await
    }
}

pub fn router(state: AppState) -> Router {
    let protected = Router::new()
        .route("/v1/exec", post(exec::create))
        .route("/v1/exec/:id", get(exec::snapshot))
        .route("/v1/exec/:id/events", get(exec::events))
        .route(
            "/v1/workspace",
            get(workspace::root).delete(workspace::delete_root),
        )
        .route(
            "/v1/workspace/*path",
            get(workspace::get)
                .put(workspace::put)
                .delete(workspace::delete),
        )
        .layer(middleware::from_fn_with_state(state.clone(), auth::check));

    Router::new()
        .route("/healthz", get(health))
        .merge(protected)
        .with_state(state)
}

async fn health() -> impl IntoResponse {
    Json(json!({"status":"ok"}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use http::Request;
    use std::{collections::HashMap, sync::Arc};
    use tempfile::tempdir;
    use tower::ServiceExt;

    fn test_state() -> AppState {
        let sandbox: crate::config::SandboxConfig = toml::from_str(
            r#"
rootfs_dir = "."
"#,
        )
        .unwrap();
        let config = Config {
            api: crate::config::ApiConfig {
                listen_addr: "127.0.0.1:0".parse().unwrap(),
                secret: "secret".into(),
            },
            sandbox,
            tools: HashMap::new(),
        };
        AppState::new(config, Arc::new(Sandbox::test_instance()))
    }

    #[tokio::test]
    async fn health_is_public_and_exec_route_is_protected() {
        let app = router(test_state());

        let health = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(health.status(), axum::http::StatusCode::OK);

        let unauthorized = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/exec")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"argv":[]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauthorized.status(), axum::http::StatusCode::UNAUTHORIZED);

        let bad_request = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/exec")
                    .header("authorization", "Bearer secret")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"argv":[]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(bad_request.status(), axum::http::StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn workspace_routes_use_the_same_auth_layer() {
        let temp = tempdir().unwrap();
        let mut sandbox: crate::config::SandboxConfig = toml::from_str(
            r#"
rootfs_dir = "."
"#,
        )
        .unwrap();
        sandbox.workspace_dir = Some(temp.path().to_path_buf());
        let config = Config {
            api: crate::config::ApiConfig {
                listen_addr: "127.0.0.1:0".parse().unwrap(),
                secret: "secret".into(),
            },
            sandbox,
            tools: HashMap::new(),
        };
        let app = router(AppState::new(config, Arc::new(Sandbox::test_instance())));
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/v1/workspace")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::UNAUTHORIZED);
    }
}
