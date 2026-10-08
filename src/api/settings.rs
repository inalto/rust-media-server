//! Global settings: default waiting text and default waiting video (ARCHITECTURE.md §8.1).
//!
//! PHASE 1 STUB (501). The `feat/dashboard` branch implements `GET/PATCH /settings`
//! (`Settings` JSON, backed by `db::settings`) and `POST/DELETE /settings/waiting-video`
//! (stored under `data/waiting/`, path in setting `default_waiting_video`).

use axum::Router;
use axum::routing::{get, post};

use crate::error::ApiError;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/settings", get(get_settings).patch(patch_settings))
        .route(
            "/settings/waiting-video",
            post(upload_default_waiting_video).delete(delete_default_waiting_video),
        )
}

/// TODO(feat/dashboard): `Settings`.
async fn get_settings() -> ApiError {
    ApiError::NotImplemented
}

/// TODO(feat/dashboard): `{default_waiting_text?}` → `Settings`.
async fn patch_settings() -> ApiError {
    ApiError::NotImplemented
}

/// TODO(feat/dashboard): multipart `file` → `Settings`.
async fn upload_default_waiting_video() -> ApiError {
    ApiError::NotImplemented
}

/// TODO(feat/dashboard): → `Settings`.
async fn delete_default_waiting_video() -> ApiError {
    ApiError::NotImplemented
}
