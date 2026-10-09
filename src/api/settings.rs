//! Global settings: default waiting text, default waiting video, UI language and default
//! theme (ARCHITECTURE.md §8.1, §17, §18).
//!
//! - `GET /settings` → [`Settings`]
//! - `PATCH /settings` `{default_waiting_text?, ui_language?, ui_theme?}` → [`Settings`]
//!   (empty text, or a built-in text, = built-in default; `ui_language` must be `"it"` or
//!   `"en"`, `ui_theme` `"system"`, `"light"` or `"dark"`). Emits
//!   `HubEvent::SettingsChanged` (admin SSE `settings` event) after a change.
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
use crate::db::settings::{
    DEFAULT_WAITING_TEXT, DEFAULT_WAITING_VIDEO, UI_LANGUAGE, UI_THEME, UiLanguage, UiTheme,
};
use crate::db::{self};
use crate::error::{ApiError, ApiResult};
use crate::hub::HubEvent;
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
    /// Effective default waiting text (the built-in one, in the UI language, when not
    /// customized).
    pub default_waiting_text: String,
    pub max_upload_mb: u64,
    pub ffmpeg_available: bool,
    /// Language of the dashboard and of the player texts.
    pub ui_language: UiLanguage,
    /// Default colour theme of the dashboard and of the player (a dashboard browser can
    /// override it with its toggle, a player page with `?theme=`).
    pub ui_theme: UiTheme,
}

#[derive(Debug, Deserialize)]
pub struct SettingsPatch {
    pub default_waiting_text: Option<String>,
    pub ui_language: Option<String>,
    pub ui_theme: Option<String>,
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
        default_waiting_text: ctx.effective_default_waiting_text(),
        max_upload_mb: state.config.server.max_upload_mb,
        ffmpeg_available: state.ffmpeg.is_some(),
        ui_language: ctx.language,
        ui_theme: ctx.theme,
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
    // Validate everything before writing anything.
    let text = body.default_waiting_text.as_deref().map(str::trim);
    if let Some(text) = text
        && text.chars().count() > WAITING_TEXT_MAX
    {
        return Err(ApiError::bad_request(format!(
            "default_waiting_text is too long (max {WAITING_TEXT_MAX} characters)"
        )));
    }
    let language = match body.ui_language.as_deref() {
        Some(l) => Some(
            UiLanguage::parse(l)
                .ok_or_else(|| ApiError::bad_request("ui_language must be \"it\" or \"en\""))?,
        ),
        None => None,
    };
    let theme = match body.ui_theme.as_deref() {
        Some(t) => Some(UiTheme::parse(t).ok_or_else(|| {
            ApiError::bad_request("ui_theme must be \"system\", \"light\" or \"dark\"")
        })?),
        None => None,
    };

    if let Some(text) = text {
        // A built-in text is stored as "no custom text" so it follows the UI language.
        if text.is_empty() || UiLanguage::is_builtin_waiting_text(text) {
            db::settings::delete(&state.db, DEFAULT_WAITING_TEXT).await?;
        } else {
            db::settings::set(&state.db, DEFAULT_WAITING_TEXT, text).await?;
        }
        tracing::info!("default waiting text updated");
    }
    if let Some(language) = language {
        db::settings::set(&state.db, UI_LANGUAGE, language.as_str()).await?;
        tracing::info!(language = language.as_str(), "UI language updated");
    }
    if let Some(theme) = theme {
        db::settings::set(&state.db, UI_THEME, theme.as_str()).await?;
        tracing::info!(theme = theme.as_str(), "UI theme updated");
    }
    if text.is_some() || language.is_some() || theme.is_some() {
        state.hub.emit_shared(HubEvent::SettingsChanged);
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
    notify_settings(&state);
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
        notify_settings(&state);
    }
    Ok(Json(current(&state, &headers).await?))
}

/// Cluster mode: the other nodes clear their broadcasts cache and refresh their
/// dashboards (`rms_settings`). No local event (unchanged single-node behaviour).
fn notify_settings(state: &AppState) {
    if let Some(n) = crate::cluster::bus::Notice::for_event(&HubEvent::SettingsChanged) {
        state.hub.outbox().send(n);
    }
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

    fn public_get(uri: &str) -> Request<Body> {
        Request::builder().uri(uri).body(Body::empty()).unwrap()
    }

    #[tokio::test]
    async fn ui_language_setting() {
        let (state, dir) = test_state(false).await;
        // Default: Italian, also on the public endpoint (no auth).
        let (st, s) = send(&state, json_request("GET", "/api/v1/settings", "")).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(s["ui_language"], "it");
        let (st, ui) = send(&state, public_get("/api/v1/public/ui")).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(
            ui,
            serde_json::json!({ "language": "it", "theme": "system" })
        );

        // Validation: nothing is written on a 400, even with a valid text alongside.
        for bad in [r#""fr""#, r#""EN""#, r#""""#, "1"] {
            let body = format!(r#"{{"ui_language":{bad},"default_waiting_text":"Ciao"}}"#);
            let (st, _) = send(&state, json_request("PATCH", "/api/v1/settings", &body)).await;
            assert!(st.is_client_error(), "{bad}: {st}");
        }
        let (st, e) = send(
            &state,
            json_request("PATCH", "/api/v1/settings", r#"{"ui_language":"fr"}"#),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert!(e["error"].as_str().unwrap().contains("ui_language"));
        assert_eq!(
            db::settings::get(&state.db, db::settings::DEFAULT_WAITING_TEXT)
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            db::settings::get(&state.db, db::settings::UI_LANGUAGE)
                .await
                .unwrap(),
            None
        );

        // Switch to English: persisted, announced on the hub, public endpoint follows.
        let mut events = state.hub.subscribe_events();
        let (st, s) = send(
            &state,
            json_request("PATCH", "/api/v1/settings", r#"{"ui_language":"en"}"#),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(s["ui_language"], "en");
        assert_eq!(
            s["default_waiting_text"],
            "The live stream will start shortly"
        );
        assert_eq!(
            events.try_recv().unwrap(),
            crate::hub::HubEvent::SettingsChanged
        );
        assert_eq!(
            db::settings::get(&state.db, db::settings::UI_LANGUAGE)
                .await
                .unwrap()
                .as_deref(),
            Some("en")
        );
        let (_, ui) = send(&state, public_get("/api/v1/public/ui")).await;
        assert_eq!(ui["language"], "en");
        let (_, s) = send(&state, json_request("GET", "/api/v1/settings", "")).await;
        assert_eq!(s["ui_language"], "en");

        // Back to Italian: stored explicitly.
        let (st, s) = send(
            &state,
            json_request("PATCH", "/api/v1/settings", r#"{"ui_language":"it"}"#),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(s["ui_language"], "it");
        assert_eq!(s["default_waiting_text"], "La diretta inizierà a breve");
        assert_eq!(
            db::settings::get(&state.db, db::settings::UI_LANGUAGE)
                .await
                .unwrap()
                .as_deref(),
            Some("it")
        );
        assert_eq!(
            events.try_recv().unwrap(),
            crate::hub::HubEvent::SettingsChanged
        );
        // An empty patch changes nothing and announces nothing.
        let (st, _) = send(&state, json_request("PATCH", "/api/v1/settings", "{}")).await;
        assert_eq!(st, StatusCode::OK);
        assert!(events.try_recv().is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn ui_theme_setting() {
        let (state, dir) = test_state(false).await;
        // Default: system, also on the public endpoints (no auth).
        let (st, s) = send(&state, json_request("GET", "/api/v1/settings", "")).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(s["ui_theme"], "system");
        let (st, ui) = send(&state, public_get("/api/v1/public/ui")).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(ui["theme"], "system");
        db::broadcasts::create(&state.db, NewBroadcast::new("show", "Show"))
            .await
            .unwrap();
        let (_, p) = send(&state, public_get("/api/v1/public/broadcasts/show")).await;
        assert_eq!(p["ui_theme"], "system");

        // Validation: nothing is written on a 400, even with valid fields alongside.
        for bad in [
            r#""blue""#,
            r#""Light""#,
            r#""DARK""#,
            r#""""#,
            r#"" dark""#,
            "1",
        ] {
            let body =
                format!(r#"{{"ui_theme":{bad},"ui_language":"en","default_waiting_text":"Ciao"}}"#);
            let (st, _) = send(&state, json_request("PATCH", "/api/v1/settings", &body)).await;
            assert!(st.is_client_error(), "{bad}: {st}");
        }
        let (st, e) = send(
            &state,
            json_request("PATCH", "/api/v1/settings", r#"{"ui_theme":"blue"}"#),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert!(e["error"].as_str().unwrap().contains("ui_theme"));
        for key in [
            db::settings::UI_THEME,
            db::settings::UI_LANGUAGE,
            db::settings::DEFAULT_WAITING_TEXT,
        ] {
            assert_eq!(db::settings::get(&state.db, key).await.unwrap(), None);
        }

        // Each value: persisted, announced on the hub, public endpoints follow; the
        // language is untouched.
        let mut events = state.hub.subscribe_events();
        for theme in ["light", "dark", "system"] {
            let body = format!(r#"{{"ui_theme":"{theme}"}}"#);
            let (st, s) = send(&state, json_request("PATCH", "/api/v1/settings", &body)).await;
            assert_eq!(st, StatusCode::OK);
            assert_eq!(s["ui_theme"], theme);
            assert_eq!(s["ui_language"], "it");
            assert_eq!(
                events.try_recv().unwrap(),
                crate::hub::HubEvent::SettingsChanged
            );
            assert_eq!(
                db::settings::get(&state.db, db::settings::UI_THEME)
                    .await
                    .unwrap()
                    .as_deref(),
                Some(theme)
            );
            let (_, ui) = send(&state, public_get("/api/v1/public/ui")).await;
            assert_eq!(ui, serde_json::json!({ "language": "it", "theme": theme }));
            let (_, p) = send(&state, public_get("/api/v1/public/broadcasts/show")).await;
            assert_eq!(p["ui_theme"], theme);
            let (_, s) = send(&state, json_request("GET", "/api/v1/settings", "")).await;
            assert_eq!(s["ui_theme"], theme);
        }
        // Theme and language together: one event.
        let (st, s) = send(
            &state,
            json_request(
                "PATCH",
                "/api/v1/settings",
                r#"{"ui_theme":"dark","ui_language":"en"}"#,
            ),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(
            (&s["ui_theme"], &s["ui_language"]),
            (&"dark".into(), &"en".into())
        );
        assert_eq!(
            events.try_recv().unwrap(),
            crate::hub::HubEvent::SettingsChanged
        );
        assert!(events.try_recv().is_err());
        // An empty patch changes nothing and announces nothing.
        let (st, s) = send(&state, json_request("PATCH", "/api/v1/settings", "{}")).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(s["ui_theme"], "dark");
        assert!(events.try_recv().is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn builtin_waiting_text_follows_language() {
        let (state, dir) = test_state(false).await;
        // "own": custom text; "dflt": empty (global default); "api": the Italian built-in
        // text stored by POST /broadcasts when waiting_text is omitted (same as empty).
        let mut own = NewBroadcast::new("own", "Own");
        own.waiting_text = "Torniamo alle 21".into();
        let mut dflt = NewBroadcast::new("dflt", "Dflt");
        dflt.waiting_text = String::new();
        for nb in [own, dflt, NewBroadcast::new("api", "Api")] {
            db::broadcasts::create(&state.db, nb).await.unwrap();
        }
        let text_of = |name: &'static str| {
            let state = state.clone();
            async move {
                let (st, p) = send(
                    &state,
                    public_get(&format!("/api/v1/public/broadcasts/{name}")),
                )
                .await;
                assert_eq!(st, StatusCode::OK);
                (
                    p["waiting_text"].as_str().unwrap().to_string(),
                    p["ui_language"].as_str().unwrap().to_string(),
                )
            }
        };
        let it = db::broadcasts::DEFAULT_WAITING_TEXT;
        let en = "The live stream will start shortly";
        assert_eq!(text_of("own").await.0, "Torniamo alle 21");
        assert_eq!(text_of("dflt").await, (it.to_string(), "it".to_string()));
        assert_eq!(text_of("api").await.0, it);

        send(
            &state,
            json_request("PATCH", "/api/v1/settings", r#"{"ui_language":"en"}"#),
        )
        .await;
        assert_eq!(text_of("own").await.0, "Torniamo alle 21");
        assert_eq!(text_of("dflt").await, (en.to_string(), "en".to_string()));
        assert_eq!(text_of("api").await.0, en);
        // Stored texts are untouched.
        let api = db::broadcasts::find_by_name(&state.db, "api")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(api.waiting_text, it);

        // A custom global default applies to "dflt" and "api" (a stored built-in text
        // counts as no own text), never to custom texts...
        send(
            &state,
            json_request(
                "PATCH",
                "/api/v1/settings",
                r#"{"default_waiting_text":"Back soon"}"#,
            ),
        )
        .await;
        assert_eq!(text_of("dflt").await.0, "Back soon");
        assert_eq!(text_of("api").await.0, "Back soon");
        assert_eq!(text_of("own").await.0, "Torniamo alle 21");
        // ...and saving a built-in text as the default resets it (follows the language).
        let (_, s) = send(
            &state,
            json_request(
                "PATCH",
                "/api/v1/settings",
                r#"{"default_waiting_text":"La diretta inizierà a breve"}"#,
            ),
        )
        .await;
        assert_eq!(s["default_waiting_text"], en);
        assert_eq!(
            db::settings::get(&state.db, db::settings::DEFAULT_WAITING_TEXT)
                .await
                .unwrap(),
            None
        );
        assert_eq!(text_of("dflt").await.0, en);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
