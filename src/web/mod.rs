//! Pages and static assets (ARCHITECTURE.md §8.3).
//!
//! - `/` dashboard SPA (`static/index.html` + ES modules `app.js`, `core.js`,
//!   `detail.js`, `metrics.js`, `mock.js`, `i18n.js` and `style.css`), Italian or English
//!   UI (`ui_language` setting, ARCHITECTURE.md §17).
//! - `/embed/{name}` player page for iframes, `/player/{name}` full-page player
//!   (same `embed.html`, `data-mode` differs; ES module `player.js` + `i18n.js`).
//! - `/static/*` assets embedded in the binary with `include_str!`.
//! - `/waiting/*` files from `data/waiting/` (waiting videos and posters).
//!
//! The pages get `<html lang>` from the `ui_language` setting (one small SQLite read per
//! page); `?lang=it|en` on the player pages overrides it.

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tower_http::cors::CorsLayer;
use tower_http::services::ServeDir;

use crate::db::broadcasts::is_valid_name;
use crate::db::settings::{self, UiLanguage};
use crate::state::AppState;

/// Embedded static assets: (file name, content type, content).
const ASSETS: &[(&str, &str, &str)] = &[
    (
        "index.html",
        "text/html; charset=utf-8",
        include_str!("static/index.html"),
    ),
    (
        "app.js",
        "text/javascript; charset=utf-8",
        include_str!("static/app.js"),
    ),
    (
        "core.js",
        "text/javascript; charset=utf-8",
        include_str!("static/core.js"),
    ),
    (
        "detail.js",
        "text/javascript; charset=utf-8",
        include_str!("static/detail.js"),
    ),
    (
        "metrics.js",
        "text/javascript; charset=utf-8",
        include_str!("static/metrics.js"),
    ),
    (
        "mock.js",
        "text/javascript; charset=utf-8",
        include_str!("static/mock.js"),
    ),
    (
        "i18n.js",
        "text/javascript; charset=utf-8",
        include_str!("static/i18n.js"),
    ),
    (
        "style.css",
        "text/css; charset=utf-8",
        include_str!("static/style.css"),
    ),
    (
        "favicon.svg",
        "image/svg+xml",
        include_str!("static/favicon.svg"),
    ),
    (
        "embed.html",
        "text/html; charset=utf-8",
        include_str!("static/embed.html"),
    ),
    (
        "player.js",
        "text/javascript; charset=utf-8",
        include_str!("static/player.js"),
    ),
    (
        "player.css",
        "text/css; charset=utf-8",
        include_str!("static/player.css"),
    ),
];

pub fn router(state: &AppState) -> Router<AppState> {
    let waiting = ServeDir::new(state.config.waiting_dir());
    Router::new()
        .route("/", get(index))
        .route("/favicon.ico", get(|| async { StatusCode::NO_CONTENT }))
        .route("/static/{*path}", get(static_asset))
        .route("/embed/{name}", get(embed_page))
        .route("/player/{name}", get(player_page))
        .nest_service("/waiting", waiting)
        .layer(CorsLayer::permissive())
}

fn asset(name: &str) -> Option<(&'static str, &'static str)> {
    ASSETS
        .iter()
        .find(|(n, _, _)| *n == name)
        .map(|(_, ct, body)| (*ct, *body))
}

fn html_response(body: String) -> Response {
    let mut r = body.into_response();
    let h = r.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    r
}

/// The language attribute every HTML template starts with.
const HTML_LANG_TAG: &str = "<html lang=\"it\">";

/// `template` with `<html lang>` set to `lang`.
fn with_lang(template: &str, lang: UiLanguage) -> String {
    template.replacen(
        HTML_LANG_TAG,
        &format!("<html lang=\"{}\">", lang.as_str()),
        1,
    )
}

/// UI language setting (`it` if it cannot be read).
async fn ui_language(state: &AppState) -> UiLanguage {
    match settings::ui_language(&state.db).await {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(error = %e, "cannot read the UI language setting");
            UiLanguage::default()
        }
    }
}

async fn index(State(state): State<AppState>) -> Response {
    let template = asset("index.html").map(|(_, b)| b).unwrap_or_default();
    html_response(with_lang(template, ui_language(&state).await))
}

/// Embedded assets change with the binary: `no-cache` + content-hash ETag, so
/// browsers revalidate (cheap 304) and always pick up a new version after upgrades.
async fn static_asset(Path(path): Path<String>, headers: HeaderMap) -> Response {
    let Some((ct, body)) = asset(&path) else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let digest = Sha256::digest(body.as_bytes());
    let etag = format!("\"{}\"", hex::encode(&digest[..12]));
    let etag_value =
        HeaderValue::from_str(&etag).unwrap_or_else(|_| HeaderValue::from_static("\"0\""));
    let matches = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == etag));
    let mut r = if matches {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        let mut r = body.into_response();
        r.headers_mut()
            .insert(header::CONTENT_TYPE, HeaderValue::from_static(ct));
        r
    };
    let h = r.headers_mut();
    h.insert(header::ETAG, etag_value);
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    r
}

/// Query of the player pages read by the server (the rest is read by `player.js`).
#[derive(Debug, Default, Deserialize)]
struct PlayerQuery {
    /// `it` | `en` (case-insensitive): overrides the `ui_language` setting.
    lang: Option<String>,
}

async fn player_html(state: &AppState, name: &str, mode: &str, query: PlayerQuery) -> Response {
    if !is_valid_name(name) {
        return (StatusCode::NOT_FOUND, "broadcast not found").into_response();
    }
    let lang = match query
        .lang
        .and_then(|l| UiLanguage::parse(&l.trim().to_ascii_lowercase()))
    {
        Some(l) => l,
        None => ui_language(state).await,
    };
    let template = asset("embed.html").map(|(_, b)| b).unwrap_or_default();
    // `name` is a validated slug: safe to inline.
    let body = with_lang(template, lang)
        .replace("data-mode=\"embed\"", &format!("data-mode=\"{mode}\""))
        .replace("{{NAME}}", name);
    html_response(body)
}

/// Lenient query parsing: a malformed query never fails the page.
fn player_query(uri: &Uri) -> PlayerQuery {
    Query::<PlayerQuery>::try_from_uri(uri)
        .map(|Query(q)| q)
        .unwrap_or_default()
}

async fn embed_page(State(state): State<AppState>, Path(name): Path<String>, uri: Uri) -> Response {
    player_html(&state, &name, "embed", player_query(&uri)).await
}

async fn player_page(
    State(state): State<AppState>,
    Path(name): Path<String>,
    uri: Uri,
) -> Response {
    player_html(&state, &name, "player", player_query(&uri)).await
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    use super::*;
    use crate::api::waiting::tests::test_state;

    const I18N: &str = include_str!("static/i18n.js");
    /// Static files that use translation keys (everything but i18n.js itself).
    const USERS: &[(&str, &str)] = &[
        ("index.html", include_str!("static/index.html")),
        ("embed.html", include_str!("static/embed.html")),
        ("app.js", include_str!("static/app.js")),
        ("core.js", include_str!("static/core.js")),
        ("detail.js", include_str!("static/detail.js")),
        ("metrics.js", include_str!("static/metrics.js")),
        ("mock.js", include_str!("static/mock.js")),
        ("player.js", include_str!("static/player.js")),
    ];

    /// `export const <lang> = { ... };` of i18n.js: one `'key': 'text',` per line.
    fn dictionary(lang: &str) -> BTreeMap<String, String> {
        let start = format!("export const {lang} = {{\n");
        let body = I18N
            .split_once(start.as_str())
            .unwrap_or_else(|| panic!("no `{start}` in i18n.js"))
            .1;
        let body = body.split_once("\n};").expect("unterminated dictionary").0;
        let mut out = BTreeMap::new();
        for line in body.lines().map(str::trim) {
            if line.is_empty() || line.starts_with("//") {
                continue;
            }
            let bad = || panic!("{lang}: not a `'key': 'text',` line: {line}");
            let Some((key, rest)) = line.strip_prefix('\'').and_then(|r| r.split_once('\'')) else {
                bad()
            };
            let Some(value) = rest.strip_prefix(": ").and_then(|v| v.strip_suffix(',')) else {
                bad()
            };
            let quote = value.chars().next().filter(|q| *q == '\'' || *q == '"');
            let Some(q) = quote.filter(|q| value.len() >= 2 && value.ends_with(*q)) else {
                bad()
            };
            let text = &value[1..value.len() - 1];
            assert!(
                !text.contains(q),
                "{lang}.{key}: unescaped quote in {value}"
            );
            assert!(!text.trim().is_empty(), "{lang}.{key}: empty text");
            let prev = out.insert(key.to_string(), text.to_string());
            assert!(prev.is_none(), "{lang}: duplicate key {key}");
        }
        out
    }

    /// Literal keys of `t('…')` (plural = false) and `tn('…')` (plural = true) calls.
    /// Panics on a call whose first argument is not a string literal (`strict`).
    fn call_keys(file: &str, src: &str, strict: bool) -> Vec<(String, bool)> {
        let bytes = src.as_bytes();
        let mut out = Vec::new();
        for (call, plural) in [("t(", false), ("tn(", true)] {
            let mut from = 0;
            while let Some(i) = src[from..].find(call) {
                let at = from + i;
                from = at + call.len();
                let ident = |c: u8| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'$' | b'.');
                if at > 0 && ident(bytes[at - 1]) {
                    continue;
                }
                // Skip comments (`// … t() …`, ` * … t() …`).
                let line_start = src[..at].rfind('\n').map_or(0, |p| p + 1);
                let line = src[line_start..at].trim_start();
                if line.starts_with("//") || line.starts_with('*') || line.starts_with("/*") {
                    continue;
                }
                let rest = &src[from..];
                match rest.chars().next() {
                    Some(q @ ('\'' | '"')) => {
                        let end = rest[1..].find(q).expect("unterminated key") + 1;
                        out.push((rest[1..end].to_string(), plural));
                    }
                    _ if strict => {
                        let line = src[..at].lines().count();
                        panic!("{file}:{line}: {call}…) without a literal key");
                    }
                    _ => {}
                }
            }
        }
        out
    }

    /// Keys of `data-i18n="…"`, `data-i18n-title="…"`, `data-i18n-aria-label="…"`,
    /// `data-i18n-placeholder="…"` attributes.
    fn attr_keys(src: &str) -> Vec<String> {
        let mut out = Vec::new();
        for (i, _) in src.match_indices("data-i18n") {
            let rest = &src[i + "data-i18n".len()..];
            let rest = ["-title", "-aria-label", "-placeholder"]
                .iter()
                .find_map(|s| rest.strip_prefix(s))
                .unwrap_or(rest);
            if let Some(v) = rest.strip_prefix("=\"") {
                out.push(v[..v.find('"').expect("unterminated attribute")].to_string());
            }
        }
        out
    }

    #[test]
    fn i18n_dictionaries_have_the_same_keys() {
        let it = dictionary("it");
        let en = dictionary("en");
        assert!(it.len() > 300, "only {} keys parsed", it.len());
        let it_keys: BTreeSet<_> = it.keys().collect();
        let en_keys: BTreeSet<_> = en.keys().collect();
        assert_eq!(
            it_keys.difference(&en_keys).collect::<Vec<_>>(),
            Vec::<&&String>::new(),
            "keys missing in en"
        );
        assert_eq!(
            en_keys.difference(&it_keys).collect::<Vec<_>>(),
            Vec::<&&String>::new(),
            "keys missing in it"
        );
        // The player's built-in waiting text is the server's.
        assert_eq!(it["waiting.builtin"], UiLanguage::It.builtin_waiting_text());
        assert_eq!(en["waiting.builtin"], UiLanguage::En.builtin_waiting_text());
        // Placeholders match between the two languages.
        let placeholders = |s: &str| -> BTreeSet<String> {
            s.split('{')
                .skip(1)
                .filter_map(|p| p.split_once('}').map(|(k, _)| k.to_string()))
                .collect()
        };
        for (k, v) in &it {
            assert_eq!(placeholders(v), placeholders(&en[k]), "placeholders of {k}");
        }
    }

    #[test]
    fn i18n_keys_used_by_the_ui_exist() {
        let dict = dictionary("it");
        let mut used = BTreeSet::new();
        for (file, src) in USERS {
            for (key, plural) in call_keys(file, src, true) {
                let keys = if plural {
                    vec![format!("{key}.one"), format!("{key}.other")]
                } else {
                    vec![key]
                };
                for k in keys {
                    assert!(dict.contains_key(&k), "{file}: unknown key {k}");
                    used.insert(k);
                }
            }
            for key in attr_keys(src) {
                assert!(dict.contains_key(&key), "{file}: unknown key {key}");
                used.insert(key);
            }
        }
        // i18n.js itself uses a few keys (durations, the built-in waiting text).
        for (key, _) in call_keys("i18n.js", I18N, false) {
            used.insert(key);
        }
        used.insert("waiting.builtin".into());
        let unused: Vec<_> = dict.keys().filter(|k| !used.contains(*k)).collect();
        assert!(unused.is_empty(), "unused keys: {unused:?}");
        // The player is an ES module importing i18n.js; the asset is served.
        assert!(
            USERS[1]
                .1
                .contains(r#"<script type="module" src="/static/player.js">"#)
        );
        assert!(asset("i18n.js").is_some_and(|(ct, _)| ct.starts_with("text/javascript")));
    }

    async fn page(state: &AppState, uri: &str) -> (StatusCode, String) {
        let app = router(state).with_state(state.clone());
        let resp = app
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    #[tokio::test]
    async fn pages_follow_the_ui_language() {
        let (state, dir) = test_state(false).await;
        let lang = |body: &str| {
            body.split_once("<html lang=\"")
                .and_then(|(_, r)| r.split_once('"'))
                .map(|(l, _)| l.to_string())
                .unwrap()
        };
        let (st, body) = page(&state, "/").await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(lang(&body), "it");
        assert_eq!(lang(&page(&state, "/embed/show").await.1), "it");

        settings::set(&state.db, settings::UI_LANGUAGE, "en")
            .await
            .unwrap();
        assert_eq!(lang(&page(&state, "/").await.1), "en");
        let (st, body) = page(&state, "/embed/show?autoplay=0").await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(lang(&body), "en");
        assert!(body.contains(r#"data-name="show""#));
        // ?lang= overrides the setting (case-insensitive); invalid values are ignored.
        assert_eq!(lang(&page(&state, "/embed/show?lang=it").await.1), "it");
        assert_eq!(lang(&page(&state, "/player/show?lang=IT").await.1), "it");
        assert_eq!(lang(&page(&state, "/player/show?lang=fr").await.1), "en");
        assert_eq!(
            lang(&page(&state, "/player/show?lang=it&lang=en").await.1),
            "en"
        );
        let (st, _) = page(&state, "/embed/Bad%20Name").await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        let (st, js) = page(&state, "/static/i18n.js").await;
        assert_eq!(st, StatusCode::OK);
        assert!(js.contains("export function t("));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
