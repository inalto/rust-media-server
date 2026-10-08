//! Recording endpoints (ARCHITECTURE.md §8.1). Business logic lives in
//! `recorder::RecorderManager`.
//!
//! - `POST /broadcasts/{name}/record/start` → `Recording` (409 not live / already recording)
//! - `POST /broadcasts/{name}/record/stop` → `Recording` in its post-stop state
//!   (`processing` while the MP4 remux runs, else `ready`/`failed`); 409 when not recording
//! - `GET /broadcasts/{name}/recordings`, `GET /recordings?broadcast=<name>` → `[Recording]`
//!   (newest first)
//! - `DELETE /recordings/{id}` → 204 (row + files), 404 unknown, 409 still recording/processing
//! - `GET /recordings/{id}/download[?download=1]` → the file, with HTTP Range support
//!   (inline by default so a `<video>` can play it; `attachment` with `download=1`)

use axum::body::Body;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use tower::ServiceExt;
use tower_http::services::ServeFile;

use super::broadcasts::load;
use crate::db::{self, Recording};
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
            RecorderError::NotLive
            | RecorderError::AlreadyRecording
            | RecorderError::NotRecording
            | RecorderError::Busy => ApiError::conflict(e.to_string()),
            RecorderError::NotFound => ApiError::not_found(e.to_string()),
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
) -> ApiResult<Json<RecordingInfo>> {
    let b = load(&state, &name).await?;
    Ok(Json(state.recorder.stop_manual(&b.name).await?))
}

/// `[Recording]` of one broadcast, newest first.
async fn list_for_broadcast(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> ApiResult<Json<Vec<Recording>>> {
    let b = load(&state, &name).await?;
    Ok(Json(db::recordings::list(&state.db, Some(&b.id)).await?))
}

#[derive(Debug, Default, Deserialize)]
struct ListQuery {
    broadcast: Option<String>,
}

/// `[Recording]` of every broadcast, or of `?broadcast=<name>`; newest first.
async fn list_all(
    State(state): State<AppState>,
    q: Result<Query<ListQuery>, QueryRejection>,
) -> ApiResult<Json<Vec<Recording>>> {
    let Query(q) = q?;
    let filter = q
        .broadcast
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let rows = match filter {
        Some(name) => {
            let b = load(&state, name).await?;
            db::recordings::list(&state.db, Some(&b.id)).await?
        }
        None => db::recordings::list(&state.db, None).await?,
    };
    Ok(Json(rows))
}

/// Delete row + file(s): 204; 404 unknown; 409 still recording/processing.
async fn delete_recording(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    state.recorder.delete(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Default, Deserialize)]
struct DownloadQuery {
    download: Option<String>,
}

/// `<broadcast>-<YYYYMMDD-HHMMSS>.<ext>` (UTC start time).
fn download_filename(rec: &Recording) -> String {
    let name = rec.path.split('/').next().unwrap_or("recording");
    let ext = rec.path.rsplit_once('.').map(|(_, e)| e).unwrap_or("bin");
    let clean: String = format!("{name}-{}.{ext}", rec.started_at.format("%Y%m%d-%H%M%S"))
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .collect();
    clean
}

/// Serve the recording file with Range support (`ServeFile`). Inline by default,
/// `Content-Disposition: attachment` with `?download=1`.
async fn download(
    State(state): State<AppState>,
    Path(id): Path<String>,
    q: Result<Query<DownloadQuery>, QueryRejection>,
    req: Request,
) -> ApiResult<Response> {
    let Query(q) = q?;
    let attachment = matches!(
        q.download.as_deref().map(str::trim),
        Some("1" | "true" | "yes")
    );
    let rec = db::recordings::get(&state.db, &id)
        .await?
        .ok_or_else(|| ApiError::not_found("recording not found"))?;
    let path = state
        .recorder
        .file_path(&rec)
        .filter(|p| p.is_file())
        .ok_or_else(|| ApiError::not_found("recording file not found"))?;
    let resp = match ServeFile::new(&path).oneshot(req).await {
        Ok(r) => r,
        Err(never) => match never {},
    };
    let mut resp = resp.map(Body::new);
    if resp.status().is_success() {
        let disposition = format!(
            "{}; filename=\"{}\"",
            if attachment { "attachment" } else { "inline" },
            download_filename(&rec)
        );
        let h = resp.headers_mut();
        if let Ok(v) = HeaderValue::from_str(&disposition) {
            h.insert(header::CONTENT_DISPOSITION, v);
        }
        h.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("private, no-cache"),
        );
    }
    Ok(resp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::parse_ts;

    #[test]
    fn filenames() {
        let rec = Recording {
            id: "x".into(),
            broadcast_id: "b".into(),
            stream_session_id: None,
            path: "my-show/20261008-120000.mp4".into(),
            format: "mp4".into(),
            status: "ready".into(),
            started_at: parse_ts("2026-10-08T12:00:00.000Z"),
            ended_at: None,
            size_bytes: 0,
            duration_secs: None,
            error: None,
        };
        assert_eq!(download_filename(&rec), "my-show-20261008-120000.mp4");
    }
}
