//! `stream_sessions` repository: one row per live period of a broadcast.
//!
//! The RTMP ingest opens a row when publishing starts and closes it when publishing
//! ends (with codec info, bytes received and peak concurrent viewers).

use chrono::{DateTime, Utc};
use rusqlite::{Row, params};
use serde::Serialize;
use sqlx_core::row::Row as _;
use sqlx_postgres::PgRow;

use super::pg::{get_opt_u32, get_u32, get_u64, i64_of, q, q_dyn};
use super::{Db, DbResult, fmt_ts, parse_ts};

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct StreamSession {
    pub id: String,
    pub broadcast_id: String,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub publisher_ip: Option<String>,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<f64>,
    pub bytes_in: u64,
    pub peak_viewers: u32,
}

/// Values written when a live period ends.
#[derive(Debug, Clone, Default)]
pub struct StreamSessionEnd {
    pub ended_at: DateTime<Utc>,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<f64>,
    pub bytes_in: u64,
    pub peak_viewers: u32,
}

fn from_row(r: &Row<'_>) -> rusqlite::Result<StreamSession> {
    Ok(StreamSession {
        id: r.get(0)?,
        broadcast_id: r.get(1)?,
        started_at: parse_ts(&r.get::<_, String>(2)?),
        ended_at: r.get::<_, Option<String>>(3)?.map(|s| parse_ts(&s)),
        publisher_ip: r.get(4)?,
        video_codec: r.get(5)?,
        audio_codec: r.get(6)?,
        width: r.get(7)?,
        height: r.get(8)?,
        fps: r.get(9)?,
        bytes_in: r.get::<_, i64>(10)?.max(0) as u64,
        peak_viewers: r.get(11)?,
    })
}

/// Insert a new open row (tagged with [`Db::node_id`]) and return its id.
pub async fn open(
    db: &Db,
    broadcast_id: &str,
    started_at: DateTime<Utc>,
    publisher_ip: Option<String>,
) -> DbResult<String> {
    let id = uuid::Uuid::new_v4().to_string();
    let (rid, bid, node) = (
        id.clone(),
        broadcast_id.to_string(),
        db.node_id().to_string(),
    );
    if let Some(pool) = db.pg() {
        q(
            "INSERT INTO stream_sessions (id, broadcast_id, started_at, publisher_ip, node_id) \
           VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(rid)
        .bind(bid)
        .bind(fmt_ts(&started_at))
        .bind(publisher_ip)
        .bind(node)
        .execute(pool)
        .await?;
        return Ok(id);
    }
    db.call(move |c| {
        c.execute(
            "INSERT INTO stream_sessions (id, broadcast_id, started_at, publisher_ip, node_id) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![rid, bid, fmt_ts(&started_at), publisher_ip, node],
        )?;
        Ok(())
    })
    .await?;
    Ok(id)
}

/// Close an open row. Closing an already closed row is a no-op.
pub async fn close(db: &Db, id: &str, end: StreamSessionEnd) -> DbResult<()> {
    let id = id.to_string();
    if let Some(pool) = db.pg() {
        q("UPDATE stream_sessions SET ended_at = $2, video_codec = $3, audio_codec = $4, width = $5, \
           height = $6, fps = $7, bytes_in = $8, peak_viewers = $9 WHERE id = $1 AND ended_at IS NULL")
        .bind(id)
        .bind(fmt_ts(&end.ended_at))
        .bind(end.video_codec)
        .bind(end.audio_codec)
        .bind(end.width.map(i64::from))
        .bind(end.height.map(i64::from))
        .bind(end.fps)
        .bind(i64_of(end.bytes_in))
        .bind(i64::from(end.peak_viewers))
        .execute(pool)
        .await?;
        return Ok(());
    }
    db.call(move |c| {
        c.execute(
            "UPDATE stream_sessions SET ended_at = ?2, video_codec = ?3, audio_codec = ?4, width = ?5, \
             height = ?6, fps = ?7, bytes_in = ?8, peak_viewers = ?9 WHERE id = ?1 AND ended_at IS NULL",
            params![
                id,
                fmt_ts(&end.ended_at),
                end.video_codec,
                end.audio_codec,
                end.width,
                end.height,
                end.fps,
                i64_of(end.bytes_in),
                end.peak_viewers
            ],
        )?;
        Ok(())
    })
    .await
}

/// Crash recovery at startup: close the rows left open by a previous process of **this
/// node** (other nodes' live periods are left alone).
pub async fn close_dangling(db: &Db) -> DbResult<usize> {
    let node = db.node_id().to_string();
    if let Some(pool) = db.pg() {
        let n =
            q("UPDATE stream_sessions SET ended_at = $1 WHERE ended_at IS NULL AND node_id = $2")
                .bind(super::now_ts())
                .bind(node)
                .execute(pool)
                .await?
                .rows_affected();
        return Ok(n as usize);
    }
    db.call(move |c| {
        let now = super::now_ts();
        Ok(c.execute(
            "UPDATE stream_sessions SET ended_at = ?1 WHERE ended_at IS NULL AND node_id = ?2",
            params![now, node],
        )?)
    })
    .await
}

const SELECT: &str = "SELECT id, broadcast_id, started_at, ended_at, publisher_ip, video_codec, \
     audio_codec, width, height, fps, bytes_in, peak_viewers FROM stream_sessions";

/// Most recent sessions of a broadcast, newest first.
pub async fn list_for_broadcast(
    db: &Db,
    broadcast_id: &str,
    limit: u32,
) -> DbResult<Vec<StreamSession>> {
    let bid = broadcast_id.to_string();
    if let Some(pool) = db.pg() {
        return q_dyn(format!(
            "{SELECT} WHERE broadcast_id = $1 ORDER BY started_at DESC LIMIT $2"
        ))
        .bind(bid)
        .bind(i64::from(limit))
        .fetch_all(pool)
        .await?
        .iter()
        .map(pg_row)
        .collect();
    }
    db.call(move |c| {
        let mut st = c.prepare(&format!(
            "{SELECT} WHERE broadcast_id = ?1 ORDER BY started_at DESC LIMIT ?2"
        ))?;
        let rows = st
            .query_map(params![bid, limit], from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })
    .await
}

fn pg_row(r: &PgRow) -> DbResult<StreamSession> {
    Ok(StreamSession {
        id: r.try_get(0)?,
        broadcast_id: r.try_get(1)?,
        started_at: parse_ts(&r.try_get::<String, _>(2)?),
        ended_at: r.try_get::<Option<String>, _>(3)?.map(|s| parse_ts(&s)),
        publisher_ip: r.try_get(4)?,
        video_codec: r.try_get(5)?,
        audio_codec: r.try_get(6)?,
        width: get_opt_u32(r, 7)?,
        height: get_opt_u32(r, 8)?,
        fps: r.try_get(9)?,
        bytes_in: get_u64(r, 10)?,
        peak_viewers: get_u32(r, 11)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn open_close_body(db: Db) {
        let id = open(&db, "b1", Utc::now(), Some("1.2.3.4".into()))
            .await
            .unwrap();
        let rows = list_for_broadcast(&db, "b1", 10).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].ended_at.is_none());
        close(
            &db,
            &id,
            StreamSessionEnd {
                ended_at: Utc::now(),
                video_codec: Some("h264".into()),
                bytes_in: 42,
                peak_viewers: 3,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let rows = list_for_broadcast(&db, "b1", 10).await.unwrap();
        assert!(rows[0].ended_at.is_some());
        assert_eq!(rows[0].bytes_in, 42);
        assert_eq!(rows[0].peak_viewers, 3);
        assert_eq!(rows[0].video_codec.as_deref(), Some("h264"));

        assert_eq!(rows[0].publisher_ip.as_deref(), Some("1.2.3.4"));

        let _open2 = open(&db, "b1", Utc::now(), None).await.unwrap();
        // Rows of another node are not "dangling" for this one.
        let other = db.clone().with_node_id("other-node");
        let _open3 = open(&other, "b1", Utc::now(), None).await.unwrap();
        assert_eq!(close_dangling(&db).await.unwrap(), 1);
        assert_eq!(close_dangling(&other).await.unwrap(), 1);
        assert_eq!(close_dangling(&db).await.unwrap(), 0);
        assert_eq!(list_for_broadcast(&db, "b1", 2).await.unwrap().len(), 2);
        assert_eq!(list_for_broadcast(&db, "b1", 10).await.unwrap().len(), 3);
    }
    crate::db::testkit::dual_db_test!(open_close, open_close_body);
}
