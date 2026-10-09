//! Test helpers shared by the repository tests: every repository test body takes a
//! [`Db`] and runs on SQLite (in memory) and, when `RMS_TEST_DATABASE_URL` points to a
//! PostgreSQL database, on PostgreSQL too (skipped otherwise). Each PostgreSQL run gets
//! its own schema (`rms_test_<uuid>`, via `search_path`), dropped at the end, so tests
//! can run in parallel against one database.

use sqlx_core::raw_sql::raw_sql;
use sqlx_core::row::Row;
use sqlx_postgres::{PgConnectOptions, PgPoolOptions};

use super::{Db, PgPool};

/// Environment variable with the PostgreSQL URL of the integration tests.
pub const PG_URL_ENV: &str = "RMS_TEST_DATABASE_URL";

/// A migrated PostgreSQL database living in a throwaway schema.
pub struct PgTestDb {
    pub db: Db,
    admin: PgPool,
    schema: String,
}

impl PgTestDb {
    /// Drop the schema (call at the end of the test body).
    pub async fn cleanup(self) {
        self.db.close().await;
        let _ = raw_sql(sqlx_core::sql_str::AssertSqlSafe(format!(
            "DROP SCHEMA IF EXISTS {} CASCADE",
            self.schema
        )))
        .execute(&self.admin)
        .await;
        self.admin.close().await;
    }
}

/// `None` when `RMS_TEST_DATABASE_URL` is not set; panics when it is set but unusable
/// (a configured database that cannot be reached must fail the suite).
pub async fn postgres() -> Option<PgTestDb> {
    let url = std::env::var(PG_URL_ENV)
        .ok()
        .filter(|u| !u.trim().is_empty())?;
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .unwrap_or_else(|e| panic!("{PG_URL_ENV}: cannot connect: {e}"));
    let schema = format!("rms_test_{}", uuid::Uuid::new_v4().simple());
    raw_sql(sqlx_core::sql_str::AssertSqlSafe(format!(
        "CREATE SCHEMA {schema}"
    )))
    .execute(&admin)
    .await
    .unwrap_or_else(|e| panic!("cannot create schema {schema}: {e}"));
    let options: PgConnectOptions = url.parse().expect("valid PostgreSQL URL");
    let db = Db::connect_postgres_with(options.options([("search_path", schema.as_str())]))
        .await
        .unwrap_or_else(|e| panic!("cannot open PostgreSQL test schema: {e}"));
    Some(PgTestDb { db, admin, schema })
}

/// First column of the first row of a dialect-neutral `SELECT` (as `i64`).
pub async fn scalar_i64(db: &Db, sql: &str) -> i64 {
    let owned = sql.to_string();
    match db.pg() {
        Some(pool) => super::pg::q_dyn(owned)
            .fetch_one(pool)
            .await
            .and_then(|r| r.try_get::<i64, _>(0))
            .unwrap_or_else(|e| panic!("{sql}: {e}")),
        None => db
            .call(move |c| Ok(c.query_row(&owned, [], |r| r.get::<_, i64>(0))?))
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}")),
    }
}

/// Execute a dialect-neutral statement (test fixtures).
pub async fn execute(db: &Db, sql: &str) {
    let owned = sql.to_string();
    match db.pg() {
        Some(pool) => {
            super::pg::q_dyn(owned)
                .execute(pool)
                .await
                .unwrap_or_else(|e| panic!("{sql}: {e}"));
        }
        None => {
            db.call(move |c| Ok(c.execute(&owned, [])?))
                .await
                .unwrap_or_else(|e| panic!("{sql}: {e}"));
        }
    }
}

/// `dual_db_test!(name, body)`: module `name` with the tests `sqlite` and `postgres`, both
/// running `async fn body(db: Db)` (an item of the enclosing module).
macro_rules! dual_db_test {
    ($name:ident, $body:ident) => {
        mod $name {
            #[allow(unused_imports)]
            use super::*;

            #[tokio::test]
            async fn sqlite() {
                $body($crate::db::Db::open_in_memory().unwrap()).await;
            }

            #[tokio::test]
            async fn postgres() {
                let Some(pg) = $crate::db::testkit::postgres().await else {
                    eprintln!("skipped: {} not set", $crate::db::testkit::PG_URL_ENV);
                    return;
                };
                $body(pg.db.clone()).await;
                pg.cleanup().await;
            }
        }
    };
}
pub(crate) use dual_db_test;

#[cfg(test)]
mod tests {
    use super::*;

    async fn schema_and_migrations(db: Db) {
        for table in [
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
            "users",
            "api_keys",
            "buckets",
            "bucket_objects",
        ] {
            let sql = format!("SELECT COUNT(*) FROM {table}");
            assert_eq!(scalar_i64(&db, &sql).await, 0, "{table}");
        }
        assert!(db.ping(std::time::Duration::from_secs(2)).await);
        if let Some(pool) = db.pg() {
            // Re-running the migrations is a no-op.
            crate::db::pg::migrate(pool).await.unwrap();
            assert_eq!(
                scalar_i64(
                    &db,
                    "SELECT CAST(MAX(version) AS BIGINT) FROM rms_schema_migrations"
                )
                .await,
                crate::db::pg::schema_version() as i64
            );
            assert_eq!(db.backend(), crate::db::BackendKind::Postgres);
        }
    }
    dual_db_test!(schema, schema_and_migrations);
}
