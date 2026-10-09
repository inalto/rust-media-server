//! `buckets` repository (phase 8A, docs/ACCOUNTS.md §1).
//!
//! A bucket is a directory tree of files owned by a user, addressed by its global name
//! in the storage API (`/storage/{name}/...`) and stored under
//! `<storage root>/<bucket id>/` (renames are free). `bytes_used` (files + encodings)
//! and `objects_count` (files) are recomputed from `bucket_objects` and `vod_items` by
//! [`recount`] after every change; `signing_key` (32 random bytes, hex) signs the
//! playback URLs of its private encodings.

use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use sqlx_core::row::Row as _;
use sqlx_postgres::PgRow;

use super::pg::{get_u64, q, q_dyn};
use super::{Db, DbResult, fmt_ts, now_ts, parse_ts};

/// One row of `buckets`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Bucket {
    pub id: String,
    pub user_id: String,
    /// Global slug used in the storage URLs.
    pub name: String,
    pub description: String,
    /// Ladder of the encodings of this bucket (else the user's, else the server default).
    pub default_ladder: Option<String>,
    /// Encode uploaded videos automatically.
    pub auto_encode: bool,
    /// Default visibility of new encodings: `true` = public, `false` = private (signed URLs).
    pub public_playback: bool,
    /// HMAC key of the signed playback URLs (hex).
    pub signing_key: String,
    /// Files + encodings, bytes.
    pub bytes_used: u64,
    /// Files (directories excluded).
    pub objects_count: u64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// 32 random bytes, lowercase hex.
pub fn new_signing_key() -> String {
    let bytes: [u8; 32] = rand::random();
    hex::encode(bytes)
}

impl Bucket {
    /// A new empty bucket with a fresh signing key.
    pub fn new(user_id: &str, name: &str) -> Self {
        let now = parse_ts(&now_ts());
        Self {
            id: uuid::Uuid::new_v4().hyphenated().to_string(),
            user_id: user_id.to_string(),
            name: name.to_string(),
            description: String::new(),
            default_ladder: None,
            auto_encode: true,
            public_playback: true,
            signing_key: new_signing_key(),
            bytes_used: 0,
            objects_count: 0,
            created_at: now,
            updated_at: now,
        }
    }
}

const COLUMNS: &str = "id, user_id, name, description, default_ladder, auto_encode, \
     public_playback, signing_key, bytes_used, objects_count, created_at, updated_at";

fn from_row(r: &Row<'_>) -> rusqlite::Result<Bucket> {
    Ok(Bucket {
        id: r.get(0)?,
        user_id: r.get(1)?,
        name: r.get(2)?,
        description: r.get(3)?,
        default_ladder: r.get(4)?,
        auto_encode: r.get(5)?,
        public_playback: r.get(6)?,
        signing_key: r.get(7)?,
        bytes_used: r.get::<_, i64>(8)?.max(0) as u64,
        objects_count: r.get::<_, i64>(9)?.max(0) as u64,
        created_at: parse_ts(&r.get::<_, String>(10)?),
        updated_at: parse_ts(&r.get::<_, String>(11)?),
    })
}

fn pg_row(r: &PgRow) -> DbResult<Bucket> {
    Ok(Bucket {
        id: r.try_get(0)?,
        user_id: r.try_get(1)?,
        name: r.try_get(2)?,
        description: r.try_get(3)?,
        default_ladder: r.try_get(4)?,
        auto_encode: r.try_get(5)?,
        public_playback: r.try_get(6)?,
        signing_key: r.try_get(7)?,
        bytes_used: get_u64(r, 8)?,
        objects_count: get_u64(r, 9)?,
        created_at: parse_ts(&r.try_get::<String, _>(10)?),
        updated_at: parse_ts(&r.try_get::<String, _>(11)?),
    })
}

/// Insert a new row (unique violation when the name is taken).
pub async fn insert(db: &Db, bucket: &Bucket) -> DbResult<()> {
    let b = bucket.clone();
    if let Some(pool) = db.pg() {
        q_dyn(format!(
            "INSERT INTO buckets ({COLUMNS}) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)"
        ))
        .bind(b.id)
        .bind(b.user_id)
        .bind(b.name)
        .bind(b.description)
        .bind(b.default_ladder)
        .bind(b.auto_encode)
        .bind(b.public_playback)
        .bind(b.signing_key)
        .bind(super::pg::i64_of(b.bytes_used))
        .bind(super::pg::i64_of(b.objects_count))
        .bind(fmt_ts(&b.created_at))
        .bind(fmt_ts(&b.updated_at))
        .execute(pool)
        .await?;
        return Ok(());
    }
    db.call(move |c| {
        c.execute(
            &format!(
                "INSERT INTO buckets ({COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)"
            ),
            params![
                b.id,
                b.user_id,
                b.name,
                b.description,
                b.default_ladder,
                b.auto_encode,
                b.public_playback,
                b.signing_key,
                super::pg::i64_of(b.bytes_used),
                super::pg::i64_of(b.objects_count),
                fmt_ts(&b.created_at),
                fmt_ts(&b.updated_at),
            ],
        )?;
        Ok(())
    })
    .await
}

async fn get_by(db: &Db, column: &'static str, value: &str) -> DbResult<Option<Bucket>> {
    let value = value.to_string();
    if let Some(pool) = db.pg() {
        return q_dyn(format!("SELECT {COLUMNS} FROM buckets WHERE {column} = $1"))
            .bind(value)
            .fetch_optional(pool)
            .await?
            .as_ref()
            .map(pg_row)
            .transpose();
    }
    db.call(move |c| {
        Ok(c.query_row(
            &format!("SELECT {COLUMNS} FROM buckets WHERE {column} = ?1"),
            [value],
            from_row,
        )
        .optional()?)
    })
    .await
}

/// One bucket by id.
pub async fn get(db: &Db, id: &str) -> DbResult<Option<Bucket>> {
    get_by(db, "id", id).await
}

/// One bucket by its (global) name.
pub async fn find_by_name(db: &Db, name: &str) -> DbResult<Option<Bucket>> {
    get_by(db, "name", name).await
}

/// Buckets of a user, by name.
pub async fn list_for_user(db: &Db, user_id: &str) -> DbResult<Vec<Bucket>> {
    let user_id = user_id.to_string();
    if let Some(pool) = db.pg() {
        return q_dyn(format!(
            "SELECT {COLUMNS} FROM buckets WHERE user_id = $1 ORDER BY name"
        ))
        .bind(user_id)
        .fetch_all(pool)
        .await?
        .iter()
        .map(pg_row)
        .collect();
    }
    db.call(move |c| {
        let mut st = c.prepare(&format!(
            "SELECT {COLUMNS} FROM buckets WHERE user_id = ?1 ORDER BY name"
        ))?;
        let rows = st
            .query_map([user_id], from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })
    .await
}

/// Number of buckets of a user.
pub async fn count_for_user(db: &Db, user_id: &str) -> DbResult<u64> {
    let user_id = user_id.to_string();
    if let Some(pool) = db.pg() {
        let r = q("SELECT COUNT(*) FROM buckets WHERE user_id = $1")
            .bind(user_id)
            .fetch_one(pool)
            .await?;
        return get_u64(&r, 0);
    }
    db.call(move |c| {
        Ok(c.query_row(
            "SELECT COUNT(*) FROM buckets WHERE user_id = ?1",
            [user_id],
            |r| r.get::<_, i64>(0),
        )?
        .max(0) as u64)
    })
    .await
}

/// Write name, description, default ladder, auto-encode and public playback of
/// `bucket` (`updated_at` = now). Returns the stored row (`None` when missing).
pub async fn update(db: &Db, bucket: &Bucket) -> DbResult<Option<Bucket>> {
    let b = bucket.clone();
    let now = now_ts();
    if let Some(pool) = db.pg() {
        return q_dyn(format!(
            "UPDATE buckets SET name = $2, description = $3, default_ladder = $4, auto_encode = $5, \
             public_playback = $6, updated_at = $7 WHERE id = $1 RETURNING {COLUMNS}"
        ))
        .bind(b.id)
        .bind(b.name)
        .bind(b.description)
        .bind(b.default_ladder)
        .bind(b.auto_encode)
        .bind(b.public_playback)
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
                "UPDATE buckets SET name = ?2, description = ?3, default_ladder = ?4, auto_encode = ?5, \
                 public_playback = ?6, updated_at = ?7 WHERE id = ?1 RETURNING {COLUMNS}"
            ),
            params![
                b.id,
                b.name,
                b.description,
                b.default_ladder,
                b.auto_encode,
                b.public_playback,
                now
            ],
            from_row,
        )
        .optional()?)
    })
    .await
}

/// New random signing key (every token signed with the old one stops working).
pub async fn rotate_signing_key(db: &Db, id: &str) -> DbResult<Option<Bucket>> {
    let id = id.to_string();
    let key = new_signing_key();
    if let Some(pool) = db.pg() {
        return q_dyn(format!(
            "UPDATE buckets SET signing_key = $2, updated_at = $3 WHERE id = $1 RETURNING {COLUMNS}"
        ))
        .bind(id)
        .bind(key)
        .bind(now_ts())
        .fetch_optional(pool)
        .await?
        .as_ref()
        .map(pg_row)
        .transpose();
    }
    db.call(move |c| {
        Ok(c.query_row(
            &format!(
                "UPDATE buckets SET signing_key = ?2, updated_at = ?3 WHERE id = ?1 RETURNING {COLUMNS}"
            ),
            params![id, key, now_ts()],
            from_row,
        )
        .optional()?)
    })
    .await
}

const RECOUNT_SQLITE: &str = "UPDATE buckets SET \
     objects_count = (SELECT COUNT(*) FROM bucket_objects WHERE bucket_id = ?1 AND is_dir = 0), \
     bytes_used = (SELECT COALESCE(SUM(size), 0) FROM bucket_objects WHERE bucket_id = ?1 AND is_dir = 0) \
       + (SELECT COALESCE(SUM(output_bytes), 0) FROM vod_items WHERE bucket_id = ?1) \
     WHERE id = ?1 RETURNING bytes_used, objects_count";
const RECOUNT_PG: &str = "UPDATE buckets SET \
     objects_count = (SELECT COUNT(*) FROM bucket_objects WHERE bucket_id = $1 AND NOT is_dir), \
     bytes_used = CAST((SELECT COALESCE(SUM(size), 0) FROM bucket_objects WHERE bucket_id = $1 AND NOT is_dir) \
       + (SELECT COALESCE(SUM(output_bytes), 0) FROM vod_items WHERE bucket_id = $1) AS BIGINT) \
     WHERE id = $1 RETURNING bytes_used, objects_count";

/// Recompute `bytes_used` (files + encoding outputs) and `objects_count` (files) from
/// the index. Returns `(bytes_used, objects_count)`, `None` for an unknown bucket.
pub async fn recount(db: &Db, id: &str) -> DbResult<Option<(u64, u64)>> {
    let id = id.to_string();
    if let Some(pool) = db.pg() {
        return q(RECOUNT_PG)
            .bind(id)
            .fetch_optional(pool)
            .await?
            .map(|r| Ok((get_u64(&r, 0)?, get_u64(&r, 1)?)))
            .transpose();
    }
    db.call(move |c| {
        Ok(c.query_row(RECOUNT_SQLITE, [id], |r| {
            Ok((
                r.get::<_, i64>(0)?.max(0) as u64,
                r.get::<_, i64>(1)?.max(0) as u64,
            ))
        })
        .optional()?)
    })
    .await
}

const DELETE_SQLITE: [&str; 3] = [
    "DELETE FROM bucket_objects WHERE bucket_id = ?1",
    "DELETE FROM vod_items WHERE bucket_id = ?1",
    "DELETE FROM buckets WHERE id = ?1",
];
const DELETE_PG: [&str; 3] = [
    "DELETE FROM bucket_objects WHERE bucket_id = $1",
    "DELETE FROM vod_items WHERE bucket_id = $1",
    "DELETE FROM buckets WHERE id = $1",
];

/// Delete a bucket with its objects and encoding rows (one transaction; files are not
/// touched). False when it does not exist.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::testkit::{dual_db_test, execute, scalar_i64};
    use crate::db::users::{self, User};

    async fn lifecycle_body(db: Db) {
        let alice = User::new("alice", "Alice");
        users::insert(&db, &alice).await.unwrap();
        let mut a = Bucket::new(&alice.id, "media");
        a.description = "Videos".into();
        a.public_playback = false;
        let b = Bucket::new(&alice.id, "archive");
        assert_eq!(a.signing_key.len(), 64);
        assert_ne!(a.signing_key, b.signing_key);
        insert(&db, &a).await.unwrap();
        insert(&db, &b).await.unwrap();
        let dup = Bucket::new("someone-else", "media");
        assert!(
            insert(&db, &dup).await.unwrap_err().is_unique_violation(),
            "names are global"
        );

        assert_eq!(get(&db, &a.id).await.unwrap().unwrap(), a);
        assert_eq!(
            find_by_name(&db, "archive").await.unwrap().unwrap().id,
            b.id
        );
        assert!(find_by_name(&db, "nope").await.unwrap().is_none());
        let names: Vec<String> = list_for_user(&db, &alice.id)
            .await
            .unwrap()
            .into_iter()
            .map(|b| b.name)
            .collect();
        assert_eq!(names, ["archive", "media"]);
        assert_eq!(count_for_user(&db, &alice.id).await.unwrap(), 2);
        assert_eq!(count_for_user(&db, "x").await.unwrap(), 0);

        let mut changed = a.clone();
        changed.name = "media2".into();
        changed.auto_encode = false;
        changed.default_ladder = Some("mobile".into());
        let stored = update(&db, &changed).await.unwrap().unwrap();
        assert_eq!(
            (
                stored.name.as_str(),
                stored.auto_encode,
                stored.public_playback
            ),
            ("media2", false, false)
        );
        assert_eq!(stored.signing_key, a.signing_key, "update keeps the key");
        let rotated = rotate_signing_key(&db, &a.id).await.unwrap().unwrap();
        assert_ne!(rotated.signing_key, a.signing_key);
        assert!(rotate_signing_key(&db, "nope").await.unwrap().is_none());

        // Recount: files (not directories) + encoding outputs.
        for (id, path, dir, size) in [
            ("o1", "/v", 1, 0),
            ("o2", "/v/a.mp4", 0, 1000),
            ("o3", "/b.txt", 0, 24),
        ] {
            execute(
                &db,
                &format!(
                    "INSERT INTO bucket_objects (id, bucket_id, path, is_dir, size, created_at, updated_at) \
                     VALUES ('{id}', '{}', '{path}', {}, {size}, 'x', 'x')",
                    a.id,
                    if db.pg().is_some() { if dir == 1 { "true" } else { "false" } } else if dir == 1 { "1" } else { "0" }
                ),
            )
            .await;
        }
        execute(
            &db,
            &format!(
                "INSERT INTO vod_items (id, title, source_path, status, ladder, created_at, updated_at, \
                 user_id, bucket_id, object_id, output_bytes) VALUES \
                 ('v1', 'V', '/v/a.mp4', 'ready', 'standard', 'x', 'x', '{}', '{}', 'o2', 300)",
                alice.id, a.id
            ),
        )
        .await;
        assert_eq!(recount(&db, &a.id).await.unwrap(), Some((1324, 2)));
        assert_eq!(recount(&db, &b.id).await.unwrap(), Some((0, 0)));
        assert_eq!(recount(&db, "nope").await.unwrap(), None);
        let stored = get(&db, &a.id).await.unwrap().unwrap();
        assert_eq!((stored.bytes_used, stored.objects_count), (1324, 2));

        assert!(delete(&db, &a.id).await.unwrap());
        assert!(!delete(&db, &a.id).await.unwrap());
        assert_eq!(
            scalar_i64(&db, "SELECT COUNT(*) FROM bucket_objects").await,
            0
        );
        assert_eq!(scalar_i64(&db, "SELECT COUNT(*) FROM vod_items").await, 0);
        assert_eq!(count_for_user(&db, &alice.id).await.unwrap(), 1);
    }
    dual_db_test!(lifecycle, lifecycle_body);
}
