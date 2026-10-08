//! Shared plumbing of the LL-HLS (`hls.rs`) and LL-DASH (`dash.rs`) routes:
//! broadcast/package resolution with the §6.3 status codes, response builders and
//! viewer tracking.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures::Stream;
use serde_json::json;

use crate::db::{self, broadcasts::is_valid_name};
use crate::error::ApiError;
use crate::metrics::{Protocol, client_ip, user_agent};
use crate::packager::StreamPackage;
use crate::state::AppState;

/// How long a playlist/MPD request waits for the first segment of a just-started
/// stream before answering 503.
pub(crate) const STARTUP_WAIT: Duration = Duration::from_secs(8);

/// Error of the HLS/DASH handlers: an [`ApiError`] or 415 for unsupported codecs.
#[derive(Debug)]
pub(crate) enum EgressError {
    Api(ApiError),
    Unsupported(String),
}

impl From<ApiError> for EgressError {
    fn from(e: ApiError) -> Self {
        EgressError::Api(e)
    }
}

impl From<crate::db::DbError> for EgressError {
    fn from(e: crate::db::DbError) -> Self {
        EgressError::Api(e.into())
    }
}

impl IntoResponse for EgressError {
    fn into_response(self) -> Response {
        match self {
            EgressError::Api(e) => e.into_response(),
            EgressError::Unsupported(reason) => {
                let mut r = (
                    StatusCode::UNSUPPORTED_MEDIA_TYPE,
                    Json(json!({ "error": reason })),
                )
                    .into_response();
                r.headers_mut()
                    .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
                r
            }
        }
    }
}

pub(crate) type EgressResult<T> = Result<T, EgressError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Proto {
    Hls,
    Dash,
}

impl Proto {
    fn label(self) -> &'static str {
        match self {
            Proto::Hls => "HLS",
            Proto::Dash => "DASH",
        }
    }

    pub(crate) fn protocol(self) -> Protocol {
        match self {
            Proto::Hls => Protocol::Hls,
            Proto::Dash => Protocol::Dash,
        }
    }
}

/// Resolve `name` to its package: 404 unknown broadcast or protocol disabled on the
/// server, 403 protocol disabled for the broadcast, 404 `stream offline` when nothing
/// is live or lingering, 415 when the codecs cannot be packaged.
pub(crate) async fn resolve(
    state: &AppState,
    name: &str,
    proto: Proto,
) -> EgressResult<Arc<StreamPackage>> {
    if !is_valid_name(name) {
        return Err(ApiError::broadcast_not_found().into());
    }
    let server_enabled = match proto {
        Proto::Hls => state.config.hls.enabled,
        Proto::Dash => state.config.dash.enabled,
    };
    if !server_enabled {
        return Err(
            ApiError::not_found(format!("{} is disabled on this server", proto.label())).into(),
        );
    }
    let pkg = state.packager.get(name);
    let (enable_hls, enable_dash) = match pkg.as_ref().and_then(|p| p.cached_flags()) {
        Some(flags) => flags,
        None => {
            let b = db::broadcasts::find_by_name(&state.db, name)
                .await?
                .ok_or_else(ApiError::broadcast_not_found)?;
            if let Some(p) = &pkg {
                p.set_flags(b.enable_hls, b.enable_dash);
            }
            (b.enable_hls, b.enable_dash)
        }
    };
    let enabled = match proto {
        Proto::Hls => enable_hls,
        Proto::Dash => enable_dash,
    };
    if !enabled {
        return Err(ApiError::forbidden(format!(
            "{} playback is disabled for this broadcast",
            proto.label()
        ))
        .into());
    }
    let pkg = pkg.ok_or_else(|| ApiError::not_found("stream offline"))?;
    if let Some(reason) = pkg.unsupported_reason() {
        return Err(EgressError::Unsupported(reason));
    }
    Ok(pkg)
}

/// Register a request-based viewer session.
pub(crate) fn touch(
    state: &AppState,
    name: &str,
    proto: Proto,
    peer: SocketAddr,
    headers: &HeaderMap,
) {
    let ip = client_ip(headers, peer, state.config.server.trust_proxy);
    state
        .tracker
        .touch(name, proto.protocol(), ip, &user_agent(headers));
}

fn common_headers(resp: &mut Response, content_type: &'static str, cache: &'static str) {
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    h.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
}

/// Playlist / MPD / text response (`Cache-Control: no-cache`).
pub(crate) fn text_response(body: String, content_type: &'static str) -> Response {
    let mut r = Response::new(Body::from(body));
    common_headers(&mut r, content_type, "no-cache");
    r
}

/// Complete media object. `immutable` → `max-age=3600` (segment/part URLs are unique
/// per publishing session), otherwise `no-cache` (init segments).
pub(crate) fn bytes_response(body: Bytes, content_type: &'static str, immutable: bool) -> Response {
    let mut r = Response::new(Body::from(body));
    let cache = if immutable {
        "max-age=3600"
    } else {
        "no-cache"
    };
    common_headers(&mut r, content_type, cache);
    r
}

/// In-progress segment delivered with chunked transfer, one CMAF chunk at a time.
pub(crate) fn streaming_response<S>(stream: S, content_type: &'static str) -> Response
where
    S: Stream<Item = Bytes> + Send + 'static,
{
    use futures::StreamExt;
    let body = Body::from_stream(stream.map(Ok::<Bytes, std::io::Error>));
    let mut r = Response::new(body);
    common_headers(&mut r, content_type, "no-cache");
    // Disable proxy buffering (nginx) so chunks flow immediately.
    r.headers_mut()
        .insert("x-accel-buffering", HeaderValue::from_static("no"));
    r
}

pub(crate) fn not_found(msg: &str) -> EgressError {
    ApiError::not_found(msg).into()
}

pub(crate) fn unavailable(msg: &str) -> EgressError {
    ApiError::unavailable(msg).into()
}

pub(crate) fn bad_request(msg: &str) -> EgressError {
    ApiError::bad_request(msg).into()
}
