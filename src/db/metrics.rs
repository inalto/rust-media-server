//! Viewer metrics persistence: `viewer_samples` (time series) and `viewer_sessions`.
//!
//! PHASE 1 STUBS. The `feat/metrics-recording` branch implements these functions
//! (and adds the query side used by `GET /broadcasts/{name}/metrics[/summary]`).
//! They already have their final signatures and are called by
//! [`crate::metrics::ViewerTracker`], so implementing them is enough to start
//! persisting viewer sessions.

use chrono::{DateTime, Utc};

use super::{Db, DbResult};
use crate::metrics::{Protocol, ViewerCounts};

/// A viewer session as recorded in `viewer_sessions`.
///
/// The tracker only knows the broadcast *name*; implementations resolve
/// `broadcast_id` with `(SELECT id FROM broadcasts WHERE name = ?)`.
#[derive(Debug, Clone)]
pub struct ViewerSessionRecord {
    pub id: String,
    pub broadcast_name: String,
    pub protocol: Protocol,
    pub ip: Option<String>,
    pub user_agent: Option<String>,
    pub started_at: DateTime<Utc>,
}

/// Insert a `viewer_sessions` row when a viewer session opens.
/// TODO(feat/metrics-recording): implement.
pub async fn insert_viewer_session(_db: &Db, _record: ViewerSessionRecord) -> DbResult<()> {
    Ok(())
}

/// Set `ended_at` of a `viewer_sessions` row when a viewer session closes.
/// TODO(feat/metrics-recording): implement.
pub async fn close_viewer_session(_db: &Db, _id: &str, _ended_at: DateTime<Utc>) -> DbResult<()> {
    Ok(())
}

/// Insert one `viewer_samples` row.
/// TODO(feat/metrics-recording): implement.
pub async fn insert_sample(
    _db: &Db,
    _broadcast_id: &str,
    _ts: DateTime<Utc>,
    _counts: ViewerCounts,
) -> DbResult<()> {
    Ok(())
}

/// Retention: delete `viewer_samples` older than `cutoff`; returns deleted rows.
/// TODO(feat/metrics-recording): implement.
pub async fn delete_samples_older_than(_db: &Db, _cutoff: DateTime<Utc>) -> DbResult<usize> {
    Ok(0)
}
