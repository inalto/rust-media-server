//! Public VOD playback routes (CORS `*`, HTTP Range):
//!
//! - `GET /vod/{id}/hls/master.m3u8`, `/vod/{id}/hls/media_<n>.m3u8` and the segments
//!   `/vod/{id}/hls/{init-<n>,chunk-<n>-<k>}.m4s`;
//! - `GET /vod/{id}/dash/manifest.mpd` and the same segments under `/vod/{id}/dash/`;
//! - `GET /vod/{id}/poster.jpg`.
//!
//! Both prefixes serve `data/vod/<id>/abr/`, so the relative URLs inside the manifests
//! stay valid. Segments and init segments: `Cache-Control: public, max-age=31536000,
//! immutable` (the manifests reference them with `?v=<revision>`); playlists, MPD and
//! poster: `public, max-age=60`. Every request touches the viewer tracker (name
//! `vod:<id>`, protocol HLS/DASH); a master/MPD request that opens a new session counts a
//! play. The master/MPD of an item that is not `ready` is a 404.
//!
//! **Private items** (phase 8A, docs/ACCOUNTS.md §5): every request needs a valid signed
//! token, from the query (`?token=&expires=`) or from the cookie `rms_play_<id>` that the
//! first accepted query sets (`Path=/vod/<id>/`, until the expiry); 401 otherwise. Their
//! playlists and MPD are rewritten so that every URI carries the token (players that do
//! not forward the page query, e.g. native HLS, keep working) and every response is
//! `Cache-Control: private` (playlists `no-store`). Public items ignore tokens.

use std::net::SocketAddr;

use axum::Router;
use axum::body::Body;
use axum::extract::{ConnectInfo, Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use tower::ServiceExt;
use tower_http::cors::CorsLayer;
use tower_http::services::ServeFile;

use super::parse_id;
use crate::api::BaseUrl;
use crate::db::vod as repo;
use crate::error::ApiError;
use crate::metrics::{Protocol, client_ip, user_agent};
use crate::state::AppState;
use crate::storage::sign::{self, SignedToken};

pub const CACHE_IMMUTABLE: &str = "public, max-age=31536000, immutable";
pub const CACHE_SHORT: &str = "public, max-age=60";
/// Private items: segments and poster stay in the viewer's cache only.
pub const CACHE_PRIVATE_IMMUTABLE: &str = "private, max-age=31536000, immutable";
pub const CACHE_PRIVATE_SHORT: &str = "private, max-age=60";
/// Private playlists embed the token: never stored.
pub const CACHE_PRIVATE_NO_STORE: &str = "private, no-store";

/// Outcome of the access check of a playback request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Grant {
    /// Public item (or an unknown id: the file lookup answers 404).
    Public,
    /// Private item with a valid token (`from_query`: set the cookie).
    Signed {
        token: SignedToken,
        from_query: bool,
    },
}

/// Check the signed token of a request for video `id` (see the module docs).
pub async fn authorize(
    state: &AppState,
    id: &str,
    headers: &HeaderMap,
    query: Option<&str>,
) -> Result<Grant, ApiError> {
    let Some(access) = state.vod.playback_access(id).await?.filter(|a| a.private) else {
        return Ok(Grant::Public);
    };
    let key = access.signing_key.unwrap_or_default();
    let now = chrono::Utc::now().timestamp();
    if let Some(t) = SignedToken::from_query(query)
        && sign::verify(&key, id, &t.token, t.expires, now)
    {
        return Ok(Grant::Signed {
            token: t,
            from_query: true,
        });
    }
    if let Some(t) = crate::api::principal::cookie(headers, &sign::cookie_name(id))
        .as_deref()
        .and_then(SignedToken::from_cookie_value)
        && sign::verify(&key, id, &t.token, t.expires, now)
    {
        return Ok(Grant::Signed {
            token: t,
            from_query: false,
        });
    }
    Err(ApiError::TokenRequired(
        "this video is private: a valid signed link (token, expires) is required".into(),
    ))
}

/// Private responses: `Cache-Control` and, for a token taken from the query, the
/// playback cookie.
fn finish(resp: &mut Response, grant: &Grant, id: &str, private_cache: &'static str, https: bool) {
    let Grant::Signed { token, from_query } = grant else {
        return;
    };
    if !(resp.status().is_success() || resp.status().as_u16() == 304) {
        return;
    }
    let h = resp.headers_mut();
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(private_cache),
    );
    if *from_query {
        let max_age = (token.expires - chrono::Utc::now().timestamp()).max(0);
        let mut c = format!(
            "{}={}; Path=/vod/{id}/; HttpOnly; SameSite=Lax; Max-Age={max_age}",
            sign::cookie_name(id),
            token.cookie_value()
        );
        if https {
            c.push_str("; Secure");
        }
        if let Ok(v) = HeaderValue::from_str(&c) {
            h.append(header::SET_COOKIE, v);
        }
    }
}

/// A playlist/MPD of a private item with the token appended to every URI.
async fn rewritten(
    path: std::path::PathBuf,
    kind: Kind,
    content_type: &'static str,
    query: &str,
) -> Response {
    let Ok(text) = tokio::fs::read_to_string(&path).await else {
        return ApiError::not_found("file not found").into_response();
    };
    let body = match kind {
        Kind::Hls => sign::rewrite_hls(&text, query),
        Kind::Dash => sign::rewrite_mpd(&text, query),
    };
    let mut resp = body.into_response();
    resp.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    resp
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/vod/{id}/poster.jpg", get(poster))
        .route("/vod/{id}/hls/{file}", get(hls_file))
        .route("/vod/{id}/dash/{file}", get(dash_file))
        .layer(CorsLayer::permissive())
}

/// Tracker name of a video (never a valid broadcast name, so the live metrics ignore it).
pub fn tracker_name(id: &str) -> String {
    format!("vod:{id}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Hls,
    Dash,
}

/// What a requested file is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileClass {
    /// `master.m3u8` / `manifest.mpd`: needs a `ready` item, counts plays.
    Entry,
    /// HLS media playlist.
    Playlist,
    InitSegment,
    Segment,
}

/// Classify `file` for `kind`; `None` for anything that is not served.
pub fn classify(kind: Kind, file: &str) -> Option<FileClass> {
    let safe = !file.is_empty()
        && file.len() <= 64
        && !file.starts_with('.')
        && file
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'));
    if !safe {
        return None;
    }
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    match (kind, file) {
        (Kind::Hls, "master.m3u8") | (Kind::Dash, "manifest.mpd") => Some(FileClass::Entry),
        (Kind::Hls, f)
            if f.strip_prefix("media_")
                .and_then(|r| r.strip_suffix(".m3u8"))
                .is_some_and(digits) =>
        {
            Some(FileClass::Playlist)
        }
        (_, f)
            if f.strip_prefix("init-")
                .and_then(|r| r.strip_suffix(".m4s"))
                .is_some_and(digits) =>
        {
            Some(FileClass::InitSegment)
        }
        (_, f) => {
            let rest = f.strip_prefix("chunk-")?.strip_suffix(".m4s")?;
            let (rep, num) = rest.split_once('-')?;
            (digits(rep) && digits(num)).then_some(FileClass::Segment)
        }
    }
}

fn content_type(kind: Kind, class: FileClass) -> &'static str {
    match (kind, class) {
        (Kind::Hls, FileClass::Entry | FileClass::Playlist) => "application/vnd.apple.mpegurl",
        (Kind::Dash, FileClass::Entry | FileClass::Playlist) => "application/dash+xml",
        (_, FileClass::InitSegment) => "video/mp4",
        (_, FileClass::Segment) => "video/iso.segment",
    }
}

/// Serve `path` with Range support, `content_type` and `cache`.
async fn send_file(
    path: std::path::PathBuf,
    req: Request,
    content_type: &'static str,
    cache: &'static str,
) -> Response {
    if !path.is_file() {
        return ApiError::not_found("file not found").into_response();
    }
    let resp = match ServeFile::new(&path).oneshot(req).await {
        Ok(r) => r,
        Err(never) => match never {},
    };
    let mut resp = resp.map(Body::new);
    if resp.status().is_success() || resp.status().as_u16() == 304 {
        let h = resp.headers_mut();
        h.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    }
    resp
}

async fn serve(
    state: AppState,
    id: String,
    kind: Kind,
    file: String,
    peer: SocketAddr,
    headers: HeaderMap,
    req: Request,
) -> Response {
    let Some(id) = parse_id(&id) else {
        return ApiError::not_found("video not found").into_response();
    };
    let Some(class) = classify(kind, &file) else {
        return ApiError::not_found("file not found").into_response();
    };
    let grant = match authorize(&state, &id, &headers, req.uri().query()).await {
        Ok(g) => g,
        Err(e) => return e.into_response(),
    };
    if class == FileClass::Entry {
        match repo::get(&state.db, &id).await {
            Ok(Some(item)) if item.is_ready() => {}
            Ok(Some(_)) => return ApiError::not_found("video not ready").into_response(),
            Ok(None) => return ApiError::not_found("video not found").into_response(),
            Err(e) => return ApiError::from(e).into_response(),
        }
    }
    let protocol = match kind {
        Kind::Hls => Protocol::Hls,
        Kind::Dash => Protocol::Dash,
    };
    let ip = client_ip(&headers, peer, state.config.server.trust_proxy);
    let new_session =
        state
            .tracker
            .touch_is_new(&tracker_name(&id), protocol, ip, &user_agent(&headers));
    if new_session && class == FileClass::Entry {
        match repo::increment_plays(&state.db, &id).await {
            Ok(Some(_)) => state.vod.changed(&id),
            Ok(None) => {}
            Err(e) => tracing::warn!(id = %id, error = %e, "cannot count a VOD play"),
        }
    }
    let (cache, private_cache) = match class {
        FileClass::Entry | FileClass::Playlist => (CACHE_SHORT, CACHE_PRIVATE_NO_STORE),
        FileClass::InitSegment | FileClass::Segment => (CACHE_IMMUTABLE, CACHE_PRIVATE_IMMUTABLE),
    };
    let https = BaseUrl::from_request(&state.config, &headers).https;
    let path = state.vod.abr_dir(&id).join(&file);
    let mut resp = match (&grant, class) {
        (Grant::Signed { token, .. }, FileClass::Entry | FileClass::Playlist) => {
            rewritten(path, kind, content_type(kind, class), &token.query()).await
        }
        _ => send_file(path, req, content_type(kind, class), cache).await,
    };
    finish(&mut resp, &grant, &id, private_cache, https);
    resp
}

async fn hls_file(
    State(state): State<AppState>,
    Path((id, file)): Path<(String, String)>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    req: Request,
) -> Response {
    serve(state, id, Kind::Hls, file, peer, headers, req).await
}

async fn dash_file(
    State(state): State<AppState>,
    Path((id, file)): Path<(String, String)>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    req: Request,
) -> Response {
    serve(state, id, Kind::Dash, file, peer, headers, req).await
}

async fn poster(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    req: Request,
) -> Response {
    let Some(id) = parse_id(&id) else {
        return ApiError::not_found("video not found").into_response();
    };
    let grant = match authorize(&state, &id, &headers, req.uri().query()).await {
        Ok(g) => g,
        Err(e) => return e.into_response(),
    };
    let https = BaseUrl::from_request(&state.config, &headers).https;
    let mut resp = send_file(state.vod.poster_path(&id), req, "image/jpeg", CACHE_SHORT).await;
    finish(&mut resp, &grant, &id, CACHE_PRIVATE_SHORT, https);
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_classification() {
        use FileClass::*;
        assert_eq!(classify(Kind::Hls, "master.m3u8"), Some(Entry));
        assert_eq!(classify(Kind::Dash, "manifest.mpd"), Some(Entry));
        assert_eq!(classify(Kind::Hls, "manifest.mpd"), None);
        assert_eq!(classify(Kind::Dash, "master.m3u8"), None);
        assert_eq!(classify(Kind::Hls, "media_3.m3u8"), Some(Playlist));
        assert_eq!(classify(Kind::Dash, "media_3.m3u8"), None);
        assert_eq!(classify(Kind::Hls, "init-0.m4s"), Some(InitSegment));
        assert_eq!(classify(Kind::Dash, "chunk-2-00017.m4s"), Some(Segment));
        for bad in [
            "",
            "../master.m3u8",
            ".hidden",
            "chunk-a-1.m4s",
            "chunk-1.m4s",
            "init-.m4s",
            "media_x.m3u8",
            "source.mp4",
            "poster.jpg",
            "chunk-1-2.m4s/x",
        ] {
            assert_eq!(classify(Kind::Hls, bad), None, "{bad}");
        }
        assert_eq!(
            content_type(Kind::Hls, Entry),
            "application/vnd.apple.mpegurl"
        );
        assert_eq!(content_type(Kind::Dash, Entry), "application/dash+xml");
        assert_eq!(content_type(Kind::Dash, Segment), "video/iso.segment");
        assert_eq!(tracker_name("x"), "vod:x");
    }
}
