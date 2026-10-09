//! `GET /api/v1/system`: server information.

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;

use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new().route("/system", get(system))
}

#[derive(Debug, Serialize)]
pub struct FfmpegInfo {
    pub available: bool,
    pub path: Option<String>,
    pub version: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct HubInfo {
    pub live_streams: usize,
}

#[derive(Debug, Serialize)]
pub struct SystemInfo {
    pub version: &'static str,
    pub uptime_secs: u64,
    pub ffmpeg: FfmpegInfo,
    pub rtmp_bind: String,
    pub http_bind: String,
    pub public_url: String,
    pub hub: HubInfo,
    pub webrtc: crate::webrtc::WebRtcStatus,
}

async fn system(State(state): State<AppState>) -> Json<SystemInfo> {
    let version = state.ffmpeg_version().await;
    Json(SystemInfo {
        version: env!("CARGO_PKG_VERSION"),
        uptime_secs: state.uptime().as_secs(),
        ffmpeg: FfmpegInfo {
            available: state.ffmpeg.is_some(),
            path: state.ffmpeg.as_ref().map(|p| p.display().to_string()),
            version,
        },
        rtmp_bind: state.config.rtmp.bind.clone(),
        http_bind: state.config.server.http_bind.clone(),
        public_url: state.config.server.public_url.clone(),
        hub: HubInfo {
            live_streams: state.hub.live_count(),
        },
        webrtc: state.webrtc.status(),
    })
}
