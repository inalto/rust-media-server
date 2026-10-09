//! `settings` key/value repository (global defaults such as the default waiting video).

use std::collections::HashMap;

use rusqlite::{OptionalExtension, params};
use serde::Serialize;
use sqlx_core::row::Row;

use super::pg::q;
use super::{Db, DbResult};

/// Relative path (under `data/waiting/`) of the global default waiting video.
pub const DEFAULT_WAITING_VIDEO: &str = "default_waiting_video";
/// Global default waiting text (used when a broadcast's `waiting_text` is empty).
pub const DEFAULT_WAITING_TEXT: &str = "default_waiting_text";
/// Language of the dashboard and of the player texts (`it` | `en`, default `it`).
pub const UI_LANGUAGE: &str = "ui_language";
/// Default colour theme of the dashboard and of the player (`system` | `light` | `dark`,
/// default `system`).
pub const UI_THEME: &str = "ui_theme";

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

/// Default colour theme (dashboard and embedded player). Serialized as `"system"` /
/// `"light"` / `"dark"`; `system` follows the browser's `prefers-color-scheme`. Each
/// dashboard browser can override it (toggle, `localStorage`), each player page with
/// `?theme=`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum UiTheme {
    #[default]
    System,
    Light,
    Dark,
}

impl UiTheme {
    pub const ALL: [UiTheme; 3] = [UiTheme::System, UiTheme::Light, UiTheme::Dark];

    /// Exact `"system"` / `"light"` / `"dark"`; anything else is `None`.
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.as_str() == s)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            UiTheme::System => "system",
            UiTheme::Light => "light",
            UiTheme::Dark => "dark",
        }
    }

    /// Theme stored in a settings map (`system` when missing or invalid).
    pub fn from_settings(settings: &HashMap<String, String>) -> Self {
        settings
            .get(UI_THEME)
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

/// UI language and default theme with a single read (HTML pages).
pub async fn ui_prefs(db: &Db) -> DbResult<(UiLanguage, UiTheme)> {
    let settings = all(db).await?;
    Ok((
        UiLanguage::from_settings(&settings),
        UiTheme::from_settings(&settings),
    ))
}

pub async fn get(db: &Db, key: &str) -> DbResult<Option<String>> {
    if let Some(pool) = db.pg() {
        return Ok(q("SELECT value FROM settings WHERE key = $1")
            .bind(key.to_string())
            .fetch_optional(pool)
            .await?
            .map(|r| r.try_get::<String, _>(0))
            .transpose()?);
    }
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

/// Insert or replace a setting. Clears the broadcasts cache (views derived from the
/// broadcasts may depend on the global defaults).
pub async fn set(db: &Db, key: &str, value: &str) -> DbResult<()> {
    let r = match db.pg() {
        Some(pool) => q("INSERT INTO settings (key, value) VALUES ($1, $2) \
                ON CONFLICT (key) DO UPDATE SET value = excluded.value")
        .bind(key.to_string())
        .bind(value.to_string())
        .execute(pool)
        .await
        .map(|_| ())
        .map_err(Into::into),
        None => {
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
    };
    db.broadcast_cache().invalidate_all();
    r
}

pub async fn delete(db: &Db, key: &str) -> DbResult<()> {
    let r = match db.pg() {
        Some(pool) => q("DELETE FROM settings WHERE key = $1")
            .bind(key.to_string())
            .execute(pool)
            .await
            .map(|_| ())
            .map_err(Into::into),
        None => {
            let key = key.to_string();
            db.call(move |c| {
                c.execute("DELETE FROM settings WHERE key = ?1", [&key])?;
                Ok(())
            })
            .await
        }
    };
    db.broadcast_cache().invalidate_all();
    r
}

pub async fn all(db: &Db) -> DbResult<HashMap<String, String>> {
    if let Some(pool) = db.pg() {
        return q("SELECT key, value FROM settings")
            .fetch_all(pool)
            .await?
            .iter()
            .map(|r| Ok((r.try_get::<String, _>(0)?, r.try_get::<String, _>(1)?)))
            .collect();
    }
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

    async fn get_set_delete_body(db: Db) {
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
    crate::db::testkit::dual_db_test!(get_set_delete, get_set_delete_body);

    async fn ui_language_default_and_parse_body(db: Db) {
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
    crate::db::testkit::dual_db_test!(
        ui_language_default_and_parse,
        ui_language_default_and_parse_body
    );

    async fn ui_theme_default_and_parse_body(db: Db) {
        assert_eq!(
            ui_prefs(&db).await.unwrap(),
            (UiLanguage::It, UiTheme::System)
        );
        set(&db, UI_THEME, "light").await.unwrap();
        set(&db, UI_LANGUAGE, "en").await.unwrap();
        assert_eq!(
            ui_prefs(&db).await.unwrap(),
            (UiLanguage::En, UiTheme::Light)
        );
        set(&db, UI_THEME, " dark ").await.unwrap();
        assert_eq!(ui_prefs(&db).await.unwrap().1, UiTheme::Dark);
        set(&db, UI_THEME, "blue").await.unwrap();
        assert_eq!(ui_prefs(&db).await.unwrap().1, UiTheme::System);
        for t in UiTheme::ALL {
            assert_eq!(UiTheme::parse(t.as_str()), Some(t));
            assert_eq!(
                serde_json::to_value(t).unwrap(),
                serde_json::json!(t.as_str())
            );
        }
        assert_eq!(UiTheme::parse("Light"), None);
        assert_eq!(UiTheme::parse(""), None);
    }
    crate::db::testkit::dual_db_test!(ui_theme_default_and_parse, ui_theme_default_and_parse_body);
}
