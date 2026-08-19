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
use tracing::{debug, info, warn};
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
    let (id, _, lease) = state.tasks.create().await;
    let tasks = state.tasks.clone();
    let sandbox = state.sandbox.clone();
    tokio::spawn(async move {
        let _lease = lease;
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
    info!(
        task_id = %id,
        program = %req.argv.first().map(String::as_str).unwrap_or(""),
        argc = req.argv.len(),
        timeout_seconds = timeout.as_secs(),
        "execution task started"
    );
    tasks.publish(id, Event::Started).await;
    let running = match sandbox.exec(&req).await {
        Ok(x) => x,
        Err(e) => {
            warn!(task_id = %id, error = %e, "failed to start execution task");
            tasks.publish(id, Event::Failed(e.to_string())).await;
            return;
        }
    };
    let mut child = running.child;
    if let Some(input) = req.stdin {
        if let Some(mut pipe) = running.stdin {
            if let Err(error) = pipe.write_all(input.as_bytes()).await {
                debug!(task_id = %id, error = %error, "failed to write task stdin");
            }
        }
    }
    let mut stdout = running.stdout;
    let mut stderr = running.stderr;
    let output_tasks = tasks.clone();
    let read_stdout = tokio::spawn(async move {
        if let Some(mut pipe) = stdout.take() {
            let mut buf = [0; 8192];
            loop {
                match pipe.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        output_tasks
                            .publish(id, Event::Stdout(String::from_utf8_lossy(&buf[..n]).into()))
                            .await;
                    }
                    Err(error) => {
                        warn!(task_id = %id, error = %error, "failed to read task stdout");
                        break;
                    }
                }
            }
        }
    });
    let error_tasks = tasks.clone();
    let read_stderr = tokio::spawn(async move {
        if let Some(mut pipe) = stderr.take() {
            let mut buf = [0; 8192];
            loop {
                match pipe.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        error_tasks
                            .publish(id, Event::Stderr(String::from_utf8_lossy(&buf[..n]).into()))
                            .await;
                    }
                    Err(error) => {
                        warn!(task_id = %id, error = %error, "failed to read task stderr");
                        break;
                    }
                }
            }
        }
    });
    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => {
            let _ = read_stdout.await;
            let _ = read_stderr.await;
            info!(task_id = %id, exit_code = ?status.code(), "execution task finished");
            tasks.publish(id, Event::Finished(status.code())).await
        }
        Ok(Err(e)) => {
            warn!(task_id = %id, error = %e, "execution task failed while waiting");
            let _ = read_stdout.await;
            let _ = read_stderr.await;
            tasks.publish(id, Event::Failed(e.to_string())).await
        }
        Err(_) => {
            info!(task_id = %id, "execution task timed out");
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
