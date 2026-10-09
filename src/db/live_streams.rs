//! `live_streams` repository: who owns a live stream in cluster mode (docs/CLUSTER.md
//! §3.3). One row per live broadcast name = its owner node (the node the publisher is
//! connected to).
//!
//! - [`claim`] at publish: insert, or take over a row whose heartbeat is older than the
//!   TTL (or that this node already owns); otherwise the stream is live on another
//!   node ([`Claim::OwnedBy`]).
//! - [`heartbeat_all`] every `cluster.heartbeat_secs` refreshes this node's rows; fewer
//!   rows than local streams means ownership was lost ([`owned_by`] tells which).
//! - [`release`] when the publisher leaves (only the claiming session's row).
//! - [`reap`] (every node) deletes the rows of dead owners; the caller NOTIFYs.
//!
//! PostgreSQL compares heartbeats on the database clock; SQLite (single node, tests)
//! uses the process clock and RFC 3339 text. Single-node mode never calls this module.

use std::time::Duration;

use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sqlx_core::row::Row as _;
use sqlx_postgres::PgRow;

use super::pg::q;
use super::{Db, DbResult, fmt_ts, now_ts, parse_ts};

/// One `live_streams` row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LiveStreamRow {
    pub name: String,
    pub node_id: String,
    /// Claim id of the publishing session (a new one at every publish).
    pub session_id: String,
    pub started_at: DateTime<Utc>,
    pub heartbeat: DateTime<Utc>,
}

/// Outcome of [`claim`].
#[derive(Debug, Clone, PartialEq)]
pub enum Claim {
    /// This node owns the stream now.
    Claimed,
    /// Live on another node (fresh heartbeat).
    OwnedBy(LiveStreamRow),
}

fn secs(ttl: Duration) -> f64 {
    ttl.as_secs_f64()
}

fn cutoff(ttl: Duration) -> String {
    fmt_ts(&(Utc::now() - chrono::Duration::milliseconds(ttl.as_millis() as i64)))
}

const COLUMNS: &str = "name, node_id, session_id, started_at, heartbeat";

fn pg_row(r: &PgRow) -> DbResult<LiveStreamRow> {
    Ok(LiveStreamRow {
        name: r.try_get(0)?,
        node_id: r.try_get(1)?,
        session_id: r.try_get(2)?,
        started_at: r.try_get(3)?,
        heartbeat: r.try_get(4)?,
    })
}

fn sqlite_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<LiveStreamRow> {
    Ok(LiveStreamRow {
        name: r.get(0)?,
        node_id: r.get(1)?,
        session_id: r.get(2)?,
        started_at: parse_ts(&r.get::<_, String>(3)?),
        heartbeat: parse_ts(&r.get::<_, String>(4)?),
    })
}

/// Take ownership of `name` for `node_id` (CLUSTER.md §3.3): insert the row, or
/// overwrite it when its heartbeat is older than `ttl` or it already belongs to
/// `node_id` (a reconnect on the same node). Otherwise [`Claim::OwnedBy`] the live
/// owner. The node row must exist (foreign key on PostgreSQL).
pub async fn claim(
    db: &Db,
    name: &str,
    node_id: &str,
    session_id: &str,
    ttl: Duration,
) -> DbResult<Claim> {
    // The row can vanish between the conflicting insert and the read (released or
    // reaped): one more attempt then.
    for _ in 0..2 {
        if try_claim(db, name, node_id, session_id, ttl).await? {
            return Ok(Claim::Claimed);
        }
        if let Some(row) = owner(db, name).await? {
            return Ok(Claim::OwnedBy(row));
        }
    }
    Ok(Claim::OwnedBy(LiveStreamRow {
        name: name.to_string(),
        node_id: String::new(),
        session_id: String::new(),
        started_at: Utc::now(),
        heartbeat: Utc::now(),
    }))
}

async fn try_claim(
    db: &Db,
    name: &str,
    node_id: &str,
    session_id: &str,
    ttl: Duration,
) -> DbResult<bool> {
    let (name, node, sid) = (
        name.to_string(),
        node_id.to_string(),
        session_id.to_string(),
    );
    if let Some(pool) = db.pg() {
        let row = q(
            "INSERT INTO live_streams (name, node_id, session_id, started_at, heartbeat) \
             VALUES ($1, $2, $3, now(), now()) \
             ON CONFLICT (name) DO UPDATE SET node_id = excluded.node_id, \
               session_id = excluded.session_id, started_at = now(), heartbeat = now() \
             WHERE live_streams.heartbeat < now() - make_interval(secs => $4) \
               OR live_streams.node_id = excluded.node_id \
             RETURNING session_id",
        )
        .bind(name)
        .bind(node)
        .bind(sid)
        .bind(secs(ttl))
        .fetch_optional(pool)
        .await?;
        return Ok(row.is_some());
    }
    let cut = cutoff(ttl);
    db.call(move |c| {
        let now = now_ts();
        let row = c
            .query_row(
                "INSERT INTO live_streams (name, node_id, session_id, started_at, heartbeat) \
                 VALUES (?1, ?2, ?3, ?4, ?4) \
                 ON CONFLICT (name) DO UPDATE SET node_id = excluded.node_id, \
                   session_id = excluded.session_id, started_at = excluded.started_at, \
                   heartbeat = excluded.heartbeat \
                 WHERE live_streams.heartbeat < ?5 OR live_streams.node_id = excluded.node_id \
                 RETURNING session_id",
                params![name, node, sid, now, cut],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        Ok(row.is_some())
    })
    .await
}

/// Refresh the heartbeat of every row of `node_id`; returns how many rows were
/// refreshed (fewer than the node's local streams = ownership lost).
pub async fn heartbeat_all(db: &Db, node_id: &str) -> DbResult<u64> {
    let node = node_id.to_string();
    if let Some(pool) = db.pg() {
        return Ok(
            q("UPDATE live_streams SET heartbeat = now() WHERE node_id = $1")
                .bind(node)
                .execute(pool)
                .await?
                .rows_affected(),
        );
    }
    db.call(move |c| {
        Ok(c.execute(
            "UPDATE live_streams SET heartbeat = ?2 WHERE node_id = ?1",
            params![node, now_ts()],
        )? as u64)
    })
    .await
}

/// Rows owned by `node_id`, by name.
pub async fn owned_by(db: &Db, node_id: &str) -> DbResult<Vec<LiveStreamRow>> {
    let node = node_id.to_string();
    if let Some(pool) = db.pg() {
        return q(
            "SELECT name, node_id, session_id, started_at, heartbeat FROM live_streams \
                  WHERE node_id = $1 ORDER BY name",
        )
        .bind(node)
        .fetch_all(pool)
        .await?
        .iter()
        .map(pg_row)
        .collect();
    }
    db.call(move |c| {
        let mut st = c.prepare(&format!(
            "SELECT {COLUMNS} FROM live_streams WHERE node_id = ?1 ORDER BY name"
        ))?;
        let rows = st
            .query_map([node], sqlite_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })
    .await
}

/// Give up ownership of `name`: deletes the row only if it still belongs to `node_id`
/// and, when given, to the claiming `session_id` (a newer publish on the same node keeps
/// its row). `true` when a row was deleted.
pub async fn release(
    db: &Db,
    name: &str,
    node_id: &str,
    session_id: Option<&str>,
) -> DbResult<bool> {
    let (name, node, sid) = (
        name.to_string(),
        node_id.to_string(),
        session_id.map(str::to_string),
    );
    if let Some(pool) = db.pg() {
        let n = q("DELETE FROM live_streams WHERE name = $1 AND node_id = $2 \
                   AND ($3::TEXT IS NULL OR session_id = $3)")
        .bind(name)
        .bind(node)
        .bind(sid)
        .execute(pool)
        .await?
        .rows_affected();
        return Ok(n > 0);
    }
    db.call(move |c| {
        let n = c.execute(
            "DELETE FROM live_streams WHERE name = ?1 AND node_id = ?2 \
             AND (?3 IS NULL OR session_id = ?3)",
            params![name, node, sid],
        )?;
        Ok(n > 0)
    })
    .await
}

/// The owner row of `name` (fresh or not).
pub async fn owner(db: &Db, name: &str) -> DbResult<Option<LiveStreamRow>> {
    let name = name.to_string();
    if let Some(pool) = db.pg() {
        return q("SELECT name, node_id, session_id, started_at, heartbeat \
                  FROM live_streams WHERE name = $1")
        .bind(name)
        .fetch_optional(pool)
        .await?
        .as_ref()
        .map(pg_row)
        .transpose();
    }
    db.call(move |c| {
        Ok(c.query_row(
            &format!("SELECT {COLUMNS} FROM live_streams WHERE name = ?1"),
            [name],
            sqlite_row,
        )
        .optional()?)
    })
    .await
}

/// Rows whose heartbeat is newer than `ttl`, by name.
pub async fn list_alive(db: &Db, ttl: Duration) -> DbResult<Vec<LiveStreamRow>> {
    if let Some(pool) = db.pg() {
        return q(
            "SELECT name, node_id, session_id, started_at, heartbeat FROM live_streams \
                  WHERE heartbeat >= now() - make_interval(secs => $1) ORDER BY name",
        )
        .bind(secs(ttl))
        .fetch_all(pool)
        .await?
        .iter()
        .map(pg_row)
        .collect();
    }
    let cut = cutoff(ttl);
    db.call(move |c| {
        let mut st = c.prepare(&format!(
            "SELECT {COLUMNS} FROM live_streams WHERE heartbeat >= ?1 ORDER BY name"
        ))?;
        let rows = st
            .query_map([cut], sqlite_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })
    .await
}

/// Delete the rows whose heartbeat is older than `ttl` (dead owner); returns them, by
/// name (the caller NOTIFYs `rms_streams`).
pub async fn reap(db: &Db, ttl: Duration) -> DbResult<Vec<LiveStreamRow>> {
    if let Some(pool) = db.pg() {
        let mut rows = q("DELETE FROM live_streams \
                          WHERE heartbeat < now() - make_interval(secs => $1) \
                          RETURNING name, node_id, session_id, started_at, heartbeat")
        .bind(secs(ttl))
        .fetch_all(pool)
        .await?
        .iter()
        .map(pg_row)
        .collect::<DbResult<Vec<_>>>()?;
        rows.sort_by(|a, b| a.name.cmp(&b.name));
        return Ok(rows);
    }
    let cut = cutoff(ttl);
    db.call(move |c| {
        let mut st = c.prepare(&format!(
            "DELETE FROM live_streams WHERE heartbeat < ?1 RETURNING {COLUMNS}"
        ))?;
        let mut rows = st
            .query_map([cut], sqlite_row)?
            .collect::<Result<Vec<_>, _>>()?;
        rows.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(rows)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::nodes::{self, NodeRegistration};
    use crate::db::testkit::execute;

    async fn node(db: &Db, id: &str) {
        nodes::register(
            db,
            &NodeRegistration {
                node_id: id.into(),
                advertise_url: format!("http://{id}:8081"),
                whep_url: None,
                version: "t".into(),
                started_at: Utc::now(),
            },
        )
        .await
        .unwrap();
    }

    async fn age(db: &Db, name: &str) {
        let sql = if db.pg().is_some() {
            format!(
                "UPDATE live_streams SET heartbeat = now() - interval '1 minute' WHERE name = '{name}'"
            )
        } else {
            format!(
                "UPDATE live_streams SET heartbeat = '2000-01-01T00:00:00.000Z' WHERE name = '{name}'"
            )
        };
        execute(db, &sql).await;
    }

    async fn ownership_body(db: Db) {
        let ttl = Duration::from_secs(15);
        node(&db, "n1").await;
        node(&db, "n2").await;

        // First claim wins; the other node sees the owner.
        assert_eq!(
            claim(&db, "show", "n1", "s1", ttl).await.unwrap(),
            Claim::Claimed
        );
        match claim(&db, "show", "n2", "s2", ttl).await.unwrap() {
            Claim::OwnedBy(row) => {
                assert_eq!(
                    (row.node_id.as_str(), row.session_id.as_str()),
                    ("n1", "s1")
                )
            }
            other => panic!("expected OwnedBy, got {other:?}"),
        }
        // The owner can claim again (reconnect on the same node): new session id.
        assert_eq!(
            claim(&db, "show", "n1", "s3", ttl).await.unwrap(),
            Claim::Claimed
        );
        let row = owner(&db, "show").await.unwrap().unwrap();
        assert_eq!(
            (row.node_id.as_str(), row.session_id.as_str()),
            ("n1", "s3")
        );
        // Releasing an older session of the same node keeps the newer claim.
        assert!(!release(&db, "show", "n1", Some("s1")).await.unwrap());
        assert!(!release(&db, "show", "n2", None).await.unwrap());
        assert!(owner(&db, "show").await.unwrap().is_some());

        // Heartbeat refreshes only the node's rows.
        assert_eq!(
            claim(&db, "other", "n2", "o1", ttl).await.unwrap(),
            Claim::Claimed
        );
        age(&db, "show").await;
        assert_eq!(
            list_alive(&db, ttl).await.unwrap().len(),
            1,
            "show is stale"
        );
        assert_eq!(heartbeat_all(&db, "n1").await.unwrap(), 1);
        assert_eq!(heartbeat_all(&db, "ghost").await.unwrap(), 0);
        let alive: Vec<String> = list_alive(&db, ttl)
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.name)
            .collect();
        assert_eq!(alive, ["other", "show"]);
        let names = |rows: Vec<LiveStreamRow>| rows.into_iter().map(|r| r.name).collect::<Vec<_>>();
        assert_eq!(names(owned_by(&db, "n1").await.unwrap()), ["show"]);
        assert_eq!(names(owned_by(&db, "n2").await.unwrap()), ["other"]);

        // Takeover of a stale row by another node.
        age(&db, "show").await;
        assert_eq!(
            claim(&db, "show", "n2", "s4", ttl).await.unwrap(),
            Claim::Claimed
        );
        let row = owner(&db, "show").await.unwrap().unwrap();
        assert_eq!(
            (row.node_id.as_str(), row.session_id.as_str()),
            ("n2", "s4")
        );
        // n1 lost it: its heartbeat refreshes nothing.
        assert_eq!(heartbeat_all(&db, "n1").await.unwrap(), 0);
        assert!(owned_by(&db, "n1").await.unwrap().is_empty());

        // Reaper: only stale rows, returned once.
        age(&db, "other").await;
        let reaped = reap(&db, ttl).await.unwrap();
        assert_eq!(reaped.len(), 1);
        assert_eq!(
            (reaped[0].name.as_str(), reaped[0].node_id.as_str()),
            ("other", "n2")
        );
        assert!(reap(&db, ttl).await.unwrap().is_empty());
        assert!(owner(&db, "other").await.unwrap().is_none());

        // Release by the claiming session.
        assert!(release(&db, "show", "n2", Some("s4")).await.unwrap());
        assert!(owner(&db, "show").await.unwrap().is_none());
        assert!(list_alive(&db, ttl).await.unwrap().is_empty());
    }
    crate::db::testkit::dual_db_test!(ownership, ownership_body);
}
