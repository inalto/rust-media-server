//! `recordings` repository.
//!
//! The [`Recording`] type is part of the API contract (and of
//! `BroadcastView.live.recording`); rows are written by `recorder::RecorderManager`.

use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use sqlx_core::row::Row as _;
use sqlx_postgres::PgRow;

use super::pg::{get_u64, q, q_dyn};
use super::{Db, DbResult, fmt_ts, parse_ts};

/// `recordings.status` values.
pub const STATUS_RECORDING: &str = "recording";
pub const STATUS_PROCESSING: &str = "processing";
pub const STATUS_READY: &str = "ready";
pub const STATUS_FAILED: &str = "failed";

/// `recordings.format` values.
pub const FORMAT_FLV: &str = "flv";
pub const FORMAT_MP4: &str = "mp4";

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

impl Recording {
    /// Still being written or post-processed (cannot be deleted).
    pub fn is_busy(&self) -> bool {
        self.status == STATUS_RECORDING || self.status == STATUS_PROCESSING
    }
}

const COLUMNS: &str = "id, broadcast_id, stream_session_id, path, format, status, started_at, \
     ended_at, size_bytes, duration_secs, error";

fn from_row(r: &Row<'_>) -> rusqlite::Result<Recording> {
    Ok(Recording {
        id: r.get(0)?,
        broadcast_id: r.get(1)?,
        stream_session_id: r.get(2)?,
        path: r.get(3)?,
        format: r.get(4)?,
        status: r.get(5)?,
        started_at: parse_ts(&r.get::<_, String>(6)?),
        ended_at: r.get::<_, Option<String>>(7)?.map(|s| parse_ts(&s)),
        size_bytes: r.get::<_, i64>(8)?.max(0) as u64,
        duration_secs: r.get(9)?,
        error: r.get(10)?,
    })
}

fn size_i64(n: u64) -> i64 {
    n.min(i64::MAX as u64) as i64
}

/// Insert a row (tagged with [`Db::node_id`]: the node holding the file).
pub async fn insert(db: &Db, rec: &Recording) -> DbResult<()> {
    let r = rec.clone();
    let node = db.node_id().to_string();
    if let Some(pool) = db.pg() {
        q_dyn(format!(
            "INSERT INTO recordings ({COLUMNS}, node_id) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)"
        ))
        .bind(r.id)
        .bind(r.broadcast_id)
        .bind(r.stream_session_id)
        .bind(r.path)
        .bind(r.format)
        .bind(r.status)
        .bind(fmt_ts(&r.started_at))
        .bind(r.ended_at.as_ref().map(fmt_ts))
        .bind(size_i64(r.size_bytes))
        .bind(r.duration_secs)
        .bind(r.error)
        .bind(node)
        .execute(pool)
        .await?;
        return Ok(());
    }
    db.call(move |c| {
        c.execute(
            &format!(
                "INSERT INTO recordings ({COLUMNS}, node_id) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)"
            ),
            params![
                r.id,
                r.broadcast_id,
                r.stream_session_id,
                r.path,
                r.format,
                r.status,
                fmt_ts(&r.started_at),
                r.ended_at.as_ref().map(fmt_ts),
                size_i64(r.size_bytes),
                r.duration_secs,
                r.error,
                node
            ],
        )?;
        Ok(())
    })
    .await
}

/// Update the mutable columns (path, format, status, ended_at, size, duration, error)
/// of an existing row. Returns false when the row does not exist (e.g. deleted).
pub async fn update(db: &Db, rec: &Recording) -> DbResult<bool> {
    let r = rec.clone();
    if let Some(pool) = db.pg() {
        let n = q(
            "UPDATE recordings SET path = $2, format = $3, status = $4, ended_at = $5, \
                   size_bytes = $6, duration_secs = $7, error = $8 WHERE id = $1",
        )
        .bind(r.id)
        .bind(r.path)
        .bind(r.format)
        .bind(r.status)
        .bind(r.ended_at.as_ref().map(fmt_ts))
        .bind(size_i64(r.size_bytes))
        .bind(r.duration_secs)
        .bind(r.error)
        .execute(pool)
        .await?
        .rows_affected();
        return Ok(n > 0);
    }
    db.call(move |c| {
        let n = c.execute(
            "UPDATE recordings SET path = ?2, format = ?3, status = ?4, ended_at = ?5, size_bytes = ?6, \
             duration_secs = ?7, error = ?8 WHERE id = ?1",
            params![
                r.id,
                r.path,
                r.format,
                r.status,
                r.ended_at.as_ref().map(fmt_ts),
                size_i64(r.size_bytes),
                r.duration_secs,
                r.error
            ],
        )?;
        Ok(n > 0)
    })
    .await
}

/// Fetch one row.
pub async fn get(db: &Db, id: &str) -> DbResult<Option<Recording>> {
    let id = id.to_string();
    if let Some(pool) = db.pg() {
        return q_dyn(format!("SELECT {COLUMNS} FROM recordings WHERE id = $1"))
            .bind(id)
            .fetch_optional(pool)
            .await?
            .as_ref()
            .map(pg_row)
            .transpose();
    }
    db.call(move |c| {
        Ok(c.query_row(
            &format!("SELECT {COLUMNS} FROM recordings WHERE id = ?1"),
            [id],
            from_row,
        )
        .optional()?)
    })
    .await
}

/// The node that wrote the file of recording `id` (`recordings.node_id`; `None` for
/// unknown ids and rows written before node tags existed).
pub async fn node_of(db: &Db, id: &str) -> DbResult<Option<String>> {
    let id = id.to_string();
    if let Some(pool) = db.pg() {
        let row = q("SELECT node_id FROM recordings WHERE id = $1")
            .bind(id)
            .fetch_optional(pool)
            .await?;
        return Ok(match row {
            Some(r) => r.try_get::<Option<String>, _>(0)?,
            None => None,
        }
        .filter(|n| !n.is_empty()));
    }
    db.call(move |c| {
        let node: Option<Option<String>> = c
            .query_row("SELECT node_id FROM recordings WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .optional()?;
        Ok(node.flatten().filter(|n| !n.is_empty()))
    })
    .await
}

/// List rows (all, or of one broadcast id), newest first.
pub async fn list(db: &Db, broadcast_id: Option<&str>) -> DbResult<Vec<Recording>> {
    let bid = broadcast_id.map(str::to_string);
    if let Some(pool) = db.pg() {
        let rows = match bid {
            Some(bid) => {
                q_dyn(format!(
                    "SELECT {COLUMNS} FROM recordings WHERE broadcast_id = $1 ORDER BY started_at DESC, id"
                ))
                .bind(bid)
                .fetch_all(pool)
                .await?
            }
            None => {
                q_dyn(format!(
                    "SELECT {COLUMNS} FROM recordings ORDER BY started_at DESC, id"
                ))
                .fetch_all(pool)
                .await?
            }
        };
        return rows.iter().map(pg_row).collect();
    }
    db.call(move |c| {
        let rows = match bid {
            Some(bid) => {
                let mut st = c.prepare(&format!(
                    "SELECT {COLUMNS} FROM recordings WHERE broadcast_id = ?1 ORDER BY started_at DESC, id"
                ))?;
                st.query_map([bid], from_row)?
                    .collect::<Result<Vec<_>, _>>()?
            }
            None => {
                let mut st = c.prepare(&format!(
                    "SELECT {COLUMNS} FROM recordings ORDER BY started_at DESC, id"
                ))?;
                st.query_map([], from_row)?.collect::<Result<Vec<_>, _>>()?
            }
        };
        Ok(rows)
    })
    .await
}

/// Rows of **this node** still `recording` or `processing` (crash recovery at startup),
/// oldest first. Rows written before node tags existed (`node_id` NULL) count as the
/// single node's.
pub async fn list_unfinished(db: &Db) -> DbResult<Vec<Recording>> {
    let node = db.node_id().to_string();
    if let Some(pool) = db.pg() {
        return q_dyn(format!(
            "SELECT {COLUMNS} FROM recordings WHERE status IN ($1, $2) \
             AND COALESCE(node_id, '') = $3 ORDER BY started_at"
        ))
        .bind(STATUS_RECORDING)
        .bind(STATUS_PROCESSING)
        .bind(node)
        .fetch_all(pool)
        .await?
        .iter()
        .map(pg_row)
        .collect();
    }
    db.call(move |c| {
        let mut st = c.prepare(&format!(
            "SELECT {COLUMNS} FROM recordings WHERE status IN (?1, ?2) \
             AND COALESCE(node_id, '') = ?3 ORDER BY started_at"
        ))?;
        let rows = st
            .query_map(params![STATUS_RECORDING, STATUS_PROCESSING, node], from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })
    .await
}

/// Delete a row, returning it (the caller deletes the file).
pub async fn delete(db: &Db, id: &str) -> DbResult<Option<Recording>> {
    let id = id.to_string();
    if let Some(pool) = db.pg() {
        return q_dyn(format!(
            "DELETE FROM recordings WHERE id = $1 RETURNING {COLUMNS}"
        ))
        .bind(id)
        .fetch_optional(pool)
        .await?
        .as_ref()
        .map(pg_row)
        .transpose();
    }
    db.call(move |c| {
        let tx = c.transaction()?;
        let rec = tx
            .query_row(
                &format!("SELECT {COLUMNS} FROM recordings WHERE id = ?1"),
                [&id],
                from_row,
            )
            .optional()?;
        if rec.is_some() {
            tx.execute("DELETE FROM recordings WHERE id = ?1", [&id])?;
        }
        tx.commit()?;
        Ok(rec)
    })
    .await
}

fn pg_row(r: &PgRow) -> DbResult<Recording> {
    Ok(Recording {
        id: r.try_get(0)?,
        broadcast_id: r.try_get(1)?,
        stream_session_id: r.try_get(2)?,
        path: r.try_get(3)?,
        format: r.try_get(4)?,
        status: r.try_get(5)?,
        started_at: parse_ts(&r.try_get::<String, _>(6)?),
        ended_at: r.try_get::<Option<String>, _>(7)?.map(|s| parse_ts(&s)),
        size_bytes: get_u64(r, 8)?,
        duration_secs: r.try_get(9)?,
        error: r.try_get(10)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn rec(id: &str, bid: &str, started: DateTime<Utc>) -> Recording {
        Recording {
            id: id.into(),
            broadcast_id: bid.into(),
            stream_session_id: Some("ss".into()),
            path: format!("show/{id}.flv"),
            format: FORMAT_FLV.into(),
            status: STATUS_RECORDING.into(),
            // Millisecond precision, as stored.
            started_at: parse_ts(&fmt_ts(&started)),
            ended_at: None,
            size_bytes: 0,
            duration_secs: None,
            error: None,
        }
    }

    async fn crud_body(db: Db) {
        let t0 = Utc::now();
        let a = rec("a", "b1", t0 - Duration::minutes(10));
        let b = rec("b", "b1", t0 - Duration::minutes(5));
        let c = rec("c", "b2", t0);
        for r in [&a, &b, &c] {
            insert(&db, r).await.unwrap();
        }
        assert_eq!(get(&db, "a").await.unwrap().unwrap(), a);
        assert!(get(&db, "zzz").await.unwrap().is_none());
        assert!(a.is_busy());

        let all: Vec<String> = list(&db, None)
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(all, ["c", "b", "a"]);
        let b1: Vec<String> = list(&db, Some("b1"))
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(b1, ["b", "a"]);
        assert!(list(&db, Some("none")).await.unwrap().is_empty());

        let mut done = a.clone();
        done.status = STATUS_READY.into();
        done.format = FORMAT_MP4.into();
        done.path = "show/a.mp4".into();
        done.ended_at = Some(parse_ts(&fmt_ts(&t0)));
        done.size_bytes = 12345;
        done.duration_secs = Some(29.97);
        done.error = Some("note".into());
        assert!(update(&db, &done).await.unwrap());
        assert_eq!(get(&db, "a").await.unwrap().unwrap(), done);
        assert!(!done.is_busy());

        let unfinished: Vec<String> = list_unfinished(&db)
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(unfinished, ["b", "c"]);
        // Another node's unfinished rows are not this node's crash leftovers.
        let other = db.clone().with_node_id("n2");
        assert!(list_unfinished(&other).await.unwrap().is_empty());
        let mut d = rec("d", "b1", t0 + Duration::minutes(1));
        d.duration_secs = Some(1.5);
        insert(&other, &d).await.unwrap();
        assert_eq!(list_unfinished(&other).await.unwrap(), vec![d.clone()]);
        assert_eq!(list_unfinished(&db).await.unwrap().len(), 2);
        assert_eq!(delete(&db, "d").await.unwrap().unwrap(), d);

        assert_eq!(delete(&db, "a").await.unwrap().unwrap(), done);
        assert!(delete(&db, "a").await.unwrap().is_none());
        assert!(!update(&db, &done).await.unwrap());
        assert_eq!(list(&db, None).await.unwrap().len(), 2);
    }
    crate::db::testkit::dual_db_test!(crud, crud_body);
}
