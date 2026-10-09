//! `settings` key/value repository (global defaults such as the default waiting video).

use std::collections::HashMap;

use rusqlite::{OptionalExtension, params};
use serde::Serialize;

use super::{Db, DbResult};

/// Relative path (under `data/waiting/`) of the global default waiting video.
pub const DEFAULT_WAITING_VIDEO: &str = "default_waiting_video";
/// Global default waiting text (used when a broadcast's `waiting_text` is empty).
pub const DEFAULT_WAITING_TEXT: &str = "default_waiting_text";
/// Language of the dashboard and of the player texts (`it` | `en`, default `it`).
pub const UI_LANGUAGE: &str = "ui_language";

/// UI language (dashboard and embedded player). Serialized as `"it"` / `"en"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum UiLanguage {
    #[default]
    It,
    En,
}

impl UiLanguage {
    pub const ALL: [UiLanguage; 2] = [UiLanguage::It, UiLanguage::En];

    /// Exact `"it"` / `"en"`; anything else is `None`.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "it" => Some(UiLanguage::It),
            "en" => Some(UiLanguage::En),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            UiLanguage::It => "it",
            UiLanguage::En => "en",
        }
    }

    /// Built-in waiting text, used when neither the broadcast nor the global settings
    /// define one.
    pub fn builtin_waiting_text(self) -> &'static str {
        match self {
            UiLanguage::It => super::broadcasts::DEFAULT_WAITING_TEXT,
            UiLanguage::En => "The live stream will start shortly",
        }
    }

    /// Whether `text` is the built-in waiting text of any language (such a stored text
    /// is not a customization: it is shown in the current language).
    pub fn is_builtin_waiting_text(text: &str) -> bool {
        let text = text.trim();
        Self::ALL.iter().any(|l| l.builtin_waiting_text() == text)
    }

    /// Language stored in a settings map (`it` when missing or invalid).
    pub fn from_settings(settings: &HashMap<String, String>) -> Self {
        settings
            .get(UI_LANGUAGE)
            .and_then(|v| Self::parse(v.trim()))
            .unwrap_or_default()
    }
}

/// Current UI language (`it` when not set).
pub async fn ui_language(db: &Db) -> DbResult<UiLanguage> {
    Ok(get(db, UI_LANGUAGE)
        .await?
        .and_then(|v| UiLanguage::parse(v.trim()))
        .unwrap_or_default())
}

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

    #[tokio::test]
    async fn ui_language_default_and_parse() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(ui_language(&db).await.unwrap(), UiLanguage::It);
        set(&db, UI_LANGUAGE, "en").await.unwrap();
        assert_eq!(ui_language(&db).await.unwrap(), UiLanguage::En);
        set(&db, UI_LANGUAGE, "fr").await.unwrap();
        assert_eq!(ui_language(&db).await.unwrap(), UiLanguage::It);
        assert_eq!(UiLanguage::parse("EN"), None);
        assert_eq!(UiLanguage::En.as_str(), "en");
        assert_eq!(
            UiLanguage::It.builtin_waiting_text(),
            "La diretta inizierà a breve"
        );
        assert_eq!(
            UiLanguage::En.builtin_waiting_text(),
            "The live stream will start shortly"
        );
        assert!(UiLanguage::is_builtin_waiting_text(
            " The live stream will start shortly "
        ));
        assert!(!UiLanguage::is_builtin_waiting_text("Torniamo presto"));
    }
}
