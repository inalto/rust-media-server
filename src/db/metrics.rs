//! Viewer metrics persistence: `viewer_samples` (time series) and `viewer_sessions`.
//!
//! Write side: [`insert_viewer_session`] / [`close_viewer_session`] are called by
//! [`crate::metrics::ViewerTracker`] hooks; [`insert_sample`] and the retention helpers by
//! the sampler task (`metrics::sampler`).
//!
//! Read side (used by `GET /broadcasts/{name}/metrics[/summary]`): [`series`] (bucketed
//! with **max** per node, then **sum** over the nodes), [`peak`], [`sessions_total`],
//! [`avg_watch_secs`] and [`stream_bytes_in`].
//!
//! Every row carries the writing node ([`Db::node_id`]: empty on a single node), so the
//! crash recovery only touches this node's rows and the aggregates follow
//! docs/CLUSTER.md §3.6; with one node the results are those of the per-bucket max.
//!
//! Timestamps are stored with [`fmt_ts`] (RFC 3339, milliseconds, `Z`), so range
//! filters are plain lexicographic comparisons that use the `(broadcast_id, ts)` index.

use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, params};
use serde::Serialize;
use sqlx_core::row::Row as _;

use super::pg::{get_u32, q, q_dyn};
use super::{Db, DbResult, fmt_ts, parse_ts};
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

/// One point of the viewer time series: bucket start `t` plus the maximum of every
/// column over the samples of the bucket.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct Point {
    pub t: DateTime<Utc>,
    pub total: u32,
    pub flv: u32,
    pub ws_flv: u32,
    pub hls: u32,
    pub dash: u32,
    pub webrtc: u32,
}

impl Point {
    /// A bucket without samples (no viewers / stream offline).
    pub fn zero(t: DateTime<Utc>) -> Self {
        Self {
            t,
            total: 0,
            flv: 0,
            ws_flv: 0,
            hls: 0,
            dash: 0,
            webrtc: 0,
        }
    }
}

/// One `viewer_sessions` write, see [`apply_session_writes`].
#[derive(Debug, Clone)]
pub enum SessionWrite {
    Open(ViewerSessionRecord),
    Close { id: String, ended_at: DateTime<Utc> },
}

/// Insert a `viewer_sessions` row when a viewer session opens.
///
/// Unknown broadcast names are ignored (the broadcast may have been deleted
/// meanwhile). Request-based sessions (HLS/DASH) reuse their deterministic id when the
/// same viewer comes back after expiring; the previous row is then archived under a
/// unique id (`<id>:<uuid>`) so the history is kept and `close_viewer_session(id)`
/// keeps addressing the current session.
pub async fn insert_viewer_session(db: &Db, record: ViewerSessionRecord) -> DbResult<()> {
    apply_session_writes(db, vec![SessionWrite::Open(record)]).await
}

/// Set `ended_at` of a `viewer_sessions` row when a viewer session closes
/// (no-op for unknown or already closed rows).
pub async fn close_viewer_session(db: &Db, id: &str, ended_at: DateTime<Utc>) -> DbResult<()> {
    let id = id.to_string();
    apply_session_writes(db, vec![SessionWrite::Close { id, ended_at }]).await
}

/// Apply `writes` in order, in one transaction (one savepoint per write: a failing
/// write is logged and skipped without affecting the others). Rows are tagged with
/// [`Db::node_id`].
///
/// The tracker funnels every open/close through a single ordered queue into this
/// function: applying them in issue order is what guarantees that the close of a
/// short-lived session never runs before its insert (which would leave the row open
/// forever and skew `sessions_total` / `avg_watch_secs`).
pub async fn apply_session_writes(db: &Db, writes: Vec<SessionWrite>) -> DbResult<()> {
    if writes.is_empty() {
        return Ok(());
    }
    let node = db.node_id().to_string();
    if let Some(pool) = db.pg() {
        return pg::apply_session_writes(pool, &node, &writes).await;
    }
    db.call(move |c| {
        let mut tx = c.transaction()?;
        for w in &writes {
            let sp = tx.savepoint()?;
            let r = match w {
                SessionWrite::Open(record) => open_session_row(&sp, record, &node),
                SessionWrite::Close { id, ended_at } => sp
                    .execute(
                        "UPDATE viewer_sessions SET ended_at = ?2 WHERE id = ?1 AND ended_at IS NULL",
                        params![id, fmt_ts(ended_at)],
                    )
                    .map(|_| ()),
            };
            match r {
                Ok(()) => sp.commit()?,
                Err(e) => tracing::warn!(error = %e, write = ?w, "viewer session write failed"),
            }
        }
        tx.commit()?;
        Ok(())
    })
    .await
}

fn open_session_row(
    c: &rusqlite::Connection,
    record: &ViewerSessionRecord,
    node: &str,
) -> rusqlite::Result<()> {
    let bid: Option<String> = c
        .query_row(
            "SELECT id FROM broadcasts WHERE name = ?1",
            [&record.broadcast_name],
            |r| r.get(0),
        )
        .optional()?;
    let Some(bid) = bid else {
        return Ok(());
    };
    let started = fmt_ts(&record.started_at);
    let exists = c
        .query_row(
            "SELECT 1 FROM viewer_sessions WHERE id = ?1",
            [&record.id],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if exists {
        let archived = format!("{}:{}", record.id, uuid::Uuid::new_v4().simple());
        c.execute(
            "UPDATE viewer_sessions SET id = ?2, ended_at = COALESCE(ended_at, ?3) WHERE id = ?1",
            params![record.id, archived, started],
        )?;
    }
    c.execute(
        "INSERT INTO viewer_sessions (id, broadcast_id, protocol, ip, user_agent, started_at, node_id) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            record.id,
            bid,
            record.protocol.as_str(),
            record.ip,
            record.user_agent,
            started,
            node
        ],
    )?;
    Ok(())
}

/// Crash recovery: close the `viewer_sessions` rows of **this node** left open by a
/// previous process (`started_at < before`). The end time is approximated with the last
/// non-zero sample of the broadcast taken by this node during the session, or
/// `started_at` when there is none.
pub async fn close_dangling_viewer_sessions(db: &Db, before: DateTime<Utc>) -> DbResult<usize> {
    let node = db.node_id().to_string();
    if let Some(pool) = db.pg() {
        let n = q("UPDATE viewer_sessions SET ended_at = COALESCE(\
               (SELECT MAX(s.ts) FROM viewer_samples s WHERE s.broadcast_id = viewer_sessions.broadcast_id \
                AND s.node_id = viewer_sessions.node_id \
                AND s.ts >= viewer_sessions.started_at AND s.ts < $1 AND s.total > 0), \
               started_at) \
             WHERE ended_at IS NULL AND started_at < $1 AND node_id = $2")
        .bind(fmt_ts(&before))
        .bind(node)
        .execute(pool)
        .await?
        .rows_affected();
        return Ok(n as usize);
    }
    db.call(move |c| {
        Ok(c.execute(
            "UPDATE viewer_sessions SET ended_at = COALESCE(\
               (SELECT MAX(s.ts) FROM viewer_samples s WHERE s.broadcast_id = viewer_sessions.broadcast_id \
                AND s.node_id = viewer_sessions.node_id \
                AND s.ts >= viewer_sessions.started_at AND s.ts < ?1 AND s.total > 0), \
               started_at) \
             WHERE ended_at IS NULL AND started_at < ?1 AND node_id = ?2",
            params![fmt_ts(&before), node],
        )?)
    })
    .await
}

/// Insert one `viewer_samples` row of this node (replacing a sample of this node with
/// the very same timestamp).
pub async fn insert_sample(
    db: &Db,
    broadcast_id: &str,
    ts: DateTime<Utc>,
    counts: ViewerCounts,
) -> DbResult<()> {
    let bid = broadcast_id.to_string();
    let node = db.node_id().to_string();
    if let Some(pool) = db.pg() {
        q("INSERT INTO viewer_samples (broadcast_id, node_id, ts, ts_epoch, total, flv, ws_flv, hls, dash, webrtc) \
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
           ON CONFLICT (broadcast_id, node_id, ts) DO UPDATE SET ts_epoch = excluded.ts_epoch, \
           total = excluded.total, flv = excluded.flv, ws_flv = excluded.ws_flv, hls = excluded.hls, \
           dash = excluded.dash, webrtc = excluded.webrtc")
        .bind(bid)
        .bind(node)
        .bind(fmt_ts(&ts))
        .bind(ts.timestamp())
        .bind(i64::from(counts.total()))
        .bind(i64::from(counts.flv))
        .bind(i64::from(counts.ws_flv))
        .bind(i64::from(counts.hls))
        .bind(i64::from(counts.dash))
        .bind(i64::from(counts.webrtc))
        .execute(pool)
        .await?;
        return Ok(());
    }
    db.call(move |c| {
        c.execute(
            "INSERT OR REPLACE INTO viewer_samples \
             (broadcast_id, node_id, ts, ts_epoch, total, flv, ws_flv, hls, dash, webrtc) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                bid,
                node,
                fmt_ts(&ts),
                ts.timestamp(),
                counts.total(),
                counts.flv,
                counts.ws_flv,
                counts.hls,
                counts.dash,
                counts.webrtc
            ],
        )?;
        Ok(())
    })
    .await
}

/// Retention: delete `viewer_samples` older than `cutoff`; returns deleted rows.
pub async fn delete_samples_older_than(db: &Db, cutoff: DateTime<Utc>) -> DbResult<usize> {
    if let Some(pool) = db.pg() {
        let n = q("DELETE FROM viewer_samples WHERE ts < $1")
            .bind(fmt_ts(&cutoff))
            .execute(pool)
            .await?
            .rows_affected();
        return Ok(n as usize);
    }
    db.call(move |c| {
        Ok(c.execute(
            "DELETE FROM viewer_samples WHERE ts < ?1",
            [fmt_ts(&cutoff)],
        )?)
    })
    .await
}

/// Retention: delete closed `viewer_sessions` that ended before `cutoff`; returns
/// deleted rows (keeps the table bounded like the samples).
pub async fn delete_sessions_older_than(db: &Db, cutoff: DateTime<Utc>) -> DbResult<usize> {
    if let Some(pool) = db.pg() {
        let n = q("DELETE FROM viewer_sessions WHERE ended_at IS NOT NULL AND ended_at < $1")
            .bind(fmt_ts(&cutoff))
            .execute(pool)
            .await?
            .rows_affected();
        return Ok(n as usize);
    }
    db.call(move |c| {
        Ok(c.execute(
            "DELETE FROM viewer_sessions WHERE ended_at IS NOT NULL AND ended_at < ?1",
            [fmt_ts(&cutoff)],
        )?)
    })
    .await
}

/// Samples of `broadcast_id` in `[from, to)` grouped in buckets of `step_secs` aligned
/// to the Unix epoch (`t` = bucket start). Per column: the **max** of every node's
/// samples in the bucket, then the **sum** over the nodes (docs/CLUSTER.md §3.6; the
/// samplers of different nodes are not synchronized). With one node this is the plain
/// per-bucket max.
///
/// Sparse: buckets without samples are omitted (see `metrics::query` for the dense,
/// zero-filled series served by the API). Sorted by `t`.
pub async fn series(
    db: &Db,
    broadcast_id: &str,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    step_secs: u64,
) -> DbResult<Vec<Point>> {
    let bid = broadcast_id.to_string();
    let step = step_secs.clamp(1, i64::MAX as u64) as i64;
    if let Some(pool) = db.pg() {
        return q(
            "SELECT bucket, CAST(SUM(t) AS BIGINT), CAST(SUM(flv) AS BIGINT), \
               CAST(SUM(ws_flv) AS BIGINT), CAST(SUM(hls) AS BIGINT), CAST(SUM(dash) AS BIGINT), \
               CAST(SUM(webrtc) AS BIGINT) FROM ( \
               SELECT (ts_epoch / $4) * $4 AS bucket, node_id, MAX(total) AS t, MAX(flv) AS flv, \
                 MAX(ws_flv) AS ws_flv, MAX(hls) AS hls, MAX(dash) AS dash, MAX(webrtc) AS webrtc \
               FROM viewer_samples WHERE broadcast_id = $1 AND ts >= $2 AND ts < $3 \
               GROUP BY 1, 2) s \
             GROUP BY bucket ORDER BY bucket",
        )
        .bind(bid)
        .bind(fmt_ts(&from))
        .bind(fmt_ts(&to))
        .bind(step)
        .fetch_all(pool)
        .await?
        .iter()
        .map(|r| {
            Ok(Point {
                t: DateTime::from_timestamp(r.try_get::<i64, _>(0)?, 0).unwrap_or_default(),
                total: get_u32(r, 1)?,
                flv: get_u32(r, 2)?,
                ws_flv: get_u32(r, 3)?,
                hls: get_u32(r, 4)?,
                dash: get_u32(r, 5)?,
                webrtc: get_u32(r, 6)?,
            })
        })
        .collect();
    }
    db.call(move |c| {
        let mut st = c.prepare(
            "SELECT bucket, SUM(t), SUM(flv), SUM(ws_flv), SUM(hls), SUM(dash), SUM(webrtc) FROM ( \
               SELECT (ts_epoch / ?4) * ?4 AS bucket, node_id, MAX(total) AS t, MAX(flv) AS flv, \
                 MAX(ws_flv) AS ws_flv, MAX(hls) AS hls, MAX(dash) AS dash, MAX(webrtc) AS webrtc \
               FROM viewer_samples WHERE broadcast_id = ?1 AND ts >= ?2 AND ts < ?3 \
               GROUP BY bucket, node_id) \
             GROUP BY bucket ORDER BY bucket",
        )?;
        let rows = st
            .query_map(params![bid, fmt_ts(&from), fmt_ts(&to), step], |r| {
                let bucket: i64 = r.get(0)?;
                Ok(Point {
                    t: DateTime::from_timestamp(bucket, 0).unwrap_or_default(),
                    total: r.get(1)?,
                    flv: r.get(2)?,
                    ws_flv: r.get(3)?,
                    hls: r.get(4)?,
                    dash: r.get(5)?,
                    webrtc: r.get(6)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })
    .await
}

/// Highest total in `[from, to)` and when it was first reached.
///
/// The total of an instant is the sum over the nodes of their samples, matched by
/// epoch-aligned buckets of `bucket_secs` (the sampling interval: every node samples in
/// the middle of the same wall-clock slots) with the max per node within a bucket
/// (docs/CLUSTER.md §3.6: peak = max of the sum, not the sum of the maxima). `at` is
/// the earliest sample of the winning bucket where a node reached its bucket maximum.
/// With one node this is exactly the highest sample and its timestamp.
pub async fn peak(
    db: &Db,
    broadcast_id: &str,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    bucket_secs: u64,
) -> DbResult<Option<(u32, DateTime<Utc>)>> {
    let bid = broadcast_id.to_string();
    let step = bucket_secs.clamp(1, i64::MAX as u64) as i64;
    let (from_s, to_s) = (fmt_ts(&from), fmt_ts(&to));
    if let Some(pool) = db.pg() {
        let Some(row) = q("SELECT bucket, CAST(SUM(t) AS BIGINT) AS total FROM ( \
               SELECT (ts_epoch / $4) * $4 AS bucket, node_id, MAX(total) AS t \
               FROM viewer_samples WHERE broadcast_id = $1 AND ts >= $2 AND ts < $3 \
               GROUP BY 1, 2) s \
             GROUP BY bucket ORDER BY total DESC, bucket ASC LIMIT 1")
        .bind(bid.clone())
        .bind(from_s.clone())
        .bind(to_s.clone())
        .bind(step)
        .fetch_optional(pool)
        .await?
        else {
            return Ok(None);
        };
        let (bucket, total): (i64, i64) = (row.try_get(0)?, row.try_get(1)?);
        let at: Option<String> = q("SELECT MIN(v.ts) FROM viewer_samples v JOIN ( \
               SELECT node_id, MAX(total) AS t FROM viewer_samples \
               WHERE broadcast_id = $1 AND ts >= $2 AND ts < $3 AND ts_epoch >= $4 AND ts_epoch < $4 + $5 \
               GROUP BY node_id) m ON v.node_id = m.node_id AND v.total = m.t \
             WHERE v.broadcast_id = $1 AND v.ts >= $2 AND v.ts < $3 \
               AND v.ts_epoch >= $4 AND v.ts_epoch < $4 + $5")
        .bind(bid)
        .bind(from_s)
        .bind(to_s)
        .bind(bucket)
        .bind(step)
        .fetch_one(pool)
        .await?
        .try_get(0)?;
        return Ok(Some((
            crate::db::pg::u32_of(total),
            at.map(|s| parse_ts(&s))
                .unwrap_or_else(|| DateTime::from_timestamp(bucket, 0).unwrap_or_default()),
        )));
    }
    db.call(move |c| {
        let best: Option<(i64, i64)> = c
            .query_row(
                "SELECT bucket, SUM(t) AS total FROM ( \
                   SELECT (ts_epoch / ?4) * ?4 AS bucket, node_id, MAX(total) AS t \
                   FROM viewer_samples WHERE broadcast_id = ?1 AND ts >= ?2 AND ts < ?3 \
                   GROUP BY bucket, node_id) \
                 GROUP BY bucket ORDER BY total DESC, bucket ASC LIMIT 1",
                params![bid, from_s, to_s, step],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((bucket, total)) = best else {
            return Ok(None);
        };
        let at: Option<String> = c.query_row(
            "SELECT MIN(v.ts) FROM viewer_samples v JOIN ( \
               SELECT node_id, MAX(total) AS t FROM viewer_samples \
               WHERE broadcast_id = ?1 AND ts >= ?2 AND ts < ?3 AND ts_epoch >= ?4 AND ts_epoch < ?4 + ?5 \
               GROUP BY node_id) m ON v.node_id = m.node_id AND v.total = m.t \
             WHERE v.broadcast_id = ?1 AND v.ts >= ?2 AND v.ts < ?3 \
               AND v.ts_epoch >= ?4 AND v.ts_epoch < ?4 + ?5",
            params![bid, from_s, to_s, bucket, step],
            |r| r.get(0),
        )?;
        Ok(Some((
            total.clamp(0, u32::MAX as i64) as u32,
            at.map(|s| parse_ts(&s))
                .unwrap_or_else(|| DateTime::from_timestamp(bucket, 0).unwrap_or_default()),
        )))
    })
    .await
}

/// Sessions overlapping `[from, to)`: started before `to` and not ended before `from`.
const OVERLAP: &str =
    "broadcast_id = ?1 AND started_at < ?3 AND (ended_at IS NULL OR ended_at >= ?2)";
const PG_OVERLAP: &str =
    "broadcast_id = $1 AND started_at < $3 AND (ended_at IS NULL OR ended_at >= $2)";

/// Number of viewer sessions (all protocols, all nodes) overlapping `[from, to)`.
pub async fn sessions_total(
    db: &Db,
    broadcast_id: &str,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> DbResult<u64> {
    let bid = broadcast_id.to_string();
    if let Some(pool) = db.pg() {
        let n: i64 = q_dyn(format!(
            "SELECT COUNT(*) FROM viewer_sessions WHERE {PG_OVERLAP}"
        ))
        .bind(bid)
        .bind(fmt_ts(&from))
        .bind(fmt_ts(&to))
        .fetch_one(pool)
        .await?
        .try_get(0)?;
        return Ok(n.max(0) as u64);
    }
    db.call(move |c| {
        let n: i64 = c.query_row(
            &format!("SELECT COUNT(*) FROM viewer_sessions WHERE {OVERLAP}"),
            params![bid, fmt_ts(&from), fmt_ts(&to)],
            |r| r.get(0),
        )?;
        Ok(n.max(0) as u64)
    })
    .await
}

/// Average full duration (seconds) of the viewer sessions overlapping `[from, to)`;
/// sessions still open count up to `now`. `0.0` when there are none.
pub async fn avg_watch_secs(
    db: &Db,
    broadcast_id: &str,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    now: DateTime<Utc>,
) -> DbResult<f64> {
    let bid = broadcast_id.to_string();
    if let Some(pool) = db.pg() {
        let avg: Option<f64> = q_dyn(format!(
            "SELECT CAST(AVG(GREATEST(0.0, EXTRACT(EPOCH FROM \
               (CAST(COALESCE(ended_at, $4) AS TIMESTAMPTZ) - CAST(started_at AS TIMESTAMPTZ))))) \
               AS DOUBLE PRECISION) \
             FROM viewer_sessions WHERE {PG_OVERLAP}"
        ))
        .bind(bid)
        .bind(fmt_ts(&from))
        .bind(fmt_ts(&to))
        .bind(fmt_ts(&now))
        .fetch_one(pool)
        .await?
        .try_get(0)?;
        return Ok(avg.unwrap_or(0.0));
    }
    db.call(move |c| {
        let avg: Option<f64> = c.query_row(
            &format!(
                "SELECT AVG(MAX(0.0, (julianday(COALESCE(ended_at, ?4)) - julianday(started_at)) * 86400.0)) \
                 FROM viewer_sessions WHERE {OVERLAP}"
            ),
            params![bid, fmt_ts(&from), fmt_ts(&to), fmt_ts(&now)],
            |r| r.get(0),
        )?;
        Ok(avg.unwrap_or(0.0))
    })
    .await
}

/// Bytes received in the *closed* live periods (`stream_sessions`, all nodes)
/// overlapping `[from, to)`. The open period of a live stream has `bytes_in = 0` in the
/// database until it ends: callers add the live counter.
pub async fn stream_bytes_in(
    db: &Db,
    broadcast_id: &str,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> DbResult<u64> {
    let bid = broadcast_id.to_string();
    if let Some(pool) = db.pg() {
        let n: Option<i64> = q(
            "SELECT CAST(SUM(bytes_in) AS BIGINT) FROM stream_sessions WHERE broadcast_id = $1 \
             AND started_at < $3 AND ended_at IS NOT NULL AND ended_at >= $2",
        )
        .bind(bid)
        .bind(fmt_ts(&from))
        .bind(fmt_ts(&to))
        .fetch_one(pool)
        .await?
        .try_get(0)?;
        return Ok(n.unwrap_or(0).max(0) as u64);
    }
    db.call(move |c| {
        let n: Option<i64> = c.query_row(
            "SELECT SUM(bytes_in) FROM stream_sessions WHERE broadcast_id = ?1 AND started_at < ?3 \
             AND ended_at IS NOT NULL AND ended_at >= ?2",
            params![bid, fmt_ts(&from), fmt_ts(&to)],
            |r| r.get(0),
        )?;
        Ok(n.unwrap_or(0).max(0) as u64)
    })
    .await
}

/// Live viewers of the *other* nodes (cluster mode, docs/CLUSTER.md §3.6): the latest
/// sample of every (broadcast, node) newer than `since` (Unix seconds) and not written
/// by `exclude_node`, summed per broadcast name. A node stops writing samples for a
/// stream it no longer serves (final zero sample) and a dead node's samples age out.
pub async fn recent_counts_of_other_nodes(
    db: &Db,
    exclude_node: &str,
    since: i64,
) -> DbResult<Vec<(String, ViewerCounts)>> {
    let node = exclude_node.to_string();
    let rows: Vec<(String, [i64; 5])> = if let Some(pool) = db.pg() {
        q("SELECT b.name, s.flv, s.ws_flv, s.hls, s.dash, s.webrtc FROM ( \
             SELECT DISTINCT ON (broadcast_id, node_id) broadcast_id, flv, ws_flv, hls, dash, webrtc \
             FROM viewer_samples WHERE ts_epoch >= $1 AND node_id <> $2 \
             ORDER BY broadcast_id, node_id, ts DESC) s \
           JOIN broadcasts b ON b.id = s.broadcast_id")
        .bind(since)
        .bind(node)
        .fetch_all(pool)
        .await?
        .iter()
        .map(|r| {
            Ok((
                r.try_get::<String, _>(0)?,
                [
                    r.try_get(1)?,
                    r.try_get(2)?,
                    r.try_get(3)?,
                    r.try_get(4)?,
                    r.try_get(5)?,
                ],
            ))
        })
        .collect::<DbResult<_>>()?
    } else {
        db.call(move |c| {
            let mut st = c.prepare(
                "SELECT b.name, v.flv, v.ws_flv, v.hls, v.dash, v.webrtc FROM viewer_samples v \
                 JOIN broadcasts b ON b.id = v.broadcast_id \
                 WHERE v.ts_epoch >= ?1 AND v.node_id <> ?2 AND v.ts = (SELECT MAX(w.ts) \
                   FROM viewer_samples w WHERE w.broadcast_id = v.broadcast_id AND w.node_id = v.node_id)",
            )?;
            let rows = st
                .query_map(params![since, node], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        [r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?],
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await?
    };
    let mut sums: std::collections::BTreeMap<String, ViewerCounts> = Default::default();
    for (name, [flv, ws_flv, hls, dash, webrtc]) in rows {
        let u = |v: i64| v.clamp(0, u32::MAX as i64) as u32;
        sums.entry(name).or_default().add(&ViewerCounts {
            flv: u(flv),
            ws_flv: u(ws_flv),
            hls: u(hls),
            dash: u(dash),
            webrtc: u(webrtc),
        });
    }
    Ok(sums.into_iter().filter(|(_, c)| c.total() > 0).collect())
}

/// PostgreSQL statements of the session writer.
mod pg {
    use sqlx_core::connection::Connection as _;
    use sqlx_core::row::Row as _;
    use sqlx_postgres::PgConnection;

    use super::{SessionWrite, ViewerSessionRecord};
    use crate::db::pg::q;
    use crate::db::{DbResult, PgPool, fmt_ts};

    pub async fn apply_session_writes(
        pool: &PgPool,
        node: &str,
        writes: &[SessionWrite],
    ) -> DbResult<()> {
        let mut tx = pool.begin().await?;
        for w in writes {
            let mut sp = (*tx).begin().await?;
            let r = match w {
                SessionWrite::Open(record) => open_session_row(&mut sp, record, node).await,
                SessionWrite::Close { id, ended_at } => {
                    q("UPDATE viewer_sessions SET ended_at = $2 WHERE id = $1 AND ended_at IS NULL")
                        .bind(id.clone())
                        .bind(fmt_ts(ended_at))
                        .execute(&mut *sp)
                        .await
                        .map(|_| ())
                        .map_err(Into::into)
                }
            };
            match r {
                Ok(()) => sp.commit().await?,
                Err(e) => {
                    tracing::warn!(error = %e, write = ?w, "viewer session write failed");
                    sp.rollback().await?;
                }
            }
        }
        tx.commit().await?;
        Ok(())
    }

    async fn open_session_row(
        c: &mut PgConnection,
        record: &ViewerSessionRecord,
        node: &str,
    ) -> DbResult<()> {
        let bid: Option<String> = q("SELECT id FROM broadcasts WHERE name = $1")
            .bind(record.broadcast_name.clone())
            .fetch_optional(&mut *c)
            .await?
            .map(|r| r.try_get(0))
            .transpose()?;
        let Some(bid) = bid else {
            return Ok(());
        };
        let started = fmt_ts(&record.started_at);
        let archived = format!("{}:{}", record.id, uuid::Uuid::new_v4().simple());
        q("UPDATE viewer_sessions SET id = $2, ended_at = COALESCE(ended_at, $3) WHERE id = $1")
            .bind(record.id.clone())
            .bind(archived)
            .bind(started.clone())
            .execute(&mut *c)
            .await?;
        q("INSERT INTO viewer_sessions (id, broadcast_id, protocol, ip, user_agent, started_at, node_id) \
           VALUES ($1, $2, $3, $4, $5, $6, $7)")
        .bind(record.id.clone())
        .bind(bid)
        .bind(record.protocol.as_str())
        .bind(record.ip.clone())
        .bind(record.user_agent.clone())
        .bind(started)
        .bind(node.to_string())
        .execute(&mut *c)
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::broadcasts::{self, NewBroadcast};
    use chrono::Duration;

    fn ts(s: &str) -> DateTime<Utc> {
        parse_ts(s)
    }

    fn counts(flv: u32, hls: u32) -> ViewerCounts {
        ViewerCounts {
            flv,
            hls,
            ..Default::default()
        }
    }

    async fn count_rows(db: &Db, sql: &'static str) -> i64 {
        crate::db::testkit::scalar_i64(db, sql).await
    }

    async fn viewer_sessions_roundtrip_and_reuse_body(db: Db) {
        let b = broadcasts::create(&db, NewBroadcast::new("show", "Show"))
            .await
            .unwrap();
        let t0 = ts("2026-10-08T10:00:00.000Z");
        let rec = |id: &str, at| ViewerSessionRecord {
            id: id.into(),
            broadcast_name: "show".into(),
            protocol: Protocol::Hls,
            ip: Some("10.0.0.1".into()),
            user_agent: Some("ua".into()),
            started_at: at,
        };
        insert_viewer_session(&db, rec("h1", t0)).await.unwrap();
        // Unknown broadcast: ignored.
        let mut other = rec("x", t0);
        other.broadcast_name = "nope".into();
        insert_viewer_session(&db, other).await.unwrap();
        assert_eq!(
            count_rows(&db, "SELECT COUNT(*) FROM viewer_sessions").await,
            1
        );

        close_viewer_session(&db, "h1", t0 + Duration::seconds(60))
            .await
            .unwrap();
        // Same deterministic id comes back: the old row is archived.
        insert_viewer_session(&db, rec("h1", t0 + Duration::seconds(120)))
            .await
            .unwrap();
        assert_eq!(
            count_rows(&db, "SELECT COUNT(*) FROM viewer_sessions").await,
            2
        );
        assert_eq!(
            count_rows(
                &db,
                "SELECT COUNT(*) FROM viewer_sessions WHERE id = 'h1' AND ended_at IS NULL"
            )
            .await,
            1
        );
        close_viewer_session(&db, "h1", t0 + Duration::seconds(150))
            .await
            .unwrap();

        let (from, to) = (t0 - Duration::hours(1), t0 + Duration::hours(1));
        assert_eq!(sessions_total(&db, &b.id, from, to).await.unwrap(), 2);
        // 60 s and 30 s -> 45 s.
        let avg = avg_watch_secs(&db, &b.id, from, to, t0 + Duration::hours(2))
            .await
            .unwrap();
        assert!((avg - 45.0).abs() < 0.01, "avg = {avg}");
        // A window after both sessions ended sees none.
        let later = t0 + Duration::seconds(200);
        assert_eq!(
            sessions_total(&db, &b.id, later, later + Duration::hours(1))
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            avg_watch_secs(&db, &b.id, later, later + Duration::hours(1), later)
                .await
                .unwrap(),
            0.0
        );
        // Retention of closed sessions.
        assert_eq!(
            delete_sessions_older_than(&db, t0 + Duration::seconds(100))
                .await
                .unwrap(),
            1,
            "the archived session ended at +60 s"
        );
        assert_eq!(
            count_rows(&db, "SELECT COUNT(*) FROM viewer_sessions").await,
            1
        );
    }
    crate::db::testkit::dual_db_test!(
        viewer_sessions_roundtrip_and_reuse,
        viewer_sessions_roundtrip_and_reuse_body
    );

    async fn open_sessions_count_up_to_now_and_dangling_are_closed_body(db: Db) {
        let b = broadcasts::create(&db, NewBroadcast::new("live", "Live"))
            .await
            .unwrap();
        let t0 = ts("2026-10-08T10:00:00.000Z");
        insert_viewer_session(
            &db,
            ViewerSessionRecord {
                id: "f1".into(),
                broadcast_name: "live".into(),
                protocol: Protocol::Flv,
                ip: None,
                user_agent: None,
                started_at: t0,
            },
        )
        .await
        .unwrap();
        let now = t0 + Duration::seconds(90);
        let avg = avg_watch_secs(&db, &b.id, t0, now, now).await.unwrap();
        assert!((avg - 90.0).abs() < 0.01, "avg = {avg}");

        insert_sample(&db, &b.id, t0 + Duration::seconds(5), counts(1, 0))
            .await
            .unwrap();
        insert_sample(&db, &b.id, t0 + Duration::seconds(10), counts(1, 0))
            .await
            .unwrap();
        insert_sample(&db, &b.id, t0 + Duration::seconds(15), counts(0, 0))
            .await
            .unwrap();
        // Sessions opened after `before` are left alone.
        assert_eq!(close_dangling_viewer_sessions(&db, t0).await.unwrap(), 0);
        // Another node's open session is not dangling for this node.
        let other = db.clone().with_node_id("n2");
        insert_viewer_session(
            &other,
            ViewerSessionRecord {
                id: "f2".into(),
                broadcast_name: "live".into(),
                protocol: Protocol::Flv,
                ip: None,
                user_agent: None,
                started_at: t0,
            },
        )
        .await
        .unwrap();
        assert_eq!(close_dangling_viewer_sessions(&db, now).await.unwrap(), 1);
        assert_eq!(
            count_rows(
                &db,
                "SELECT COUNT(*) FROM viewer_sessions WHERE id = 'f2' AND ended_at IS NULL"
            )
            .await,
            1
        );
        assert_eq!(
            close_dangling_viewer_sessions(&other, now).await.unwrap(),
            1
        );
        assert_eq!(
            close_dangling_viewer_sessions(&other, now).await.unwrap(),
            0
        );
        let avg = avg_watch_secs(&db, &b.id, t0, now, now).await.unwrap();
        // f1: 10 s (last non-zero sample of this node); f2: no sample of n2 -> 0 s.
        assert!((avg - 5.0).abs() < 0.01, "avg = {avg}");
    }
    crate::db::testkit::dual_db_test!(
        open_sessions_count_up_to_now_and_dangling_are_closed,
        open_sessions_count_up_to_now_and_dangling_are_closed_body
    );

    async fn series_buckets_with_max_and_peak_body(db: Db) {
        let t0 = ts("2026-10-08T10:00:00.000Z");
        // Samples at +2.5 s, +7.5 s, ... (one per 5 s slot).
        let values = [(1, 0), (3, 1), (2, 0), (0, 0), (5, 0), (4, 0)];
        for (i, (flv, hls)) in values.iter().enumerate() {
            let at = t0 + Duration::milliseconds(2500 + 5000 * i as i64);
            insert_sample(&db, "b", at, counts(*flv, *hls))
                .await
                .unwrap();
        }
        insert_sample(&db, "other", t0 + Duration::seconds(1), counts(99, 0))
            .await
            .unwrap();

        let to = t0 + Duration::seconds(30);
        let p5 = series(&db, "b", t0, to, 5).await.unwrap();
        assert_eq!(p5.len(), 6);
        assert_eq!(p5[1].t, t0 + Duration::seconds(5));
        assert_eq!((p5[1].total, p5[1].flv, p5[1].hls), (4, 3, 1));

        let p10 = series(&db, "b", t0, to, 10).await.unwrap();
        assert_eq!(p10.len(), 3);
        assert_eq!(p10[0].t, t0);
        // max per column, independently.
        assert_eq!((p10[0].total, p10[0].flv, p10[0].hls), (4, 3, 1));
        assert_eq!(p10[1].total, 2);
        assert_eq!(p10[2].total, 5);

        // Range filter is [from, to).
        let part = series(
            &db,
            "b",
            t0 + Duration::seconds(5),
            t0 + Duration::seconds(10),
            5,
        )
        .await
        .unwrap();
        assert_eq!(part.len(), 1);

        let (v, at) = peak(&db, "b", t0, to, 5).await.unwrap().unwrap();
        assert_eq!(v, 5);
        assert_eq!(at, t0 + Duration::milliseconds(22_500));
        assert!(peak(&db, "nobody", t0, to, 5).await.unwrap().is_none());
        // Same timestamp again: replaced, not duplicated.
        insert_sample(&db, "b", t0 + Duration::milliseconds(2500), counts(2, 0))
            .await
            .unwrap();
        assert_eq!(series(&db, "b", t0, to, 5).await.unwrap()[0].total, 2);

        // Retention.
        assert_eq!(
            delete_samples_older_than(&db, t0 + Duration::seconds(10))
                .await
                .unwrap(),
            3
        );
        assert_eq!(series(&db, "b", t0, to, 5).await.unwrap().len(), 4);
    }
    crate::db::testkit::dual_db_test!(
        series_buckets_with_max_and_peak,
        series_buckets_with_max_and_peak_body
    );

    /// docs/CLUSTER.md §3.6: max per (node, bucket), then sum; peak = max of the sum.
    async fn several_nodes_are_summed_body(db: Db) {
        let t0 = ts("2026-10-08T10:00:00.000Z");
        let n1 = db.clone().with_node_id("n1");
        let n2 = db.clone().with_node_id("n2");
        // Bucket [0, 5): n1 samples 3 then 4 (max 4), n2 samples 2 -> 6.
        insert_sample(&n1, "b", t0 + Duration::milliseconds(1000), counts(3, 0))
            .await
            .unwrap();
        insert_sample(&n1, "b", t0 + Duration::milliseconds(2500), counts(4, 0))
            .await
            .unwrap();
        insert_sample(&n2, "b", t0 + Duration::milliseconds(2510), counts(1, 1))
            .await
            .unwrap();
        // Bucket [5, 10): only n1 with 5 -> 5 (the sum of the maxima would be 5 + 2 = 7).
        insert_sample(&n1, "b", t0 + Duration::milliseconds(7500), counts(5, 0))
            .await
            .unwrap();
        let to = t0 + Duration::seconds(10);
        let p = series(&db, "b", t0, to, 5).await.unwrap();
        assert_eq!(p.len(), 2);
        assert_eq!((p[0].total, p[0].flv, p[0].hls), (6, 5, 1));
        assert_eq!(p[1].total, 5);
        let p10 = series(&db, "b", t0, to, 10).await.unwrap();
        assert_eq!(p10.len(), 1);
        assert_eq!(p10[0].total, 5 + 2);
        let (v, at) = peak(&db, "b", t0, to, 5).await.unwrap().unwrap();
        assert_eq!(v, 6);
        assert_eq!(at, t0 + Duration::milliseconds(2500));
    }
    crate::db::testkit::dual_db_test!(several_nodes_are_summed, several_nodes_are_summed_body);

    /// Live viewers of the other nodes: latest sample per (broadcast, node), own node
    /// and old samples excluded, summed per broadcast name.
    async fn recent_counts_of_other_nodes_body(db: Db) {
        use crate::db::broadcasts::{self, NewBroadcast};
        let b = broadcasts::create(&db, NewBroadcast::new("show", "Show"))
            .await
            .unwrap();
        let now = Utc::now();
        let n1 = db.clone().with_node_id("n1");
        let n2 = db.clone().with_node_id("n2");
        let n3 = db.clone().with_node_id("n3");
        insert_sample(&n2, &b.id, now - Duration::seconds(8), counts(5, 0))
            .await
            .unwrap();
        insert_sample(&n2, &b.id, now - Duration::seconds(3), counts(2, 1))
            .await
            .unwrap();
        insert_sample(&n3, &b.id, now - Duration::seconds(2), counts(0, 4))
            .await
            .unwrap();
        insert_sample(&n1, &b.id, now - Duration::seconds(1), counts(9, 9))
            .await
            .unwrap();
        // A stale sample of another broadcast/node is ignored.
        insert_sample(&n3, "gone", now - Duration::seconds(600), counts(7, 0))
            .await
            .unwrap();
        let since = (now - Duration::seconds(20)).timestamp();
        let got = recent_counts_of_other_nodes(&n1, "n1", since)
            .await
            .unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, "show");
        assert_eq!((got[0].1.flv, got[0].1.hls, got[0].1.total()), (2, 5, 7));
        // n2 wrote a final zero sample: only n3 is left.
        insert_sample(&n2, &b.id, now, counts(0, 0)).await.unwrap();
        let got = recent_counts_of_other_nodes(&n1, "n1", since)
            .await
            .unwrap();
        assert_eq!(got[0].1.total(), 4);
        // Everything is too old.
        assert!(
            recent_counts_of_other_nodes(&n1, "n1", now.timestamp() + 60)
                .await
                .unwrap()
                .is_empty()
        );
    }
    crate::db::testkit::dual_db_test!(
        recent_counts_of_other_nodes_test,
        recent_counts_of_other_nodes_body
    );

    async fn bytes_in_of_closed_periods_body(db: Db) {
        use crate::db::stream_sessions::{self, StreamSessionEnd};
        let t0 = Utc::now() - Duration::minutes(30);
        let id = stream_sessions::open(&db, "b", t0, None).await.unwrap();
        stream_sessions::close(
            &db,
            &id,
            StreamSessionEnd {
                ended_at: t0 + Duration::minutes(10),
                bytes_in: 1000,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let _open = stream_sessions::open(&db, "b", t0 + Duration::minutes(20), None)
            .await
            .unwrap();
        let now = Utc::now();
        assert_eq!(
            stream_bytes_in(&db, "b", now - Duration::hours(1), now)
                .await
                .unwrap(),
            1000
        );
        assert_eq!(
            stream_bytes_in(&db, "b", now - Duration::minutes(5), now)
                .await
                .unwrap(),
            0
        );
    }
    crate::db::testkit::dual_db_test!(bytes_in_of_closed_periods, bytes_in_of_closed_periods_body);
}
