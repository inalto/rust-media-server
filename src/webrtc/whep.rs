//! WHEP HTTP endpoints (`draft-ietf-wish-whep`):
//!
//! - `POST /whep/{name}` (`Content-Type: application/sdp`, body = offer) → `201 Created`,
//!   `Location: /whep/{name}/{session}`, body = SDP answer (ICE gathering is complete:
//!   host candidates on the shared UDP port, ICE-lite, no trickle needed).
//! - `DELETE /whep/{name}/{session}` → `200` (peer connection closed).
//! - `PATCH /whep/{name}/{session}` → `405` (trickle ICE / ICE restart not supported).
//! - `OPTIONS` on both → `204` (CORS preflight; `Link` ICE servers when configured).
//!
//! Errors are JSON `{"error": ...}`: 404 unknown broadcast or `stream offline`, 403 WebRTC
//! disabled (server or broadcast), 415 wrong content type or unsupported codec, 400
//! invalid SDP, 503 WebRTC unavailable / too many sessions / stream starting. Every
//! response carries permissive CORS headers exposing `Location`, `Link` and `ETag`.

use std::net::SocketAddr;

use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{ConnectInfo, Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde_json::json;

use crate::db::{self, broadcasts::is_valid_name};
use crate::error::ApiError;
use crate::metrics::{client_ip, user_agent};
use crate::state::AppState;

/// Largest accepted SDP offer.
pub const MAX_OFFER_BYTES: usize = 64 * 1024;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/whep/{name}", post(create).options(options_endpoint))
        .route(
            "/whep/{name}/{session}",
            axum::routing::delete(delete_session)
                .patch(patch_session)
                .options(options_session),
        )
        .layer(middleware::map_response(cors))
}

/// Error of the WHEP handlers: an [`ApiError`] or any status with a JSON message.
#[derive(Debug)]
pub enum WhepError {
    Api(ApiError),
    Status(StatusCode, String),
}

impl WhepError {
    pub fn status(code: StatusCode, msg: impl Into<String>) -> Self {
        WhepError::Status(code, msg.into())
    }
}

impl From<ApiError> for WhepError {
    fn from(e: ApiError) -> Self {
        WhepError::Api(e)
    }
}

impl From<crate::db::DbError> for WhepError {
    fn from(e: crate::db::DbError) -> Self {
        WhepError::Api(e.into())
    }
}

impl IntoResponse for WhepError {
    fn into_response(self) -> Response {
        match self {
            WhepError::Api(e) => e.into_response(),
            WhepError::Status(code, msg) => {
                let mut r = (code, Json(json!({ "error": msg }))).into_response();
                r.headers_mut()
                    .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
                r
            }
        }
    }
}

/// Permissive CORS on every WHEP response (players embedded on other sites).
async fn cors(mut resp: Response) -> Response {
    let h = resp.headers_mut();
    h.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    h.insert(
        header::ACCESS_CONTROL_EXPOSE_HEADERS,
        HeaderValue::from_static("Location, Link, ETag"),
    );
    resp
}

fn preflight(state: &AppState, allow: &'static str) -> Response {
    let mut r = StatusCode::NO_CONTENT.into_response();
    let h = r.headers_mut();
    h.insert(header::ALLOW, HeaderValue::from_static(allow));
    h.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static(allow),
    );
    h.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("Content-Type, Authorization, If-Match"),
    );
    h.insert(
        header::ACCESS_CONTROL_MAX_AGE,
        HeaderValue::from_static("86400"),
    );
    add_links(state, h);
    r
}

fn add_links(state: &AppState, h: &mut HeaderMap) {
    for link in state.webrtc.link_headers() {
        if let Ok(v) = HeaderValue::from_str(&link) {
            h.append(header::LINK, v);
        }
    }
}

async fn options_endpoint(State(state): State<AppState>) -> Response {
    preflight(&state, "POST, OPTIONS")
}

async fn options_session(State(state): State<AppState>) -> Response {
    preflight(&state, "DELETE, OPTIONS")
}

/// Trickle ICE and ICE restarts are not needed (ICE-lite, complete host candidates).
async fn patch_session() -> Response {
    let mut r = WhepError::status(
        StatusCode::METHOD_NOT_ALLOWED,
        "trickle ICE is not supported: the answer already contains every candidate",
    )
    .into_response();
    r.headers_mut()
        .insert(header::ALLOW, HeaderValue::from_static("DELETE, OPTIONS"));
    r
}

/// What a structurally valid offer contains.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfferSummary {
    pub video: bool,
    pub audio: bool,
    pub h264: bool,
}

/// Structural checks of a WHEP offer before it reaches the WebRTC stack: UTF-8 SDP
/// (`v=0`), size limit, at least one media section, ICE credentials and a DTLS
/// fingerprint (400 otherwise); a video section offering H.264 (415 otherwise).
pub fn validate_offer(body: &[u8]) -> Result<(String, OfferSummary), WhepError> {
    let bad =
        |m: &str| WhepError::status(StatusCode::BAD_REQUEST, format!("invalid SDP offer: {m}"));
    if body.is_empty() {
        return Err(bad("empty body"));
    }
    if body.len() > MAX_OFFER_BYTES {
        return Err(bad("too large"));
    }
    let text = std::str::from_utf8(body).map_err(|_| bad("not UTF-8"))?;
    if !text.trim_start().starts_with("v=0") {
        return Err(bad("must start with v=0"));
    }
    let mut summary = OfferSummary {
        video: false,
        audio: false,
        h264: false,
    };
    let (mut ufrag, mut pwd, mut fingerprint) = (false, false, false);
    let mut in_video = false;
    for line in text.lines().map(str::trim) {
        if let Some(m) = line.strip_prefix("m=") {
            in_video = m.starts_with("video ");
            summary.video |= in_video;
            summary.audio |= m.starts_with("audio ");
        } else if line.starts_with("a=ice-ufrag:") {
            ufrag = true;
        } else if line.starts_with("a=ice-pwd:") {
            pwd = true;
        } else if line.starts_with("a=fingerprint:") {
            fingerprint = true;
        } else if in_video
            && let Some(rtpmap) = line.strip_prefix("a=rtpmap:")
            && let Some((_, codec)) = rtpmap.split_once(' ')
            && codec.to_ascii_lowercase().starts_with("h264/90000")
        {
            summary.h264 = true;
        }
    }
    if !summary.video && !summary.audio {
        return Err(bad("no media section"));
    }
    if !(ufrag && pwd) {
        return Err(bad("missing ICE credentials"));
    }
    if !fingerprint {
        return Err(bad("missing DTLS fingerprint"));
    }
    if !summary.video {
        return Err(WhepError::status(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "the offer must receive video (H.264)",
        ));
    }
    if !summary.h264 {
        return Err(WhepError::status(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "the offer has no H.264 video codec",
        ));
    }
    Ok((text.to_string(), summary))
}

fn is_sdp(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("application/sdp"))
}

/// `POST /whep/{name}`.
async fn create(
    State(state): State<AppState>,
    Path(name): Path<String>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, WhepError> {
    if !state.config.webrtc.enabled {
        return Err(ApiError::forbidden("WebRTC is disabled on this server").into());
    }
    if !is_valid_name(&name) {
        return Err(ApiError::broadcast_not_found().into());
    }
    let broadcast = db::broadcasts::find_by_name(&state.db, &name)
        .await?
        .ok_or_else(ApiError::broadcast_not_found)?;
    if !broadcast.enable_webrtc {
        return Err(ApiError::forbidden("WebRTC playback is disabled for this broadcast").into());
    }
    if !is_sdp(&headers) {
        return Err(WhepError::status(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "expected Content-Type: application/sdp",
        ));
    }
    let (offer, _) = validate_offer(&body)?;
    let stream = state
        .hub
        .get(&broadcast.name)
        .ok_or_else(|| ApiError::not_found("stream offline"))?;
    if !state.webrtc.available() {
        return Err(WhepError::status(
            StatusCode::SERVICE_UNAVAILABLE,
            "WebRTC is not available on this server",
        ));
    }
    let ip = client_ip(&headers, peer, state.config.server.trust_proxy);
    let (id, answer) = state
        .webrtc
        .create_session(&broadcast.name, stream, offer, ip, user_agent(&headers))
        .await?;

    let mut r = (StatusCode::CREATED, answer).into_response();
    let h = r.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/sdp"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    if let Ok(v) = HeaderValue::from_str(&format!("/whep/{}/{id}", broadcast.name)) {
        h.insert(header::LOCATION, v);
    }
    if let Ok(v) = HeaderValue::from_str(&format!("\"{id}\"")) {
        h.insert(header::ETAG, v);
    }
    add_links(&state, h);
    Ok(r)
}

/// `DELETE /whep/{name}/{session}`.
async fn delete_session(
    State(state): State<AppState>,
    Path((name, session)): Path<(String, String)>,
) -> Result<StatusCode, WhepError> {
    if state.webrtc.delete_session(&name, &session) {
        Ok(StatusCode::OK)
    } else {
        Err(ApiError::not_found("session not found").into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) const OFFER: &str = "v=0\r\no=- 1 2 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\n\
        a=group:BUNDLE 0 1\r\n\
        m=video 9 UDP/TLS/RTP/SAVPF 102\r\nc=IN IP4 0.0.0.0\r\na=mid:0\r\na=recvonly\r\n\
        a=ice-ufrag:abcd\r\na=ice-pwd:aaaaaaaaaaaaaaaaaaaaaaaa\r\n\
        a=fingerprint:sha-256 AA:BB\r\na=setup:actpass\r\na=rtcp-mux\r\n\
        a=rtpmap:102 H264/90000\r\n\
        a=fmtp:102 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f\r\n\
        m=audio 9 UDP/TLS/RTP/SAVPF 111\r\nc=IN IP4 0.0.0.0\r\na=mid:1\r\na=recvonly\r\n\
        a=rtpmap:111 opus/48000/2\r\n";

    fn code(r: Result<(String, OfferSummary), WhepError>) -> StatusCode {
        match r {
            Ok(_) => StatusCode::OK,
            Err(WhepError::Status(c, _)) => c,
            Err(WhepError::Api(e)) => e.status(),
        }
    }

    #[test]
    fn offer_validation() {
        let (text, s) = validate_offer(OFFER.as_bytes()).unwrap();
        assert_eq!(text, OFFER);
        assert_eq!(
            s,
            OfferSummary {
                video: true,
                audio: true,
                h264: true
            }
        );
        assert_eq!(code(validate_offer(b"")), StatusCode::BAD_REQUEST);
        assert_eq!(code(validate_offer(b"garbage")), StatusCode::BAD_REQUEST);
        assert_eq!(
            code(validate_offer(&[0xff, 0xfe, 0x00])),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            code(validate_offer(&vec![b'v'; MAX_OFFER_BYTES + 1])),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            code(validate_offer(b"v=0\r\ns=-\r\n")),
            StatusCode::BAD_REQUEST
        );
        let no_pwd = OFFER.replace("a=ice-pwd:aaaaaaaaaaaaaaaaaaaaaaaa\r\n", "");
        assert_eq!(
            code(validate_offer(no_pwd.as_bytes())),
            StatusCode::BAD_REQUEST
        );
        let no_fp = OFFER.replace("a=fingerprint:sha-256 AA:BB\r\n", "");
        assert_eq!(
            code(validate_offer(no_fp.as_bytes())),
            StatusCode::BAD_REQUEST
        );
        // VP8 only, or audio only → 415.
        let vp8 = OFFER.replace("H264/90000", "VP8/90000");
        assert_eq!(
            code(validate_offer(vp8.as_bytes())),
            StatusCode::UNSUPPORTED_MEDIA_TYPE
        );
        let audio_only = OFFER.replace("m=video", "m=text");
        assert_eq!(
            code(validate_offer(audio_only.as_bytes())),
            StatusCode::UNSUPPORTED_MEDIA_TYPE
        );
        // H264 listed only in the audio section does not count.
        let misplaced = OFFER
            .replace("a=rtpmap:102 H264/90000", "a=rtpmap:102 VP8/90000")
            .replace("a=rtpmap:111 opus/48000/2", "a=rtpmap:111 H264/90000");
        assert_eq!(
            code(validate_offer(misplaced.as_bytes())),
            StatusCode::UNSUPPORTED_MEDIA_TYPE
        );
    }

    #[test]
    fn content_type_check() {
        let mut h = HeaderMap::new();
        assert!(!is_sdp(&h));
        h.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/SDP; charset=utf-8"),
        );
        assert!(is_sdp(&h));
        h.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        assert!(!is_sdp(&h));
    }
}
