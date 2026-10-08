//! Viewer metrics persistence: `viewer_samples` (time series) and `viewer_sessions`.
//!
//! Write side: [`insert_viewer_session`] / [`close_viewer_session`] are called by
//! [`crate::metrics::ViewerTracker`] hooks; [`insert_sample`] and the retention helpers by
//! the sampler task (`metrics::sampler`).
//!
//! Read side (used by `GET /broadcasts/{name}/metrics[/summary]`): [`series`] (bucketed
//! with **max** aggregation), [`peak`], [`sessions_total`], [`avg_watch_secs`] and
//! [`stream_bytes_in`].
//!
//! Timestamps are stored with [`fmt_ts`] (RFC 3339, milliseconds, `Z`), so range
//! filters are plain lexicographic comparisons that use the `(broadcast_id, ts)` index.

use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, params};
use serde::Serialize;

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
/// write is logged and skipped without affecting the others).
///
/// The tracker funnels every open/close through a single ordered queue into this
/// function: applying them in issue order is what guarantees that the close of a
/// short-lived session never runs before its insert (which would leave the row open
/// forever and skew `sessions_total` / `avg_watch_secs`).
pub async fn apply_session_writes(db: &Db, writes: Vec<SessionWrite>) -> DbResult<()> {
    if writes.is_empty() {
        return Ok(());
    }
    db.call(move |c| {
        let mut tx = c.transaction()?;
        for w in &writes {
            let sp = tx.savepoint()?;
            let r = match w {
                SessionWrite::Open(record) => open_session_row(&sp, record),
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
        "INSERT INTO viewer_sessions (id, broadcast_id, protocol, ip, user_agent, started_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            record.id,
            bid,
            record.protocol.as_str(),
            record.ip,
            record.user_agent,
            started
        ],
    )?;
    Ok(())
}

/// Crash recovery: close `viewer_sessions` rows left open by a previous process
/// (`started_at < before`). The end time is approximated with the last non-zero
/// sample of the broadcast taken during the session, or `started_at` when there is none.
pub async fn close_dangling_viewer_sessions(db: &Db, before: DateTime<Utc>) -> DbResult<usize> {
    db.call(move |c| {
        Ok(c.execute(
            "UPDATE viewer_sessions SET ended_at = COALESCE(\
               (SELECT MAX(s.ts) FROM viewer_samples s WHERE s.broadcast_id = viewer_sessions.broadcast_id \
                AND s.ts >= viewer_sessions.started_at AND s.ts < ?1 AND s.total > 0), \
               started_at) \
             WHERE ended_at IS NULL AND started_at < ?1",
            [fmt_ts(&before)],
        )?)
    })
    .await
}

/// Insert one `viewer_samples` row (replacing a sample with the very same timestamp).
pub async fn insert_sample(
    db: &Db,
    broadcast_id: &str,
    ts: DateTime<Utc>,
    counts: ViewerCounts,
) -> DbResult<()> {
    let bid = broadcast_id.to_string();
    db.call(move |c| {
        c.execute(
            "INSERT OR REPLACE INTO viewer_samples (broadcast_id, ts, total, flv, ws_flv, hls, dash, webrtc) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                bid,
                fmt_ts(&ts),
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
    db.call(move |c| {
        Ok(c.execute(
            "DELETE FROM viewer_sessions WHERE ended_at IS NOT NULL AND ended_at < ?1",
            [fmt_ts(&cutoff)],
        )?)
    })
    .await
}

/// Samples of `broadcast_id` in `[from, to)` grouped in buckets of `step_secs` aligned
/// to the Unix epoch (`t` = bucket start), every column aggregated with `MAX`.
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
    db.call(move |c| {
        let mut st = c.prepare(
            "SELECT (CAST(strftime('%s', ts) AS INTEGER) / ?4) * ?4 AS bucket, \
               MAX(total), MAX(flv), MAX(ws_flv), MAX(hls), MAX(dash), MAX(webrtc) \
             FROM viewer_samples WHERE broadcast_id = ?1 AND ts >= ?2 AND ts < ?3 \
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

/// Highest sampled `total` in `[from, to)` and when it was first reached.
pub async fn peak(
    db: &Db,
    broadcast_id: &str,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> DbResult<Option<(u32, DateTime<Utc>)>> {
    let bid = broadcast_id.to_string();
    db.call(move |c| {
        Ok(c.query_row(
            "SELECT total, ts FROM viewer_samples WHERE broadcast_id = ?1 AND ts >= ?2 AND ts < ?3 \
             ORDER BY total DESC, ts ASC LIMIT 1",
            params![bid, fmt_ts(&from), fmt_ts(&to)],
            |r| Ok((r.get::<_, u32>(0)?, parse_ts(&r.get::<_, String>(1)?))),
        )
        .optional()?)
    })
    .await
}

/// Sessions overlapping `[from, to)`: started before `to` and not ended before `from`.
const OVERLAP: &str =
    "broadcast_id = ?1 AND started_at < ?3 AND (ended_at IS NULL OR ended_at >= ?2)";

/// Number of viewer sessions (all protocols) overlapping `[from, to)`.
pub async fn sessions_total(
    db: &Db,
    broadcast_id: &str,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> DbResult<u64> {
    let bid = broadcast_id.to_string();
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

/// Bytes received in the *closed* live periods (`stream_sessions`) overlapping
/// `[from, to)`. The open period of a live stream has `bytes_in = 0` in the database
/// until it ends: callers add the live counter.
pub async fn stream_bytes_in(
    db: &Db,
    broadcast_id: &str,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> DbResult<u64> {
    let bid = broadcast_id.to_string();
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
        db.call(move |c| Ok(c.query_row(sql, [], |r| r.get(0))?))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn viewer_sessions_roundtrip_and_reuse() {
        let db = Db::open_in_memory().unwrap();
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
    }

    #[tokio::test]
    async fn open_sessions_count_up_to_now_and_dangling_are_closed() {
        let db = Db::open_in_memory().unwrap();
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
        assert_eq!(close_dangling_viewer_sessions(&db, now).await.unwrap(), 1);
        let avg = avg_watch_secs(&db, &b.id, t0, now, now).await.unwrap();
        assert!((avg - 10.0).abs() < 0.01, "avg = {avg}");
    }

    #[tokio::test]
    async fn series_buckets_with_max_and_peak() {
        let db = Db::open_in_memory().unwrap();
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

        let (v, at) = peak(&db, "b", t0, to).await.unwrap().unwrap();
        assert_eq!(v, 5);
        assert_eq!(at, t0 + Duration::milliseconds(22_500));
        assert!(peak(&db, "nobody", t0, to).await.unwrap().is_none());

        // Retention.
        assert_eq!(
            delete_samples_older_than(&db, t0 + Duration::seconds(10))
                .await
                .unwrap(),
            3
        );
        assert_eq!(series(&db, "b", t0, to, 5).await.unwrap().len(), 4);
    }

    #[tokio::test]
    async fn bytes_in_of_closed_periods() {
        use crate::db::stream_sessions::{self, StreamSessionEnd};
        let db = Db::open_in_memory().unwrap();
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
}
