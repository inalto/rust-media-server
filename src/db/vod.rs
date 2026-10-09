//! `vod_items` repository (video on demand, ARCHITECTURE.md §19).
//!
//! One row per video: the source file under `data/vod/<id>/`, its probe results, the
//! transcoding status (`queued` → `processing` → `ready` | `failed`) and the renditions
//! actually produced. Jobs are claimed with a conditional `UPDATE` (see [`claim`]) so
//! several runners (threads, or instances sharing the database) never process the same
//! item twice.

use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};

use super::{Db, DbResult, fmt_ts, now_ts, parse_ts};

/// `vod_items.status` values.
pub const STATUS_QUEUED: &str = "queued";
pub const STATUS_PROCESSING: &str = "processing";
pub const STATUS_READY: &str = "ready";
pub const STATUS_FAILED: &str = "failed";
pub const STATUSES: [&str; 4] = [
    STATUS_QUEUED,
    STATUS_PROCESSING,
    STATUS_READY,
    STATUS_FAILED,
];

/// `vod_items.origin` values.
pub const ORIGIN_UPLOAD: &str = "upload";
pub const ORIGIN_INBOX: &str = "inbox";

/// One rendition produced by a transcode (stored as JSON in `renditions_json`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Rendition {
    pub name: String,
    pub width: u32,
    pub height: u32,
    /// Target video bitrate; 0 = constant quality (CRF).
    pub video_kbps: u32,
    pub audio_kbps: u32,
    /// Measured peak bitrate (video + audio, bit/s) as announced in the HLS master.
    #[serde(default)]
    pub bandwidth: u64,
}

/// One row of `vod_items`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VodItem {
    pub id: String,
    pub title: String,
    pub description: String,
    /// Source file, relative to `data/vod/` (`<id>/source.<ext>`).
    pub source_path: String,
    /// Original file name (download name).
    pub source_name: String,
    pub source_size: u64,
    pub source_duration_secs: Option<f64>,
    pub source_width: Option<u32>,
    pub source_height: Option<u32>,
    pub source_codec: Option<String>,
    /// `upload` | `inbox`
    pub origin: String,
    /// `queued` | `processing` | `ready` | `failed`
    pub status: String,
    /// 0-100 (meaningful while `processing`; 100 when `ready`).
    pub progress: u32,
    pub ladder: String,
    pub renditions: Vec<Rendition>,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub ready_at: Option<DateTime<Utc>>,
    pub plays: u64,
}

impl VodItem {
    /// A new `queued` item.
    pub fn new(id: &str, title: &str, source_path: &str, ladder: &str, origin: &str) -> Self {
        let now = parse_ts(&now_ts());
        Self {
            id: id.into(),
            title: title.into(),
            description: String::new(),
            source_path: source_path.into(),
            source_name: String::new(),
            source_size: 0,
            source_duration_secs: None,
            source_width: None,
            source_height: None,
            source_codec: None,
            origin: origin.into(),
            status: STATUS_QUEUED.into(),
            progress: 0,
            ladder: ladder.into(),
            renditions: Vec::new(),
            error: None,
            created_at: now,
            updated_at: now,
            ready_at: None,
            plays: 0,
        }
    }

    pub fn is_ready(&self) -> bool {
        self.status == STATUS_READY
    }
}

const COLUMNS: &str = "id, title, description, source_path, source_name, source_size, \
     source_duration_secs, source_width, source_height, source_codec, origin, status, progress, \
     ladder, renditions_json, error, created_at, updated_at, ready_at, plays";

fn from_row(r: &Row<'_>) -> rusqlite::Result<VodItem> {
    let renditions: String = r.get(14)?;
    Ok(VodItem {
        id: r.get(0)?,
        title: r.get(1)?,
        description: r.get(2)?,
        source_path: r.get(3)?,
        source_name: r.get(4)?,
        source_size: r.get::<_, i64>(5)?.max(0) as u64,
        source_duration_secs: r.get(6)?,
        source_width: r
            .get::<_, Option<i64>>(7)?
            .map(|v| v.clamp(0, u32::MAX as i64) as u32),
        source_height: r
            .get::<_, Option<i64>>(8)?
            .map(|v| v.clamp(0, u32::MAX as i64) as u32),
        source_codec: r.get(9)?,
        origin: r.get(10)?,
        status: r.get(11)?,
        progress: r.get::<_, i64>(12)?.clamp(0, 100) as u32,
        ladder: r.get(13)?,
        // Malformed JSON (manual edits) is shown as "no renditions", never an error.
        renditions: serde_json::from_str(&renditions).unwrap_or_default(),
        error: r.get(15)?,
        created_at: parse_ts(&r.get::<_, String>(16)?),
        updated_at: parse_ts(&r.get::<_, String>(17)?),
        ready_at: r.get::<_, Option<String>>(18)?.map(|s| parse_ts(&s)),
        plays: r.get::<_, i64>(19)?.max(0) as u64,
    })
}

fn i64_of(n: u64) -> i64 {
    n.min(i64::MAX as u64) as i64
}

/// Insert a new row.
pub async fn insert(db: &Db, item: &VodItem) -> DbResult<()> {
    let it = item.clone();
    let renditions = serde_json::to_string(&it.renditions).unwrap_or_else(|_| "[]".into());
    db.call(move |c| {
        c.execute(
            &format!(
                "INSERT INTO vod_items ({COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, \
                 ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)"
            ),
            params![
                it.id,
                it.title,
                it.description,
                it.source_path,
                it.source_name,
                i64_of(it.source_size),
                it.source_duration_secs,
                it.source_width,
                it.source_height,
                it.source_codec,
                it.origin,
                it.status,
                it.progress,
                it.ladder,
                renditions,
                it.error,
                fmt_ts(&it.created_at),
                fmt_ts(&it.updated_at),
                it.ready_at.as_ref().map(fmt_ts),
                i64_of(it.plays),
            ],
        )?;
        Ok(())
    })
    .await
}

/// One row.
pub async fn get(db: &Db, id: &str) -> DbResult<Option<VodItem>> {
    let id = id.to_string();
    db.call(move |c| {
        Ok(c.query_row(
            &format!("SELECT {COLUMNS} FROM vod_items WHERE id = ?1"),
            [id],
            from_row,
        )
        .optional()?)
    })
    .await
}

/// All rows (or those with `status`), newest first.
pub async fn list(db: &Db, status: Option<&str>) -> DbResult<Vec<VodItem>> {
    let status = status.map(str::to_string);
    db.call(move |c| {
        let rows = match status {
            Some(s) => {
                let mut st = c.prepare(&format!(
                    "SELECT {COLUMNS} FROM vod_items WHERE status = ?1 ORDER BY created_at DESC, id"
                ))?;
                st.query_map([s], from_row)?
                    .collect::<Result<Vec<_>, _>>()?
            }
            None => {
                let mut st = c.prepare(&format!(
                    "SELECT {COLUMNS} FROM vod_items ORDER BY created_at DESC, id"
                ))?;
                st.query_map([], from_row)?.collect::<Result<Vec<_>, _>>()?
            }
        };
        Ok(rows)
    })
    .await
}

/// Ids of the `queued` items, oldest first (job order).
pub async fn queued_ids(db: &Db, limit: usize) -> DbResult<Vec<String>> {
    db.call(move |c| {
        let mut st = c.prepare(
            "SELECT id FROM vod_items WHERE status = 'queued' ORDER BY created_at, id LIMIT ?1",
        )?;
        let rows = st
            .query_map([i64_of(limit as u64)], |r| r.get(0))?
            .collect::<Result<Vec<String>, _>>()?;
        Ok(rows)
    })
    .await
}

/// Number of items per status: `(queued, processing)`.
pub async fn pending_counts(db: &Db) -> DbResult<(u64, u64)> {
    db.call(|c| {
        let count = |s: &str| -> rusqlite::Result<u64> {
            c.query_row(
                "SELECT COUNT(*) FROM vod_items WHERE status = ?1",
                [s],
                |r| r.get::<_, i64>(0),
            )
            .map(|n| n.max(0) as u64)
        };
        Ok((count(STATUS_QUEUED)?, count(STATUS_PROCESSING)?))
    })
    .await
}

/// Atomically take a `queued` item: `queued` → `processing`. False when another runner
/// (or a delete) got there first.
pub async fn claim(db: &Db, id: &str) -> DbResult<bool> {
    let id = id.to_string();
    db.call(move |c| {
        let n = c.execute(
            "UPDATE vod_items SET status = 'processing', progress = 0, error = NULL, updated_at = ?2 \
             WHERE id = ?1 AND status = 'queued'",
            params![id, now_ts()],
        )?;
        Ok(n == 1)
    })
    .await
}

/// Probe results of the source (set when the job starts).
pub async fn set_source_info(
    db: &Db,
    id: &str,
    duration: Option<f64>,
    width: Option<u32>,
    height: Option<u32>,
    codec: Option<String>,
) -> DbResult<bool> {
    let id = id.to_string();
    db.call(move |c| {
        let n = c.execute(
            "UPDATE vod_items SET source_duration_secs = ?2, source_width = ?3, source_height = ?4, \
             source_codec = ?5, updated_at = ?6 WHERE id = ?1",
            params![id, duration, width, height, codec, now_ts()],
        )?;
        Ok(n == 1)
    })
    .await
}

/// Progress of a `processing` item (0-100).
pub async fn set_progress(db: &Db, id: &str, progress: u32) -> DbResult<bool> {
    let id = id.to_string();
    db.call(move |c| {
        let n = c.execute(
            "UPDATE vod_items SET progress = ?2, updated_at = ?3 WHERE id = ?1 AND status = 'processing'",
            params![id, progress.min(100), now_ts()],
        )?;
        Ok(n == 1)
    })
    .await
}

/// `processing` → `ready` with the produced renditions. False when the row is gone or
/// no longer `processing` (deleted / re-queued meanwhile).
pub async fn mark_ready(db: &Db, id: &str, renditions: &[Rendition]) -> DbResult<bool> {
    let id = id.to_string();
    let json = serde_json::to_string(renditions).unwrap_or_else(|_| "[]".into());
    db.call(move |c| {
        let now = now_ts();
        let n = c.execute(
            "UPDATE vod_items SET status = 'ready', progress = 100, renditions_json = ?2, error = NULL, \
             updated_at = ?3, ready_at = ?3 WHERE id = ?1 AND status = 'processing'",
            params![id, json, now],
        )?;
        Ok(n == 1)
    })
    .await
}

/// `processing` → `failed` with an error message.
pub async fn mark_failed(db: &Db, id: &str, error: &str) -> DbResult<bool> {
    let id = id.to_string();
    let error = error.to_string();
    db.call(move |c| {
        let n = c.execute(
            "UPDATE vod_items SET status = 'failed', error = ?2, renditions_json = '[]', updated_at = ?3 \
             WHERE id = ?1 AND status = 'processing'",
            params![id, error, now_ts()],
        )?;
        Ok(n == 1)
    })
    .await
}

/// `processing` → `queued` (job interrupted by a shutdown).
pub async fn unclaim(db: &Db, id: &str) -> DbResult<bool> {
    let id = id.to_string();
    db.call(move |c| {
        let n = c.execute(
            "UPDATE vod_items SET status = 'queued', progress = 0, updated_at = ?2 \
             WHERE id = ?1 AND status = 'processing'",
            params![id, now_ts()],
        )?;
        Ok(n == 1)
    })
    .await
}

/// Re-queue a `ready` / `failed` / `queued` item with `ladder` (re-transcode). Returns the
/// updated row; `None` when it does not exist or is `processing`.
pub async fn requeue(db: &Db, id: &str, ladder: &str) -> DbResult<Option<VodItem>> {
    let id = id.to_string();
    let ladder = ladder.to_string();
    db.call(move |c| {
        let n = c.execute(
            "UPDATE vod_items SET status = 'queued', progress = 0, ladder = ?2, renditions_json = '[]', \
             error = NULL, ready_at = NULL, updated_at = ?3 WHERE id = ?1 AND status != 'processing'",
            params![id, ladder, now_ts()],
        )?;
        if n == 0 {
            return Ok(None);
        }
        Ok(c.query_row(
            &format!("SELECT {COLUMNS} FROM vod_items WHERE id = ?1"),
            [&id],
            from_row,
        )
        .optional()?)
    })
    .await
}

/// Startup: items left `processing` by a previous run go back to `queued`.
pub async fn requeue_interrupted(db: &Db) -> DbResult<usize> {
    db.call(|c| {
        Ok(c.execute(
            "UPDATE vod_items SET status = 'queued', progress = 0, updated_at = ?1 WHERE status = 'processing'",
            [now_ts()],
        )?)
    })
    .await
}

/// Change title and/or description. `None` when the row does not exist.
pub async fn update_meta(
    db: &Db,
    id: &str,
    title: Option<String>,
    description: Option<String>,
) -> DbResult<Option<VodItem>> {
    let id = id.to_string();
    db.call(move |c| {
        let tx = c.transaction()?;
        let n = tx.execute(
            "UPDATE vod_items SET title = COALESCE(?2, title), description = COALESCE(?3, description), \
             updated_at = ?4 WHERE id = ?1",
            params![id, title, description, now_ts()],
        )?;
        let item = if n == 0 {
            None
        } else {
            tx.query_row(
                &format!("SELECT {COLUMNS} FROM vod_items WHERE id = ?1"),
                [&id],
                from_row,
            )
            .optional()?
        };
        tx.commit()?;
        Ok(item)
    })
    .await
}

/// `plays += 1`; returns the new count (`None` when the row does not exist).
pub async fn increment_plays(db: &Db, id: &str) -> DbResult<Option<u64>> {
    let id = id.to_string();
    db.call(move |c| {
        Ok(c.query_row(
            "UPDATE vod_items SET plays = plays + 1 WHERE id = ?1 RETURNING plays",
            [id],
            |r| r.get::<_, i64>(0),
        )
        .optional()?
        .map(|n| n.max(0) as u64))
    })
    .await
}

/// Delete a row (any status unless `allow_processing` is false and it is processing).
/// Returns the deleted row; `Err`-free `None` when missing or refused.
pub async fn delete(db: &Db, id: &str, allow_processing: bool) -> DbResult<Option<VodItem>> {
    let id = id.to_string();
    db.call(move |c| {
        let tx = c.transaction()?;
        let item = tx
            .query_row(
                &format!("SELECT {COLUMNS} FROM vod_items WHERE id = ?1"),
                [&id],
                from_row,
            )
            .optional()?;
        let item = match item {
            Some(i) if allow_processing || i.status != STATUS_PROCESSING => {
                tx.execute("DELETE FROM vod_items WHERE id = ?1", [&id])?;
                Some(i)
            }
            _ => None,
        };
        tx.commit()?;
        Ok(item)
    })
    .await
}

/// Ids of every row (orphan directory sweep at startup).
pub async fn all_ids(db: &Db) -> DbResult<Vec<String>> {
    db.call(|c| {
        let mut st = c.prepare("SELECT id FROM vod_items")?;
        let rows = st
            .query_map([], |r| r.get(0))?
            .collect::<Result<Vec<String>, _>>()?;
        Ok(rows)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, created: &str) -> VodItem {
        let mut it = VodItem::new(
            id,
            &format!("Video {id}"),
            &format!("{id}/source.mp4"),
            "standard",
            ORIGIN_UPLOAD,
        );
        it.created_at = parse_ts(created);
        it.updated_at = it.created_at;
        it.source_name = "clip.mp4".into();
        it.source_size = 1234;
        it
    }

    #[tokio::test]
    async fn lifecycle() {
        let db = Db::open_in_memory().unwrap();
        let a = item("a", "2026-10-09T10:00:00.000Z");
        let b = item("b", "2026-10-09T11:00:00.000Z");
        let c = item("c", "2026-10-09T12:00:00.000Z");
        for it in [&a, &b, &c] {
            insert(&db, it).await.unwrap();
        }
        assert_eq!(get(&db, "a").await.unwrap().unwrap(), a);
        assert!(get(&db, "zz").await.unwrap().is_none());
        let ids = |v: Vec<VodItem>| v.into_iter().map(|i| i.id).collect::<Vec<_>>();
        assert_eq!(ids(list(&db, None).await.unwrap()), ["c", "b", "a"]);
        assert_eq!(queued_ids(&db, 10).await.unwrap(), ["a", "b", "c"]);
        assert_eq!(queued_ids(&db, 1).await.unwrap(), ["a"]);
        assert_eq!(pending_counts(&db).await.unwrap(), (3, 0));

        // Claim is atomic: only the first one wins.
        assert!(claim(&db, "a").await.unwrap());
        assert!(!claim(&db, "a").await.unwrap());
        assert!(!claim(&db, "nope").await.unwrap());
        assert_eq!(pending_counts(&db).await.unwrap(), (2, 1));
        assert_eq!(
            ids(list(&db, Some(STATUS_PROCESSING)).await.unwrap()),
            ["a"]
        );
        assert!(
            set_source_info(
                &db,
                "a",
                Some(30.0),
                Some(1920),
                Some(1080),
                Some("h264".into())
            )
            .await
            .unwrap()
        );
        assert!(set_progress(&db, "a", 42).await.unwrap());
        assert!(
            !set_progress(&db, "b", 42).await.unwrap(),
            "only while processing"
        );
        let got = get(&db, "a").await.unwrap().unwrap();
        assert_eq!(
            (got.progress, got.source_width, got.source_codec.as_deref()),
            (42, Some(1920), Some("h264"))
        );

        // Processing items can neither be re-queued nor deleted without force.
        assert!(requeue(&db, "a", "mobile").await.unwrap().is_none());
        assert!(delete(&db, "a", false).await.unwrap().is_none());
        let r = vec![Rendition {
            name: "720p".into(),
            width: 1280,
            height: 720,
            video_kbps: 2800,
            audio_kbps: 128,
            bandwidth: 3_100_000,
        }];
        assert!(mark_ready(&db, "a", &r).await.unwrap());
        assert!(
            !mark_ready(&db, "a", &r).await.unwrap(),
            "only from processing"
        );
        let ready = get(&db, "a").await.unwrap().unwrap();
        assert_eq!(ready.status, STATUS_READY);
        assert_eq!(ready.progress, 100);
        assert_eq!(ready.renditions, r);
        assert!(ready.ready_at.is_some());

        assert_eq!(increment_plays(&db, "a").await.unwrap(), Some(1));
        assert_eq!(increment_plays(&db, "a").await.unwrap(), Some(2));
        assert_eq!(increment_plays(&db, "zz").await.unwrap(), None);

        // Re-transcode with another ladder.
        let re = requeue(&db, "a", "mobile").await.unwrap().unwrap();
        assert_eq!(
            (re.status.as_str(), re.ladder.as_str(), re.progress),
            (STATUS_QUEUED, "mobile", 0)
        );
        assert!(re.renditions.is_empty() && re.ready_at.is_none());
        assert_eq!(re.plays, 2, "plays survive a re-transcode");

        // Failure keeps the row for a retry.
        assert!(claim(&db, "b").await.unwrap());
        assert!(mark_failed(&db, "b", "boom").await.unwrap());
        let failed = get(&db, "b").await.unwrap().unwrap();
        assert_eq!(
            (failed.status.as_str(), failed.error.as_deref()),
            (STATUS_FAILED, Some("boom"))
        );
        assert!(
            requeue(&db, "b", "standard")
                .await
                .unwrap()
                .unwrap()
                .error
                .is_none()
        );

        // Shutdown / restart.
        assert!(claim(&db, "c").await.unwrap());
        assert!(unclaim(&db, "c").await.unwrap());
        assert!(claim(&db, "c").await.unwrap());
        assert_eq!(requeue_interrupted(&db).await.unwrap(), 1);
        assert_eq!(get(&db, "c").await.unwrap().unwrap().status, STATUS_QUEUED);

        // Metadata.
        let m = update_meta(&db, "c", Some("New".into()), None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!((m.title.as_str(), m.description.as_str()), ("New", ""));
        let m = update_meta(&db, "c", None, Some("Desc".into()))
            .await
            .unwrap()
            .unwrap();
        assert_eq!((m.title.as_str(), m.description.as_str()), ("New", "Desc"));
        assert!(
            update_meta(&db, "zz", Some("x".into()), None)
                .await
                .unwrap()
                .is_none()
        );

        // Delete (forced while processing).
        assert!(claim(&db, "c").await.unwrap());
        assert!(delete(&db, "c", false).await.unwrap().is_none());
        assert_eq!(delete(&db, "c", true).await.unwrap().unwrap().id, "c");
        assert!(delete(&db, "c", true).await.unwrap().is_none());
        let mut all = all_ids(&db).await.unwrap();
        all.sort();
        assert_eq!(all, ["a", "b"]);
    }
}
