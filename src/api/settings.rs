//! Global settings: default waiting text and default waiting video (ARCHITECTURE.md §8.1).
//!
//! - `GET /settings` → [`Settings`]
//! - `PATCH /settings` `{default_waiting_text?}` → [`Settings`] (empty text = built-in default)
//! - `POST /settings/waiting-video` multipart `file` → [`Settings`] (same pipeline as the
//!   per-broadcast upload, stored under `data/waiting/_default/`)
//! - `DELETE /settings/waiting-video` → [`Settings`]
//!
//! The defaults apply to every broadcast without its own waiting video / text (see
//! `api::ViewContext`), both in `BroadcastView` and in the public player API.

use axum::extract::multipart::MultipartRejection;
use axum::extract::{DefaultBodyLimit, Multipart, State};
use axum::http::HeaderMap;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use super::waiting::{
    DEFAULT_SUBDIR, UploadError, multipart_rejection, poster_url_if_exists, receive_waiting_video,
    remove_video_files, upload_body_limit,
};
use super::{ApiJson, ViewContext, waiting_url};
use crate::db::settings::{DEFAULT_WAITING_TEXT, DEFAULT_WAITING_VIDEO};
use crate::db::{self};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

pub fn routes(state: &AppState) -> Router<AppState> {
    Router::new()
        .route("/settings", get(get_settings).patch(patch_settings))
        .route(
            "/settings/waiting-video",
            post(upload_default_waiting_video)
                .delete(delete_default_waiting_video)
                .layer(DefaultBodyLimit::max(upload_body_limit(&state.config))),
        )
}

/// Same limit as a broadcast's `waiting_text`.
const WAITING_TEXT_MAX: usize = 500;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Settings {
    /// Relative path under `data/waiting/` of the global default waiting video.
    pub default_waiting_video: Option<String>,
    pub default_waiting_video_url: Option<String>,
    /// Poster of the default video, when it exists.
    pub default_poster_url: Option<String>,
    /// Effective default waiting text (the built-in one when not customized).
    pub default_waiting_text: String,
    pub max_upload_mb: u64,
    pub ffmpeg_available: bool,
}

#[derive(Debug, Deserialize)]
pub struct SettingsPatch {
    pub default_waiting_text: Option<String>,
}

async fn current(state: &AppState, headers: &HeaderMap) -> ApiResult<Settings> {
    let ctx = ViewContext::load(state, headers).await?;
    let video = ctx.default_waiting_video.clone();
    Ok(Settings {
        default_waiting_video_url: video.as_deref().map(|v| waiting_url(&ctx.base, v)),
        default_poster_url: video
            .as_deref()
            .and_then(|v| poster_url_if_exists(&state.config, &ctx.base, v)),
        default_waiting_video: video,
        default_waiting_text: ctx
            .default_waiting_text
            .clone()
            .unwrap_or_else(|| db::broadcasts::DEFAULT_WAITING_TEXT.to_string()),
        max_upload_mb: state.config.server.max_upload_mb,
        ffmpeg_available: state.ffmpeg.is_some(),
    })
}

async fn get_settings(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Settings>> {
    Ok(Json(current(&state, &headers).await?))
}

async fn patch_settings(
    State(state): State<AppState>,
    headers: HeaderMap,
    ApiJson(body): ApiJson<SettingsPatch>,
) -> ApiResult<Json<Settings>> {
    if let Some(text) = body.default_waiting_text {
        let text = text.trim();
        if text.chars().count() > WAITING_TEXT_MAX {
            return Err(ApiError::bad_request(format!(
                "default_waiting_text is too long (max {WAITING_TEXT_MAX} characters)"
            )));
        }
        if text.is_empty() {
            db::settings::delete(&state.db, DEFAULT_WAITING_TEXT).await?;
        } else {
            db::settings::set(&state.db, DEFAULT_WAITING_TEXT, text).await?;
        }
        tracing::info!("default waiting text updated");
    }
    Ok(Json(current(&state, &headers).await?))
}

async fn upload_default_waiting_video(
    State(state): State<AppState>,
    headers: HeaderMap,
    multipart: Result<Multipart, MultipartRejection>,
) -> Result<Json<Settings>, UploadError> {
    let multipart = multipart.map_err(multipart_rejection)?;
    let rel = receive_waiting_video(&state, multipart, DEFAULT_SUBDIR).await?;
    let previous = db::settings::get(&state.db, DEFAULT_WAITING_VIDEO).await?;
    db::settings::set(&state.db, DEFAULT_WAITING_VIDEO, &rel).await?;
    if let Some(prev) = previous.filter(|p| !p.trim().is_empty() && *p != rel) {
        remove_video_files(&state, &prev).await;
    }
    tracing::info!(video = %rel, "default waiting video updated");
    Ok(Json(current(&state, &headers).await?))
}

async fn delete_default_waiting_video(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Settings>> {
    if let Some(prev) = db::settings::get(&state.db, DEFAULT_WAITING_VIDEO).await? {
        db::settings::delete(&state.db, DEFAULT_WAITING_VIDEO).await?;
        if !prev.trim().is_empty() {
            remove_video_files(&state, &prev).await;
        }
        tracing::info!("default waiting video removed");
    }
    Ok(Json(current(&state, &headers).await?))
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};

    use super::super::waiting::tests::{
        TOKEN, delete_request, multipart_request, send, test_state,
    };
    use crate::db;
    use crate::db::broadcasts::NewBroadcast;

    fn json_request(method: &str, uri: &str, body: &str) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    #[tokio::test]
    async fn settings_roundtrip_and_fallbacks() {
        let (state, dir) = test_state(false).await;
        let (st, s) = send(&state, json_request("GET", "/api/v1/settings", "")).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(
            s["default_waiting_text"],
            db::broadcasts::DEFAULT_WAITING_TEXT
        );
        assert!(s["default_waiting_video"].is_null());
        assert_eq!(s["max_upload_mb"], 8);
        assert_eq!(s["ffmpeg_available"], false);

        let (st, s) = send(
            &state,
            json_request(
                "PATCH",
                "/api/v1/settings",
                r#"{"default_waiting_text":"  Torniamo presto  "}"#,
            ),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(s["default_waiting_text"], "Torniamo presto");
        let long = format!(r#"{{"default_waiting_text":"{}"}}"#, "x".repeat(501));
        let (st, _) = send(&state, json_request("PATCH", "/api/v1/settings", &long)).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);

        // Default video: upload, then a broadcast without its own falls back to it.
        let fake_mp4 = b"\0\0\0\x18ftypisom\0\0\x02\0isomiso2rest";
        let (st, s) = send(
            &state,
            multipart_request(
                "POST",
                "/api/v1/settings/waiting-video",
                "d.mp4",
                "video/mp4",
                fake_mp4,
            ),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{s}");
        let rel = s["default_waiting_video"].as_str().unwrap().to_string();
        assert!(rel.starts_with("_default/"));
        assert!(
            s["default_waiting_video_url"]
                .as_str()
                .unwrap()
                .contains("/waiting/_default/")
        );
        assert!(dir.join("waiting").join(&rel).is_file());

        let mut nb = NewBroadcast::new("show", "Show");
        nb.waiting_text = String::new();
        db::broadcasts::create(&state.db, nb).await.unwrap();
        let req = Request::builder()
            .uri("/api/v1/public/broadcasts/show")
            .body(Body::empty())
            .unwrap();
        let (st, public) = send(&state, req).await;
        assert_eq!(st, StatusCode::OK);
        assert!(
            public["waiting_video_url"]
                .as_str()
                .unwrap()
                .ends_with(&rel)
        );
        assert_eq!(public["waiting_text"], "Torniamo presto");
        let (_, view) = send(&state, json_request("GET", "/api/v1/broadcasts/show", "")).await;
        assert!(view["waiting_video"].is_null());
        assert!(view["waiting_video_url"].as_str().unwrap().ends_with(&rel));

        // Reset text to the built-in default, delete the default video.
        let (_, s) = send(
            &state,
            json_request(
                "PATCH",
                "/api/v1/settings",
                r#"{"default_waiting_text":""}"#,
            ),
        )
        .await;
        assert_eq!(
            s["default_waiting_text"],
            db::broadcasts::DEFAULT_WAITING_TEXT
        );
        let (st, s) = send(&state, delete_request("/api/v1/settings/waiting-video")).await;
        assert_eq!(st, StatusCode::OK);
        assert!(s["default_waiting_video"].is_null());
        assert!(!dir.join("waiting").join(&rel).exists());
        let req = Request::builder()
            .uri("/api/v1/public/broadcasts/show")
            .body(Body::empty())
            .unwrap();
        let (_, public) = send(&state, req).await;
        assert!(public["waiting_video_url"].is_null());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
