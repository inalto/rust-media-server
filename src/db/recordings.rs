//! `recordings` repository.
//!
//! PHASE 1: the [`Recording`] type is final (it is part of the API contract and of
//! `BroadcastView.live.recording`); the repository functions are documented stubs
//! that the `feat/metrics-recording` branch implements.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{Db, DbResult};

/// `recordings.status` values.
pub const STATUS_RECORDING: &str = "recording";
pub const STATUS_PROCESSING: &str = "processing";
pub const STATUS_READY: &str = "ready";
pub const STATUS_FAILED: &str = "failed";

/// One recording (row of `recordings`), serialized as-is by the API.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Recording {
    pub id: String,
    pub broadcast_id: String,
    pub stream_session_id: Option<String>,
    /// Path relative to `data/recordings/`.
    pub path: String,
    /// "flv" | "mp4"
    pub format: String,
    /// "recording" | "processing" | "ready" | "failed"
    pub status: String,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub size_bytes: u64,
    pub duration_secs: Option<f64>,
    pub error: Option<String>,
}

/// TODO(feat/metrics-recording): insert a row.
pub async fn insert(_db: &Db, _rec: &Recording) -> DbResult<()> {
    Ok(())
}

/// TODO(feat/metrics-recording): update status/size/duration/error/ended_at of a row.
pub async fn update(_db: &Db, _rec: &Recording) -> DbResult<()> {
    Ok(())
}

/// TODO(feat/metrics-recording): fetch one row.
pub async fn get(_db: &Db, _id: &str) -> DbResult<Option<Recording>> {
    Ok(None)
}

/// TODO(feat/metrics-recording): list rows (optionally for one broadcast id), newest first.
pub async fn list(_db: &Db, _broadcast_id: Option<&str>) -> DbResult<Vec<Recording>> {
    Ok(Vec::new())
}

/// TODO(feat/metrics-recording): delete a row, returning it (caller deletes the file).
pub async fn delete(_db: &Db, _id: &str) -> DbResult<Option<Recording>> {
    Ok(None)
}
