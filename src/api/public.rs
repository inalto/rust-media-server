//! Unauthenticated player API (ARCHITECTURE.md §8.2). Never exposes the stream key.
//!
//! - `GET /api/v1/public/broadcasts/{name}` → [`PublicBroadcast`]
//! - `GET /api/v1/public/broadcasts/{name}/events` → SSE `status` events `{status}`
//!   (sent immediately, then on every change) + `heartbeat` every 15 s.
//! - `GET /api/v1/public/ui` → `{"language": "it" | "en", "theme": "system" | "light" |
//!   "dark"}` (UI language and default theme settings, for pages rendered before login).

use std::convert::Infallible;

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::response::sse::Event;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use serde_json::json;
use tokio::sync::broadcast::error::RecvError;

use super::broadcasts::load;
use super::events::{HEARTBEAT, heartbeat_event, sse_response};
use super::{BroadcastStatus, PlaybackUrls, ViewContext, poster_path_for, waiting_url};
use crate::db::settings::{UiLanguage, UiTheme};
use crate::db::{self, Broadcast};
use crate::error::ApiResult;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/public/broadcasts/{name}", get(info))
        .route("/public/broadcasts/{name}/events", get(events))
        .route("/public/ui", get(ui))
}

/// `GET /api/v1/public/ui`.
#[derive(Debug, Clone, Serialize)]
pub struct PublicUi {
    pub language: UiLanguage,
    pub theme: UiTheme,
}

async fn ui(State(state): State<AppState>) -> ApiResult<Json<PublicUi>> {
    let (language, theme) = db::settings::ui_prefs(&state.db).await?;
    Ok(Json(PublicUi { language, theme }))
}

#[derive(Debug, Clone, Serialize)]
pub struct PublicSources {
    pub flv: Option<String>,
    pub ws_flv: Option<String>,
    pub hls: Option<String>,
    pub dash: Option<String>,
    /// WHEP endpoint; `null` when WebRTC is disabled (server or broadcast), unavailable,
    /// or the live video codec cannot be sent over WebRTC (not H.264).
    pub whep: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PublicBroadcast {
    pub name: String,
    pub title: String,
    pub status: BroadcastStatus,
    pub waiting_video_url: Option<String>,
    pub waiting_text: String,
    /// Poster of the waiting video (`<video>.jpg` next to it), if the file exists.
    pub poster_url: Option<String>,
    /// Playback URLs; `null` when the protocol is disabled.
    pub sources: PublicSources,
    pub enable_flv: bool,
    pub enable_hls: bool,
    pub enable_dash: bool,
    pub enable_webrtc: bool,
    /// UI language setting: the player's texts follow it (`?lang=` overrides it).
    pub ui_language: UiLanguage,
    /// Default theme setting: the player follows it unless the page has `?theme=`.
    pub ui_theme: UiTheme,
}

/// WebRTC usable for `b` right now (see [`PublicSources::whep`]).
fn whep_available(state: &AppState, b: &Broadcast) -> bool {
    if !(b.enable_webrtc && state.webrtc.available()) {
        return false;
    }
    match state.hub.get(&b.name) {
        Some(s) => s.info().video_codec.is_none_or(|c| c == "h264"),
        None => true,
    }
}

pub fn public_view(state: &AppState, ctx: &ViewContext, b: &Broadcast) -> PublicBroadcast {
    let urls = PlaybackUrls::new(&ctx.base, &b.name);
    let enable_hls = b.enable_hls && state.config.hls.enabled;
    let enable_dash = b.enable_dash && state.config.dash.enabled;
    let enable_webrtc = whep_available(state, b);
    let waiting_rel = ctx.waiting_video_path(b);
    let poster_url = waiting_rel.as_deref().and_then(|rel| {
        let poster = poster_path_for(rel);
        let safe = std::path::Path::new(&poster)
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)));
        (safe && state.config.waiting_dir().join(&poster).is_file())
            .then(|| waiting_url(&ctx.base, &poster))
    });
    PublicBroadcast {
        name: b.name.clone(),
        title: b.title.clone(),
        status: BroadcastStatus::of(b, &state.hub),
        waiting_video_url: waiting_rel
            .as_deref()
            .map(|rel| waiting_url(&ctx.base, rel)),
        waiting_text: ctx.waiting_text(b),
        poster_url,
        sources: PublicSources {
            flv: b.enable_flv.then(|| urls.flv.clone()),
            ws_flv: b.enable_flv.then(|| urls.ws_flv.clone()),
            hls: enable_hls.then(|| urls.hls.clone()),
            dash: enable_dash.then(|| urls.dash.clone()),
            whep: enable_webrtc.then(|| urls.whep.clone()),
        },
        enable_flv: b.enable_flv,
        enable_hls,
        enable_dash,
        enable_webrtc,
        ui_language: ctx.language,
        ui_theme: ctx.theme,
    }
}

async fn info(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<PublicBroadcast>> {
    let b = load(&state, &name).await?;
    let ctx = ViewContext::load(&state, &headers).await?;
    Ok(Json(public_view(&state, &ctx, &b)))
}

fn status_event(status: BroadcastStatus) -> Event {
    Event::default()
        .event("status")
        .data(json!({ "status": status }).to_string())
}

async fn events(State(state): State<AppState>, Path(name): Path<String>) -> ApiResult<Response> {
    let b = load(&state, &name).await?;
    // Subscribe before computing the initial status so no change is missed.
    let mut rx = state.hub.subscribe_events();
    let mut current = BroadcastStatus::of(&b, &state.hub);
    let shutdown = state.shutdown.clone();
    let stream = async_stream::stream! {
        yield Ok::<Event, Infallible>(status_event(current));
        let mut tick = tokio::time::interval(HEARTBEAT);
        tick.tick().await;
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = tick.tick() => yield Ok(heartbeat_event()),
                r = rx.recv() => {
                    let relevant = match &r {
                        Ok(ev) => ev.name() == name && !matches!(ev, crate::hub::HubEvent::ViewerCountChanged { .. } | crate::hub::HubEvent::Metadata { .. }),
                        Err(RecvError::Lagged(_)) => true,
                        Err(RecvError::Closed) => break,
                    };
                    if !relevant {
                        continue;
                    }
                    let next = match db::broadcasts::find_by_name(&state.db, &name).await {
                        Ok(Some(b)) => BroadcastStatus::of(&b, &state.hub),
                        Ok(None) => BroadcastStatus::Offline,
                        Err(e) => {
                            tracing::warn!(error = %e, "public SSE: failed to load broadcast");
                            continue;
                        }
                    };
                    if next != current {
                        current = next;
                        yield Ok(status_event(current));
                    }
                }
            }
        }
    };
    Ok(sse_response(stream))
}
