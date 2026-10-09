//! PostgreSQL backend: schema, migrations and small helpers shared by the repositories.
//!
//! The schema is created fresh (no SQLite history to replay): migration 1 is the full
//! phase 7C-1 schema, i.e. ARCHITECTURE.md §7 + `vod_items` + the columns and tables of
//! docs/CLUSTER.md §3.2. Applied versions are recorded in `rms_schema_migrations`; every
//! node runs [`migrate`] at startup inside one transaction holding a transaction-level
//! advisory lock, so concurrent starts apply each migration exactly once (DDL is
//! transactional in PostgreSQL).
//!
//! Differences from the SQLite schema: booleans are `BOOLEAN`, integers `BIGINT`
//! (except `INTEGER` where CLUSTER.md §3.2 says `INT`), reals `DOUBLE PRECISION`, the
//! text columns that are compared or sorted use `COLLATE "C"` (byte order, like SQLite,
//! whatever the database locale), `viewer_samples` has the primary key
//! `(broadcast_id, node_id, ts)` and a mandatory `ts_epoch`, and the cluster bookkeeping
//! columns (`nodes`, `live_streams`, `singleton_tasks`, `lease_until`) are `TIMESTAMPTZ`
//! driven by the database clock (`now()`).

use sqlx_core::query::Query;
use sqlx_core::raw_sql::raw_sql;
use sqlx_core::row::Row;
use sqlx_postgres::{PgArguments, PgRow, Postgres};

use super::{DbResult, PgPool};

/// Key of the transaction-level advisory lock serializing migrations ("rms_mig").
pub const MIGRATION_LOCK_KEY: i64 = 0x0072_6d73_5f6d_6967;

/// `(version, SQL)` in order. Append new versions; never edit an applied one.
pub const MIGRATIONS: &[(i32, &str)] = &[(1, SCHEMA_V1)];

/// Latest PostgreSQL schema version.
pub fn schema_version() -> i32 {
    MIGRATIONS.last().map(|m| m.0).unwrap_or(0)
}

const SCHEMA_V1: &str = r#"
CREATE TABLE broadcasts (
  id TEXT COLLATE "C" PRIMARY KEY, name TEXT COLLATE "C" NOT NULL UNIQUE, title TEXT NOT NULL,
  description TEXT NOT NULL DEFAULT '', stream_key TEXT COLLATE "C" NOT NULL UNIQUE,
  record BOOLEAN NOT NULL DEFAULT false, enable_flv BOOLEAN NOT NULL DEFAULT true,
  enable_hls BOOLEAN NOT NULL DEFAULT true, enable_dash BOOLEAN NOT NULL DEFAULT true,
  paused BOOLEAN NOT NULL DEFAULT false, waiting_video TEXT,
  waiting_text TEXT NOT NULL DEFAULT 'La diretta inizierà a breve',
  created_at TEXT COLLATE "C" NOT NULL, updated_at TEXT COLLATE "C" NOT NULL,
  enable_webrtc BOOLEAN NOT NULL DEFAULT true);
CREATE TABLE stream_sessions (
  id TEXT COLLATE "C" PRIMARY KEY, broadcast_id TEXT COLLATE "C" NOT NULL,
  started_at TEXT COLLATE "C" NOT NULL, ended_at TEXT COLLATE "C", publisher_ip TEXT,
  video_codec TEXT, audio_codec TEXT, width BIGINT, height BIGINT, fps DOUBLE PRECISION,
  bytes_in BIGINT NOT NULL DEFAULT 0, peak_viewers BIGINT NOT NULL DEFAULT 0,
  node_id TEXT COLLATE "C" NOT NULL DEFAULT '');
CREATE INDEX idx_stream_sessions ON stream_sessions(broadcast_id, started_at);
CREATE TABLE viewer_samples (
  broadcast_id TEXT COLLATE "C" NOT NULL, ts TEXT COLLATE "C" NOT NULL, total BIGINT NOT NULL,
  flv BIGINT NOT NULL, ws_flv BIGINT NOT NULL, hls BIGINT NOT NULL, dash BIGINT NOT NULL,
  webrtc BIGINT NOT NULL, node_id TEXT COLLATE "C" NOT NULL DEFAULT '', ts_epoch BIGINT NOT NULL,
  PRIMARY KEY (broadcast_id, node_id, ts));
CREATE INDEX idx_samples_ts ON viewer_samples(broadcast_id, ts);
CREATE TABLE viewer_sessions (
  id TEXT COLLATE "C" PRIMARY KEY, broadcast_id TEXT COLLATE "C" NOT NULL, protocol TEXT NOT NULL,
  ip TEXT, user_agent TEXT, started_at TEXT COLLATE "C" NOT NULL, ended_at TEXT COLLATE "C",
  node_id TEXT COLLATE "C" NOT NULL DEFAULT '');
CREATE INDEX idx_vsessions ON viewer_sessions(broadcast_id, started_at);
CREATE TABLE recordings (
  id TEXT COLLATE "C" PRIMARY KEY, broadcast_id TEXT COLLATE "C" NOT NULL, stream_session_id TEXT,
  path TEXT NOT NULL, format TEXT NOT NULL, status TEXT COLLATE "C" NOT NULL,
  started_at TEXT COLLATE "C" NOT NULL, ended_at TEXT COLLATE "C",
  size_bytes BIGINT NOT NULL DEFAULT 0, duration_secs DOUBLE PRECISION, error TEXT,
  node_id TEXT COLLATE "C", lease_until TIMESTAMPTZ);
CREATE TABLE settings (key TEXT COLLATE "C" PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE vod_items (
  id TEXT COLLATE "C" PRIMARY KEY, title TEXT NOT NULL, description TEXT NOT NULL DEFAULT '',
  source_path TEXT NOT NULL, source_name TEXT NOT NULL DEFAULT '', source_size BIGINT NOT NULL DEFAULT 0,
  source_duration_secs DOUBLE PRECISION, source_width BIGINT, source_height BIGINT, source_codec TEXT,
  origin TEXT NOT NULL DEFAULT 'upload', status TEXT COLLATE "C" NOT NULL,
  progress BIGINT NOT NULL DEFAULT 0, ladder TEXT NOT NULL, renditions_json TEXT NOT NULL DEFAULT '[]',
  error TEXT, created_at TEXT COLLATE "C" NOT NULL, updated_at TEXT COLLATE "C" NOT NULL,
  ready_at TEXT COLLATE "C", plays BIGINT NOT NULL DEFAULT 0,
  claimed_by TEXT COLLATE "C", lease_until TIMESTAMPTZ, attempts INTEGER NOT NULL DEFAULT 0,
  storage_node TEXT);
CREATE INDEX idx_vod_status ON vod_items(status, created_at);
CREATE TABLE nodes (
  node_id TEXT COLLATE "C" PRIMARY KEY, advertise_url TEXT NOT NULL, whep_url TEXT,
  version TEXT NOT NULL, started_at TIMESTAMPTZ NOT NULL, heartbeat TIMESTAMPTZ NOT NULL DEFAULT now(),
  live_streams INTEGER NOT NULL DEFAULT 0, viewers INTEGER NOT NULL DEFAULT 0,
  draining BOOLEAN NOT NULL DEFAULT false);
CREATE TABLE live_streams (
  name TEXT COLLATE "C" PRIMARY KEY,
  node_id TEXT COLLATE "C" NOT NULL REFERENCES nodes(node_id) ON DELETE CASCADE,
  session_id TEXT NOT NULL, started_at TIMESTAMPTZ NOT NULL, heartbeat TIMESTAMPTZ NOT NULL DEFAULT now());
CREATE TABLE singleton_tasks (
  name TEXT COLLATE "C" PRIMARY KEY, leased_by TEXT NOT NULL DEFAULT '',
  leased_until TIMESTAMPTZ NOT NULL DEFAULT '-infinity');
"#;

/// Apply the pending [`MIGRATIONS`] in one transaction under
/// `pg_advisory_xact_lock(MIGRATION_LOCK_KEY)` (released at commit/rollback, so it also
/// works behind PgBouncer in transaction mode).
pub async fn migrate(pool: &PgPool) -> DbResult<()> {
    let mut tx = pool.begin().await?;
    sqlx_core::query::query::<Postgres>("SELECT pg_advisory_xact_lock($1)")
        .bind(MIGRATION_LOCK_KEY)
        .execute(&mut *tx)
        .await?;
    // Checked first (instead of `CREATE TABLE IF NOT EXISTS`) to avoid a NOTICE in the
    // log of every node start.
    let exists: bool = sqlx_core::query::query::<Postgres>(
        "SELECT to_regclass('rms_schema_migrations') IS NOT NULL",
    )
    .fetch_one(&mut *tx)
    .await?
    .try_get(0)?;
    if !exists {
        raw_sql(
            "CREATE TABLE rms_schema_migrations (\
             version INTEGER PRIMARY KEY, applied_at TIMESTAMPTZ NOT NULL DEFAULT now())",
        )
        .execute(&mut *tx)
        .await?;
    }
    let current: i32 = sqlx_core::query::query::<Postgres>(
        "SELECT COALESCE(MAX(version), 0) FROM rms_schema_migrations",
    )
    .fetch_one(&mut *tx)
    .await?
    .try_get(0)?;
    for (version, sql) in MIGRATIONS {
        if *version <= current {
            continue;
        }
        raw_sql(*sql).execute(&mut *tx).await?;
        sqlx_core::query::query::<Postgres>(
            "INSERT INTO rms_schema_migrations (version) VALUES ($1)",
        )
        .bind(*version)
        .execute(&mut *tx)
        .await?;
        tracing::info!(version, "PostgreSQL schema migrated");
    }
    tx.commit().await?;
    Ok(())
}

/// `sqlx::query` for PostgreSQL with a static SQL string.
pub fn q(sql: &'static str) -> Query<'static, Postgres, PgArguments> {
    sqlx_core::query::query::<Postgres>(sql)
}

/// `sqlx::query` for a SQL string built at runtime from constants (column lists, never
/// request data: values are always bound).
pub fn q_dyn(sql: String) -> Query<'static, Postgres, PgArguments> {
    sqlx_core::query::query::<Postgres>(sqlx_core::sql_str::AssertSqlSafe(sql))
}

/// Column `i` as `u32` (stored as BIGINT; clamped).
pub fn get_u32(row: &PgRow, i: usize) -> DbResult<u32> {
    Ok(u32_of(row.try_get::<i64, _>(i)?))
}

/// Nullable column `i` as `u32`.
pub fn get_opt_u32(row: &PgRow, i: usize) -> DbResult<Option<u32>> {
    Ok(row.try_get::<Option<i64>, _>(i)?.map(u32_of))
}

/// Column `i` as `u64` (stored as BIGINT; negative values read as 0).
pub fn get_u64(row: &PgRow, i: usize) -> DbResult<u64> {
    Ok(row.try_get::<i64, _>(i)?.max(0) as u64)
}

pub fn u32_of(v: i64) -> u32 {
    v.clamp(0, u32::MAX as i64) as u32
}

pub fn i64_of(n: u64) -> i64 {
    n.min(i64::MAX as u64) as i64
}
