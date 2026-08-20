use super::AppState;
use crate::tasks::{Event, ExecRequest};
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
use std::convert::Infallible;
use uuid::Uuid;

pub(super) async fn create(
    State(state): State<AppState>,
    Json(request): Json<ExecRequest>,
) -> impl IntoResponse {
    match state.execution.submit(request).await {
        Ok(id) => (StatusCode::ACCEPTED, Json(json!({"task_id": id}))).into_response(),
        Err(error) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": error.to_string()})),
        )
            .into_response(),
    }
}

pub(super) async fn snapshot(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> impl IntoResponse {
    match state.execution.snapshot(id).await {
        Some(snapshot) => Json(snapshot).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

pub(super) async fn events(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> impl IntoResponse {
    let Some((history, mut receiver)) = state.execution.subscribe(id).await else {
        return StatusCode::NOT_FOUND.into_response();
    };

    let stream = async_stream::stream! {
        for event in history {
            let terminal = is_terminal(&event);
            yield Ok::<_, Infallible>(to_sse(event));
            if terminal {
                return;
            }
        }
        while let Ok(event) = receiver.recv().await {
            let terminal = is_terminal(&event);
            yield Ok::<_, Infallible>(to_sse(event));
            if terminal {
                break;
            }
        }
    };

    Sse::new(stream)
        .keep_alive(axum::response::sse::KeepAlive::default())
        .into_response()
}

fn is_terminal(event: &Event) -> bool {
    matches!(
        event,
        Event::Finished(_) | Event::TimedOut | Event::Failed(_)
    )
}

fn to_sse(event: Event) -> SseEvent {
    let name = event.name();
    SseEvent::default().event(name).data(event.data())
}
