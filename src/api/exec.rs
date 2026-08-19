use super::AppState;
use crate::{
    runtime::Sandbox,
    tasks::{Event, ExecRequest},
};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{
        sse::{Event as SseEvent, Sse},
        IntoResponse,
    },
    Json,
};
use serde_json::json;
use std::{convert::Infallible, sync::Arc, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use uuid::Uuid;

pub(super) async fn create(
    State(state): State<AppState>,
    Json(req): Json<ExecRequest>,
) -> impl IntoResponse {
    if req.argv.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"argv must not be empty"})),
        )
            .into_response();
    }
    let timeout = Duration::from_secs(
        req.timeout_seconds
            .unwrap_or(300)
            .min(state.config.sandbox.max_timeout_seconds),
    );
    let (id, _) = state.tasks.create().await;
    let tasks = state.tasks.clone();
    let sandbox = state.sandbox.clone();
    tokio::spawn(async move {
        run_task(tasks, sandbox, id, req, timeout).await;
    });
    (StatusCode::ACCEPTED, Json(json!({"task_id":id}))).into_response()
}

async fn run_task(
    tasks: crate::tasks::TaskStore,
    sandbox: Arc<Sandbox>,
    id: Uuid,
    req: ExecRequest,
    timeout: Duration,
) {
    tasks.publish(id, Event::Started).await;
    let running = match sandbox.exec(&req).await {
        Ok(x) => x,
        Err(e) => {
            tasks.publish(id, Event::Failed(e.to_string())).await;
            return;
        }
    };
    let mut child = running.child;
    if let Some(input) = req.stdin {
        if let Some(mut pipe) = running.stdin {
            let _ = pipe.write_all(input.as_bytes()).await;
        }
    }
    let mut stdout = running.stdout;
    let mut stderr = running.stderr;
    let output_tasks = tasks.clone();
    let read_stdout = tokio::spawn(async move {
        if let Some(mut pipe) = stdout.take() {
            let mut buf = [0; 8192];
            while let Ok(n) = pipe.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                output_tasks
                    .publish(id, Event::Stdout(String::from_utf8_lossy(&buf[..n]).into()))
                    .await;
            }
        }
    });
    let error_tasks = tasks.clone();
    let read_stderr = tokio::spawn(async move {
        if let Some(mut pipe) = stderr.take() {
            let mut buf = [0; 8192];
            while let Ok(n) = pipe.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                error_tasks
                    .publish(id, Event::Stderr(String::from_utf8_lossy(&buf[..n]).into()))
                    .await;
            }
        }
    });
    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => {
            let _ = read_stdout.await;
            let _ = read_stderr.await;
            tasks.publish(id, Event::Finished(status.code())).await
        }
        Ok(Err(e)) => tasks.publish(id, Event::Failed(e.to_string())).await,
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            let _ = read_stdout.await;
            let _ = read_stderr.await;
            tasks.publish(id, Event::TimedOut).await
        }
    }
}

pub(super) async fn snapshot(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> impl IntoResponse {
    match state.tasks.snapshot(id).await {
        Some(s) => Json(s).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

pub(super) async fn events(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> impl IntoResponse {
    let Some((history, mut rx)) = state.tasks.subscribe(id).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let stream = async_stream::stream! {
        for ev in history {
            let terminal = matches!(ev, Event::Finished(_) | Event::TimedOut | Event::Failed(_));
            yield Ok::<_, Infallible>(SseEvent::default().event(ev.name()).data(ev.data()));
            if terminal { return; }
        }
        while let Ok(ev) = rx.recv().await {
            let terminal = matches!(ev, Event::Finished(_) | Event::TimedOut | Event::Failed(_));
            yield Ok::<_, Infallible>(SseEvent::default().event(ev.name()).data(ev.data()));
            if terminal { break; }
        }
    };
    Sse::new(stream)
        .keep_alive(axum::response::sse::KeepAlive::default())
        .into_response()
}
