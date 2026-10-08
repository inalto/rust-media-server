//! `settings` key/value repository (global defaults such as the default waiting video).

use std::collections::HashMap;

use rusqlite::{OptionalExtension, params};

use super::{Db, DbResult};

/// Relative path (under `data/waiting/`) of the global default waiting video.
pub const DEFAULT_WAITING_VIDEO: &str = "default_waiting_video";
/// Global default waiting text (used when a broadcast's `waiting_text` is empty).
pub const DEFAULT_WAITING_TEXT: &str = "default_waiting_text";

pub async fn get(db: &Db, key: &str) -> DbResult<Option<String>> {
    let key = key.to_string();
    db.call(move |c| {
        Ok(
            c.query_row("SELECT value FROM settings WHERE key = ?1", [&key], |r| {
                r.get(0)
            })
            .optional()?,
        )
    })
    .await
}

pub async fn set(db: &Db, key: &str, value: &str) -> DbResult<()> {
    let (key, value) = (key.to_string(), value.to_string());
    db.call(move |c| {
        c.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    })
    .await
}

pub async fn delete(db: &Db, key: &str) -> DbResult<()> {
    let key = key.to_string();
    db.call(move |c| {
        c.execute("DELETE FROM settings WHERE key = ?1", [&key])?;
        Ok(())
    })
    .await
}

pub async fn all(db: &Db) -> DbResult<HashMap<String, String>> {
    db.call(|c| {
        let mut st = c.prepare("SELECT key, value FROM settings")?;
        let map = st
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<Result<HashMap<_, _>, _>>()?;
        Ok(map)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn get_set_delete() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(get(&db, DEFAULT_WAITING_TEXT).await.unwrap(), None);
        set(&db, DEFAULT_WAITING_TEXT, "a").await.unwrap();
        set(&db, DEFAULT_WAITING_TEXT, "b").await.unwrap();
        assert_eq!(
            get(&db, DEFAULT_WAITING_TEXT).await.unwrap().as_deref(),
            Some("b")
        );
        assert_eq!(all(&db).await.unwrap().len(), 1);
        delete(&db, DEFAULT_WAITING_TEXT).await.unwrap();
        assert_eq!(get(&db, DEFAULT_WAITING_TEXT).await.unwrap(), None);
    }
}
