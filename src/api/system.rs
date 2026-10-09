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
    /// Streams published on this node.
    pub live_streams: usize,
    /// Relays of streams published on other nodes (cluster mode).
    pub relays: usize,
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
    /// Video on demand: `{enabled, available, inbox_dir, jobs_running, queued, ...}`.
    pub vod: crate::vod::VodStatus,
    /// `{enabled, node_id, backend, nodes: [...]}` (docs/API.md §4.5).
    pub cluster: crate::cluster::ClusterInfo,
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
            live_streams: state.hub.local_count(),
            relays: state
                .hub
                .live_count()
                .saturating_sub(state.hub.local_count()),
        },
        webrtc: state.webrtc.status(),
        vod: state.vod.status().await,
        cluster: state.cluster.info().await,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    #[tokio::test]
    async fn system_reports_the_cluster_and_public_port() {
        let (state, dir) = crate::api::waiting::tests::test_state(false).await;
        let app = routes().with_state(state.clone());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/system")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["cluster"]["enabled"], false);
        assert_eq!(v["cluster"]["backend"], "sqlite");
        assert_eq!(v["cluster"]["node_id"], state.cluster.node_id());
        assert!(v["cluster"]["nodes"].as_array().unwrap().is_empty());
        assert_eq!(v["webrtc"]["public_port"], v["webrtc"]["udp_port"]);
        let _ = std::fs::remove_dir_all(dir);
    }
}
