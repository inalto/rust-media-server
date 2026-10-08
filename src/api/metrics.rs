//! Metrics endpoints (ARCHITECTURE.md §8.1).
//!
//! Phase 1 implements the live-only endpoints (`/broadcasts/{name}/sessions`,
//! `/metrics/overview`) from the in-memory `ViewerTracker`. The time-series endpoints
//! (`/broadcasts/{name}/metrics`, `/broadcasts/{name}/metrics/summary`) return 501 until
//! the `feat/metrics-recording` branch implements the sampler and `db::metrics`.

use axum::extract::{Path, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;

use super::BroadcastStatus;
use super::broadcasts::load;
use crate::db;
use crate::error::{ApiError, ApiResult};
use crate::metrics::{SessionInfo, ViewerCounts};
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/broadcasts/{name}/metrics", get(metrics))
        .route("/broadcasts/{name}/metrics/summary", get(summary))
        .route("/broadcasts/{name}/sessions", get(sessions))
        .route("/metrics/overview", get(overview))
}

/// TODO(feat/metrics-recording): `{from,to,step,points:[{t,total,flv,ws_flv,hls,dash,webrtc}]}`.
async fn metrics(Path(_name): Path<String>) -> ApiError {
    ApiError::NotImplemented
}

/// TODO(feat/metrics-recording): `{current, peak:{value,at}, sessions_total, avg_watch_secs, live_since?, bytes_in}`.
async fn summary(Path(_name): Path<String>) -> ApiError {
    ApiError::NotImplemented
}

/// Currently connected viewers: `[{id, protocol, ip, user_agent, started_at}]`.
async fn sessions(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> ApiResult<Json<Vec<SessionInfo>>> {
    let b = load(&state, &name).await?;
    Ok(Json(state.tracker.sessions(&b.name)))
}

#[derive(Debug, Serialize)]
pub struct OverviewEntry {
    pub name: String,
    pub title: String,
    pub status: BroadcastStatus,
    pub viewers: ViewerCounts,
}

#[derive(Debug, Serialize)]
pub struct Overview {
    pub live_broadcasts: usize,
    pub total_viewers: ViewerCounts,
    pub per_broadcast: Vec<OverviewEntry>,
}

async fn overview(State(state): State<AppState>) -> ApiResult<Json<Overview>> {
    let all = db::broadcasts::list(&state.db).await?;
    let mut total = ViewerCounts::default();
    let mut live = 0;
    let per_broadcast = all
        .into_iter()
        .map(|b| {
            let viewers = state.tracker.counts(&b.name);
            total.add(&viewers);
            if state.hub.is_live(&b.name) {
                live += 1;
            }
            OverviewEntry {
                status: BroadcastStatus::of(&b, &state.hub),
                name: b.name,
                title: b.title,
                viewers,
            }
        })
        .collect();
    Ok(Json(Overview {
        live_broadcasts: live,
        total_viewers: total,
        per_broadcast,
    }))
}
