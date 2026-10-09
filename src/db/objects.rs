//! `bucket_objects` repository: the index of the files and directories of the buckets
//! (phase 8A, docs/ACCOUNTS.md §1, §3).
//!
//! `path` is normalized by `storage::paths` (leading `/`, no trailing `/`, NFC, no `..`);
//! directories are rows too (`is_dir`), created implicitly for the parents of an upload
//! or explicitly (`PUT /storage/{b}/{dir}/`). Writes go through the storage API only, so
//! the index matches the files under `<storage root>/<bucket id>/`.
//!
//! Directory queries use byte-ordered ranges (SQLite `BINARY`, PostgreSQL `COLLATE "C"`):
//! the descendants of `/videos` are the paths in `["/videos/", "/videos0")` (`0` follows
//! `/`), its direct children those with no further `/` after the prefix.

use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use sqlx_core::row::Row as _;
use sqlx_postgres::PgRow;

use super::pg::{get_u64, i64_of, q, q_dyn};
use super::{Db, DbResult, now_ts, parse_ts};

/// One row of `bucket_objects`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BucketObject {
    pub id: String,
    pub bucket_id: String,
    /// `/dir/file.mp4` (normalized).
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    pub content_type: Option<String>,
    /// SHA-256 (lowercase hex) of the content, computed while it was uploaded.
    pub sha256: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl BucketObject {
    /// Last path segment (`""` for the root).
    pub fn name(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or("")
    }
}

const COLUMNS: &str =
    "id, bucket_id, path, is_dir, size, content_type, sha256, created_at, updated_at";

fn from_row(r: &Row<'_>) -> rusqlite::Result<BucketObject> {
    Ok(BucketObject {
        id: r.get(0)?,
        bucket_id: r.get(1)?,
        path: r.get(2)?,
        is_dir: r.get(3)?,
        size: r.get::<_, i64>(4)?.max(0) as u64,
        content_type: r.get(5)?,
        sha256: r.get(6)?,
        created_at: parse_ts(&r.get::<_, String>(7)?),
        updated_at: parse_ts(&r.get::<_, String>(8)?),
    })
}

fn pg_row(r: &PgRow) -> DbResult<BucketObject> {
    Ok(BucketObject {
        id: r.try_get(0)?,
        bucket_id: r.try_get(1)?,
        path: r.try_get(2)?,
        is_dir: r.try_get(3)?,
        size: get_u64(r, 4)?,
        content_type: r.try_get(5)?,
        sha256: r.try_get(6)?,
        created_at: parse_ts(&r.try_get::<String, _>(7)?),
        updated_at: parse_ts(&r.try_get::<String, _>(8)?),
    })
}

/// Range of the descendants of directory `dir` (`/` = root): `(lo, hi, start)` with
/// `lo` = `dir/`, `hi` = `dir0` and `start` = the 1-based character position after `lo`
/// (SQL `substr`).
pub fn descendants_range(dir: &str) -> (String, String, i32) {
    let lo = if dir == "/" || dir.is_empty() {
        "/".to_string()
    } else {
        format!("{}/", dir.trim_end_matches('/'))
    };
    let hi = format!("{}0", &lo[..lo.len() - 1]);
    let start = i32::try_from(lo.chars().count() + 1).unwrap_or(i32::MAX);
    (lo, hi, start)
}

fn new_id() -> String {
    uuid::Uuid::new_v4().hyphenated().to_string()
}

/// The object at `path`.
pub async fn get(db: &Db, bucket_id: &str, path: &str) -> DbResult<Option<BucketObject>> {
    let (bucket_id, path) = (bucket_id.to_string(), path.to_string());
    if let Some(pool) = db.pg() {
        return q_dyn(format!(
            "SELECT {COLUMNS} FROM bucket_objects WHERE bucket_id = $1 AND path = $2"
        ))
        .bind(bucket_id)
        .bind(path)
        .fetch_optional(pool)
        .await?
        .as_ref()
        .map(pg_row)
        .transpose();
    }
    db.call(move |c| {
        Ok(c.query_row(
            &format!("SELECT {COLUMNS} FROM bucket_objects WHERE bucket_id = ?1 AND path = ?2"),
            params![bucket_id, path],
            from_row,
        )
        .optional()?)
    })
    .await
}

/// The object with id `id`.
pub async fn get_by_id(db: &Db, id: &str) -> DbResult<Option<BucketObject>> {
    let id = id.to_string();
    if let Some(pool) = db.pg() {
        return q_dyn(format!(
            "SELECT {COLUMNS} FROM bucket_objects WHERE id = $1"
        ))
        .bind(id)
        .fetch_optional(pool)
        .await?
        .as_ref()
        .map(pg_row)
        .transpose();
    }
    db.call(move |c| {
        Ok(c.query_row(
            &format!("SELECT {COLUMNS} FROM bucket_objects WHERE id = ?1"),
            [id],
            from_row,
        )
        .optional()?)
    })
    .await
}

/// Insert the file at `path` or update it (overwrite keeps `id` and `created_at`).
pub async fn upsert_file(
    db: &Db,
    bucket_id: &str,
    path: &str,
    size: u64,
    content_type: Option<String>,
    sha256: Option<String>,
) -> DbResult<BucketObject> {
    let (id, bucket_id, path, now) = (new_id(), bucket_id.to_string(), path.to_string(), now_ts());
    if let Some(pool) = db.pg() {
        let r = q_dyn(format!(
            "INSERT INTO bucket_objects ({COLUMNS}) VALUES ($1, $2, $3, false, $4, $5, $6, $7, $7) \
             ON CONFLICT (bucket_id, path) DO UPDATE SET is_dir = false, size = EXCLUDED.size, \
             content_type = EXCLUDED.content_type, sha256 = EXCLUDED.sha256, \
             updated_at = EXCLUDED.updated_at RETURNING {COLUMNS}"
        ))
        .bind(id)
        .bind(bucket_id)
        .bind(path)
        .bind(i64_of(size))
        .bind(content_type)
        .bind(sha256)
        .bind(now)
        .fetch_one(pool)
        .await?;
        return pg_row(&r);
    }
    db.call(move |c| {
        Ok(c.query_row(
            &format!(
                "INSERT INTO bucket_objects ({COLUMNS}) VALUES (?1, ?2, ?3, 0, ?4, ?5, ?6, ?7, ?7) \
                 ON CONFLICT (bucket_id, path) DO UPDATE SET is_dir = 0, size = excluded.size, \
                 content_type = excluded.content_type, sha256 = excluded.sha256, \
                 updated_at = excluded.updated_at RETURNING {COLUMNS}"
            ),
            params![id, bucket_id, path, i64_of(size), content_type, sha256, now],
            from_row,
        )?)
    })
    .await
}

/// Create the directory rows of `dirs` that do not exist yet; returns how many were
/// created.
pub async fn ensure_dirs(db: &Db, bucket_id: &str, dirs: &[String]) -> DbResult<u64> {
    let (bucket_id, dirs, now) = (bucket_id.to_string(), dirs.to_vec(), now_ts());
    if let Some(pool) = db.pg() {
        let mut created = 0;
        for d in dirs {
            created += q(
                "INSERT INTO bucket_objects (id, bucket_id, path, is_dir, size, created_at, updated_at) \
                 VALUES ($1, $2, $3, true, 0, $4, $4) ON CONFLICT (bucket_id, path) DO NOTHING",
            )
            .bind(new_id())
            .bind(bucket_id.clone())
            .bind(d)
            .bind(now.clone())
            .execute(pool)
            .await?
            .rows_affected();
        }
        return Ok(created);
    }
    db.call(move |c| {
        let tx = c.transaction()?;
        let mut created = 0;
        for d in dirs {
            created += tx.execute(
                "INSERT INTO bucket_objects (id, bucket_id, path, is_dir, size, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, 1, 0, ?4, ?4) ON CONFLICT (bucket_id, path) DO NOTHING",
                params![new_id(), bucket_id, d, now],
            )? as u64;
        }
        tx.commit()?;
        Ok(created)
    })
    .await
}

/// Direct children of directory `dir` (`/` = root): directories first, then by path.
pub async fn list_dir(db: &Db, bucket_id: &str, dir: &str) -> DbResult<Vec<BucketObject>> {
    let bucket_id = bucket_id.to_string();
    let (lo, hi, start) = descendants_range(dir);
    if let Some(pool) = db.pg() {
        return q_dyn(format!(
            "SELECT {COLUMNS} FROM bucket_objects WHERE bucket_id = $1 AND path > $2 AND path < $3 \
             AND strpos(substr(path, $4), '/') = 0 ORDER BY is_dir DESC, path"
        ))
        .bind(bucket_id)
        .bind(lo)
        .bind(hi)
        .bind(start)
        .fetch_all(pool)
        .await?
        .iter()
        .map(pg_row)
        .collect();
    }
    db.call(move |c| {
        let mut st = c.prepare(&format!(
            "SELECT {COLUMNS} FROM bucket_objects WHERE bucket_id = ?1 AND path > ?2 AND path < ?3 \
             AND instr(substr(path, ?4), '/') = 0 ORDER BY is_dir DESC, path"
        ))?;
        let rows = st
            .query_map(params![bucket_id, lo, hi, start], from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })
    .await
}

/// Every descendant of directory `dir` (`/` = the whole bucket), by path.
pub async fn list_tree(db: &Db, bucket_id: &str, dir: &str) -> DbResult<Vec<BucketObject>> {
    let bucket_id = bucket_id.to_string();
    let (lo, hi, _) = descendants_range(dir);
    if let Some(pool) = db.pg() {
        return q_dyn(format!(
            "SELECT {COLUMNS} FROM bucket_objects WHERE bucket_id = $1 AND path >= $2 AND path < $3 \
             ORDER BY path"
        ))
        .bind(bucket_id)
        .bind(lo)
        .bind(hi)
        .fetch_all(pool)
        .await?
        .iter()
        .map(pg_row)
        .collect();
    }
    db.call(move |c| {
        let mut st = c.prepare(&format!(
            "SELECT {COLUMNS} FROM bucket_objects WHERE bucket_id = ?1 AND path >= ?2 AND path < ?3 \
             ORDER BY path"
        ))?;
        let rows = st
            .query_map(params![bucket_id, lo, hi], from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })
    .await
}

/// Number of rows (files and directories) of a bucket.
pub async fn count(db: &Db, bucket_id: &str) -> DbResult<u64> {
    let bucket_id = bucket_id.to_string();
    if let Some(pool) = db.pg() {
        let r = q("SELECT COUNT(*) FROM bucket_objects WHERE bucket_id = $1")
            .bind(bucket_id)
            .fetch_one(pool)
            .await?;
        return get_u64(&r, 0);
    }
    db.call(move |c| {
        Ok(c.query_row(
            "SELECT COUNT(*) FROM bucket_objects WHERE bucket_id = ?1",
            [bucket_id],
            |r| r.get::<_, i64>(0),
        )?
        .max(0) as u64)
    })
    .await
}

/// Delete the file row at `path` (not a directory). Returns the deleted row.
pub async fn delete_file(db: &Db, bucket_id: &str, path: &str) -> DbResult<Option<BucketObject>> {
    let (bucket_id, path) = (bucket_id.to_string(), path.to_string());
    if let Some(pool) = db.pg() {
        return q_dyn(format!(
            "DELETE FROM bucket_objects WHERE bucket_id = $1 AND path = $2 AND NOT is_dir \
             RETURNING {COLUMNS}"
        ))
        .bind(bucket_id)
        .bind(path)
        .fetch_optional(pool)
        .await?
        .as_ref()
        .map(pg_row)
        .transpose();
    }
    db.call(move |c| {
        Ok(c.query_row(
            &format!(
                "DELETE FROM bucket_objects WHERE bucket_id = ?1 AND path = ?2 AND is_dir = 0 \
                 RETURNING {COLUMNS}"
            ),
            params![bucket_id, path],
            from_row,
        )
        .optional()?)
    })
    .await
}

/// Delete directory `dir` and everything below it (`/` = every row of the bucket).
/// Returns the number of rows deleted.
pub async fn delete_tree(db: &Db, bucket_id: &str, dir: &str) -> DbResult<u64> {
    let (bucket_id, dir_path) = (bucket_id.to_string(), dir.to_string());
    let (lo, hi, _) = descendants_range(dir);
    if let Some(pool) = db.pg() {
        return Ok(q("DELETE FROM bucket_objects WHERE bucket_id = $1 AND \
                     (path = $2 OR (path >= $3 AND path < $4))")
        .bind(bucket_id)
        .bind(dir_path)
        .bind(lo)
        .bind(hi)
        .execute(pool)
        .await?
        .rows_affected());
    }
    db.call(move |c| {
        Ok(c.execute(
            "DELETE FROM bucket_objects WHERE bucket_id = ?1 AND \
             (path = ?2 OR (path >= ?3 AND path < ?4))",
            params![bucket_id, dir_path, lo, hi],
        )? as u64)
    })
    .await
}

/// Correct the stored size of a file (recount repair).
pub async fn set_size(db: &Db, id: &str, size: u64) -> DbResult<bool> {
    let id = id.to_string();
    if let Some(pool) = db.pg() {
        let n = q("UPDATE bucket_objects SET size = $2 WHERE id = $1")
            .bind(id)
            .bind(i64_of(size))
            .execute(pool)
            .await?
            .rows_affected();
        return Ok(n == 1);
    }
    db.call(move |c| {
        Ok(c.execute(
            "UPDATE bucket_objects SET size = ?2 WHERE id = ?1",
            params![id, i64_of(size)],
        )? == 1)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::testkit::dual_db_test;

    #[test]
    fn ranges() {
        assert_eq!(descendants_range("/"), ("/".into(), "0".into(), 2));
        assert_eq!(
            descendants_range("/videos"),
            ("/videos/".into(), "/videos0".into(), 9)
        );
        assert_eq!(descendants_range("/vidéo").2, 8, "characters, not bytes");
    }

    fn paths(v: &[BucketObject]) -> Vec<&str> {
        v.iter().map(|o| o.path.as_str()).collect()
    }

    async fn tree_body(db: Db) {
        let user = crate::db::users::User::new("alice", "Alice");
        crate::db::users::insert(&db, &user).await.unwrap();
        for name in ["bucket-1", "bucket-2"] {
            let mut bucket = crate::db::buckets::Bucket::new(&user.id, name);
            bucket.id = name.into();
            crate::db::buckets::insert(&db, &bucket).await.unwrap();
        }
        let b = "bucket-1";
        assert_eq!(
            ensure_dirs(&db, b, &["/videos".into(), "/videos/2026".into()])
                .await
                .unwrap(),
            2
        );
        assert_eq!(ensure_dirs(&db, b, &["/videos".into()]).await.unwrap(), 0);
        let f = upsert_file(
            &db,
            b,
            "/videos/a.mp4",
            100,
            Some("video/mp4".into()),
            Some("aa".into()),
        )
        .await
        .unwrap();
        assert_eq!((f.size, f.is_dir, f.name()), (100, false, "a.mp4"));
        upsert_file(&db, b, "/videos/2026/b.mp4", 5, None, None)
            .await
            .unwrap();
        upsert_file(&db, b, "/videos-old.txt", 7, None, None)
            .await
            .unwrap();
        upsert_file(&db, b, "/vidéo.mp4", 9, None, None)
            .await
            .unwrap();
        upsert_file(&db, "bucket-2", "/videos/a.mp4", 1, None, None)
            .await
            .unwrap();

        // Overwrite keeps id and created_at.
        let f2 = upsert_file(
            &db,
            b,
            "/videos/a.mp4",
            200,
            Some("video/mp4".into()),
            Some("bb".into()),
        )
        .await
        .unwrap();
        assert_eq!(
            (f2.id.as_str(), f2.created_at, f2.size),
            (f.id.as_str(), f.created_at, 200)
        );
        assert_eq!(f2.sha256.as_deref(), Some("bb"));
        assert_eq!(get(&db, b, "/videos/a.mp4").await.unwrap().unwrap(), f2);
        assert_eq!(
            get_by_id(&db, &f.id).await.unwrap().unwrap().path,
            "/videos/a.mp4"
        );
        assert!(get(&db, b, "/nope").await.unwrap().is_none());

        assert_eq!(
            paths(&list_dir(&db, b, "/").await.unwrap()),
            ["/videos", "/videos-old.txt", "/vidéo.mp4"],
            "directories first"
        );
        assert_eq!(
            paths(&list_dir(&db, b, "/videos").await.unwrap()),
            ["/videos/2026", "/videos/a.mp4"]
        );
        assert!(list_dir(&db, b, "/missing").await.unwrap().is_empty());
        assert_eq!(
            paths(&list_tree(&db, b, "/videos").await.unwrap()),
            ["/videos/2026", "/videos/2026/b.mp4", "/videos/a.mp4"]
        );
        assert_eq!(list_tree(&db, b, "/").await.unwrap().len(), 6);
        assert_eq!(count(&db, b).await.unwrap(), 6);

        assert!(set_size(&db, &f.id, 300).await.unwrap());
        assert!(
            delete_file(&db, b, "/videos").await.unwrap().is_none(),
            "not a file"
        );
        assert_eq!(
            delete_file(&db, b, "/videos/a.mp4")
                .await
                .unwrap()
                .unwrap()
                .size,
            300
        );
        assert!(
            delete_file(&db, b, "/videos/a.mp4")
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(delete_tree(&db, b, "/videos").await.unwrap(), 3);
        assert_eq!(
            paths(&list_dir(&db, b, "/").await.unwrap()),
            ["/videos-old.txt", "/vidéo.mp4"],
            "a sibling with the same prefix survives"
        );
        assert_eq!(delete_tree(&db, b, "/").await.unwrap(), 2);
        assert_eq!(count(&db, b).await.unwrap(), 0);
        assert_eq!(count(&db, "bucket-2").await.unwrap(), 1);
    }
    dual_db_test!(tree, tree_body);
}
