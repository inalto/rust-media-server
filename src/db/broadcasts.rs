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
     enable_dash, paused, waiting_video, waiting_text, created_at, updated_at";

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
        paused: false,
        waiting_video: None,
        waiting_text: new.waiting_text.clone(),
        created_at: now,
        updated_at: now,
    };
    let ts = fmt_ts(&now);
    c.execute(
        "INSERT INTO broadcasts (id, name, title, description, stream_key, record, enable_flv, \
         enable_hls, enable_dash, paused, waiting_video, waiting_text, created_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 0, NULL, ?10, ?11, ?11)",
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
            ts
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
    let waiting_text = p.waiting_text.clone().unwrap_or(cur.waiting_text);
    update_and_get(
        c,
        name,
        "title = ?1, description = ?2, record = ?3, enable_flv = ?4, enable_hls = ?5, \
         enable_dash = ?6, waiting_text = ?7",
        &[
            &title,
            &description,
            &record,
            &enable_flv,
            &enable_hls,
            &enable_dash,
            &waiting_text,
        ],
    )
}

// ---- async API ----

pub async fn list(db: &Db) -> DbResult<Vec<Broadcast>> {
    db.call(|c| list_sync(c)).await
}

pub async fn find_by_name(db: &Db, name: &str) -> DbResult<Option<Broadcast>> {
    let name = name.to_string();
    db.call(move |c| find_by_name_sync(c, &name)).await
}

/// Lookup by secret stream key (RTMP ingest auth).
pub async fn find_by_key(db: &Db, stream_key: &str) -> DbResult<Option<Broadcast>> {
    let key = stream_key.to_string();
    db.call(move |c| query_one(c, "stream_key", &key)).await
}

pub async fn find_by_id(db: &Db, id: &str) -> DbResult<Option<Broadcast>> {
    let id = id.to_string();
    db.call(move |c| query_one(c, "id", &id)).await
}

pub async fn name_exists(db: &Db, name: &str) -> DbResult<bool> {
    Ok(find_by_name(db, name).await?.is_some())
}

/// Insert a new broadcast with a fresh id and stream key. A duplicate name yields a
/// unique-violation [`super::DbError`] (HTTP 409).
pub async fn create(db: &Db, new: NewBroadcast) -> DbResult<Broadcast> {
    db.call(move |c| insert_sync(c, &new)).await
}

pub async fn update(db: &Db, name: &str, patch: BroadcastPatch) -> DbResult<Option<Broadcast>> {
    let name = name.to_string();
    db.call(move |c| update_sync(c, &name, &patch)).await
}

/// Delete and return the deleted row (so callers can clean up files).
pub async fn delete(db: &Db, name: &str) -> DbResult<Option<Broadcast>> {
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

pub async fn regenerate_key(db: &Db, name: &str) -> DbResult<Option<Broadcast>> {
    let name = name.to_string();
    db.call(move |c| {
        let key = generate_stream_key();
        update_and_get(c, &name, "stream_key = ?1", &[&key])
    })
    .await
}

pub async fn set_paused(db: &Db, name: &str, paused: bool) -> DbResult<Option<Broadcast>> {
    let name = name.to_string();
    db.call(move |c| update_and_get(c, &name, "paused = ?1", &[&paused]))
        .await
}

/// Set (or clear with `None`) the waiting video path, relative to `data/waiting/`.
pub async fn set_waiting_video(
    db: &Db,
    name: &str,
    path: Option<String>,
) -> DbResult<Option<Broadcast>> {
    let name = name.to_string();
    db.call(move |c| update_and_get(c, &name, "waiting_video = ?1", &[&path]))
        .await
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

    #[tokio::test]
    async fn crud_roundtrip() {
        let db = Db::open_in_memory().unwrap();
        let b = create(&db, NewBroadcast::new("show", "Show"))
            .await
            .unwrap();
        assert_eq!(b.name, "show");
        assert!(b.enable_flv && b.enable_hls && b.enable_dash);
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
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(patched.title, "New");
        assert!(!patched.enable_dash);
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
        assert!(delete(&db, "show").await.unwrap().is_some());
        assert!(find_by_name(&db, "show").await.unwrap().is_none());
        assert!(delete(&db, "show").await.unwrap().is_none());
    }
}
