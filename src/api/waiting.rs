//! Per-broadcast waiting video upload/removal (ARCHITECTURE.md §8.1).
//!
//! PHASE 1 STUB (501). The `feat/dashboard` branch implements the multipart upload
//! (≤ `server.max_upload_mb`), ffmpeg normalization to H.264/AAC MP4 + poster, storage
//! under `data/waiting/` and `db::broadcasts::set_waiting_video`.
//!
//! Poster convention (used by the public API, see `api::poster_path_for`): the poster
//! is stored next to the video with the same stem and a `.jpg` extension.

use axum::Router;
use axum::extract::Path;
use axum::routing::post;

use crate::error::ApiError;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new().route(
        "/broadcasts/{name}/waiting-video",
        post(upload_waiting_video).delete(delete_waiting_video),
    )
}

/// TODO(feat/dashboard): multipart `file` → `BroadcastView`.
async fn upload_waiting_video(Path(_name): Path<String>) -> ApiError {
    ApiError::NotImplemented
}

/// TODO(feat/dashboard): remove the waiting video → `BroadcastView`.
async fn delete_waiting_video(Path(_name): Path<String>) -> ApiError {
    ApiError::NotImplemented
}
