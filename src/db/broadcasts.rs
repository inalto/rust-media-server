//! `broadcasts` repository (ARCHITECTURE.md §4.1).

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};

use super::{Db, DbResult, fmt_ts, parse_ts};

/// Default text shown on the waiting screen (also the column default in the schema).
pub const DEFAULT_WAITING_TEXT: &str = "La diretta inizierà a breve";

/// Maximum length of a broadcast name (slug).
pub const NAME_MAX_LEN: usize = 64;

/// A persisted broadcast. Serialized as-is inside `BroadcastView` (stream key included:
/// only admins see it).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Broadcast {
    pub id: String,
    /// URL-safe slug `[a-z0-9-_]{1,64}`, unique, used in all playback URLs.
    pub name: String,
    pub title: String,
    pub description: String,
    /// Secret RTMP stream key, 32 hex chars.
    pub stream_key: String,
    /// Auto-record when live.
    pub record: bool,
    pub enable_flv: bool,
    pub enable_hls: bool,
    pub enable_dash: bool,
    /// WebRTC egress via WHEP (`/whep/{name}`).
    pub enable_webrtc: bool,
    /// Admin forced "waiting" state.
    pub paused: bool,
    /// Relative path under `data/waiting/`.
    pub waiting_video: Option<String>,
    pub waiting_text: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Fields for a new broadcast. `name` must already be validated with [`is_valid_name`].
#[derive(Debug, Clone)]
pub struct NewBroadcast {
    pub name: String,
    pub title: String,
    pub description: String,
    pub record: bool,
    pub enable_flv: bool,
    pub enable_hls: bool,
    pub enable_dash: bool,
    pub enable_webrtc: bool,
    pub waiting_text: String,
}

impl NewBroadcast {
    pub fn new(name: impl Into<String>, title: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            title: title.into(),
            description: String::new(),
            record: false,
            enable_flv: true,
            enable_hls: true,
            enable_dash: true,
            enable_webrtc: true,
            waiting_text: DEFAULT_WAITING_TEXT.into(),
        }
    }
}

/// Partial update (`PATCH /broadcasts/{name}`); `None` = unchanged.
#[derive(Debug, Clone, Default)]
pub struct BroadcastPatch {
    pub title: Option<String>,
    pub description: Option<String>,
    pub record: Option<bool>,
    pub enable_flv: Option<bool>,
    pub enable_hls: Option<bool>,
    pub enable_dash: Option<bool>,
    pub enable_webrtc: Option<bool>,
    pub waiting_text: Option<String>,
}

/// `[a-z0-9-_]{1,64}`
pub fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= NAME_MAX_LEN
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

/// Derive a slug from a free-form title: lowercase ASCII, common accented letters
/// transliterated (Italian-friendly), other characters collapsed to single dashes,
/// trimmed and truncated to 64 chars. May return an empty string.
pub fn slugify(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    let mut pending_dash = false;
    for ch in title.chars().flat_map(char::to_lowercase) {
        let mapped: Option<&str> = match ch {
            'a'..='z' | '0'..='9' | '_' => None,
            'à' | 'á' | 'â' | 'ä' | 'ã' | 'å' => Some("a"),
            'è' | 'é' | 'ê' | 'ë' => Some("e"),
            'ì' | 'í' | 'î' | 'ï' => Some("i"),
            'ò' | 'ó' | 'ô' | 'ö' | 'õ' => Some("o"),
            'ù' | 'ú' | 'û' | 'ü' => Some("u"),
            'ç' => Some("c"),
            'ñ' => Some("n"),
            'ß' => Some("ss"),
            _ => Some(""),
        };
        match mapped {
            None => {
                if pending_dash && !out.is_empty() {
                    out.push('-');
                }
                pending_dash = false;
                out.push(ch);
            }
            Some("") => pending_dash = true,
            Some(s) => {
                if pending_dash && !out.is_empty() {
                    out.push('-');
                }
                pending_dash = false;
                out.push_str(s);
            }
        }
    }
    if out.len() > NAME_MAX_LEN {
        out.truncate(NAME_MAX_LEN);
    }
    out.trim_matches(|c| c == '-' || c == '_').to_string()
}

/// New random stream key: 32 lowercase hex chars (128 bits).
pub fn generate_stream_key() -> String {
    let bytes: [u8; 16] = rand::random();
    hex::encode(bytes)
}

const COLUMNS: &str = "id, name, title, description, stream_key, record, enable_flv, enable_hls, \
     enable_dash, paused, waiting_video, waiting_text, created_at, updated_at, enable_webrtc";

fn from_row(r: &Row<'_>) -> rusqlite::Result<Broadcast> {
    Ok(Broadcast {
        id: r.get(0)?,
        name: r.get(1)?,
        title: r.get(2)?,
        description: r.get(3)?,
        stream_key: r.get(4)?,
        record: r.get(5)?,
        enable_flv: r.get(6)?,
        enable_hls: r.get(7)?,
        enable_dash: r.get(8)?,
        paused: r.get(9)?,
        waiting_video: r.get(10)?,
        waiting_text: r.get(11)?,
        created_at: parse_ts(&r.get::<_, String>(12)?),
        updated_at: parse_ts(&r.get::<_, String>(13)?),
        enable_webrtc: r.get(14)?,
    })
}

fn query_one(c: &Connection, where_clause: &str, value: &str) -> DbResult<Option<Broadcast>> {
    let sql = format!("SELECT {COLUMNS} FROM broadcasts WHERE {where_clause} = ?1");
    Ok(c.query_row(&sql, [value], from_row).optional()?)
}

// ---- synchronous repository functions (usable inside `Db::call`) ----

pub fn list_sync(c: &Connection) -> DbResult<Vec<Broadcast>> {
    let sql = format!("SELECT {COLUMNS} FROM broadcasts ORDER BY created_at DESC, name");
    let mut st = c.prepare(&sql)?;
    let rows = st.query_map([], from_row)?.collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn find_by_name_sync(c: &Connection, name: &str) -> DbResult<Option<Broadcast>> {
    query_one(c, "name", name)
}

pub fn insert_sync(c: &Connection, new: &NewBroadcast) -> DbResult<Broadcast> {
    let now = Utc::now();
    let b = Broadcast {
        id: uuid::Uuid::new_v4().to_string(),
        name: new.name.clone(),
        title: new.title.clone(),
        description: new.description.clone(),
        stream_key: generate_stream_key(),
        record: new.record,
        enable_flv: new.enable_flv,
        enable_hls: new.enable_hls,
        enable_dash: new.enable_dash,
        enable_webrtc: new.enable_webrtc,
        paused: false,
        waiting_video: None,
        waiting_text: new.waiting_text.clone(),
        created_at: now,
        updated_at: now,
    };
    let ts = fmt_ts(&now);
    c.execute(
        "INSERT INTO broadcasts (id, name, title, description, stream_key, record, enable_flv, \
         enable_hls, enable_dash, paused, waiting_video, waiting_text, created_at, updated_at, \
         enable_webrtc) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 0, NULL, ?10, ?11, ?11, ?12)",
        params![
            b.id,
            b.name,
            b.title,
            b.description,
            b.stream_key,
            b.record,
            b.enable_flv,
            b.enable_hls,
            b.enable_dash,
            b.waiting_text,
            ts,
            b.enable_webrtc
        ],
    )?;
    // Re-read so the returned value matches what is stored (millisecond timestamps).
    find_by_name_sync(c, &b.name)?
        .ok_or_else(|| super::DbError::Other("inserted row vanished".into()))
}

/// Run an UPDATE for `name` (bumping `updated_at`) and return the fresh row.
fn update_and_get(
    c: &Connection,
    name: &str,
    set_clause: &str,
    values: &[&dyn rusqlite::ToSql],
) -> DbResult<Option<Broadcast>> {
    let sql = format!(
        "UPDATE broadcasts SET {set_clause}, updated_at = ?{} WHERE name = ?{}",
        values.len() + 1,
        values.len() + 2
    );
    let now = super::now_ts();
    let mut all: Vec<&dyn rusqlite::ToSql> = values.to_vec();
    all.push(&now);
    all.push(&name);
    let n = c.execute(&sql, all.as_slice())?;
    if n == 0 {
        return Ok(None);
    }
    find_by_name_sync(c, name)
}

pub fn update_sync(c: &Connection, name: &str, p: &BroadcastPatch) -> DbResult<Option<Broadcast>> {
    let Some(cur) = find_by_name_sync(c, name)? else {
        return Ok(None);
    };
    let title = p.title.clone().unwrap_or(cur.title);
    let description = p.description.clone().unwrap_or(cur.description);
    let record = p.record.unwrap_or(cur.record);
    let enable_flv = p.enable_flv.unwrap_or(cur.enable_flv);
    let enable_hls = p.enable_hls.unwrap_or(cur.enable_hls);
    let enable_dash = p.enable_dash.unwrap_or(cur.enable_dash);
    let enable_webrtc = p.enable_webrtc.unwrap_or(cur.enable_webrtc);
    let waiting_text = p.waiting_text.clone().unwrap_or(cur.waiting_text);
    update_and_get(
        c,
        name,
        "title = ?1, description = ?2, record = ?3, enable_flv = ?4, enable_hls = ?5, \
         enable_dash = ?6, waiting_text = ?7, enable_webrtc = ?8",
        &[
            &title,
            &description,
            &record,
            &enable_flv,
            &enable_hls,
            &enable_dash,
            &waiting_text,
            &enable_webrtc,
        ],
    )
}

// ---- async API (dispatch on the backend; lookups go through the cache) ----

pub async fn list(db: &Db) -> DbResult<Vec<Broadcast>> {
    if let Some(pool) = db.pg() {
        return pg::list(pool).await;
    }
    db.call(|c| list_sync(c)).await
}

/// Lookup by name (cached, see `db::cache`).
pub async fn find_by_name(db: &Db, name: &str) -> DbResult<Option<Broadcast>> {
    let cache = db.broadcast_cache();
    if let Some(b) = cache.get_by_name(name) {
        return Ok(Some(b));
    }
    let generation = cache.generation();
    let found = find_uncached(db, "name", name).await?;
    if let Some(b) = &found {
        cache.insert(generation, b);
    }
    Ok(found)
}

/// Lookup by secret stream key (RTMP ingest auth; cached, see `db::cache`).
pub async fn find_by_key(db: &Db, stream_key: &str) -> DbResult<Option<Broadcast>> {
    let cache = db.broadcast_cache();
    if let Some(b) = cache.get_by_key(stream_key) {
        return Ok(Some(b));
    }
    let generation = cache.generation();
    let found = find_uncached(db, "stream_key", stream_key).await?;
    if let Some(b) = &found {
        cache.insert(generation, b);
    }
    Ok(found)
}

pub async fn find_by_id(db: &Db, id: &str) -> DbResult<Option<Broadcast>> {
    find_uncached(db, "id", id).await
}

/// `column` is one of the constants `name` / `stream_key` / `id` (never request data).
async fn find_uncached(db: &Db, column: &'static str, value: &str) -> DbResult<Option<Broadcast>> {
    if let Some(pool) = db.pg() {
        return pg::find(pool, column, value).await;
    }
    let value = value.to_string();
    db.call(move |c| query_one(c, column, &value)).await
}

pub async fn name_exists(db: &Db, name: &str) -> DbResult<bool> {
    Ok(find_by_name(db, name).await?.is_some())
}

/// Insert a new broadcast with a fresh id and stream key. A duplicate name yields a
/// unique-violation [`super::DbError`] (HTTP 409).
pub async fn create(db: &Db, new: NewBroadcast) -> DbResult<Broadcast> {
    let name = new.name.clone();
    let r = match db.pg() {
        Some(pool) => pg::insert(pool, &new).await,
        None => db.call(move |c| insert_sync(c, &new)).await,
    };
    db.broadcast_cache().invalidate(&name);
    r
}

pub async fn update(db: &Db, name: &str, patch: BroadcastPatch) -> DbResult<Option<Broadcast>> {
    let r = match db.pg() {
        Some(pool) => pg::update(pool, name, &patch).await,
        None => {
            let name = name.to_string();
            db.call(move |c| update_sync(c, &name, &patch)).await
        }
    };
    db.broadcast_cache().invalidate(name);
    r
}

/// Delete and return the deleted row (so callers can clean up files).
pub async fn delete(db: &Db, name: &str) -> DbResult<Option<Broadcast>> {
    let r = match db.pg() {
        Some(pool) => pg::delete(pool, name).await,
        None => {
            let name = name.to_string();
            db.call(move |c| {
                let Some(b) = find_by_name_sync(c, &name)? else {
                    return Ok(None);
                };
                c.execute("DELETE FROM broadcasts WHERE id = ?1", [&b.id])?;
                Ok(Some(b))
            })
            .await
        }
    };
    db.broadcast_cache().invalidate(name);
    r
}

pub async fn regenerate_key(db: &Db, name: &str) -> DbResult<Option<Broadcast>> {
    let key = generate_stream_key();
    let r = match db.pg() {
        Some(pool) => pg::set_column(pool, name, "stream_key", key).await,
        None => {
            let name = name.to_string();
            db.call(move |c| update_and_get(c, &name, "stream_key = ?1", &[&key]))
                .await
        }
    };
    db.broadcast_cache().invalidate(name);
    r
}

pub async fn set_paused(db: &Db, name: &str, paused: bool) -> DbResult<Option<Broadcast>> {
    let r = match db.pg() {
        Some(pool) => pg::set_column(pool, name, "paused", paused).await,
        None => {
            let name = name.to_string();
            db.call(move |c| update_and_get(c, &name, "paused = ?1", &[&paused]))
                .await
        }
    };
    db.broadcast_cache().invalidate(name);
    r
}

/// Set (or clear with `None`) the waiting video path, relative to `data/waiting/`.
pub async fn set_waiting_video(
    db: &Db,
    name: &str,
    path: Option<String>,
) -> DbResult<Option<Broadcast>> {
    let r = match db.pg() {
        Some(pool) => pg::set_column(pool, name, "waiting_video", path).await,
        None => {
            let name = name.to_string();
            db.call(move |c| update_and_get(c, &name, "waiting_video = ?1", &[&path]))
                .await
        }
    };
    db.broadcast_cache().invalidate(name);
    r
}

/// PostgreSQL statements.
mod pg {
    use chrono::Utc;
    use sqlx_core::row::Row;
    use sqlx_postgres::PgRow;

    use super::{Broadcast, BroadcastPatch, COLUMNS, NewBroadcast, generate_stream_key};
    use crate::db::pg::q_dyn;
    use crate::db::{DbResult, PgPool, fmt_ts, now_ts, parse_ts};

    fn from_row(r: &PgRow) -> DbResult<Broadcast> {
        Ok(Broadcast {
            id: r.try_get(0)?,
            name: r.try_get(1)?,
            title: r.try_get(2)?,
            description: r.try_get(3)?,
            stream_key: r.try_get(4)?,
            record: r.try_get(5)?,
            enable_flv: r.try_get(6)?,
            enable_hls: r.try_get(7)?,
            enable_dash: r.try_get(8)?,
            paused: r.try_get(9)?,
            waiting_video: r.try_get(10)?,
            waiting_text: r.try_get(11)?,
            created_at: parse_ts(&r.try_get::<String, _>(12)?),
            updated_at: parse_ts(&r.try_get::<String, _>(13)?),
            enable_webrtc: r.try_get(14)?,
        })
    }

    pub async fn list(pool: &PgPool) -> DbResult<Vec<Broadcast>> {
        q_dyn(format!(
            "SELECT {COLUMNS} FROM broadcasts ORDER BY created_at DESC, name"
        ))
        .fetch_all(pool)
        .await?
        .iter()
        .map(from_row)
        .collect()
    }

    /// `column`: `name` | `stream_key` | `id`.
    pub async fn find(pool: &PgPool, column: &str, value: &str) -> DbResult<Option<Broadcast>> {
        q_dyn(format!(
            "SELECT {COLUMNS} FROM broadcasts WHERE {column} = $1"
        ))
        .bind(value.to_string())
        .fetch_optional(pool)
        .await?
        .as_ref()
        .map(from_row)
        .transpose()
    }

    pub async fn insert(pool: &PgPool, new: &NewBroadcast) -> DbResult<Broadcast> {
        let ts = fmt_ts(&Utc::now());
        let row = q_dyn(format!(
            "INSERT INTO broadcasts (id, name, title, description, stream_key, record, enable_flv, \
             enable_hls, enable_dash, paused, waiting_video, waiting_text, created_at, updated_at, \
             enable_webrtc) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, false, NULL, $10, $11, $11, $12) \
             RETURNING {COLUMNS}"
        ))
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(new.name.clone())
        .bind(new.title.clone())
        .bind(new.description.clone())
        .bind(generate_stream_key())
        .bind(new.record)
        .bind(new.enable_flv)
        .bind(new.enable_hls)
        .bind(new.enable_dash)
        .bind(new.waiting_text.clone())
        .bind(ts)
        .bind(new.enable_webrtc)
        .fetch_one(pool)
        .await?;
        from_row(&row)
    }

    pub async fn update(
        pool: &PgPool,
        name: &str,
        p: &BroadcastPatch,
    ) -> DbResult<Option<Broadcast>> {
        // COALESCE keeps the current value for the fields absent from the patch.
        q_dyn(format!(
            "UPDATE broadcasts SET title = COALESCE($2, title), \
             description = COALESCE($3, description), record = COALESCE($4, record), \
             enable_flv = COALESCE($5, enable_flv), enable_hls = COALESCE($6, enable_hls), \
             enable_dash = COALESCE($7, enable_dash), waiting_text = COALESCE($8, waiting_text), \
             enable_webrtc = COALESCE($9, enable_webrtc), updated_at = $10 \
             WHERE name = $1 RETURNING {COLUMNS}"
        ))
        .bind(name.to_string())
        .bind(p.title.clone())
        .bind(p.description.clone())
        .bind(p.record)
        .bind(p.enable_flv)
        .bind(p.enable_hls)
        .bind(p.enable_dash)
        .bind(p.waiting_text.clone())
        .bind(p.enable_webrtc)
        .bind(now_ts())
        .fetch_optional(pool)
        .await?
        .as_ref()
        .map(from_row)
        .transpose()
    }

    /// `UPDATE broadcasts SET <column> = $2, updated_at = now WHERE name = $1`.
    /// `column` is a constant of this module's callers.
    pub async fn set_column<T>(
        pool: &PgPool,
        name: &str,
        column: &'static str,
        value: T,
    ) -> DbResult<Option<Broadcast>>
    where
        T: for<'q> sqlx_core::encode::Encode<'q, sqlx_postgres::Postgres>
            + sqlx_core::types::Type<sqlx_postgres::Postgres>
            + Send
            + 'static,
    {
        q_dyn(format!(
            "UPDATE broadcasts SET {column} = $2, updated_at = $3 WHERE name = $1 RETURNING {COLUMNS}"
        ))
        .bind(name.to_string())
        .bind(value)
        .bind(now_ts())
        .fetch_optional(pool)
        .await?
        .as_ref()
        .map(from_row)
        .transpose()
    }

    pub async fn delete(pool: &PgPool, name: &str) -> DbResult<Option<Broadcast>> {
        q_dyn(format!(
            "DELETE FROM broadcasts WHERE name = $1 RETURNING {COLUMNS}"
        ))
        .bind(name.to_string())
        .fetch_optional(pool)
        .await?
        .as_ref()
        .map(from_row)
        .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_validation() {
        assert!(is_valid_name("a"));
        assert!(is_valid_name("my-show_2026"));
        assert!(is_valid_name(&"x".repeat(64)));
        assert!(!is_valid_name(""));
        assert!(!is_valid_name(&"x".repeat(65)));
        assert!(!is_valid_name("Upper"));
        assert!(!is_valid_name("with space"));
        assert!(!is_valid_name("dot.flv"));
        assert!(!is_valid_name("slash/x"));
        assert!(!is_valid_name("../etc"));
        assert!(!is_valid_name("perché"));
    }

    #[test]
    fn slugify_titles() {
        assert_eq!(slugify("Messa della Domenica"), "messa-della-domenica");
        assert_eq!(slugify("  Perché è così? Città!  "), "perche-e-cosi-citta");
        assert_eq!(slugify("a__b"), "a__b");
        assert_eq!(slugify("Live #1 -- 2026"), "live-1-2026");
        assert_eq!(slugify("!!!"), "");
        assert!(slugify(&"Lungo ".repeat(30)).len() <= 64);
        assert!(is_valid_name(&slugify(&"Lungo titolo ".repeat(30))));
    }

    #[test]
    fn stream_key_format() {
        let k = generate_stream_key();
        assert_eq!(k.len(), 32);
        assert!(k.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(k, generate_stream_key());
    }

    async fn crud_roundtrip_body(db: Db) {
        let b = create(&db, NewBroadcast::new("show", "Show"))
            .await
            .unwrap();
        assert_eq!(b.name, "show");
        assert!(b.enable_flv && b.enable_hls && b.enable_dash && b.enable_webrtc);
        assert_eq!(b.waiting_text, DEFAULT_WAITING_TEXT);
        assert!(!b.paused);

        // Duplicate name -> unique violation.
        let dup = create(&db, NewBroadcast::new("show", "Other"))
            .await
            .unwrap_err();
        assert!(dup.is_unique_violation());

        let by_key = find_by_key(&db, &b.stream_key).await.unwrap().unwrap();
        assert_eq!(by_key.id, b.id);
        assert!(find_by_key(&db, "nope").await.unwrap().is_none());

        let patched = update(
            &db,
            "show",
            BroadcastPatch {
                title: Some("New".into()),
                enable_dash: Some(false),
                enable_webrtc: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(patched.title, "New");
        assert!(!patched.enable_dash);
        assert!(!patched.enable_webrtc);
        assert!(patched.enable_hls);

        let rk = regenerate_key(&db, "show").await.unwrap().unwrap();
        assert_ne!(rk.stream_key, b.stream_key);
        assert!(find_by_key(&db, &b.stream_key).await.unwrap().is_none());

        let p = set_paused(&db, "show", true).await.unwrap().unwrap();
        assert!(p.paused);
        let w = set_waiting_video(&db, "show", Some("show/waiting.mp4".into()))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(w.waiting_video.as_deref(), Some("show/waiting.mp4"));

        assert_eq!(list(&db).await.unwrap().len(), 1);
        assert!(
            update(&db, "missing", BroadcastPatch::default())
                .await
                .unwrap()
                .is_none()
        );
        // Lookups by id bypass the cache.
        assert_eq!(find_by_id(&db, &b.id).await.unwrap().unwrap().name, "show");
        assert!(name_exists(&db, "show").await.unwrap());
        // Patch keeps the fields that are not part of it.
        let kept = update(
            &db,
            "show",
            BroadcastPatch {
                description: Some("d".into()),
                record: Some(true),
                waiting_text: Some("w".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            (
                kept.title.as_str(),
                kept.description.as_str(),
                kept.waiting_text.as_str()
            ),
            ("New", "d", "w")
        );
        assert!(kept.record && kept.paused && !kept.enable_dash && kept.enable_flv);
        let cleared = set_waiting_video(&db, "show", None).await.unwrap().unwrap();
        assert!(cleared.waiting_video.is_none());
        assert!(set_paused(&db, "missing", true).await.unwrap().is_none());
        assert!(regenerate_key(&db, "missing").await.unwrap().is_none());

        // Ordering: newest first.
        let second = create(&db, NewBroadcast::new("zz", "Zz")).await.unwrap();
        let names: Vec<String> = list(&db)
            .await
            .unwrap()
            .into_iter()
            .map(|b| b.name)
            .collect();
        if second.created_at > b.created_at {
            assert_eq!(names, ["zz", "show"]);
        }
        assert!(delete(&db, "zz").await.unwrap().is_some());

        assert!(delete(&db, "show").await.unwrap().is_some());
        assert!(find_by_name(&db, "show").await.unwrap().is_none());
        assert!(delete(&db, "show").await.unwrap().is_none());
    }
    crate::db::testkit::dual_db_test!(crud_roundtrip, crud_roundtrip_body);
}
