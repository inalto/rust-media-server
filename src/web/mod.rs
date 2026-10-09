//! Pages and static assets (ARCHITECTURE.md §8.3).
//!
//! - `/` dashboard SPA (`static/index.html` + ES modules `app.js`, `core.js`,
//!   `detail.js`, `metrics.js`, `mock.js`, `vod.js`, `i18n.js`, `theme.js` and
//!   `style.css`), Italian or English UI (`ui_language` setting, ARCHITECTURE.md §17), dark
//!   or light theme (`ui_theme`, §18). Role-aware (§23): the admin pages plus `users.js`
//!   (admin "Utenti"), or the user portal `portal.js` (buckets, API keys, account) with
//!   the bucket browser `browser.js`; `accounts.js` holds their shared dialogs.
//! - `/embed/{name}` player page for iframes, `/player/{name}` full-page player
//!   (same `embed.html`, `data-mode` differs; ES module `player.js` + `i18n.js` +
//!   `theme.js`).
//! - `/embed/vod/{id}` video on demand player for iframes, `/watch/{id}` full page with
//!   title and description (same `vod.html`; `player.js` runs its VOD mode,
//!   `vod-player.js`), `/vod` public catalogue of the ready videos (`catalog.html` +
//!   `catalog.js`). ARCHITECTURE.md §19.
//! - `/static/*` assets embedded in the binary with `include_str!`.
//! - `/waiting/*` files from `data/waiting/` (waiting videos and posters).
//!
//! The pages get `<html lang>` from the `ui_language` setting and `<html data-theme>` from
//! `ui_theme` (one small SQLite read per page); `?lang=it|en` and
//! `?theme=system|light|dark` on the player pages override them. An inline script in the
//! `<head>` of both pages applies the per-browser override (dashboard, `localStorage`
//! `rms.theme`) or `?theme=` before the first paint.

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
use crate::db::settings::{self, UiLanguage, UiTheme};
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
        "theme.js",
        "text/javascript; charset=utf-8",
        include_str!("static/theme.js"),
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
    (
        "vod.html",
        "text/html; charset=utf-8",
        include_str!("static/vod.html"),
    ),
    (
        "vod-player.js",
        "text/javascript; charset=utf-8",
        include_str!("static/vod-player.js"),
    ),
    (
        "catalog.html",
        "text/html; charset=utf-8",
        include_str!("static/catalog.html"),
    ),
    (
        "catalog.js",
        "text/javascript; charset=utf-8",
        include_str!("static/catalog.js"),
    ),
    (
        "vod.js",
        "text/javascript; charset=utf-8",
        include_str!("static/vod.js"),
    ),
    (
        "accounts.js",
        "text/javascript; charset=utf-8",
        include_str!("static/accounts.js"),
    ),
    (
        "browser.js",
        "text/javascript; charset=utf-8",
        include_str!("static/browser.js"),
    ),
    (
        "users.js",
        "text/javascript; charset=utf-8",
        include_str!("static/users.js"),
    ),
    (
        "portal.js",
        "text/javascript; charset=utf-8",
        include_str!("static/portal.js"),
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
        .route("/embed/vod/{id}", get(vod_embed_page))
        .route("/watch/{id}", get(vod_watch_page))
        .route("/vod", get(catalog_page))
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

/// The `<html>` tag every HTML template starts with.
const HTML_TAG: &str = "<html lang=\"it\" data-theme=\"system\">";

/// `template` with `<html lang>` set to `lang` and `<html data-theme>` to `theme`.
fn with_prefs(template: &str, lang: UiLanguage, theme: UiTheme) -> String {
    template.replacen(
        HTML_TAG,
        &format!(
            "<html lang=\"{}\" data-theme=\"{}\">",
            lang.as_str(),
            theme.as_str()
        ),
        1,
    )
}

/// UI language and default theme settings (defaults if they cannot be read).
async fn ui_prefs(state: &AppState) -> (UiLanguage, UiTheme) {
    match settings::ui_prefs(&state.db).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "cannot read the UI language/theme settings");
            Default::default()
        }
    }
}

async fn index(State(state): State<AppState>) -> Response {
    let template = asset("index.html").map(|(_, b)| b).unwrap_or_default();
    let (lang, theme) = ui_prefs(&state).await;
    html_response(with_prefs(template, lang, theme))
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
    /// `system` | `light` | `dark` (case-insensitive): overrides the `ui_theme` setting.
    theme: Option<String>,
}

async fn player_html(state: &AppState, name: &str, mode: &str, query: PlayerQuery) -> Response {
    if !is_valid_name(name) {
        return (StatusCode::NOT_FOUND, "broadcast not found").into_response();
    }
    let (lang, theme) = page_prefs(state, query).await;
    let template = asset("embed.html").map(|(_, b)| b).unwrap_or_default();
    // `name` is a validated slug: safe to inline.
    let body = with_prefs(template, lang, theme)
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

/// `<html lang data-theme>` from `?lang=` / `?theme=`, else the settings.
async fn page_prefs(state: &AppState, query: PlayerQuery) -> (UiLanguage, UiTheme) {
    let (setting_lang, setting_theme) = ui_prefs(state).await;
    let lang = query
        .lang
        .and_then(|l| UiLanguage::parse(&l.trim().to_ascii_lowercase()))
        .unwrap_or(setting_lang);
    let theme = query
        .theme
        .and_then(|t| UiTheme::parse(&t.trim().to_ascii_lowercase()))
        .unwrap_or(setting_theme);
    (lang, theme)
}

/// VOD player page (`vod.html`): `mode` is `embed` or `watch`. Unknown ids still get the
/// page (the player shows "not found"), malformed ones a 404.
async fn vod_html(state: &AppState, id: &str, mode: &str, query: PlayerQuery) -> Response {
    let Some(id) = crate::vod::parse_id(id) else {
        return (StatusCode::NOT_FOUND, "video not found").into_response();
    };
    let (lang, theme) = page_prefs(state, query).await;
    let template = asset("vod.html").map(|(_, b)| b).unwrap_or_default();
    // `id` is a canonical UUID: safe to inline.
    let body = with_prefs(template, lang, theme)
        .replace("data-mode=\"embed\"", &format!("data-mode=\"{mode}\""))
        .replace("{{ID}}", &id);
    html_response(body)
}

async fn vod_embed_page(
    State(state): State<AppState>,
    Path(id): Path<String>,
    uri: Uri,
) -> Response {
    vod_html(&state, &id, "embed", player_query(&uri)).await
}

async fn vod_watch_page(
    State(state): State<AppState>,
    Path(id): Path<String>,
    uri: Uri,
) -> Response {
    vod_html(&state, &id, "watch", player_query(&uri)).await
}

/// `GET /vod`: public catalogue page.
async fn catalog_page(State(state): State<AppState>, uri: Uri) -> Response {
    let (lang, theme) = page_prefs(&state, player_query(&uri)).await;
    let template = asset("catalog.html").map(|(_, b)| b).unwrap_or_default();
    html_response(with_prefs(template, lang, theme))
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
        ("vod.html", include_str!("static/vod.html")),
        ("vod-player.js", include_str!("static/vod-player.js")),
        ("catalog.html", include_str!("static/catalog.html")),
        ("catalog.js", include_str!("static/catalog.js")),
        ("vod.js", include_str!("static/vod.js")),
        ("accounts.js", include_str!("static/accounts.js")),
        ("browser.js", include_str!("static/browser.js")),
        ("users.js", include_str!("static/users.js")),
        ("portal.js", include_str!("static/portal.js")),
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
    async fn pages_follow_the_ui_theme() {
        let (state, dir) = test_state(false).await;
        let attr = |body: &str, name: &str| {
            // The tag itself, not the `<html lang>` mentioned in comments.
            let tag = &body[body.find("<html lang=\"").unwrap()..];
            let tag = &tag[..tag.find('>').unwrap()];
            let key = format!(" {name}=\"");
            let v = &tag[tag
                .find(&key)
                .unwrap_or_else(|| panic!("no {name} in {tag}"))
                + key.len()..];
            v[..v.find('"').unwrap()].to_string()
        };
        // Every template starts with the tag the server rewrites.
        for page in ["index.html", "embed.html", "vod.html", "catalog.html"] {
            assert!(asset(page).unwrap().1.contains(HTML_TAG), "{page}");
        }
        let (st, body) = page(&state, "/").await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(attr(&body, "data-theme"), "system");
        assert_eq!(
            attr(&page(&state, "/embed/show").await.1, "data-theme"),
            "system"
        );

        settings::set(&state.db, settings::UI_THEME, "light")
            .await
            .unwrap();
        let body = page(&state, "/").await.1;
        assert_eq!(
            (attr(&body, "lang"), attr(&body, "data-theme")),
            ("it".into(), "light".into())
        );
        assert_eq!(
            attr(&page(&state, "/player/show").await.1, "data-theme"),
            "light"
        );
        // ?theme= overrides the setting (case-insensitive); invalid values are ignored;
        // ?lang= is independent.
        for (query, want) in [
            ("theme=dark", "dark"),
            ("theme=DARK", "dark"),
            ("theme=%20System%20", "system"),
            ("theme=blue", "light"),
            ("theme=", "light"),
            ("lang=en&theme=dark&autoplay=0", "dark"),
        ] {
            let (st, body) = page(&state, &format!("/embed/show?{query}")).await;
            assert_eq!(st, StatusCode::OK);
            assert_eq!(attr(&body, "data-theme"), want, "{query}");
        }
        let body = page(&state, "/player/show?lang=en&theme=dark").await.1;
        assert_eq!(
            (attr(&body, "lang"), attr(&body, "data-theme")),
            ("en".into(), "dark".into())
        );
        // An invalid stored value means `system`.
        settings::set(&state.db, settings::UI_THEME, "blue")
            .await
            .unwrap();
        assert_eq!(attr(&page(&state, "/").await.1, "data-theme"), "system");
        // Both pages support both schemes and apply the override before the first paint.
        for (page_name, needle) in [
            ("index.html", "localStorage.getItem('rms.theme')"),
            ("embed.html", "get('theme')"),
            ("vod.html", "get('theme')"),
            ("catalog.html", "get('theme')"),
        ] {
            let html = asset(page_name).unwrap().1;
            assert!(
                html.contains(r#"<meta name="color-scheme" content="light dark">"#),
                "{page_name}"
            );
            let head = &html[..html.find("</head>").unwrap()];
            let script = head.find(needle).expect("inline theme script in <head>");
            assert!(
                script < head.find("<link rel=\"stylesheet\"").unwrap(),
                "{page_name}"
            );
        }
        let (st, js) = page(&state, "/static/theme.js").await;
        assert_eq!(st, StatusCode::OK);
        assert!(js.contains("export function initTheme("));
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Stylesheets checked by [`css_colours_are_theme_tokens`].
    const STYLESHEETS: &[(&str, &str)] = &[
        ("style.css", include_str!("static/style.css")),
        ("player.css", include_str!("static/player.css")),
    ];

    /// Colour literals allowed outside the theme token blocks: `(file, literal)`. Keep it
    /// empty unless a colour truly must not change with the theme *and* cannot be a token
    /// (document why here).
    const COLOUR_EXCEPTIONS: &[(&str, &str)] = &[];

    /// A style rule: enclosing at-rule preludes, selector, declarations.
    struct CssRule {
        at: Vec<String>,
        selector: String,
        decls: Vec<(String, String)>,
    }

    /// Minimal CSS parser (comments stripped, nested at-rules, no nested style rules).
    fn css_rules(src: &str) -> Vec<CssRule> {
        let mut text = String::new();
        let mut rest = src;
        while let Some(i) = rest.find("/*") {
            text.push_str(&rest[..i]);
            rest = &rest[i + 2..];
            rest = &rest[rest.find("*/").expect("unterminated comment") + 2..];
        }
        text.push_str(rest);
        let mut rules = Vec::new();
        let mut at: Vec<String> = Vec::new();
        let mut prelude = String::new();
        let mut chars = text.chars();
        while let Some(c) = chars.next() {
            match c {
                '{' => {
                    let p = prelude.trim().to_string();
                    prelude.clear();
                    if p.starts_with('@') {
                        at.push(p);
                        continue;
                    }
                    let mut body = String::new();
                    for c in chars.by_ref() {
                        if c == '}' {
                            break;
                        }
                        assert_ne!(c, '{', "nested rule in {p}");
                        body.push(c);
                    }
                    let decls = body
                        .split(';')
                        .map(str::trim)
                        .filter(|d| !d.is_empty())
                        .map(|d| {
                            let (k, v) = d
                                .split_once(':')
                                .unwrap_or_else(|| panic!("bad declaration {d}"));
                            (
                                k.trim().to_string(),
                                v.split_whitespace().collect::<Vec<_>>().join(" "),
                            )
                        })
                        .collect();
                    rules.push(CssRule {
                        at: at.clone(),
                        selector: p,
                        decls,
                    });
                }
                '}' => {
                    assert!(at.pop().is_some(), "unbalanced }}");
                    prelude.clear();
                }
                ';' if prelude.trim_start().starts_with('@') => prelude.clear(),
                _ => prelude.push(c),
            }
        }
        assert!(at.is_empty(), "unclosed at-rule");
        rules
    }

    /// Colour literals in a declaration value: hex, rgb[a](), hsl[a](), white/black.
    fn colour_literals(value: &str) -> Vec<String> {
        let mut out = Vec::new();
        let lower = value.to_ascii_lowercase();
        let b = lower.as_bytes();
        for (i, _) in lower.match_indices('#') {
            let n = b[i + 1..]
                .iter()
                .take_while(|c| c.is_ascii_hexdigit())
                .count();
            let ident_follows = b
                .get(i + 1 + n)
                .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'-');
            if matches!(n, 3 | 4 | 6 | 8) && !ident_follows {
                out.push(lower[i..i + 1 + n].to_string());
            }
        }
        for f in [
            "rgb(", "rgba(", "hsl(", "hsla(", "hwb(", "lab(", "lch(", "oklab(", "oklch(", "color(",
        ] {
            for (i, _) in lower.match_indices(f) {
                if i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'-') {
                    out.push(f.to_string());
                }
            }
        }
        for word in lower.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-')) {
            if matches!(word, "white" | "black") {
                out.push(word.to_string());
            }
        }
        out
    }

    /// Theme token block: only `:root…` selectors (no descendants), only custom
    /// properties and `color-scheme`.
    fn is_token_block(r: &CssRule) -> bool {
        r.selector
            .split(',')
            .map(str::trim)
            .all(|s| s.starts_with(":root") && !s.contains(' '))
            && r.decls
                .iter()
                .all(|(k, _)| k.starts_with("--") || k == "color-scheme")
    }

    #[test]
    fn css_colours_are_theme_tokens() {
        for (file, src) in STYLESHEETS {
            let rules = css_rules(src);
            assert!(
                rules.len() > 20,
                "{file}: only {} rules parsed",
                rules.len()
            );
            // 1. No colour literal outside the token blocks.
            let mut offending = Vec::new();
            for r in rules.iter().filter(|r| !is_token_block(r)) {
                for (k, v) in &r.decls {
                    for lit in colour_literals(v) {
                        if !COLOUR_EXCEPTIONS.contains(&(file, lit.as_str())) {
                            offending.push(format!("{} {{ {k}: {v} }} ({lit})", r.selector));
                        }
                    }
                }
            }
            assert!(
                offending.is_empty(),
                "{file}: colour literals outside the theme tokens:\n{}",
                offending.join("\n")
            );

            // 2. Dark tokens on `:root`, a complete light palette twice (explicit choice and
            //    `system` with a light OS preference), identical in both places.
            let scheme =
                |r: &&CssRule, v: &str| r.decls.iter().any(|(k, x)| k == "color-scheme" && x == v);
            let tokens = |r: &CssRule| -> BTreeMap<String, String> {
                r.decls
                    .iter()
                    .filter(|(k, _)| k.starts_with("--"))
                    .cloned()
                    .collect()
            };
            let dark: Vec<_> = rules.iter().filter(|r| scheme(r, "dark")).collect();
            assert_eq!(dark.len(), 1, "{file}: one dark block");
            assert!(
                dark[0].selector == ":root" && dark[0].at.is_empty(),
                "{file}: dark tokens on :root"
            );
            let explicit: Vec<_> = rules
                .iter()
                .filter(|r| scheme(r, "light") && r.at.is_empty())
                .collect();
            assert_eq!(explicit.len(), 1, "{file}: one explicit light block");
            assert_eq!(
                explicit[0].selector, r#":root[data-theme="light"]"#,
                "{file}"
            );
            let system: Vec<_> = rules
                .iter()
                .filter(|r| scheme(r, "light") && !r.at.is_empty())
                .collect();
            assert_eq!(
                system.len(),
                1,
                "{file}: one prefers-color-scheme light block"
            );
            assert_eq!(
                system[0].at,
                vec!["@media (prefers-color-scheme: light)".to_string()],
                "{file}"
            );
            assert_eq!(
                system[0].selector, r#":root:not([data-theme="dark"]):not([data-theme="light"])"#,
                "{file}"
            );
            let (dark, light) = (tokens(dark[0]), tokens(explicit[0]));
            assert_eq!(
                light,
                tokens(system[0]),
                "{file}: the two light blocks differ"
            );
            assert_eq!(
                dark.keys().collect::<Vec<_>>(),
                light.keys().collect::<Vec<_>>(),
                "{file}: dark and light define different tokens"
            );
            assert!(dark.len() > 20, "{file}: only {} tokens", dark.len());
            // 3. Every token used is defined (in some :root block).
            let defined: BTreeSet<String> = rules
                .iter()
                .filter(|r| is_token_block(r))
                .flat_map(|r| r.decls.iter().map(|(k, _)| k.clone()))
                .collect();
            for r in &rules {
                for (_, v) in &r.decls {
                    for (i, _) in v.match_indices("var(--") {
                        let name: String = v[i + 4..]
                            .chars()
                            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
                            .collect();
                        assert!(defined.contains(&name), "{file}: undefined token {name}");
                    }
                }
            }
        }
        // The checker itself finds literals.
        assert_eq!(
            colour_literals("0 0 0 3px rgba(1, 2, 3, .5), #fff solid white"),
            ["#fff", "rgba(", "white"]
        );
        assert!(colour_literals("var(--accent) #app-x").is_empty());
    }

    #[tokio::test]
    async fn vod_pages() {
        let (state, dir) = test_state(false).await;
        let attr = |body: &str, name: &str| {
            let tag = &body[body.find("<html lang=\"").unwrap()..];
            let tag = &tag[..tag.find('>').unwrap()];
            let key = format!(" {name}=\"");
            let v = &tag[tag.find(&key).unwrap() + key.len()..];
            v[..v.find('"').unwrap()].to_string()
        };
        let id = uuid::Uuid::new_v4().hyphenated().to_string();
        let (st, body) = page(&state, &format!("/embed/vod/{id}")).await;
        assert_eq!(st, StatusCode::OK);
        assert!(body.contains(&format!("data-id=\"{id}\"")), "{body}");
        assert!(body.contains("data-kind=\"vod\" data-mode=\"embed\""));
        assert!(body.contains(r#"<script type="module" src="/static/player.js">"#));
        // Ids are canonicalized (upper case accepted); garbage is a 404.
        let (st, body) = page(
            &state,
            &format!("/watch/{}?lang=en&theme=dark", id.to_uppercase()),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert!(
            body.contains(&format!("data-id=\"{id}\"")) && body.contains("data-mode=\"watch\"")
        );
        assert_eq!(
            (attr(&body, "lang"), attr(&body, "data-theme")),
            ("en".into(), "dark".into())
        );
        let (st, _) = page(&state, "/watch/not-a-uuid").await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        let (st, _) = page(&state, "/embed/vod/%3Cscript%3E").await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        // Catalogue follows the settings and ?lang= / ?theme=.
        settings::set(&state.db, settings::UI_LANGUAGE, "en")
            .await
            .unwrap();
        let (st, body) = page(&state, "/vod").await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(attr(&body, "lang"), "en");
        assert!(body.contains("/static/catalog.js"));
        let body = page(&state, "/vod?lang=it&theme=light").await.1;
        assert_eq!(
            (attr(&body, "lang"), attr(&body, "data-theme")),
            ("it".into(), "light".into())
        );
        // The VOD modules are served; player.js dispatches to the VOD mode.
        for js in ["vod-player.js", "catalog.js", "vod.js"] {
            let (st, _) = page(&state, &format!("/static/{js}")).await;
            assert_eq!(st, StatusCode::OK, "{js}");
        }
        let player = asset("player.js").unwrap().1;
        assert!(player.contains("import { startVodPlayer } from './vod-player.js'"));
        assert!(
            asset("vod-player.js")
                .unwrap()
                .1
                .contains("capLevelToPlayerSize: true")
        );
        std::fs::remove_dir_all(dir).unwrap();
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
