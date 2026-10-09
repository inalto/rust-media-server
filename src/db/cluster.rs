//! Cluster-wide singleton tasks (`singleton_tasks`, docs/CLUSTER.md §3.7): periodic work
//! that must run on one node at a time (metrics retention, the VOD inbox scanner).
//!
//! [`try_lease`] takes or renews the named lease for `duration`; the holder keeps
//! renewing it every time it runs, another node takes over once it expired. On SQLite
//! (single node) the lease is a no-op that always succeeds. The lease is a plain
//! conditional `UPSERT … RETURNING` on the database clock: no session-level advisory
//! lock, so it also works behind PgBouncer in transaction mode.

use std::time::Duration;

use super::Db;
use super::DbResult;
use super::pg::q;

/// Lease of the metrics retention (daily task; at most about two runs a day cluster-wide).
pub const TASK_METRICS_RETENTION: &str = "metrics-retention";
/// Lease of the VOD inbox scanner (renewed at every scan).
pub const TASK_VOD_INBOX: &str = "vod-inbox";

/// Take or renew the singleton lease `name` for `duration` as this node
/// ([`Db::node_id`]). `true` when this node holds it now.
pub async fn try_lease(db: &Db, name: &str, duration: Duration) -> DbResult<bool> {
    let Some(pool) = db.pg() else {
        return Ok(true);
    };
    let row = q(
        "INSERT INTO singleton_tasks (name, leased_by, leased_until) \
                 VALUES ($1, $2, now() + make_interval(secs => $3)) \
                 ON CONFLICT (name) DO UPDATE SET leased_by = excluded.leased_by, \
                   leased_until = excluded.leased_until \
                 WHERE singleton_tasks.leased_until < now() \
                   OR singleton_tasks.leased_by = excluded.leased_by \
                 RETURNING name",
    )
    .bind(name.to_string())
    .bind(db.node_id().to_string())
    .bind(duration.as_secs_f64())
    .fetch_optional(pool)
    .await?;
    Ok(row.is_some())
}

/// Give a lease back early (graceful shutdown of the holder). No-op on SQLite.
pub async fn release(db: &Db, name: &str) -> DbResult<()> {
    if let Some(pool) = db.pg() {
        q("UPDATE singleton_tasks SET leased_until = '-infinity' WHERE name = $1 AND leased_by = $2")
            .bind(name.to_string())
            .bind(db.node_id().to_string())
            .execute(pool)
            .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn singleton_body(db: Db) {
        let n1 = db.clone().with_node_id("n1");
        let n2 = db.clone().with_node_id("n2");
        let d = Duration::from_secs(60);
        assert!(try_lease(&n1, TASK_VOD_INBOX, d).await.unwrap());
        assert!(try_lease(&n1, TASK_VOD_INBOX, d).await.unwrap(), "renewal");
        if db.pg().is_none() {
            // SQLite: single node, no-op lease.
            assert!(try_lease(&n2, TASK_VOD_INBOX, d).await.unwrap());
            return;
        }
        assert!(
            !try_lease(&n2, TASK_VOD_INBOX, d).await.unwrap(),
            "held by n1"
        );
        assert!(
            try_lease(&n2, TASK_METRICS_RETENTION, d).await.unwrap(),
            "other task"
        );
        release(&n1, TASK_VOD_INBOX).await.unwrap();
        assert!(try_lease(&n2, TASK_VOD_INBOX, d).await.unwrap(), "released");
        assert!(!try_lease(&n1, TASK_VOD_INBOX, d).await.unwrap());
        // Expiry.
        assert!(
            try_lease(&n2, TASK_VOD_INBOX, Duration::from_millis(1))
                .await
                .unwrap()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(try_lease(&n1, TASK_VOD_INBOX, d).await.unwrap(), "expired");
    }
    crate::db::testkit::dual_db_test!(singleton, singleton_body);
}
