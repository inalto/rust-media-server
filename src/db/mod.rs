//! SQLite persistence (`data/rms.sqlite`, WAL mode).
//!
//! [`Db`] wraps a single `rusqlite::Connection` behind a mutex; [`Db::call`] runs a
//! closure on tokio's blocking pool so SQLite never blocks the async runtime.
//! Repositories live in the sub-modules and expose `async fn(&Db, ...)` helpers.
//!
//! All timestamps are stored as RFC 3339 UTC strings with millisecond precision
//! (`2026-10-08T14:00:00.000Z`), which also sort lexicographically.

pub mod broadcasts;
pub mod metrics;
pub mod recordings;
pub mod settings;
pub mod stream_sessions;

use std::path::Path;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::Connection;

pub use broadcasts::Broadcast;
pub use recordings::Recording;

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
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
            _ => false,
        }
    }
}

pub type DbResult<T> = Result<T, DbError>;

/// Async handle to the database. Cheap to clone.
#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

impl Db {
    /// Open (creating if needed) the database file and run migrations.
    /// Synchronous: call at startup before serving requests.
    pub fn open(path: &Path) -> DbResult<Db> {
        let conn = Connection::open(path)?;
        Self::init(conn, true)
    }

    /// In-memory database with the full schema (tests).
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
        Ok(Db {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Run `f` with exclusive access to the connection on a blocking thread.
    pub async fn call<F, T>(&self, f: F) -> DbResult<T>
    where
        F: FnOnce(&mut Connection) -> DbResult<T> + Send + 'static,
        T: Send + 'static,
    {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let mut guard = conn.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            f(&mut guard)
        })
        .await
        .map_err(|e| DbError::Task(e.to_string()))?
    }
}

/// Current schema version (stored in `PRAGMA user_version`).
pub const SCHEMA_VERSION: i64 = 2;

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

/// Apply pending migrations. Later schema changes append a new `if version < N` block
/// and bump [`SCHEMA_VERSION`].
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
        ] {
            assert!(tables.iter().any(|x| x == t), "missing table {t}");
        }
        // Idempotent.
        db.call(migrate).await.unwrap();
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
