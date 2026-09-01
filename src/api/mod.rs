mod auth;
mod exec;
mod workspace;
mod workspace_service;

use crate::{
    config::Config,
    runtime::{execution::ExecutionService, Sandbox},
};
use axum::{
    middleware,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde_json::json;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Clone)]
pub struct AppState {
    pub auth_token: Arc<str>,
    pub execution: Arc<ExecutionService>,
    workspace_root: Option<PathBuf>,
}

impl AppState {
    pub fn new(config: Config, sandbox: Arc<Sandbox>) -> Self {
        let max_timeout = config
            .sandbox
            .resolved()
            .map(|settings| settings.max_timeout)
            .unwrap_or_else(|_| std::time::Duration::from_secs(config.sandbox.max_timeout_seconds));
        let workspace_root = config.mounts.get(Path::new("/workspace")).cloned();
        Self {
            auth_token: Arc::from(config.api.token),
            execution: Arc::new(ExecutionService::new(sandbox, max_timeout)),
            workspace_root,
        }
    }

    pub async fn shutdown(&self) -> anyhow::Result<()> {
        self.execution.shutdown().await
    }
}

pub fn router(state: AppState) -> Router {
    let protected = Router::new()
        .route("/v1/exec", post(exec::create))
        .route("/v1/exec/:id", get(exec::snapshot).delete(exec::cancel))
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
    use std::{
        collections::{BTreeMap, HashMap},
        sync::Arc,
    };
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
                token: "secret".into(),
            },
            sandbox,
            features: crate::config::FeaturesConfig::default(),
            mounts: BTreeMap::new(),
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
        let sandbox: crate::config::SandboxConfig = toml::from_str(
            r#"
rootfs_dir = "."
"#,
        )
        .unwrap();
        let mut mounts = BTreeMap::new();
        mounts.insert(
            std::path::PathBuf::from("/workspace"),
            temp.path().to_path_buf(),
        );
        let config = Config {
            api: crate::config::ApiConfig {
                listen_addr: "127.0.0.1:0".parse().unwrap(),
                token: "secret".into(),
            },
            sandbox,
            features: crate::config::FeaturesConfig::default(),
            mounts,
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

    #[tokio::test]
    async fn workspace_routes_are_unavailable_without_a_workspace_mount() {
        let app = router(test_state());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/v1/workspace")
                    .header("authorization", "Bearer secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), axum::http::StatusCode::NOT_FOUND);
    }
}
