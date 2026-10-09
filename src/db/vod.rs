//! `vod_items` repository (video on demand, ARCHITECTURE.md §19).
//!
//! One row per video: the source file under `data/vod/<id>/`, its probe results, the
//! transcoding status (`queued` → `processing` → `ready` | `failed`) and the renditions
//! actually produced. Jobs are claimed with a conditional `UPDATE` (see [`claim`]) so
//! several runners (threads, or nodes sharing the database) never process the same
//! item twice. A claim records the node (`claimed_by`) and a lease (`lease_until`,
//! renewed by the running job, docs/CLUSTER.md §3.7): `ready`/`failed` are only written
//! by the lease holder, and expired leases are re-queued (`attempts` bounds the retries).

use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use sqlx_core::row::Row as _;
use sqlx_postgres::PgRow;

use super::pg::{get_opt_u32, get_u32, get_u64, i64_of, q, q_dyn};
use super::{Db, DbResult, fmt_ts, now_ts, parse_ts};

/// `vod_items.status` values.
pub const STATUS_QUEUED: &str = "queued";
pub const STATUS_PROCESSING: &str = "processing";
pub const STATUS_READY: &str = "ready";
pub const STATUS_FAILED: &str = "failed";
pub const STATUSES: [&str; 4] = [
    STATUS_QUEUED,
    STATUS_PROCESSING,
    STATUS_READY,
    STATUS_FAILED,
];

/// `vod_items.origin` values.
pub const ORIGIN_UPLOAD: &str = "upload";
pub const ORIGIN_INBOX: &str = "inbox";
/// Encoding of a file of a user's bucket (phase 8A): `source_path` is the object path.
pub const ORIGIN_STORAGE: &str = "storage";

/// `vod_items.visibility` values: a private item needs a signed URL (phase 8A).
pub const VISIBILITY_PUBLIC: &str = "public";
pub const VISIBILITY_PRIVATE: &str = "private";
pub const VISIBILITIES: [&str; 2] = [VISIBILITY_PUBLIC, VISIBILITY_PRIVATE];

/// One rendition produced by a transcode (stored as JSON in `renditions_json`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Rendition {
    pub name: String,
    pub width: u32,
    pub height: u32,
    /// Target video bitrate; 0 = constant quality (CRF).
    pub video_kbps: u32,
    pub audio_kbps: u32,
    /// Measured peak bitrate (video + audio, bit/s) as announced in the HLS master.
    #[serde(default)]
    pub bandwidth: u64,
}

/// One row of `vod_items`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VodItem {
    pub id: String,
    pub title: String,
    pub description: String,
    /// Source file, relative to `data/vod/` (`<id>/source.<ext>`); for an encoding of a
    /// bucket file (`bucket_id` set) the object path inside the bucket (`/videos/a.mp4`).
    pub source_path: String,
    /// Original file name (download name).
    pub source_name: String,
    pub source_size: u64,
    pub source_duration_secs: Option<f64>,
    pub source_width: Option<u32>,
    pub source_height: Option<u32>,
    pub source_codec: Option<String>,
    /// `upload` | `inbox`
    pub origin: String,
    /// `queued` | `processing` | `ready` | `failed`
    pub status: String,
    /// 0-100 (meaningful while `processing`; 100 when `ready`).
    pub progress: u32,
    pub ladder: String,
    pub renditions: Vec<Rendition>,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub ready_at: Option<DateTime<Utc>>,
    pub plays: u64,
    /// Owner (phase 8A); `None` = admin library.
    pub user_id: Option<String>,
    /// Bucket of the original file (encodings of bucket files).
    pub bucket_id: Option<String>,
    /// `bucket_objects.id` of the original file.
    pub object_id: Option<String>,
    /// `public` | `private` (signed URLs needed for playback).
    pub visibility: String,
    /// Size of the encoding output (renditions + poster), counted in the quotas.
    pub output_bytes: u64,
}

impl VodItem {
    /// A new `queued` item.
    pub fn new(id: &str, title: &str, source_path: &str, ladder: &str, origin: &str) -> Self {
        let now = parse_ts(&now_ts());
        Self {
            id: id.into(),
            title: title.into(),
            description: String::new(),
            source_path: source_path.into(),
            source_name: String::new(),
            source_size: 0,
            source_duration_secs: None,
            source_width: None,
            source_height: None,
            source_codec: None,
            origin: origin.into(),
            status: STATUS_QUEUED.into(),
            progress: 0,
            ladder: ladder.into(),
            renditions: Vec::new(),
            error: None,
            created_at: now,
            updated_at: now,
            ready_at: None,
            plays: 0,
            user_id: None,
            bucket_id: None,
            object_id: None,
            visibility: VISIBILITY_PUBLIC.into(),
            output_bytes: 0,
        }
    }

    pub fn is_ready(&self) -> bool {
        self.status == STATUS_READY
    }

    /// Playback needs a signed URL.
    pub fn is_private(&self) -> bool {
        self.visibility == VISIBILITY_PRIVATE
    }
}

const COLUMNS: &str = "id, title, description, source_path, source_name, source_size, \
     source_duration_secs, source_width, source_height, source_codec, origin, status, progress, \
     ladder, renditions_json, error, created_at, updated_at, ready_at, plays, user_id, bucket_id, \
     object_id, visibility, output_bytes";

fn from_row(r: &Row<'_>) -> rusqlite::Result<VodItem> {
    let renditions: String = r.get(14)?;
    Ok(VodItem {
        id: r.get(0)?,
        title: r.get(1)?,
        description: r.get(2)?,
        source_path: r.get(3)?,
        source_name: r.get(4)?,
        source_size: r.get::<_, i64>(5)?.max(0) as u64,
        source_duration_secs: r.get(6)?,
        source_width: r
            .get::<_, Option<i64>>(7)?
            .map(|v| v.clamp(0, u32::MAX as i64) as u32),
        source_height: r
            .get::<_, Option<i64>>(8)?
            .map(|v| v.clamp(0, u32::MAX as i64) as u32),
        source_codec: r.get(9)?,
        origin: r.get(10)?,
        status: r.get(11)?,
        progress: r.get::<_, i64>(12)?.clamp(0, 100) as u32,
        ladder: r.get(13)?,
        // Malformed JSON (manual edits) is shown as "no renditions", never an error.
        renditions: serde_json::from_str(&renditions).unwrap_or_default(),
        error: r.get(15)?,
        created_at: parse_ts(&r.get::<_, String>(16)?),
        updated_at: parse_ts(&r.get::<_, String>(17)?),
        ready_at: r.get::<_, Option<String>>(18)?.map(|s| parse_ts(&s)),
        plays: r.get::<_, i64>(19)?.max(0) as u64,
        user_id: r.get(20)?,
        bucket_id: r.get(21)?,
        object_id: r.get(22)?,
        visibility: r.get(23)?,
        output_bytes: r.get::<_, i64>(24)?.max(0) as u64,
    })
}

/// Lease of a claimed job: renewed every [`LEASE_RENEW_SECS`] by the running job; a row
/// whose lease expired is re-queued by any node ([`requeue_expired`]) or, for this
/// node's own rows, at startup ([`requeue_interrupted`]).
pub const LEASE_SECS: u64 = 60;
/// Renewal period of a job lease (with the progress updates).
pub const LEASE_RENEW_SECS: u64 = 15;
/// A job interrupted (crash, lost lease) this many times is marked `failed` instead of
/// being re-queued.
pub const MAX_ATTEMPTS: i64 = 3;
/// Error of an item that exhausted [`MAX_ATTEMPTS`].
pub const TOO_MANY_ATTEMPTS: &str =
    "transcoding interrupted too many times (node crashed or lost its lease)";

/// SQLite lease deadline (`now + LEASE_SECS`, RFC 3339 text like every timestamp).
fn sqlite_lease_until() -> String {
    fmt_ts(&(Utc::now() + chrono::Duration::seconds(LEASE_SECS as i64)))
}

/// Insert a new row.
pub async fn insert(db: &Db, item: &VodItem) -> DbResult<()> {
    let it = item.clone();
    let renditions = serde_json::to_string(&it.renditions).unwrap_or_else(|_| "[]".into());
    if let Some(pool) = db.pg() {
        q_dyn(format!(
            "INSERT INTO vod_items ({COLUMNS}) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, \
             $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21, $22, $23, $24, $25)"
        ))
        .bind(it.id)
        .bind(it.title)
        .bind(it.description)
        .bind(it.source_path)
        .bind(it.source_name)
        .bind(i64_of(it.source_size))
        .bind(it.source_duration_secs)
        .bind(it.source_width.map(i64::from))
        .bind(it.source_height.map(i64::from))
        .bind(it.source_codec)
        .bind(it.origin)
        .bind(it.status)
        .bind(i64::from(it.progress))
        .bind(it.ladder)
        .bind(renditions)
        .bind(it.error)
        .bind(fmt_ts(&it.created_at))
        .bind(fmt_ts(&it.updated_at))
        .bind(it.ready_at.as_ref().map(fmt_ts))
        .bind(i64_of(it.plays))
        .bind(it.user_id)
        .bind(it.bucket_id)
        .bind(it.object_id)
        .bind(it.visibility)
        .bind(i64_of(it.output_bytes))
        .execute(pool)
        .await?;
        return Ok(());
    }
    db.call(move |c| {
        c.execute(
            &format!(
                "INSERT INTO vod_items ({COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, \
                 ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25)"
            ),
            params![
                it.id,
                it.title,
                it.description,
                it.source_path,
                it.source_name,
                i64_of(it.source_size),
                it.source_duration_secs,
                it.source_width,
                it.source_height,
                it.source_codec,
                it.origin,
                it.status,
                it.progress,
                it.ladder,
                renditions,
                it.error,
                fmt_ts(&it.created_at),
                fmt_ts(&it.updated_at),
                it.ready_at.as_ref().map(fmt_ts),
                i64_of(it.plays),
                it.user_id,
                it.bucket_id,
                it.object_id,
                it.visibility,
                i64_of(it.output_bytes),
            ],
        )?;
        Ok(())
    })
    .await
}

/// One row.
pub async fn get(db: &Db, id: &str) -> DbResult<Option<VodItem>> {
    let id = id.to_string();
    if let Some(pool) = db.pg() {
        return pg_get(pool, &id).await;
    }
    db.call(move |c| {
        Ok(c.query_row(
            &format!("SELECT {COLUMNS} FROM vod_items WHERE id = ?1"),
            [id],
            from_row,
        )
        .optional()?)
    })
    .await
}

async fn pg_get(pool: &super::PgPool, id: &str) -> DbResult<Option<VodItem>> {
    q_dyn(format!("SELECT {COLUMNS} FROM vod_items WHERE id = $1"))
        .bind(id.to_string())
        .fetch_optional(pool)
        .await?
        .as_ref()
        .map(pg_row)
        .transpose()
}

/// All rows (or those with `status`), newest first.
pub async fn list(db: &Db, status: Option<&str>) -> DbResult<Vec<VodItem>> {
    let status = status.map(str::to_string);
    if let Some(pool) = db.pg() {
        let rows =
            match status {
                Some(s) => q_dyn(format!(
                    "SELECT {COLUMNS} FROM vod_items WHERE status = $1 ORDER BY created_at DESC, id"
                ))
                .bind(s)
                .fetch_all(pool)
                .await?,
                None => {
                    q_dyn(format!(
                        "SELECT {COLUMNS} FROM vod_items ORDER BY created_at DESC, id"
                    ))
                    .fetch_all(pool)
                    .await?
                }
            };
        return rows.iter().map(pg_row).collect();
    }
    db.call(move |c| {
        let rows = match status {
            Some(s) => {
                let mut st = c.prepare(&format!(
                    "SELECT {COLUMNS} FROM vod_items WHERE status = ?1 ORDER BY created_at DESC, id"
                ))?;
                st.query_map([s], from_row)?
                    .collect::<Result<Vec<_>, _>>()?
            }
            None => {
                let mut st = c.prepare(&format!(
                    "SELECT {COLUMNS} FROM vod_items ORDER BY created_at DESC, id"
                ))?;
                st.query_map([], from_row)?.collect::<Result<Vec<_>, _>>()?
            }
        };
        Ok(rows)
    })
    .await
}

/// Ids of the `queued` items, oldest first (job order).
pub async fn queued_ids(db: &Db, limit: usize) -> DbResult<Vec<String>> {
    if let Some(pool) = db.pg() {
        return q(
            "SELECT id FROM vod_items WHERE status = 'queued' ORDER BY created_at, id LIMIT $1",
        )
        .bind(i64_of(limit as u64))
        .fetch_all(pool)
        .await?
        .iter()
        .map(|r| Ok(r.try_get::<String, _>(0)?))
        .collect();
    }
    db.call(move |c| {
        let mut st = c.prepare(
            "SELECT id FROM vod_items WHERE status = 'queued' ORDER BY created_at, id LIMIT ?1",
        )?;
        let rows = st
            .query_map([i64_of(limit as u64)], |r| r.get(0))?
            .collect::<Result<Vec<String>, _>>()?;
        Ok(rows)
    })
    .await
}

/// Number of items per status: `(queued, processing)`.
pub async fn pending_counts(db: &Db) -> DbResult<(u64, u64)> {
    if let Some(pool) = db.pg() {
        let r = q("SELECT COUNT(*) FILTER (WHERE status = 'queued'), \
                   COUNT(*) FILTER (WHERE status = 'processing') FROM vod_items")
        .fetch_one(pool)
        .await?;
        return Ok((get_u64(&r, 0)?, get_u64(&r, 1)?));
    }
    db.call(|c| {
        let count = |s: &str| -> rusqlite::Result<u64> {
            c.query_row(
                "SELECT COUNT(*) FROM vod_items WHERE status = ?1",
                [s],
                |r| r.get::<_, i64>(0),
            )
            .map(|n| n.max(0) as u64)
        };
        Ok((count(STATUS_QUEUED)?, count(STATUS_PROCESSING)?))
    })
    .await
}

/// Atomically take a `queued` item: `queued` → `processing`, `claimed_by` = this node,
/// lease for [`LEASE_SECS`], `attempts + 1`. False when another runner (or a delete)
/// got there first (the conditional `UPDATE` is atomic on both backends).
pub async fn claim(db: &Db, id: &str) -> DbResult<bool> {
    let (id, node) = (id.to_string(), db.node_id().to_string());
    if let Some(pool) = db.pg() {
        let n = q("UPDATE vod_items SET status = 'processing', progress = 0, error = NULL, \
                   updated_at = $2, claimed_by = $3, lease_until = now() + make_interval(secs => $4), \
                   attempts = attempts + 1 WHERE id = $1 AND status = 'queued'")
        .bind(id)
        .bind(now_ts())
        .bind(node)
        .bind(LEASE_SECS as f64)
        .execute(pool)
        .await?
        .rows_affected();
        return Ok(n == 1);
    }
    db.call(move |c| {
        let n = c.execute(
            "UPDATE vod_items SET status = 'processing', progress = 0, error = NULL, updated_at = ?2, \
             claimed_by = ?3, lease_until = ?4, attempts = attempts + 1 \
             WHERE id = ?1 AND status = 'queued'",
            params![id, now_ts(), node, sqlite_lease_until()],
        )?;
        Ok(n == 1)
    })
    .await
}

/// Extend the lease of a job claimed by this node. False when the lease was lost (row
/// deleted, re-queued after expiry, or claimed by another node).
pub async fn renew_lease(db: &Db, id: &str) -> DbResult<bool> {
    let (id, node) = (id.to_string(), db.node_id().to_string());
    if let Some(pool) = db.pg() {
        let n = q(
            "UPDATE vod_items SET lease_until = now() + make_interval(secs => $3) \
                   WHERE id = $1 AND status = 'processing' AND claimed_by = $2",
        )
        .bind(id)
        .bind(node)
        .bind(LEASE_SECS as f64)
        .execute(pool)
        .await?
        .rows_affected();
        return Ok(n == 1);
    }
    db.call(move |c| {
        let n = c.execute(
            "UPDATE vod_items SET lease_until = ?3 \
             WHERE id = ?1 AND status = 'processing' AND claimed_by = ?2",
            params![id, node, sqlite_lease_until()],
        )?;
        Ok(n == 1)
    })
    .await
}

/// Probe results of the source (set when the job starts).
pub async fn set_source_info(
    db: &Db,
    id: &str,
    duration: Option<f64>,
    width: Option<u32>,
    height: Option<u32>,
    codec: Option<String>,
) -> DbResult<bool> {
    let id = id.to_string();
    if let Some(pool) = db.pg() {
        let n = q(
            "UPDATE vod_items SET source_duration_secs = $2, source_width = $3, \
                   source_height = $4, source_codec = $5, updated_at = $6 WHERE id = $1",
        )
        .bind(id)
        .bind(duration)
        .bind(width.map(i64::from))
        .bind(height.map(i64::from))
        .bind(codec)
        .bind(now_ts())
        .execute(pool)
        .await?
        .rows_affected();
        return Ok(n == 1);
    }
    db.call(move |c| {
        let n = c.execute(
            "UPDATE vod_items SET source_duration_secs = ?2, source_width = ?3, source_height = ?4, \
             source_codec = ?5, updated_at = ?6 WHERE id = ?1",
            params![id, duration, width, height, codec, now_ts()],
        )?;
        Ok(n == 1)
    })
    .await
}

/// Progress of a `processing` item claimed by this node (0-100); also renews the lease.
pub async fn set_progress(db: &Db, id: &str, progress: u32) -> DbResult<bool> {
    let (id, node) = (id.to_string(), db.node_id().to_string());
    if let Some(pool) = db.pg() {
        let n = q("UPDATE vod_items SET progress = $2, updated_at = $3, \
                   lease_until = now() + make_interval(secs => $5) \
                   WHERE id = $1 AND status = 'processing' AND claimed_by = $4")
        .bind(id)
        .bind(i64::from(progress.min(100)))
        .bind(now_ts())
        .bind(node)
        .bind(LEASE_SECS as f64)
        .execute(pool)
        .await?
        .rows_affected();
        return Ok(n == 1);
    }
    db.call(move |c| {
        let n = c.execute(
            "UPDATE vod_items SET progress = ?2, updated_at = ?3, lease_until = ?5 \
             WHERE id = ?1 AND status = 'processing' AND claimed_by = ?4",
            params![id, progress.min(100), now_ts(), node, sqlite_lease_until()],
        )?;
        Ok(n == 1)
    })
    .await
}

/// `processing` → `ready` with the produced renditions, only while this node holds the
/// claim. False when the row is gone, no longer `processing` (deleted / re-queued
/// meanwhile) or claimed by another node (lease lost: the result is discarded).
pub async fn mark_ready(db: &Db, id: &str, renditions: &[Rendition]) -> DbResult<bool> {
    let (id, node) = (id.to_string(), db.node_id().to_string());
    let json = serde_json::to_string(renditions).unwrap_or_else(|_| "[]".into());
    if let Some(pool) = db.pg() {
        let n = q(
            "UPDATE vod_items SET status = 'ready', progress = 100, renditions_json = $2, \
                   error = NULL, updated_at = $3, ready_at = $3, lease_until = NULL \
                   WHERE id = $1 AND status = 'processing' AND claimed_by = $4",
        )
        .bind(id)
        .bind(json)
        .bind(now_ts())
        .bind(node)
        .execute(pool)
        .await?
        .rows_affected();
        return Ok(n == 1);
    }
    db.call(move |c| {
        let now = now_ts();
        let n = c.execute(
            "UPDATE vod_items SET status = 'ready', progress = 100, renditions_json = ?2, error = NULL, \
             updated_at = ?3, ready_at = ?3, lease_until = NULL \
             WHERE id = ?1 AND status = 'processing' AND claimed_by = ?4",
            params![id, json, now, node],
        )?;
        Ok(n == 1)
    })
    .await
}

/// `processing` → `failed` with an error message (only while this node holds the claim).
pub async fn mark_failed(db: &Db, id: &str, error: &str) -> DbResult<bool> {
    let (id, node, error) = (id.to_string(), db.node_id().to_string(), error.to_string());
    if let Some(pool) = db.pg() {
        let n = q(
            "UPDATE vod_items SET status = 'failed', error = $2, renditions_json = '[]', \
                   updated_at = $3, lease_until = NULL \
                   WHERE id = $1 AND status = 'processing' AND claimed_by = $4",
        )
        .bind(id)
        .bind(error)
        .bind(now_ts())
        .bind(node)
        .execute(pool)
        .await?
        .rows_affected();
        return Ok(n == 1);
    }
    db.call(move |c| {
        let n = c.execute(
            "UPDATE vod_items SET status = 'failed', error = ?2, renditions_json = '[]', updated_at = ?3, \
             lease_until = NULL WHERE id = ?1 AND status = 'processing' AND claimed_by = ?4",
            params![id, error, now_ts(), node],
        )?;
        Ok(n == 1)
    })
    .await
}

/// `processing` → `queued` (job of this node interrupted by a shutdown). A graceful
/// interruption does not count as an attempt.
pub async fn unclaim(db: &Db, id: &str) -> DbResult<bool> {
    let (id, node) = (id.to_string(), db.node_id().to_string());
    if let Some(pool) = db.pg() {
        let n = q(
            "UPDATE vod_items SET status = 'queued', progress = 0, updated_at = $2, \
                   claimed_by = NULL, lease_until = NULL, \
                   attempts = CASE WHEN attempts > 0 THEN attempts - 1 ELSE 0 END \
                   WHERE id = $1 AND status = 'processing' AND claimed_by = $3",
        )
        .bind(id)
        .bind(now_ts())
        .bind(node)
        .execute(pool)
        .await?
        .rows_affected();
        return Ok(n == 1);
    }
    db.call(move |c| {
        let n = c.execute(
            "UPDATE vod_items SET status = 'queued', progress = 0, updated_at = ?2, \
             claimed_by = NULL, lease_until = NULL, \
             attempts = CASE WHEN attempts > 0 THEN attempts - 1 ELSE 0 END \
             WHERE id = ?1 AND status = 'processing' AND claimed_by = ?3",
            params![id, now_ts(), node],
        )?;
        Ok(n == 1)
    })
    .await
}

/// Re-queue a `ready` / `failed` / `queued` item with `ladder` (re-transcode; the attempt
/// counter starts over). Returns the updated row; `None` when it does not exist or is
/// `processing`.
pub async fn requeue(db: &Db, id: &str, ladder: &str) -> DbResult<Option<VodItem>> {
    let id = id.to_string();
    let ladder = ladder.to_string();
    if let Some(pool) = db.pg() {
        let n = q(
            "UPDATE vod_items SET status = 'queued', progress = 0, ladder = $2, \
                   renditions_json = '[]', error = NULL, ready_at = NULL, updated_at = $3, \
                   claimed_by = NULL, lease_until = NULL, attempts = 0, output_bytes = 0 \
                   WHERE id = $1 AND status != 'processing'",
        )
        .bind(id.clone())
        .bind(ladder)
        .bind(now_ts())
        .execute(pool)
        .await?
        .rows_affected();
        if n == 0 {
            return Ok(None);
        }
        return pg_get(pool, &id).await;
    }
    db.call(move |c| {
        let n = c.execute(
            "UPDATE vod_items SET status = 'queued', progress = 0, ladder = ?2, renditions_json = '[]', \
             error = NULL, ready_at = NULL, updated_at = ?3, claimed_by = NULL, lease_until = NULL, \
             attempts = 0, output_bytes = 0 WHERE id = ?1 AND status != 'processing'",
            params![id, ladder, now_ts()],
        )?;
        if n == 0 {
            return Ok(None);
        }
        Ok(c.query_row(
            &format!("SELECT {COLUMNS} FROM vod_items WHERE id = ?1"),
            [&id],
            from_row,
        )
        .optional()?)
    })
    .await
}

/// SET clause shared by the re-queue of interrupted jobs: back to `queued`, or `failed`
/// once [`MAX_ATTEMPTS`] is reached. Parameters: `?1`/`$1` = updated_at, the attempt
/// limit and the error text follow the WHERE parameters (see the callers).
const REQUEUE_SET_SQLITE: &str = "status = CASE WHEN attempts >= ?3 THEN 'failed' ELSE 'queued' END, \
     error = CASE WHEN attempts >= ?3 THEN ?4 ELSE error END, \
     renditions_json = CASE WHEN attempts >= ?3 THEN '[]' ELSE renditions_json END, \
     progress = 0, claimed_by = NULL, lease_until = NULL, updated_at = ?1";
const REQUEUE_SET_PG: &str = "status = CASE WHEN attempts >= $3 THEN 'failed' ELSE 'queued' END, \
     error = CASE WHEN attempts >= $3 THEN $4 ELSE error END, \
     renditions_json = CASE WHEN attempts >= $3 THEN '[]' ELSE renditions_json END, \
     progress = 0, claimed_by = NULL, lease_until = NULL, updated_at = $1";

/// Startup: items left `processing` by a previous run **of this node** (or unclaimed, or
/// whose lease expired) go back to `queued`, or to `failed` after [`MAX_ATTEMPTS`].
/// Jobs of other live nodes (valid lease) are left alone. Returns the rows changed.
pub async fn requeue_interrupted(db: &Db) -> DbResult<usize> {
    let node = db.node_id().to_string();
    if let Some(pool) = db.pg() {
        let n = q_dyn(format!(
            "UPDATE vod_items SET {REQUEUE_SET_PG} WHERE status = 'processing' AND \
             (claimed_by IS NULL OR claimed_by = $2 OR lease_until IS NULL OR lease_until < now())"
        ))
        .bind(now_ts())
        .bind(node)
        .bind(MAX_ATTEMPTS as i32)
        .bind(TOO_MANY_ATTEMPTS)
        .execute(pool)
        .await?
        .rows_affected();
        return Ok(n as usize);
    }
    db.call(move |c| {
        let now = now_ts();
        Ok(c.execute(
            &format!(
                "UPDATE vod_items SET {REQUEUE_SET_SQLITE} WHERE status = 'processing' AND \
                 (claimed_by IS NULL OR claimed_by = ?2 OR lease_until IS NULL OR lease_until < ?1)"
            ),
            params![now, node, MAX_ATTEMPTS, TOO_MANY_ATTEMPTS],
        )?)
    })
    .await
}

/// Periodic (every node): jobs of **other** nodes whose lease expired (node dead or cut
/// off from the database) go back to `queued` (or `failed` after [`MAX_ATTEMPTS`]).
/// Returns the ids changed.
pub async fn requeue_expired(db: &Db) -> DbResult<Vec<String>> {
    let node = db.node_id().to_string();
    if let Some(pool) = db.pg() {
        return q_dyn(format!(
            "UPDATE vod_items SET {REQUEUE_SET_PG} WHERE status = 'processing' AND \
             lease_until < now() AND claimed_by IS DISTINCT FROM $2 RETURNING id"
        ))
        .bind(now_ts())
        .bind(node)
        .bind(MAX_ATTEMPTS as i32)
        .bind(TOO_MANY_ATTEMPTS)
        .fetch_all(pool)
        .await?
        .iter()
        .map(|r| Ok(r.try_get::<String, _>(0)?))
        .collect();
    }
    db.call(move |c| {
        let now = now_ts();
        let mut st = c.prepare(&format!(
            "UPDATE vod_items SET {REQUEUE_SET_SQLITE} WHERE status = 'processing' AND \
             lease_until < ?1 AND (claimed_by IS NULL OR claimed_by != ?2) RETURNING id"
        ))?;
        let ids = st
            .query_map(params![now, node, MAX_ATTEMPTS, TOO_MANY_ATTEMPTS], |r| {
                r.get::<_, String>(0)
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ids)
    })
    .await
}

/// Change title and/or description. `None` when the row does not exist.
pub async fn update_meta(
    db: &Db,
    id: &str,
    title: Option<String>,
    description: Option<String>,
) -> DbResult<Option<VodItem>> {
    let id = id.to_string();
    if let Some(pool) = db.pg() {
        return q_dyn(format!(
            "UPDATE vod_items SET title = COALESCE($2, title), description = COALESCE($3, description), \
             updated_at = $4 WHERE id = $1 RETURNING {COLUMNS}"
        ))
        .bind(id)
        .bind(title)
        .bind(description)
        .bind(now_ts())
        .fetch_optional(pool)
        .await?
        .as_ref()
        .map(pg_row)
        .transpose();
    }
    db.call(move |c| {
        let tx = c.transaction()?;
        let n = tx.execute(
            "UPDATE vod_items SET title = COALESCE(?2, title), description = COALESCE(?3, description), \
             updated_at = ?4 WHERE id = ?1",
            params![id, title, description, now_ts()],
        )?;
        let item = if n == 0 {
            None
        } else {
            tx.query_row(
                &format!("SELECT {COLUMNS} FROM vod_items WHERE id = ?1"),
                [&id],
                from_row,
            )
            .optional()?
        };
        tx.commit()?;
        Ok(item)
    })
    .await
}

/// `plays += 1`; returns the new count (`None` when the row does not exist).
pub async fn increment_plays(db: &Db, id: &str) -> DbResult<Option<u64>> {
    let id = id.to_string();
    if let Some(pool) = db.pg() {
        return q("UPDATE vod_items SET plays = plays + 1 WHERE id = $1 RETURNING plays")
            .bind(id)
            .fetch_optional(pool)
            .await?
            .as_ref()
            .map(|r| get_u64(r, 0))
            .transpose();
    }
    db.call(move |c| {
        Ok(c.query_row(
            "UPDATE vod_items SET plays = plays + 1 WHERE id = ?1 RETURNING plays",
            [id],
            |r| r.get::<_, i64>(0),
        )
        .optional()?
        .map(|n| n.max(0) as u64))
    })
    .await
}

/// Delete a row (any status unless `allow_processing` is false and it is processing).
/// Returns the deleted row; `Err`-free `None` when missing or refused.
pub async fn delete(db: &Db, id: &str, allow_processing: bool) -> DbResult<Option<VodItem>> {
    let id = id.to_string();
    if let Some(pool) = db.pg() {
        return q_dyn(format!(
            "DELETE FROM vod_items WHERE id = $1 AND ($2 OR status != 'processing') RETURNING {COLUMNS}"
        ))
        .bind(id)
        .bind(allow_processing)
        .fetch_optional(pool)
        .await?
        .as_ref()
        .map(pg_row)
        .transpose();
    }
    db.call(move |c| {
        let tx = c.transaction()?;
        let item = tx
            .query_row(
                &format!("SELECT {COLUMNS} FROM vod_items WHERE id = ?1"),
                [&id],
                from_row,
            )
            .optional()?;
        let item = match item {
            Some(i) if allow_processing || i.status != STATUS_PROCESSING => {
                tx.execute("DELETE FROM vod_items WHERE id = ?1", [&id])?;
                Some(i)
            }
            _ => None,
        };
        tx.commit()?;
        Ok(item)
    })
    .await
}

/// Ids of every row (orphan directory sweep at startup).
pub async fn all_ids(db: &Db) -> DbResult<Vec<String>> {
    if let Some(pool) = db.pg() {
        return q("SELECT id FROM vod_items")
            .fetch_all(pool)
            .await?
            .iter()
            .map(|r| Ok(r.try_get::<String, _>(0)?))
            .collect();
    }
    db.call(|c| {
        let mut st = c.prepare("SELECT id FROM vod_items")?;
        let rows = st
            .query_map([], |r| r.get(0))?
            .collect::<Result<Vec<String>, _>>()?;
        Ok(rows)
    })
    .await
}

fn pg_row(r: &PgRow) -> DbResult<VodItem> {
    let renditions: String = r.try_get(14)?;
    Ok(VodItem {
        id: r.try_get(0)?,
        title: r.try_get(1)?,
        description: r.try_get(2)?,
        source_path: r.try_get(3)?,
        source_name: r.try_get(4)?,
        source_size: get_u64(r, 5)?,
        source_duration_secs: r.try_get(6)?,
        source_width: get_opt_u32(r, 7)?,
        source_height: get_opt_u32(r, 8)?,
        source_codec: r.try_get(9)?,
        origin: r.try_get(10)?,
        status: r.try_get(11)?,
        progress: get_u32(r, 12)?.min(100),
        ladder: r.try_get(13)?,
        renditions: serde_json::from_str(&renditions).unwrap_or_default(),
        error: r.try_get(15)?,
        created_at: parse_ts(&r.try_get::<String, _>(16)?),
        updated_at: parse_ts(&r.try_get::<String, _>(17)?),
        ready_at: r.try_get::<Option<String>, _>(18)?.map(|s| parse_ts(&s)),
        plays: get_u64(r, 19)?,
        user_id: r.try_get(20)?,
        bucket_id: r.try_get(21)?,
        object_id: r.try_get(22)?,
        visibility: r.try_get(23)?,
        output_bytes: get_u64(r, 24)?,
    })
}

// ---------------------------------------------------------------------------
// Encodings of bucket files (phase 8A)
// ---------------------------------------------------------------------------

/// Rows matching `column = value`, newest first.
async fn list_where(db: &Db, column: &'static str, value: &str) -> DbResult<Vec<VodItem>> {
    let value = value.to_string();
    if let Some(pool) = db.pg() {
        return q_dyn(format!(
            "SELECT {COLUMNS} FROM vod_items WHERE {column} = $1 ORDER BY created_at DESC, id"
        ))
        .bind(value)
        .fetch_all(pool)
        .await?
        .iter()
        .map(pg_row)
        .collect();
    }
    db.call(move |c| {
        let mut st = c.prepare(&format!(
            "SELECT {COLUMNS} FROM vod_items WHERE {column} = ?1 ORDER BY created_at DESC, id"
        ))?;
        let rows = st
            .query_map([value], from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })
    .await
}

/// Encodings of a user, newest first.
pub async fn list_for_user(db: &Db, user_id: &str) -> DbResult<Vec<VodItem>> {
    list_where(db, "user_id", user_id).await
}

/// Encodings of the files of a bucket, newest first.
pub async fn list_by_bucket(db: &Db, bucket_id: &str) -> DbResult<Vec<VodItem>> {
    list_where(db, "bucket_id", bucket_id).await
}

/// The encoding of a bucket object (the most recent one if there were several).
pub async fn find_by_object(db: &Db, object_id: &str) -> DbResult<Option<VodItem>> {
    Ok(list_where(db, "object_id", object_id)
        .await?
        .into_iter()
        .next())
}

/// Encodings of the given bucket objects (directory listings).
pub async fn list_by_objects(db: &Db, object_ids: &[String]) -> DbResult<Vec<VodItem>> {
    if object_ids.is_empty() {
        return Ok(Vec::new());
    }
    let ids = object_ids.to_vec();
    if let Some(pool) = db.pg() {
        return q_dyn(format!(
            "SELECT {COLUMNS} FROM vod_items WHERE object_id = ANY($1) ORDER BY created_at DESC, id"
        ))
        .bind(ids)
        .fetch_all(pool)
        .await?
        .iter()
        .map(pg_row)
        .collect();
    }
    db.call(move |c| {
        let mut out = Vec::new();
        // Bounded statements (SQLite's variable limit).
        for chunk in ids.chunks(500) {
            let marks = vec!["?"; chunk.len()].join(", ");
            let mut st = c.prepare(&format!(
                "SELECT {COLUMNS} FROM vod_items WHERE object_id IN ({marks}) ORDER BY created_at DESC, id"
            ))?;
            let rows = st
                .query_map(rusqlite::params_from_iter(chunk.iter()), from_row)?
                .collect::<Result<Vec<_>, _>>()?;
            out.extend(rows);
        }
        Ok(out)
    })
    .await
}

/// Store the measured size of the encoding output.
pub async fn set_output_bytes(db: &Db, id: &str, bytes: u64) -> DbResult<bool> {
    let id = id.to_string();
    if let Some(pool) = db.pg() {
        let n = q("UPDATE vod_items SET output_bytes = $2 WHERE id = $1")
            .bind(id)
            .bind(i64_of(bytes))
            .execute(pool)
            .await?
            .rows_affected();
        return Ok(n == 1);
    }
    db.call(move |c| {
        Ok(c.execute(
            "UPDATE vod_items SET output_bytes = ?2 WHERE id = ?1",
            params![id, i64_of(bytes)],
        )? == 1)
    })
    .await
}

/// Change the visibility. Returns the row, `None` when it does not exist.
pub async fn set_visibility(db: &Db, id: &str, visibility: &str) -> DbResult<Option<VodItem>> {
    let (id, visibility) = (id.to_string(), visibility.to_string());
    if let Some(pool) = db.pg() {
        return q_dyn(format!(
            "UPDATE vod_items SET visibility = $2, updated_at = $3 WHERE id = $1 RETURNING {COLUMNS}"
        ))
        .bind(id)
        .bind(visibility)
        .bind(now_ts())
        .fetch_optional(pool)
        .await?
        .as_ref()
        .map(pg_row)
        .transpose();
    }
    db.call(move |c| {
        Ok(c.query_row(
            &format!(
                "UPDATE vod_items SET visibility = ?2, updated_at = ?3 WHERE id = ?1 RETURNING {COLUMNS}"
            ),
            params![id, visibility, now_ts()],
            from_row,
        )
        .optional()?)
    })
    .await
}

/// Re-queue an item in **any** status (overwrite of a bucket file, `?action=encode`):
/// like [`requeue`] but a `processing` row is taken away from its job too (the job loses
/// its claim: it cancels itself at the next lease renewal and its result is discarded).
/// `source_size` updates the size of the new original. Returns the row.
pub async fn requeue_any(
    db: &Db,
    id: &str,
    ladder: &str,
    source_size: Option<u64>,
) -> DbResult<Option<VodItem>> {
    let (id, ladder) = (id.to_string(), ladder.to_string());
    let size = source_size.map(i64_of);
    if let Some(pool) = db.pg() {
        return q_dyn(format!(
            "UPDATE vod_items SET status = 'queued', progress = 0, ladder = $2, \
             renditions_json = '[]', error = NULL, ready_at = NULL, updated_at = $3, \
             claimed_by = NULL, lease_until = NULL, attempts = 0, output_bytes = 0, \
             source_size = COALESCE($4, source_size), source_duration_secs = CASE WHEN $4 IS NULL \
             THEN source_duration_secs ELSE NULL END WHERE id = $1 RETURNING {COLUMNS}"
        ))
        .bind(id)
        .bind(ladder)
        .bind(now_ts())
        .bind(size)
        .fetch_optional(pool)
        .await?
        .as_ref()
        .map(pg_row)
        .transpose();
    }
    db.call(move |c| {
        Ok(c.query_row(
            &format!(
                "UPDATE vod_items SET status = 'queued', progress = 0, ladder = ?2, \
                 renditions_json = '[]', error = NULL, ready_at = NULL, updated_at = ?3, \
                 claimed_by = NULL, lease_until = NULL, attempts = 0, output_bytes = 0, \
                 source_size = COALESCE(?4, source_size), source_duration_secs = CASE WHEN ?4 IS NULL \
                 THEN source_duration_secs ELSE NULL END WHERE id = ?1 RETURNING {COLUMNS}"
            ),
            params![id, ladder, now_ts(), size],
            from_row,
        )
        .optional()?)
    })
    .await
}

/// What `/vod/{id}/*` needs to authorize a request: the visibility and, for an encoding
/// of a bucket file, the bucket's signing key. `None` for an unknown id.
pub async fn playback_access(db: &Db, id: &str) -> DbResult<Option<(String, Option<String>)>> {
    let id = id.to_string();
    let sql_pg = "SELECT v.visibility, b.signing_key FROM vod_items v \
                  LEFT JOIN buckets b ON b.id = v.bucket_id WHERE v.id = $1";
    if let Some(pool) = db.pg() {
        return q(sql_pg)
            .bind(id)
            .fetch_optional(pool)
            .await?
            .map(|r| {
                Ok((
                    r.try_get::<String, _>(0)?,
                    r.try_get::<Option<String>, _>(1)?,
                ))
            })
            .transpose();
    }
    db.call(move |c| {
        Ok(c.query_row(
            "SELECT v.visibility, b.signing_key FROM vod_items v \
             LEFT JOIN buckets b ON b.id = v.bucket_id WHERE v.id = ?1",
            [id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
        )
        .optional()?)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, created: &str) -> VodItem {
        let mut it = VodItem::new(
            id,
            &format!("Video {id}"),
            &format!("{id}/source.mp4"),
            "standard",
            ORIGIN_UPLOAD,
        );
        it.created_at = parse_ts(created);
        it.updated_at = it.created_at;
        it.source_name = "clip.mp4".into();
        it.source_size = 1234;
        it
    }

    async fn lifecycle_body(db: Db) {
        let a = item("a", "2026-10-09T10:00:00.000Z");
        let b = item("b", "2026-10-09T11:00:00.000Z");
        let c = item("c", "2026-10-09T12:00:00.000Z");
        for it in [&a, &b, &c] {
            insert(&db, it).await.unwrap();
        }
        assert_eq!(get(&db, "a").await.unwrap().unwrap(), a);
        assert!(get(&db, "zz").await.unwrap().is_none());
        let ids = |v: Vec<VodItem>| v.into_iter().map(|i| i.id).collect::<Vec<_>>();
        assert_eq!(ids(list(&db, None).await.unwrap()), ["c", "b", "a"]);
        assert_eq!(queued_ids(&db, 10).await.unwrap(), ["a", "b", "c"]);
        assert_eq!(queued_ids(&db, 1).await.unwrap(), ["a"]);
        assert_eq!(pending_counts(&db).await.unwrap(), (3, 0));

        // Claim is atomic: only the first one wins.
        assert!(claim(&db, "a").await.unwrap());
        assert!(!claim(&db, "a").await.unwrap());
        assert!(!claim(&db, "nope").await.unwrap());
        assert_eq!(pending_counts(&db).await.unwrap(), (2, 1));
        assert_eq!(
            ids(list(&db, Some(STATUS_PROCESSING)).await.unwrap()),
            ["a"]
        );
        assert!(
            set_source_info(
                &db,
                "a",
                Some(30.0),
                Some(1920),
                Some(1080),
                Some("h264".into())
            )
            .await
            .unwrap()
        );
        assert!(set_progress(&db, "a", 42).await.unwrap());
        assert!(
            !set_progress(&db, "b", 42).await.unwrap(),
            "only while processing"
        );
        let got = get(&db, "a").await.unwrap().unwrap();
        assert_eq!(
            (got.progress, got.source_width, got.source_codec.as_deref()),
            (42, Some(1920), Some("h264"))
        );

        // Processing items can neither be re-queued nor deleted without force.
        assert!(requeue(&db, "a", "mobile").await.unwrap().is_none());
        assert!(delete(&db, "a", false).await.unwrap().is_none());
        let r = vec![Rendition {
            name: "720p".into(),
            width: 1280,
            height: 720,
            video_kbps: 2800,
            audio_kbps: 128,
            bandwidth: 3_100_000,
        }];
        assert!(mark_ready(&db, "a", &r).await.unwrap());
        assert!(
            !mark_ready(&db, "a", &r).await.unwrap(),
            "only from processing"
        );
        let ready = get(&db, "a").await.unwrap().unwrap();
        assert_eq!(ready.status, STATUS_READY);
        assert_eq!(ready.progress, 100);
        assert_eq!(ready.renditions, r);
        assert!(ready.ready_at.is_some());

        assert_eq!(increment_plays(&db, "a").await.unwrap(), Some(1));
        assert_eq!(increment_plays(&db, "a").await.unwrap(), Some(2));
        assert_eq!(increment_plays(&db, "zz").await.unwrap(), None);

        // Re-transcode with another ladder.
        let re = requeue(&db, "a", "mobile").await.unwrap().unwrap();
        assert_eq!(
            (re.status.as_str(), re.ladder.as_str(), re.progress),
            (STATUS_QUEUED, "mobile", 0)
        );
        assert!(re.renditions.is_empty() && re.ready_at.is_none());
        assert_eq!(re.plays, 2, "plays survive a re-transcode");

        // Failure keeps the row for a retry.
        assert!(claim(&db, "b").await.unwrap());
        assert!(mark_failed(&db, "b", "boom").await.unwrap());
        let failed = get(&db, "b").await.unwrap().unwrap();
        assert_eq!(
            (failed.status.as_str(), failed.error.as_deref()),
            (STATUS_FAILED, Some("boom"))
        );
        assert!(
            requeue(&db, "b", "standard")
                .await
                .unwrap()
                .unwrap()
                .error
                .is_none()
        );

        // Shutdown / restart.
        assert!(claim(&db, "c").await.unwrap());
        assert!(unclaim(&db, "c").await.unwrap());
        assert!(claim(&db, "c").await.unwrap());
        assert_eq!(requeue_interrupted(&db).await.unwrap(), 1);
        assert_eq!(get(&db, "c").await.unwrap().unwrap().status, STATUS_QUEUED);

        // Metadata.
        let m = update_meta(&db, "c", Some("New".into()), None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!((m.title.as_str(), m.description.as_str()), ("New", ""));
        let m = update_meta(&db, "c", None, Some("Desc".into()))
            .await
            .unwrap()
            .unwrap();
        assert_eq!((m.title.as_str(), m.description.as_str()), ("New", "Desc"));
        assert!(
            update_meta(&db, "zz", Some("x".into()), None)
                .await
                .unwrap()
                .is_none()
        );

        // Delete (forced while processing).
        assert!(claim(&db, "c").await.unwrap());
        assert!(delete(&db, "c", false).await.unwrap().is_none());
        assert_eq!(delete(&db, "c", true).await.unwrap().unwrap().id, "c");
        assert!(delete(&db, "c", true).await.unwrap().is_none());
        let mut all = all_ids(&db).await.unwrap();
        all.sort();
        assert_eq!(all, ["a", "b"]);
    }
    crate::db::testkit::dual_db_test!(lifecycle, lifecycle_body);

    async fn attempts(db: &Db, id: &str) -> i64 {
        crate::db::testkit::scalar_i64(
            db,
            &format!("SELECT CAST(attempts AS BIGINT) FROM vod_items WHERE id = '{id}'"),
        )
        .await
    }

    /// docs/CLUSTER.md §3.7: claim with lease, renewal, ready only by the holder,
    /// re-queue of expired leases, attempts limit.
    async fn leases_body(db: Db) {
        let n1 = db.clone().with_node_id("n1");
        let n2 = db.clone().with_node_id("n2");
        insert(&db, &item("a", "2026-10-09T10:00:00.000Z"))
            .await
            .unwrap();
        insert(&db, &item("b", "2026-10-09T11:00:00.000Z"))
            .await
            .unwrap();

        assert!(claim(&n1, "a").await.unwrap());
        assert_eq!(attempts(&db, "a").await, 1);
        assert!(renew_lease(&n1, "a").await.unwrap());
        assert!(!renew_lease(&n2, "a").await.unwrap(), "not the holder");
        assert!(set_progress(&n1, "a", 10).await.unwrap());
        assert!(!set_progress(&n2, "a", 20).await.unwrap());
        // A valid lease of another node survives this node's startup and sweeps.
        assert_eq!(requeue_interrupted(&n2).await.unwrap(), 0);
        assert!(requeue_expired(&n2).await.unwrap().is_empty());
        assert!(!mark_ready(&n2, "a", &[]).await.unwrap(), "only the holder");
        assert!(
            !mark_failed(&n2, "a", "x").await.unwrap(),
            "only the holder"
        );
        assert!(!unclaim(&n2, "a").await.unwrap(), "only the holder");

        // n1 dies: its lease expires and n2 re-queues the job.
        crate::db::testkit::execute(
            &db,
            "UPDATE vod_items SET lease_until = NULL WHERE id = 'a'",
        )
        .await;
        assert_eq!(
            requeue_interrupted(&n2).await.unwrap(),
            1,
            "no lease = orphan"
        );
        assert_eq!(get(&db, "a").await.unwrap().unwrap().status, STATUS_QUEUED);
        assert!(claim(&n2, "a").await.unwrap());
        assert_eq!(attempts(&db, "a").await, 2);
        // The old holder can no longer finish it.
        assert!(!mark_ready(&n1, "a", &[]).await.unwrap());
        // Expired lease (DB clock on PostgreSQL, wall clock on SQLite).
        let expire = if db.pg().is_some() {
            "UPDATE vod_items SET lease_until = now() - interval '1 second' WHERE id = 'a'"
        } else {
            "UPDATE vod_items SET lease_until = '2000-01-01T00:00:00.000Z' WHERE id = 'a'"
        };
        crate::db::testkit::execute(&db, expire).await;
        assert!(
            requeue_expired(&n2).await.unwrap().is_empty(),
            "own rows: startup only"
        );
        assert_eq!(requeue_expired(&n1).await.unwrap(), ["a"]);
        assert!(claim(&n1, "a").await.unwrap());
        assert_eq!(attempts(&db, "a").await, 3);
        // Graceful shutdown does not count as an attempt.
        assert!(unclaim(&n1, "a").await.unwrap());
        assert_eq!(attempts(&db, "a").await, 2);
        assert!(claim(&n1, "a").await.unwrap());
        // Third crash of n1: failed instead of queued.
        assert_eq!(requeue_interrupted(&n1).await.unwrap(), 1);
        let failed = get(&db, "a").await.unwrap().unwrap();
        assert_eq!(failed.status, STATUS_FAILED);
        assert_eq!(failed.error.as_deref(), Some(TOO_MANY_ATTEMPTS));
        // A re-transcode starts over.
        requeue(&db, "a", "mobile").await.unwrap().unwrap();
        assert_eq!(attempts(&db, "a").await, 0);

        // The holder finishes normally.
        assert!(claim(&n2, "b").await.unwrap());
        assert!(mark_ready(&n2, "b", &[]).await.unwrap());
        assert_eq!(get(&db, "b").await.unwrap().unwrap().status, STATUS_READY);
    }
    crate::db::testkit::dual_db_test!(leases, leases_body);

    /// Phase 8A: encodings linked to bucket objects, visibility, output size, forced
    /// re-queue, playback access with the bucket signing key.
    async fn storage_links_body(db: Db) {
        let user = crate::db::users::User::new("alice", "Alice");
        crate::db::users::insert(&db, &user).await.unwrap();
        let bucket = crate::db::buckets::Bucket::new(&user.id, "media");
        crate::db::buckets::insert(&db, &bucket).await.unwrap();
        let mut a = item("a", "2026-10-09T10:00:00.000Z");
        a.user_id = Some(user.id.clone());
        a.bucket_id = Some(bucket.id.clone());
        a.object_id = Some("o1".into());
        a.visibility = VISIBILITY_PRIVATE.into();
        a.origin = ORIGIN_STORAGE.into();
        a.source_path = "/videos/a.mp4".into();
        let b = item("b", "2026-10-09T11:00:00.000Z");
        insert(&db, &a).await.unwrap();
        insert(&db, &b).await.unwrap();
        assert_eq!(get(&db, "a").await.unwrap().unwrap(), a);
        assert!(a.is_private() && !b.is_private());
        assert_eq!(b.visibility, VISIBILITY_PUBLIC);

        let ids = |v: Vec<VodItem>| v.into_iter().map(|i| i.id).collect::<Vec<_>>();
        assert_eq!(ids(list_for_user(&db, &user.id).await.unwrap()), ["a"]);
        assert_eq!(ids(list_by_bucket(&db, &bucket.id).await.unwrap()), ["a"]);
        assert_eq!(find_by_object(&db, "o1").await.unwrap().unwrap().id, "a");
        assert!(find_by_object(&db, "o2").await.unwrap().is_none());
        assert_eq!(
            ids(list_by_objects(&db, &["o1".into(), "zz".into()])
                .await
                .unwrap()),
            ["a"]
        );
        assert!(list_by_objects(&db, &[]).await.unwrap().is_empty());

        assert!(set_output_bytes(&db, "a", 4242).await.unwrap());
        assert_eq!(get(&db, "a").await.unwrap().unwrap().output_bytes, 4242);
        assert_eq!(
            crate::db::users::usage(&db, &user.id)
                .await
                .unwrap()
                .encodings_bytes,
            4242
        );

        assert_eq!(
            playback_access(&db, "a").await.unwrap(),
            Some((
                VISIBILITY_PRIVATE.to_string(),
                Some(bucket.signing_key.clone())
            ))
        );
        assert_eq!(
            playback_access(&db, "b").await.unwrap(),
            Some((VISIBILITY_PUBLIC.to_string(), None))
        );
        assert_eq!(playback_access(&db, "zz").await.unwrap(), None);
        let v = set_visibility(&db, "a", VISIBILITY_PUBLIC)
            .await
            .unwrap()
            .unwrap();
        assert!(!v.is_private());
        assert!(
            set_visibility(&db, "zz", VISIBILITY_PUBLIC)
                .await
                .unwrap()
                .is_none()
        );

        // Forced re-queue works while processing and drops the claim.
        assert!(claim(&db, "a").await.unwrap());
        let r = requeue_any(&db, "a", "mobile", Some(99))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (
                r.status.as_str(),
                r.ladder.as_str(),
                r.source_size,
                r.output_bytes
            ),
            (STATUS_QUEUED, "mobile", 99, 0)
        );
        assert!(
            !set_progress(&db, "a", 50).await.unwrap(),
            "old job lost its claim"
        );
        assert!(!mark_ready(&db, "a", &[]).await.unwrap());
        let r = requeue_any(&db, "a", "standard", None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(r.source_size, 99);
        assert!(
            requeue_any(&db, "zz", "standard", None)
                .await
                .unwrap()
                .is_none()
        );
    }
    crate::db::testkit::dual_db_test!(storage_links, storage_links_body);
}
