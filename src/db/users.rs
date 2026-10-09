//! `users` repository (phase 8A, docs/ACCOUNTS.md §1).
//!
//! A user owns buckets (files + encodings) and API keys. The admin token stays the
//! superuser; users never see each other's data. Deleting a user removes its keys,
//! buckets, bucket objects and encoding rows in one transaction (the API layer removes
//! the files and cancels the jobs first). `quota_bytes = 0` means unlimited.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use sqlx_core::row::Row as _;
use sqlx_postgres::PgRow;

use super::pg::{get_u32, get_u64, i64_of, q, q_dyn};
use super::{Db, DbResult, fmt_ts, now_ts, parse_ts};

/// `users.status` values.
pub const STATUS_ACTIVE: &str = "active";
pub const STATUS_SUSPENDED: &str = "suspended";
pub const STATUSES: [&str; 2] = [STATUS_ACTIVE, STATUS_SUSPENDED];

/// Default `max_buckets` of a new user.
pub const DEFAULT_MAX_BUCKETS: u32 = 10;

/// One row of `users`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct User {
    pub id: String,
    /// Slug `[a-z0-9_-]{2,32}`, unique.
    pub username: String,
    pub display_name: String,
    pub email: Option<String>,
    /// `active` | `suspended` (every key of a suspended user answers 403).
    pub status: String,
    /// Originals + encodings, bytes; 0 = unlimited.
    pub quota_bytes: u64,
    pub max_buckets: u32,
    /// Ladder of the encodings when neither the upload nor the bucket names one.
    pub default_ladder: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub last_login_at: Option<DateTime<Utc>>,
}

impl User {
    /// A new active user (`display_name` defaults to the username when empty).
    pub fn new(username: &str, display_name: &str) -> Self {
        let now = parse_ts(&now_ts());
        let display = display_name.trim();
        Self {
            id: uuid::Uuid::new_v4().hyphenated().to_string(),
            username: username.to_string(),
            display_name: if display.is_empty() {
                username.to_string()
            } else {
                display.to_string()
            },
            email: None,
            status: STATUS_ACTIVE.into(),
            quota_bytes: 0,
            max_buckets: DEFAULT_MAX_BUCKETS,
            default_ladder: None,
            created_at: now,
            updated_at: now,
            last_login_at: None,
        }
    }

    pub fn is_active(&self) -> bool {
        self.status == STATUS_ACTIVE
    }
}

/// Storage usage of a user (computed from the bucket counters and the encodings).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Usage {
    /// Originals + encodings (sum of the buckets' `bytes_used`): what the quota limits.
    pub bytes_used: u64,
    /// Size of the files stored in the buckets.
    pub originals_bytes: u64,
    /// Size of the encoding outputs (renditions + posters).
    pub encodings_bytes: u64,
    pub buckets: u64,
    /// Files in the buckets (directories excluded).
    pub objects: u64,
    pub encodings: u64,
    /// Keys not revoked (expired ones included).
    pub keys: u64,
}

const COLUMNS: &str = "id, username, display_name, email, status, quota_bytes, max_buckets, \
     default_ladder, created_at, updated_at, last_login_at";

fn from_row(r: &Row<'_>) -> rusqlite::Result<User> {
    Ok(User {
        id: r.get(0)?,
        username: r.get(1)?,
        display_name: r.get(2)?,
        email: r.get(3)?,
        status: r.get(4)?,
        quota_bytes: r.get::<_, i64>(5)?.max(0) as u64,
        max_buckets: r.get::<_, i64>(6)?.clamp(0, u32::MAX as i64) as u32,
        default_ladder: r.get(7)?,
        created_at: parse_ts(&r.get::<_, String>(8)?),
        updated_at: parse_ts(&r.get::<_, String>(9)?),
        last_login_at: r.get::<_, Option<String>>(10)?.map(|s| parse_ts(&s)),
    })
}

fn pg_row(r: &PgRow) -> DbResult<User> {
    Ok(User {
        id: r.try_get(0)?,
        username: r.try_get(1)?,
        display_name: r.try_get(2)?,
        email: r.try_get(3)?,
        status: r.try_get(4)?,
        quota_bytes: get_u64(r, 5)?,
        max_buckets: get_u32(r, 6)?,
        default_ladder: r.try_get(7)?,
        created_at: parse_ts(&r.try_get::<String, _>(8)?),
        updated_at: parse_ts(&r.try_get::<String, _>(9)?),
        last_login_at: r.try_get::<Option<String>, _>(10)?.map(|s| parse_ts(&s)),
    })
}

/// Insert a new row (unique violation when the username or id exists).
pub async fn insert(db: &Db, user: &User) -> DbResult<()> {
    let u = user.clone();
    if let Some(pool) = db.pg() {
        q_dyn(format!(
            "INSERT INTO users ({COLUMNS}) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)"
        ))
        .bind(u.id)
        .bind(u.username)
        .bind(u.display_name)
        .bind(u.email)
        .bind(u.status)
        .bind(i64_of(u.quota_bytes))
        .bind(i64::from(u.max_buckets))
        .bind(u.default_ladder)
        .bind(fmt_ts(&u.created_at))
        .bind(fmt_ts(&u.updated_at))
        .bind(u.last_login_at.as_ref().map(fmt_ts))
        .execute(pool)
        .await?;
        return Ok(());
    }
    db.call(move |c| {
        c.execute(
            &format!(
                "INSERT INTO users ({COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)"
            ),
            params![
                u.id,
                u.username,
                u.display_name,
                u.email,
                u.status,
                i64_of(u.quota_bytes),
                u.max_buckets,
                u.default_ladder,
                fmt_ts(&u.created_at),
                fmt_ts(&u.updated_at),
                u.last_login_at.as_ref().map(fmt_ts),
            ],
        )?;
        Ok(())
    })
    .await
}

async fn get_by(db: &Db, column: &'static str, value: &str) -> DbResult<Option<User>> {
    let value = value.to_string();
    if let Some(pool) = db.pg() {
        return q_dyn(format!("SELECT {COLUMNS} FROM users WHERE {column} = $1"))
            .bind(value)
            .fetch_optional(pool)
            .await?
            .as_ref()
            .map(pg_row)
            .transpose();
    }
    db.call(move |c| {
        Ok(c.query_row(
            &format!("SELECT {COLUMNS} FROM users WHERE {column} = ?1"),
            [value],
            from_row,
        )
        .optional()?)
    })
    .await
}

/// One user by id.
pub async fn get(db: &Db, id: &str) -> DbResult<Option<User>> {
    get_by(db, "id", id).await
}

/// One user by username.
pub async fn find_by_username(db: &Db, username: &str) -> DbResult<Option<User>> {
    get_by(db, "username", username).await
}

/// Every user, by username.
pub async fn list(db: &Db) -> DbResult<Vec<User>> {
    if let Some(pool) = db.pg() {
        return q_dyn(format!("SELECT {COLUMNS} FROM users ORDER BY username"))
            .fetch_all(pool)
            .await?
            .iter()
            .map(pg_row)
            .collect();
    }
    db.call(|c| {
        let mut st = c.prepare(&format!("SELECT {COLUMNS} FROM users ORDER BY username"))?;
        let rows = st.query_map([], from_row)?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })
    .await
}

/// Write the mutable fields of `user` (username, display name, email, status, quota,
/// max buckets, default ladder); `updated_at` = now. Returns the stored row (`None` when
/// it does not exist).
pub async fn update(db: &Db, user: &User) -> DbResult<Option<User>> {
    let u = user.clone();
    let now = now_ts();
    if let Some(pool) = db.pg() {
        return q_dyn(format!(
            "UPDATE users SET username = $2, display_name = $3, email = $4, status = $5, \
             quota_bytes = $6, max_buckets = $7, default_ladder = $8, updated_at = $9 \
             WHERE id = $1 RETURNING {COLUMNS}"
        ))
        .bind(u.id)
        .bind(u.username)
        .bind(u.display_name)
        .bind(u.email)
        .bind(u.status)
        .bind(i64_of(u.quota_bytes))
        .bind(i64::from(u.max_buckets))
        .bind(u.default_ladder)
        .bind(now)
        .fetch_optional(pool)
        .await?
        .as_ref()
        .map(pg_row)
        .transpose();
    }
    db.call(move |c| {
        Ok(c.query_row(
            &format!(
                "UPDATE users SET username = ?2, display_name = ?3, email = ?4, status = ?5, \
                 quota_bytes = ?6, max_buckets = ?7, default_ladder = ?8, updated_at = ?9 \
                 WHERE id = ?1 RETURNING {COLUMNS}"
            ),
            params![
                u.id,
                u.username,
                u.display_name,
                u.email,
                u.status,
                i64_of(u.quota_bytes),
                u.max_buckets,
                u.default_ladder,
                now
            ],
            from_row,
        )
        .optional()?)
    })
    .await
}

/// `last_login_at = now` (dashboard login with one of the user's keys).
pub async fn touch_login(db: &Db, id: &str) -> DbResult<bool> {
    let id = id.to_string();
    if let Some(pool) = db.pg() {
        let n = q("UPDATE users SET last_login_at = $2 WHERE id = $1")
            .bind(id)
            .bind(now_ts())
            .execute(pool)
            .await?
            .rows_affected();
        return Ok(n == 1);
    }
    db.call(move |c| {
        Ok(c.execute(
            "UPDATE users SET last_login_at = ?2 WHERE id = ?1",
            params![id, now_ts()],
        )? == 1)
    })
    .await
}

/// Statements of a cascading user delete, in order (`?1`/`$1` = user id).
const DELETE_SQLITE: [&str; 5] = [
    "DELETE FROM bucket_objects WHERE bucket_id IN (SELECT id FROM buckets WHERE user_id = ?1)",
    "DELETE FROM buckets WHERE user_id = ?1",
    "DELETE FROM api_keys WHERE user_id = ?1",
    "DELETE FROM vod_items WHERE user_id = ?1",
    "DELETE FROM users WHERE id = ?1",
];
const DELETE_PG: [&str; 5] = [
    "DELETE FROM bucket_objects WHERE bucket_id IN (SELECT id FROM buckets WHERE user_id = $1)",
    "DELETE FROM buckets WHERE user_id = $1",
    "DELETE FROM api_keys WHERE user_id = $1",
    "DELETE FROM vod_items WHERE user_id = $1",
    "DELETE FROM users WHERE id = $1",
];

/// Delete a user with its keys, buckets, bucket objects and encoding rows (one
/// transaction). False when the user does not exist. Files are not touched.
pub async fn delete(db: &Db, id: &str) -> DbResult<bool> {
    let id = id.to_string();
    if let Some(pool) = db.pg() {
        let mut tx = pool.begin().await?;
        let mut last = 0;
        for sql in DELETE_PG {
            last = q(sql)
                .bind(id.clone())
                .execute(&mut *tx)
                .await?
                .rows_affected();
        }
        tx.commit().await?;
        return Ok(last == 1);
    }
    db.call(move |c| {
        let tx = c.transaction()?;
        let mut last = 0;
        for sql in DELETE_SQLITE {
            last = tx.execute(sql, [&id])?;
        }
        tx.commit()?;
        Ok(last == 1)
    })
    .await
}

/// Usage columns (`?1`/`$1` = user id); PostgreSQL casts its NUMERIC sums.
const USAGE_SQLITE: &str = "SELECT \
     (SELECT COALESCE(SUM(bytes_used), 0) FROM buckets WHERE user_id = ?1), \
     (SELECT COUNT(*) FROM buckets WHERE user_id = ?1), \
     (SELECT COALESCE(SUM(objects_count), 0) FROM buckets WHERE user_id = ?1), \
     (SELECT COUNT(*) FROM vod_items WHERE user_id = ?1), \
     (SELECT COALESCE(SUM(output_bytes), 0) FROM vod_items WHERE user_id = ?1), \
     (SELECT COUNT(*) FROM api_keys WHERE user_id = ?1 AND revoked_at IS NULL)";
const USAGE_PG: &str = "SELECT \
     CAST((SELECT COALESCE(SUM(bytes_used), 0) FROM buckets WHERE user_id = $1) AS BIGINT), \
     (SELECT COUNT(*) FROM buckets WHERE user_id = $1), \
     CAST((SELECT COALESCE(SUM(objects_count), 0) FROM buckets WHERE user_id = $1) AS BIGINT), \
     (SELECT COUNT(*) FROM vod_items WHERE user_id = $1), \
     CAST((SELECT COALESCE(SUM(output_bytes), 0) FROM vod_items WHERE user_id = $1) AS BIGINT), \
     (SELECT COUNT(*) FROM api_keys WHERE user_id = $1 AND revoked_at IS NULL)";

fn usage_of(v: [i64; 6]) -> Usage {
    let n = |x: i64| x.max(0) as u64;
    let bytes_used = n(v[0]);
    let encodings_bytes = n(v[4]);
    Usage {
        bytes_used,
        originals_bytes: bytes_used.saturating_sub(encodings_bytes),
        encodings_bytes,
        buckets: n(v[1]),
        objects: n(v[2]),
        encodings: n(v[3]),
        keys: n(v[5]),
    }
}

/// Usage of one user (zeros for an unknown id).
pub async fn usage(db: &Db, id: &str) -> DbResult<Usage> {
    let id = id.to_string();
    if let Some(pool) = db.pg() {
        let r = q(USAGE_PG).bind(id).fetch_one(pool).await?;
        let mut v = [0i64; 6];
        for (i, x) in v.iter_mut().enumerate() {
            *x = r.try_get::<i64, _>(i)?;
        }
        return Ok(usage_of(v));
    }
    db.call(move |c| {
        Ok(c.query_row(USAGE_SQLITE, [id], |r| {
            let mut v = [0i64; 6];
            for (i, x) in v.iter_mut().enumerate() {
                *x = r.get(i)?;
            }
            Ok(usage_of(v))
        })?)
    })
    .await
}

/// Usage of every user (admin list), keyed by user id.
pub async fn usage_all(db: &Db) -> DbResult<HashMap<String, Usage>> {
    let mut out = HashMap::new();
    for u in list(db).await? {
        let usage = usage(db, &u.id).await?;
        out.insert(u.id, usage);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::testkit::{dual_db_test, execute};

    async fn crud_body(db: Db) {
        let mut a = User::new("alice", "Alice A.");
        a.email = Some("alice@example.com".into());
        a.quota_bytes = 1 << 30;
        let b = User::new("bob", "  ");
        assert_eq!(b.display_name, "bob");
        insert(&db, &a).await.unwrap();
        insert(&db, &b).await.unwrap();
        let dup = User::new("alice", "x");
        assert!(insert(&db, &dup).await.unwrap_err().is_unique_violation());

        assert_eq!(get(&db, &a.id).await.unwrap().unwrap(), a);
        assert_eq!(
            find_by_username(&db, "bob").await.unwrap().unwrap().id,
            b.id
        );
        assert!(get(&db, "nope").await.unwrap().is_none());
        let names: Vec<String> = list(&db)
            .await
            .unwrap()
            .into_iter()
            .map(|u| u.username)
            .collect();
        assert_eq!(names, ["alice", "bob"]);

        let mut changed = a.clone();
        changed.status = STATUS_SUSPENDED.into();
        changed.max_buckets = 3;
        changed.default_ladder = Some("mobile".into());
        let stored = update(&db, &changed).await.unwrap().unwrap();
        assert_eq!(
            (
                stored.status.as_str(),
                stored.max_buckets,
                stored.default_ladder.as_deref()
            ),
            (STATUS_SUSPENDED, 3, Some("mobile"))
        );
        assert!(!stored.is_active());
        assert!(stored.updated_at >= a.updated_at);
        let mut ghost = a.clone();
        ghost.id = "ghost".into();
        assert!(update(&db, &ghost).await.unwrap().is_none());

        assert!(touch_login(&db, &b.id).await.unwrap());
        assert!(
            get(&db, &b.id)
                .await
                .unwrap()
                .unwrap()
                .last_login_at
                .is_some()
        );
        assert!(!touch_login(&db, "ghost").await.unwrap());

        // Usage and cascading delete.
        assert_eq!(usage(&db, &a.id).await.unwrap(), Usage::default());
        execute(
            &db,
            &format!(
                "INSERT INTO buckets (id, user_id, name, signing_key, bytes_used, objects_count, \
                 created_at, updated_at) VALUES ('b1', '{}', 'one', 'k', 1500, 3, 'x', 'x')",
                a.id
            ),
        )
        .await;
        execute(
            &db,
            "INSERT INTO bucket_objects (id, bucket_id, path, size, created_at, updated_at) \
             VALUES ('o1', 'b1', '/a.mp4', 1000, 'x', 'x')",
        )
        .await;
        execute(
            &db,
            &format!(
                "INSERT INTO vod_items (id, title, source_path, status, ladder, created_at, updated_at, \
                 user_id, bucket_id, object_id, output_bytes) VALUES \
                 ('v1', 'V', '/a.mp4', 'ready', 'standard', 'x', 'x', '{}', 'b1', 'o1', 500)",
                a.id
            ),
        )
        .await;
        execute(
            &db,
            &format!(
                "INSERT INTO api_keys (id, user_id, name, key_prefix, key_hash, created_at) \
                 VALUES ('k1', '{}', 'k', 'rms_x', 'h1', 'x')",
                a.id
            ),
        )
        .await;
        let u = usage(&db, &a.id).await.unwrap();
        assert_eq!(
            u,
            Usage {
                bytes_used: 1500,
                originals_bytes: 1000,
                encodings_bytes: 500,
                buckets: 1,
                objects: 3,
                encodings: 1,
                keys: 1
            }
        );
        let all = usage_all(&db).await.unwrap();
        assert_eq!(all[&a.id], u);
        assert_eq!(all[&b.id], Usage::default());

        assert!(delete(&db, &a.id).await.unwrap());
        assert!(!delete(&db, &a.id).await.unwrap());
        for table in ["buckets", "bucket_objects", "vod_items", "api_keys"] {
            assert_eq!(
                crate::db::testkit::scalar_i64(&db, &format!("SELECT COUNT(*) FROM {table}")).await,
                0,
                "{table}"
            );
        }
        assert_eq!(list(&db).await.unwrap().len(), 1);
    }
    dual_db_test!(crud, crud_body);
}
