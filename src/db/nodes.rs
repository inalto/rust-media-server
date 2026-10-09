//! `nodes` repository: the cluster node registry (docs/CLUSTER.md §2.2, §3.2).
//!
//! Each node upserts its row at startup ([`register`]), refreshes `heartbeat` and its
//! counters every `cluster.heartbeat_secs` ([`heartbeat`]) and deletes it on a graceful
//! shutdown ([`remove`]). A node is *alive* while `heartbeat > now - node_ttl`; every
//! node runs the reaper ([`reap`]) that deletes the dead rows (which also drops their
//! `live_streams` ownership rows: `ON DELETE CASCADE`).
//!
//! PostgreSQL uses the database clock (`now()`) for every heartbeat comparison, so the
//! nodes' clocks do not need to agree; SQLite (single node, tests) uses the process
//! clock and RFC 3339 text.

use std::time::Duration;

use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, params};
use serde::Serialize;
use sqlx_core::row::Row as _;
use sqlx_postgres::PgRow;

use super::pg::q;
use super::{Db, DbResult, fmt_ts, parse_ts};

/// What a node announces about itself at startup.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeRegistration {
    pub node_id: String,
    /// Internal URL reachable by the other nodes (`cluster.advertise_url`).
    pub advertise_url: String,
    /// Public WHEP base of this node (`<public_url>/whep`) when WebRTC is available.
    pub whep_url: Option<String>,
    pub version: String,
    pub started_at: DateTime<Utc>,
}

/// Values refreshed by every heartbeat.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct NodeLoad {
    pub live_streams: u32,
    pub viewers: u32,
    pub draining: bool,
}

/// One row of `nodes` (`GET /system` → `cluster.nodes[]`).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct NodeInfo {
    pub node_id: String,
    pub advertise_url: String,
    pub whep_url: Option<String>,
    pub version: String,
    pub started_at: DateTime<Utc>,
    pub heartbeat: DateTime<Utc>,
    pub live_streams: u32,
    pub viewers: u32,
    pub draining: bool,
    /// `heartbeat` newer than `now - node_ttl` (database clock).
    pub alive: bool,
}

fn secs(ttl: Duration) -> f64 {
    ttl.as_secs_f64()
}

/// Insert or refresh this node's row (`heartbeat = now`, counters reset).
pub async fn register(db: &Db, reg: &NodeRegistration) -> DbResult<()> {
    let r = reg.clone();
    if let Some(pool) = db.pg() {
        q(
            "INSERT INTO nodes (node_id, advertise_url, whep_url, version, started_at, heartbeat, \
             live_streams, viewers, draining) VALUES ($1, $2, $3, $4, $5, now(), 0, 0, false) \
           ON CONFLICT (node_id) DO UPDATE SET advertise_url = excluded.advertise_url, \
             whep_url = excluded.whep_url, version = excluded.version, \
             started_at = excluded.started_at, heartbeat = now(), live_streams = 0, viewers = 0, \
             draining = false",
        )
        .bind(r.node_id)
        .bind(r.advertise_url)
        .bind(r.whep_url)
        .bind(r.version)
        .bind(r.started_at)
        .execute(pool)
        .await?;
        return Ok(());
    }
    db.call(move |c| {
        c.execute(
            "INSERT INTO nodes (node_id, advertise_url, whep_url, version, started_at, heartbeat, \
               live_streams, viewers, draining) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, 0, 0) \
             ON CONFLICT (node_id) DO UPDATE SET advertise_url = excluded.advertise_url, \
               whep_url = excluded.whep_url, version = excluded.version, \
               started_at = excluded.started_at, heartbeat = excluded.heartbeat, \
               live_streams = 0, viewers = 0, draining = 0",
            params![
                r.node_id,
                r.advertise_url,
                r.whep_url,
                r.version,
                fmt_ts(&r.started_at),
                super::now_ts()
            ],
        )?;
        Ok(())
    })
    .await
}

/// Refresh `heartbeat` and the counters. `false` when the row is gone (reaped by another
/// node after a long pause): the caller registers again.
pub async fn heartbeat(db: &Db, node_id: &str, load: NodeLoad) -> DbResult<bool> {
    let node = node_id.to_string();
    if let Some(pool) = db.pg() {
        let n = q(
            "UPDATE nodes SET heartbeat = now(), live_streams = $2, viewers = $3, \
                   draining = $4 WHERE node_id = $1",
        )
        .bind(node)
        .bind(load.live_streams.min(i32::MAX as u32) as i32)
        .bind(load.viewers.min(i32::MAX as u32) as i32)
        .bind(load.draining)
        .execute(pool)
        .await?
        .rows_affected();
        return Ok(n == 1);
    }
    db.call(move |c| {
        let n = c.execute(
            "UPDATE nodes SET heartbeat = ?2, live_streams = ?3, viewers = ?4, draining = ?5 \
             WHERE node_id = ?1",
            params![
                node,
                super::now_ts(),
                load.live_streams,
                load.viewers,
                load.draining
            ],
        )?;
        Ok(n == 1)
    })
    .await
}

/// Every row (dead ones included until reaped), by `node_id`, with `alive` computed for
/// `ttl`.
pub async fn list(db: &Db, ttl: Duration) -> DbResult<Vec<NodeInfo>> {
    if let Some(pool) = db.pg() {
        return q(
            "SELECT node_id, advertise_url, whep_url, version, started_at, heartbeat, \
                   live_streams, viewers, draining, heartbeat > now() - make_interval(secs => $1) \
                   FROM nodes ORDER BY node_id",
        )
        .bind(secs(ttl))
        .fetch_all(pool)
        .await?
        .iter()
        .map(pg_row)
        .collect();
    }
    let cutoff = fmt_ts(&(Utc::now() - chrono::Duration::milliseconds(ttl.as_millis() as i64)));
    db.call(move |c| {
        let mut st = c.prepare(
            "SELECT node_id, advertise_url, whep_url, version, started_at, heartbeat, \
               live_streams, viewers, draining, heartbeat > ?1 FROM nodes ORDER BY node_id",
        )?;
        let rows = st
            .query_map([cutoff], |r| {
                Ok(NodeInfo {
                    node_id: r.get(0)?,
                    advertise_url: r.get(1)?,
                    whep_url: r.get(2)?,
                    version: r.get(3)?,
                    started_at: parse_ts(&r.get::<_, String>(4)?),
                    heartbeat: parse_ts(&r.get::<_, String>(5)?),
                    live_streams: r.get(6)?,
                    viewers: r.get(7)?,
                    draining: r.get(8)?,
                    alive: r.get(9)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })
    .await
}

/// The nodes whose heartbeat is newer than `ttl` (`alive`), by `node_id`.
pub async fn list_alive(db: &Db, ttl: Duration) -> DbResult<Vec<NodeInfo>> {
    Ok(list(db, ttl)
        .await?
        .into_iter()
        .filter(|n| n.alive)
        .collect())
}

/// One node.
pub async fn get(db: &Db, node_id: &str, ttl: Duration) -> DbResult<Option<NodeInfo>> {
    Ok(list(db, ttl)
        .await?
        .into_iter()
        .find(|n| n.node_id == node_id))
}

/// Delete the nodes whose heartbeat is older than `ttl`; returns their ids.
pub async fn reap(db: &Db, ttl: Duration) -> DbResult<Vec<String>> {
    if let Some(pool) = db.pg() {
        let mut ids = q(
            "DELETE FROM nodes WHERE heartbeat < now() - make_interval(secs => $1) \
                         RETURNING node_id",
        )
        .bind(secs(ttl))
        .fetch_all(pool)
        .await?
        .iter()
        .map(|r| Ok(r.try_get::<String, _>(0)?))
        .collect::<DbResult<Vec<_>>>()?;
        ids.sort();
        return Ok(ids);
    }
    let cutoff = fmt_ts(&(Utc::now() - chrono::Duration::milliseconds(ttl.as_millis() as i64)));
    db.call(move |c| {
        let tx = c.transaction()?;
        let ids = {
            let mut st =
                tx.prepare("SELECT node_id FROM nodes WHERE heartbeat < ?1 ORDER BY node_id")?;
            st.query_map([&cutoff], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?
        };
        tx.execute("DELETE FROM live_streams WHERE node_id IN (SELECT node_id FROM nodes WHERE heartbeat < ?1)", [&cutoff])?;
        tx.execute("DELETE FROM nodes WHERE heartbeat < ?1", [&cutoff])?;
        tx.commit()?;
        Ok(ids)
    })
    .await
}

/// Delete this node's row (graceful shutdown). `false` when it was already gone.
pub async fn remove(db: &Db, node_id: &str) -> DbResult<bool> {
    let node = node_id.to_string();
    if let Some(pool) = db.pg() {
        let n = q("DELETE FROM nodes WHERE node_id = $1")
            .bind(node)
            .execute(pool)
            .await?
            .rows_affected();
        return Ok(n == 1);
    }
    db.call(move |c| {
        c.execute("DELETE FROM live_streams WHERE node_id = ?1", [&node])?;
        let n = c
            .query_row(
                "DELETE FROM nodes WHERE node_id = ?1 RETURNING node_id",
                [&node],
                |_| Ok(()),
            )
            .optional()?;
        Ok(n.is_some())
    })
    .await
}

fn pg_row(r: &PgRow) -> DbResult<NodeInfo> {
    Ok(NodeInfo {
        node_id: r.try_get(0)?,
        advertise_url: r.try_get(1)?,
        whep_url: r.try_get(2)?,
        version: r.try_get(3)?,
        started_at: r.try_get(4)?,
        heartbeat: r.try_get(5)?,
        live_streams: r.try_get::<i32, _>(6)?.max(0) as u32,
        viewers: r.try_get::<i32, _>(7)?.max(0) as u32,
        draining: r.try_get(8)?,
        alive: r.try_get(9)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::testkit::{execute, scalar_i64};

    fn reg(id: &str) -> NodeRegistration {
        NodeRegistration {
            node_id: id.into(),
            advertise_url: format!("http://{id}:8081"),
            whep_url: Some(format!("https://stream.example.com/whep#{id}")),
            version: "0.1.0".into(),
            started_at: parse_ts("2026-10-09T10:00:00.000Z"),
        }
    }

    async fn registry_body(db: Db) {
        let ttl = Duration::from_secs(15);
        register(&db, &reg("n1")).await.unwrap();
        register(&db, &reg("n2")).await.unwrap();
        let all = list(&db, ttl).await.unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].node_id, "n1");
        assert_eq!(all[0].advertise_url, "http://n1:8081");
        assert_eq!(all[0].started_at, parse_ts("2026-10-09T10:00:00.000Z"));
        assert!(
            all.iter()
                .all(|n| n.alive && !n.draining && n.live_streams == 0)
        );

        let load = NodeLoad {
            live_streams: 2,
            viewers: 17,
            draining: true,
        };
        assert!(heartbeat(&db, "n1", load).await.unwrap());
        assert!(!heartbeat(&db, "ghost", load).await.unwrap());
        let n1 = get(&db, "n1", ttl).await.unwrap().unwrap();
        assert_eq!((n1.live_streams, n1.viewers, n1.draining), (2, 17, true));
        // Re-registering (restart) resets the counters.
        register(&db, &reg("n1")).await.unwrap();
        let n1 = get(&db, "n1", ttl).await.unwrap().unwrap();
        assert_eq!((n1.live_streams, n1.viewers, n1.draining), (0, 0, false));

        // n2 stops heartbeating: dead after the TTL, then reaped.
        let stale = if db.pg().is_some() {
            "UPDATE nodes SET heartbeat = now() - interval '1 minute' WHERE node_id = 'n2'"
        } else {
            "UPDATE nodes SET heartbeat = '2000-01-01T00:00:00.000Z' WHERE node_id = 'n2'"
        };
        execute(&db, stale).await;
        let alive: Vec<String> = list_alive(&db, ttl)
            .await
            .unwrap()
            .into_iter()
            .map(|n| n.node_id)
            .collect();
        assert_eq!(alive, ["n1"]);
        assert_eq!(list(&db, ttl).await.unwrap().len(), 2, "dead until reaped");
        // Its stream ownership goes with it.
        let own = if db.pg().is_some() {
            "INSERT INTO live_streams (name, node_id, session_id, started_at) VALUES ('x', 'n2', 's', now())"
        } else {
            "INSERT INTO live_streams (name, node_id, session_id, started_at, heartbeat) \
             VALUES ('x', 'n2', 's', '2000-01-01T00:00:00.000Z', '2000-01-01T00:00:00.000Z')"
        };
        execute(&db, own).await;
        assert_eq!(reap(&db, ttl).await.unwrap(), ["n2"]);
        assert!(reap(&db, ttl).await.unwrap().is_empty());
        assert_eq!(
            scalar_i64(&db, "SELECT COUNT(*) FROM live_streams").await,
            0
        );

        assert!(remove(&db, "n1").await.unwrap());
        assert!(!remove(&db, "n1").await.unwrap());
        assert!(list(&db, ttl).await.unwrap().is_empty());
    }
    crate::db::testkit::dual_db_test!(registry, registry_body);
}
