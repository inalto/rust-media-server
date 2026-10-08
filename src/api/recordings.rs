//! Recording endpoints (ARCHITECTURE.md §8.1).
//!
//! PHASE 1: `record/start|stop` are wired to the `RecorderManager` stub (which answers
//! "not implemented"); the listing/download/delete endpoints return 501. The
//! `feat/metrics-recording` branch implements them.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use axum::{Json, Router};

use super::broadcasts::load;
use crate::error::{ApiError, ApiResult};
use crate::recorder::{RecorderError, RecordingInfo};
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/broadcasts/{name}/record/start", post(record_start))
        .route("/broadcasts/{name}/record/stop", post(record_stop))
        .route("/broadcasts/{name}/recordings", get(list_for_broadcast))
        .route("/recordings", get(list_all))
        .route("/recordings/{id}", delete(delete_recording))
        .route("/recordings/{id}/download", get(download))
}

impl From<RecorderError> for ApiError {
    fn from(e: RecorderError) -> Self {
        match e {
            RecorderError::NotImplemented => ApiError::NotImplemented,
            RecorderError::NotLive => ApiError::conflict(e.to_string()),
            RecorderError::AlreadyRecording => ApiError::conflict(e.to_string()),
            RecorderError::NotRecording => ApiError::conflict(e.to_string()),
            RecorderError::Other(err) => ApiError::Internal(format!("{err:#}")),
        }
    }
}

async fn record_start(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> ApiResult<Json<RecordingInfo>> {
    let b = load(&state, &name).await?;
    Ok(Json(state.recorder.start_manual(&b.name).await?))
}

async fn record_stop(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> ApiResult<StatusCode> {
    let b = load(&state, &name).await?;
    state.recorder.stop_manual(&b.name).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// TODO(feat/metrics-recording): `[Recording]` of one broadcast.
async fn list_for_broadcast(Path(_name): Path<String>) -> ApiError {
    ApiError::NotImplemented
}

/// TODO(feat/metrics-recording): `[Recording]`, `?broadcast=` filter.
async fn list_all() -> ApiError {
    ApiError::NotImplemented
}

/// TODO(feat/metrics-recording): delete row + file, 204.
async fn delete_recording(Path(_id): Path<String>) -> ApiError {
    ApiError::NotImplemented
}

/// TODO(feat/metrics-recording): file download with `Content-Disposition: attachment`.
async fn download(Path(_id): Path<String>) -> ApiError {
    ApiError::NotImplemented
}
