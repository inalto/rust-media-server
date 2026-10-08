//! Metrics endpoints (ARCHITECTURE.md §8.1).
//!
//! - `/broadcasts/{name}/sessions`, `/metrics/overview`: live, from the in-memory
//!   `ViewerTracker`;
//! - `/broadcasts/{name}/metrics?from&to&step`, `/broadcasts/{name}/metrics/summary?from&to`:
//!   from the persisted samples/sessions (logic in `metrics::query`).

use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, State};
use axum::routing::get;
use axum::{Json, Router};
use chrono::Utc;
use serde::{Deserialize, Serialize};

use super::BroadcastStatus;
use super::broadcasts::load;
use crate::db;
use crate::error::{ApiError, ApiResult};
use crate::metrics::query::{self, Series, Summary};
use crate::metrics::{SessionInfo, ViewerCounts};
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/broadcasts/{name}/metrics", get(metrics))
        .route("/broadcasts/{name}/metrics/summary", get(summary))
        .route("/broadcasts/{name}/sessions", get(sessions))
        .route("/metrics/overview", get(overview))
}

/// `?from=<rfc3339>&to=<rfc3339>&step=<secs>` (all optional).
#[derive(Debug, Default, Deserialize)]
pub struct RangeQuery {
    pub from: Option<String>,
    pub to: Option<String>,
    pub step: Option<String>,
}

impl RangeQuery {
    fn range(&self) -> ApiResult<(chrono::DateTime<Utc>, chrono::DateTime<Utc>)> {
        query::resolve_range(self.from.as_deref(), self.to.as_deref(), Utc::now())
            .map_err(ApiError::BadRequest)
    }
}

/// `{from, to, step, points:[{t,total,flv,ws_flv,hls,dash,webrtc}]}` (max per bucket,
/// zero-filled, at most 600 points).
async fn metrics(
    State(state): State<AppState>,
    Path(name): Path<String>,
    q: Result<Query<RangeQuery>, QueryRejection>,
) -> ApiResult<Json<Series>> {
    let Query(q) = q?;
    let (from, to) = q.range()?;
    let step = match q.step.as_deref().filter(|s| !s.trim().is_empty()) {
        Some(s) => Some(query::parse_step(s).map_err(ApiError::BadRequest)?),
        None => None,
    };
    let b = load(&state, &name).await?;
    let min_step = state.config.metrics.sample_interval_secs;
    Ok(Json(
        query::series(&state.db, &b.id, from, to, step, min_step).await?,
    ))
}

/// `{from, to, current, peak:{value,at}|null, sessions_total, avg_watch_secs, live_since|null, bytes_in}`.
async fn summary(
    State(state): State<AppState>,
    Path(name): Path<String>,
    q: Result<Query<RangeQuery>, QueryRejection>,
) -> ApiResult<Json<Summary>> {
    let Query(q) = q?;
    let (from, to) = q.range()?;
    let b = load(&state, &name).await?;
    Ok(Json(
        query::summary(&state.db, &state.hub, &state.tracker, &b, from, to).await?,
    ))
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
