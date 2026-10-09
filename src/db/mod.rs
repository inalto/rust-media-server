//! Persistence: SQLite (single node, `data/rms.sqlite`, WAL mode) or PostgreSQL
//! (cluster mode, `cluster.database_url`).
//!
//! [`Db`] is a cheap-to-clone handle on one of the two backends:
//! - **SQLite** (default): a single `rusqlite::Connection` behind a mutex; [`Db::call`]
//!   runs a closure on tokio's blocking pool so SQLite never blocks the async runtime.
//! - **PostgreSQL** ([`Db::connect_postgres`]): an async `sqlx` pool (dynamic queries
//!   only, no compile-time macros); schema and migrations in [`pg`].
//!
//! Repositories live in the sub-modules and expose `async fn(&Db, ...)` helpers that
//! dispatch on the backend: the SQLite statement and the PostgreSQL statement of each
//! operation sit side by side (`?N` placeholders vs `$N`, `INSERT OR REPLACE` vs
//! `ON CONFLICT`, `strftime`/`julianday` vs epoch columns and `EXTRACT`, `0/1` vs
//! `BOOLEAN`).
//!
//! The handle also carries the **node tag** written into the `node_id` columns
//! ([`Db::node_id`]: empty on a single node, `cluster.node_id` in cluster mode) and the
//! in-memory broadcasts cache ([`Db::broadcast_cache`]).
//!
//! All timestamps are stored as RFC 3339 UTC strings with millisecond precision
//! (`2026-10-08T14:00:00.000Z`), which also sort lexicographically (PostgreSQL text
//! columns use `COLLATE "C"`). Exceptions (cluster bookkeeping, DB clock): `nodes`,
//! `live_streams`, `singleton_tasks` and the `lease_until` columns are `TIMESTAMPTZ` on
//! PostgreSQL (RFC 3339 text on SQLite); `viewer_samples.ts_epoch` holds the Unix second
//! of `ts` on both.

pub mod broadcasts;
pub mod cache;
pub mod cluster;
pub mod metrics;
pub mod nodes;
pub mod pg;
pub mod recordings;
pub mod settings;
pub mod stream_sessions;
#[cfg(test)]
pub(crate) mod testkit;
pub mod vod;

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::Connection;

pub use broadcasts::Broadcast;
pub use cache::BroadcastCache;
pub use recordings::Recording;

/// The PostgreSQL connection pool type.
pub type PgPool = sqlx_postgres::PgPool;

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Postgres(#[from] sqlx_core::Error),
    #[error("database task failed: {0}")]
    Task(String),
    #[error("{0}")]
    Other(String),
}

impl DbError {
    /// True for UNIQUE / PRIMARY KEY constraint violations (mapped to HTTP 409).
    pub fn is_unique_violation(&self) -> bool {
        match self {
            DbError::Sqlite(rusqlite::Error::SqliteFailure(e, _)) => matches!(
                e.extended_code,
                rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE
                    | rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY
            ),
            DbError::Postgres(sqlx_core::Error::Database(e)) => e.is_unique_violation(),
            _ => false,
        }
    }
}

pub type DbResult<T> = Result<T, DbError>;

/// Which database a [`Db`] talks to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    Sqlite,
    Postgres,
}

impl BackendKind {
    /// `"sqlite"` | `"postgres"` (`GET /system` → `cluster.backend`).
    pub fn as_str(self) -> &'static str {
        match self {
            BackendKind::Sqlite => "sqlite",
            BackendKind::Postgres => "postgres",
        }
    }
}

#[derive(Clone)]
enum Backend {
    Sqlite(Arc<Mutex<Connection>>),
    Postgres(PgPool),
}

/// Cache lifetime of a broadcast row on PostgreSQL: other nodes write the same table and
/// (until the NOTIFY listener of phase 7C-2 invalidates entries) only the TTL bounds how
/// long a change made elsewhere stays invisible here. SQLite has a single writer (this
/// process), so its entries never expire.
pub const PG_BROADCAST_CACHE_TTL: Duration = Duration::from_secs(5);

/// Async handle to the database. Cheap to clone.
#[derive(Clone)]
pub struct Db {
    backend: Backend,
    /// Value of the `node_id` columns written by this process.
    node_id: Arc<str>,
    broadcasts: Arc<BroadcastCache>,
}

impl Db {
    fn new(backend: Backend, cache_ttl: Option<Duration>) -> Db {
        Db {
            backend,
            node_id: Arc::from(""),
            broadcasts: Arc::new(BroadcastCache::new(cache_ttl)),
        }
    }

    /// Open (creating if needed) the SQLite database file and run migrations.
    /// Synchronous: call at startup before serving requests.
    pub fn open(path: &Path) -> DbResult<Db> {
        let conn = Connection::open(path)?;
        Self::init(conn, true)
    }

    /// In-memory SQLite database with the full schema (tests).
    pub fn open_in_memory() -> DbResult<Db> {
        let conn = Connection::open_in_memory()?;
        Self::init(conn, false)
    }

    fn init(mut conn: Connection, wal: bool) -> DbResult<Db> {
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        if wal {
            // journal_mode returns a row, so use query_row instead of execute.
            let _mode: String = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
        }
        conn.execute_batch("PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=OFF;")?;
        migrate(&mut conn)?;
        Ok(Db::new(Backend::Sqlite(Arc::new(Mutex::new(conn))), None))
    }

    /// Connect to PostgreSQL (`postgres://user:pass@host:port/db`) and apply the
    /// migrations (serialized across nodes by an advisory lock, see [`pg::migrate`]).
    pub async fn connect_postgres(url: &str) -> DbResult<Db> {
        let options: sqlx_postgres::PgConnectOptions = url
            .parse()
            .map_err(|e: sqlx_core::Error| DbError::Other(format!("invalid database URL: {e}")))?;
        Self::connect_postgres_with(options).await
    }

    /// [`Self::connect_postgres`] with explicit connection options (e.g. a
    /// `search_path` for the integration tests).
    pub async fn connect_postgres_with(options: sqlx_postgres::PgConnectOptions) -> DbResult<Db> {
        let options = options.application_name("rms");
        let pool = sqlx_postgres::PgPoolOptions::new()
            .max_connections(16)
            .min_connections(1)
            .acquire_timeout(Duration::from_secs(10))
            .connect_with(options)
            .await?;
        pg::migrate(&pool).await?;
        Ok(Db::new(
            Backend::Postgres(pool),
            Some(PG_BROADCAST_CACHE_TTL),
        ))
    }

    /// Tag written into the `node_id` columns (`viewer_samples`, `viewer_sessions`,
    /// `stream_sessions`, `recordings`, `vod_items.claimed_by`). Empty by default (single
    /// node); `main.rs` sets `cluster.node_id` in cluster mode.
    pub fn with_node_id(mut self, node_id: &str) -> Db {
        self.node_id = Arc::from(node_id);
        self
    }

    /// See [`Self::with_node_id`].
    pub fn node_id(&self) -> &str {
        &self.node_id
    }

    pub fn backend(&self) -> BackendKind {
        match self.backend {
            Backend::Sqlite(_) => BackendKind::Sqlite,
            Backend::Postgres(_) => BackendKind::Postgres,
        }
    }

    /// The PostgreSQL pool (`None` on SQLite).
    pub fn pg(&self) -> Option<&PgPool> {
        match &self.backend {
            Backend::Postgres(pool) => Some(pool),
            Backend::Sqlite(_) => None,
        }
    }

    /// In-memory cache in front of `broadcasts::find_by_name` / `find_by_key`
    /// (invalidated by every write of `db::broadcasts` and `db::settings`; the cluster
    /// NOTIFY listener invalidates it for writes made by other nodes).
    pub fn broadcast_cache(&self) -> &BroadcastCache {
        &self.broadcasts
    }

    /// Cheap liveness probe (`SELECT 1`), bounded by `timeout`.
    pub async fn ping(&self, timeout: Duration) -> bool {
        let probe = async {
            match &self.backend {
                Backend::Postgres(pool) => {
                    sqlx_core::query::query::<sqlx_postgres::Postgres>("SELECT 1")
                        .execute(pool)
                        .await
                        .is_ok()
                }
                Backend::Sqlite(_) => self
                    .call(|c| Ok(c.query_row("SELECT 1", [], |r| r.get::<_, i64>(0))?))
                    .await
                    .is_ok(),
            }
        };
        tokio::time::timeout(timeout, probe).await.unwrap_or(false)
    }

    /// Close the PostgreSQL pool (graceful shutdown); no-op on SQLite.
    pub async fn close(&self) {
        if let Backend::Postgres(pool) = &self.backend {
            pool.close().await;
        }
    }

    /// Run `f` with exclusive access to the SQLite connection on a blocking thread.
    /// SQLite only: on PostgreSQL it returns an error (repositories dispatch with
    /// [`Self::pg`] first).
    pub async fn call<F, T>(&self, f: F) -> DbResult<T>
    where
        F: FnOnce(&mut Connection) -> DbResult<T> + Send + 'static,
        T: Send + 'static,
    {
        let conn = match &self.backend {
            Backend::Sqlite(conn) => Arc::clone(conn),
            Backend::Postgres(_) => {
                return Err(DbError::Other(
                    "Db::call is only available on the SQLite backend".into(),
                ));
            }
        };
        tokio::task::spawn_blocking(move || {
            let mut guard = conn.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            f(&mut guard)
        })
        .await
        .map_err(|e| DbError::Task(e.to_string()))?
    }
}

/// Current schema version (stored in `PRAGMA user_version`).
pub const SCHEMA_VERSION: i64 = 4;

/// Schema v1 = ARCHITECTURE.md §7, verbatim.
const SCHEMA_V1: &str = r#"
CREATE TABLE IF NOT EXISTS broadcasts (
  id TEXT PRIMARY KEY, name TEXT UNIQUE NOT NULL, title TEXT NOT NULL, description TEXT NOT NULL DEFAULT '',
  stream_key TEXT UNIQUE NOT NULL, record INTEGER NOT NULL DEFAULT 0,
  enable_flv INTEGER NOT NULL DEFAULT 1, enable_hls INTEGER NOT NULL DEFAULT 1, enable_dash INTEGER NOT NULL DEFAULT 1,
  paused INTEGER NOT NULL DEFAULT 0, waiting_video TEXT, waiting_text TEXT NOT NULL DEFAULT 'La diretta inizierà a breve',
  created_at TEXT NOT NULL, updated_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS stream_sessions (
  id TEXT PRIMARY KEY, broadcast_id TEXT NOT NULL, started_at TEXT NOT NULL, ended_at TEXT,
  publisher_ip TEXT, video_codec TEXT, audio_codec TEXT, width INTEGER, height INTEGER, fps REAL,
  bytes_in INTEGER NOT NULL DEFAULT 0, peak_viewers INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS viewer_samples (
  broadcast_id TEXT NOT NULL, ts TEXT NOT NULL, total INTEGER NOT NULL, flv INTEGER NOT NULL, ws_flv INTEGER NOT NULL,
  hls INTEGER NOT NULL, dash INTEGER NOT NULL, webrtc INTEGER NOT NULL, PRIMARY KEY (broadcast_id, ts));
CREATE TABLE IF NOT EXISTS viewer_sessions (
  id TEXT PRIMARY KEY, broadcast_id TEXT NOT NULL, protocol TEXT NOT NULL, ip TEXT, user_agent TEXT,
  started_at TEXT NOT NULL, ended_at TEXT);
CREATE TABLE IF NOT EXISTS recordings (
  id TEXT PRIMARY KEY, broadcast_id TEXT NOT NULL, stream_session_id TEXT, path TEXT NOT NULL,
  format TEXT NOT NULL, status TEXT NOT NULL,
  started_at TEXT NOT NULL, ended_at TEXT, size_bytes INTEGER NOT NULL DEFAULT 0, duration_secs REAL, error TEXT);
CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS idx_samples_ts ON viewer_samples(broadcast_id, ts);
CREATE INDEX IF NOT EXISTS idx_vsessions ON viewer_sessions(broadcast_id, started_at);
"#;

/// Schema v2: per-broadcast WebRTC (WHEP) flag.
const SCHEMA_V2: &str =
    "ALTER TABLE broadcasts ADD COLUMN enable_webrtc INTEGER NOT NULL DEFAULT 1;";

/// Schema v3: video on demand items (phase 7B, see `db::vod`).
const SCHEMA_V3: &str = r#"
CREATE TABLE IF NOT EXISTS vod_items (
  id TEXT PRIMARY KEY, title TEXT NOT NULL, description TEXT NOT NULL DEFAULT '',
  source_path TEXT NOT NULL, source_name TEXT NOT NULL DEFAULT '', source_size INTEGER NOT NULL DEFAULT 0,
  source_duration_secs REAL, source_width INTEGER, source_height INTEGER, source_codec TEXT,
  origin TEXT NOT NULL DEFAULT 'upload',
  status TEXT NOT NULL, progress INTEGER NOT NULL DEFAULT 0, ladder TEXT NOT NULL,
  renditions_json TEXT NOT NULL DEFAULT '[]', error TEXT,
  created_at TEXT NOT NULL, updated_at TEXT NOT NULL, ready_at TEXT, plays INTEGER NOT NULL DEFAULT 0);
CREATE INDEX IF NOT EXISTS idx_vod_status ON vod_items(status, created_at);
"#;

/// Schema v4 (phase 7C-1, docs/CLUSTER.md §3.2): node tags, epoch seconds of the
/// samples, VOD/recording leases and the cluster tables. SQLite keeps the
/// `(broadcast_id, ts)` primary key of `viewer_samples` (a single node writes it);
/// PostgreSQL uses `(broadcast_id, node_id, ts)`.
const SCHEMA_V4: &str = r#"
ALTER TABLE viewer_samples ADD COLUMN node_id TEXT NOT NULL DEFAULT '';
ALTER TABLE viewer_samples ADD COLUMN ts_epoch INTEGER;
UPDATE viewer_samples SET ts_epoch = CAST(strftime('%s', ts) AS INTEGER) WHERE ts_epoch IS NULL;
ALTER TABLE viewer_sessions ADD COLUMN node_id TEXT NOT NULL DEFAULT '';
ALTER TABLE stream_sessions ADD COLUMN node_id TEXT NOT NULL DEFAULT '';
ALTER TABLE recordings ADD COLUMN node_id TEXT;
ALTER TABLE recordings ADD COLUMN lease_until TEXT;
ALTER TABLE vod_items ADD COLUMN claimed_by TEXT;
ALTER TABLE vod_items ADD COLUMN lease_until TEXT;
ALTER TABLE vod_items ADD COLUMN attempts INTEGER NOT NULL DEFAULT 0;
ALTER TABLE vod_items ADD COLUMN storage_node TEXT;
CREATE TABLE IF NOT EXISTS nodes (
  node_id TEXT PRIMARY KEY, advertise_url TEXT NOT NULL, whep_url TEXT, version TEXT NOT NULL,
  started_at TEXT NOT NULL, heartbeat TEXT NOT NULL, live_streams INTEGER NOT NULL DEFAULT 0,
  viewers INTEGER NOT NULL DEFAULT 0, draining INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS live_streams (
  name TEXT PRIMARY KEY, node_id TEXT NOT NULL REFERENCES nodes(node_id) ON DELETE CASCADE,
  session_id TEXT NOT NULL, started_at TEXT NOT NULL, heartbeat TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS singleton_tasks (
  name TEXT PRIMARY KEY, leased_by TEXT NOT NULL DEFAULT '', leased_until TEXT NOT NULL DEFAULT '');
"#;

/// Apply pending migrations. Later schema changes append a new `if version < N` block
/// and bump [`SCHEMA_VERSION`] (and add the same change to [`pg::MIGRATIONS`]).
pub fn migrate(conn: &mut Connection) -> DbResult<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version < 1 {
        let tx = conn.transaction()?;
        tx.execute_batch(SCHEMA_V1)?;
        tx.execute_batch("PRAGMA user_version = 1")?;
        tx.commit()?;
        tracing::info!("database schema migrated to version 1");
    }
    if version < 2 {
        let tx = conn.transaction()?;
        tx.execute_batch(SCHEMA_V2)?;
        tx.execute_batch("PRAGMA user_version = 2")?;
        tx.commit()?;
        tracing::info!("database schema migrated to version 2");
    }
    if version < 3 {
        let tx = conn.transaction()?;
        tx.execute_batch(SCHEMA_V3)?;
        tx.execute_batch("PRAGMA user_version = 3")?;
        tx.commit()?;
        tracing::info!("database schema migrated to version 3");
    }
    if version < 4 {
        let tx = conn.transaction()?;
        tx.execute_batch(SCHEMA_V4)?;
        tx.execute_batch("PRAGMA user_version = 4")?;
        tx.commit()?;
        tracing::info!("database schema migrated to version 4");
    }
    Ok(())
}

/// Format a timestamp the way it is stored in the database.
pub fn fmt_ts(ts: &DateTime<Utc>) -> String {
    ts.to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// `fmt_ts(Utc::now())`.
pub fn now_ts() -> String {
    fmt_ts(&Utc::now())
}

/// Parse a stored timestamp; falls back to the Unix epoch on malformed data (never panics).
pub fn parse_ts(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn migrations_create_full_schema() {
        let db = Db::open_in_memory().unwrap();
        let tables: Vec<String> = db
            .call(|c| {
                let mut st =
                    c.prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")?;
                let rows = st
                    .query_map([], |r| r.get(0))?
                    .collect::<Result<Vec<String>, _>>()?;
                Ok(rows)
            })
            .await
            .unwrap();
        for t in [
            "broadcasts",
            "recordings",
            "settings",
            "stream_sessions",
            "viewer_samples",
            "viewer_sessions",
            "vod_items",
            "nodes",
            "live_streams",
            "singleton_tasks",
        ] {
            assert!(tables.iter().any(|x| x == t), "missing table {t}");
        }
        // Idempotent.
        db.call(migrate).await.unwrap();
        assert_eq!(db.backend(), BackendKind::Sqlite);
        assert_eq!(db.node_id(), "");
        assert!(db.ping(Duration::from_secs(1)).await);
    }

    #[test]
    fn migration_v3_to_v4_backfills_epoch_and_node_columns() {
        let mut conn = Connection::open_in_memory().unwrap();
        for sql in [SCHEMA_V1, SCHEMA_V2, SCHEMA_V3] {
            conn.execute_batch(sql).unwrap();
        }
        conn.execute_batch("PRAGMA user_version = 3").unwrap();
        conn.execute(
            "INSERT INTO viewer_samples (broadcast_id, ts, total, flv, ws_flv, hls, dash, webrtc) \
             VALUES ('b', '2026-10-08T10:00:02.500Z', 3, 3, 0, 0, 0, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO vod_items (id, title, source_path, status, ladder, created_at, updated_at) \
             VALUES ('v', 'V', 'v/source.mp4', 'processing', 'standard', 'x', 'x')",
            [],
        )
        .unwrap();
        migrate(&mut conn).unwrap();
        let (epoch, node): (i64, String) = conn
            .query_row(
                "SELECT ts_epoch, node_id FROM viewer_samples WHERE broadcast_id = 'b'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(epoch, parse_ts("2026-10-08T10:00:02.500Z").timestamp());
        assert_eq!(node, "");
        let (claimed, attempts): (Option<String>, i64) = conn
            .query_row(
                "SELECT claimed_by, attempts FROM vod_items WHERE id = 'v'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((claimed, attempts), (None, 0));
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 4);
    }

    #[test]
    fn migration_v1_to_v2_adds_enable_webrtc() {
        // A database created by the phase 1-3 binary (schema v1, one broadcast).
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA_V1).unwrap();
        conn.execute_batch("PRAGMA user_version = 1").unwrap();
        conn.execute(
            "INSERT INTO broadcasts (id, name, title, stream_key, created_at, updated_at) \
             VALUES ('id1', 'old', 'Old', 'k1', '2026-01-01T00:00:00.000Z', '2026-01-01T00:00:00.000Z')",
            [],
        )
        .unwrap();
        migrate(&mut conn).unwrap();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        let flag: bool = conn
            .query_row(
                "SELECT enable_webrtc FROM broadcasts WHERE name = 'old'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(flag, "existing broadcasts default to WebRTC enabled");
        // Re-running is a no-op (the ALTER must not run twice).
        migrate(&mut conn).unwrap();
        let b = broadcasts::find_by_name_sync(&conn, "old")
            .unwrap()
            .unwrap();
        assert!(b.enable_webrtc);
    }

    #[test]
    fn ts_roundtrip() {
        let now = Utc::now();
        let s = fmt_ts(&now);
        assert!(s.ends_with('Z'));
        assert_eq!(parse_ts(&s).timestamp_millis(), now.timestamp_millis());
        assert_eq!(parse_ts("garbage").timestamp(), 0);
    }
}
