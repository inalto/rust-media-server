//! Pages and static assets (ARCHITECTURE.md §8.3).
//!
//! - `/` dashboard SPA (`static/index.html` + ES modules `app.js`, `core.js`,
//!   `detail.js`, `metrics.js`, `mock.js`, and `style.css`), Italian UI.
//! - `/embed/{name}` player page for iframes, `/player/{name}` full-page player
//!   (same `embed.html`, `data-mode` differs).
//! - `/static/*` assets embedded in the binary with `include_str!`.
//! - `/waiting/*` files from `data/waiting/` (waiting videos and posters).

use axum::Router;
use axum::extract::Path;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use sha2::{Digest, Sha256};
use tower_http::cors::CorsLayer;
use tower_http::services::ServeDir;

use crate::db::broadcasts::is_valid_name;
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

async fn index() -> Response {
    html_response(
        asset("index.html")
            .map(|(_, b)| b)
            .unwrap_or_default()
            .to_string(),
    )
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

fn player_html(name: &str, mode: &str) -> Response {
    if !is_valid_name(name) {
        return (StatusCode::NOT_FOUND, "broadcast not found").into_response();
    }
    let template = asset("embed.html").map(|(_, b)| b).unwrap_or_default();
    // `name` is a validated slug: safe to inline.
    let body = template
        .replace("data-mode=\"embed\"", &format!("data-mode=\"{mode}\""))
        .replace("{{NAME}}", name);
    html_response(body)
}

async fn embed_page(Path(name): Path<String>) -> Response {
    player_html(&name, "embed")
}

async fn player_page(Path(name): Path<String>) -> Response {
    player_html(&name, "player")
}
