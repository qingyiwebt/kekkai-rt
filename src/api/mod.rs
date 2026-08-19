mod auth;
mod exec;
mod workspace;

use crate::{config::Config, runtime::Sandbox, tasks::TaskStore};
use axum::{
    middleware,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde_json::json;
use std::{sync::Arc, time::Duration};
use tracing::info;

#[derive(Clone)]
pub struct AppState {
    pub config: Config,
    pub sandbox: Arc<Sandbox>,
    pub tasks: TaskStore,
}

impl AppState {
    pub fn new(config: Config, sandbox: Arc<Sandbox>) -> Self {
        let state = Self {
            config,
            sandbox,
            tasks: TaskStore::new(),
        };
        let tasks = state.tasks.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            loop {
                interval.tick().await;
                tasks.cleanup().await;
            }
        });
        state
    }

    pub async fn shutdown(&self) -> anyhow::Result<()> {
        info!("waiting for active execution tasks");
        self.tasks.wait_for_idle().await;
        self.sandbox.shutdown().await
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(health))
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
        .with_state(state.clone())
        .layer(middleware::from_fn_with_state(state, auth::check))
}

async fn health() -> impl IntoResponse {
    Json(json!({"status":"ok"}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use http::Request;
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
}
