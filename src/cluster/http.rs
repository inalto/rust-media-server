//! HTTP surface of the cluster layer: the public `GET /healthz` (merged into the main
//! router), the `X-Rms-Node` response header (cluster mode) and the internal listener
//! on `cluster.internal_bind`.
//!
//! Internal routes (all behind `Authorization: Bearer <cluster.internal_secret>`,
//! constant-time comparison; never expose them through the public proxy, docs/CLUSTER.md
//! §2.6, §3.1):
//!
//! | route                                   | purpose                                              |
//! |-----------------------------------------|------------------------------------------------------|
//! | `GET /internal/ping`                    | node id, version, draining                           |
//! | `GET /internal/relay/{name}.flv`        | FLV byte stream of a stream *published here* (no viewer session; 404 for relays: no chains) |
//! | `POST /internal/kick/{name}`            | disconnect the local publisher (204; 404 none)       |
//! | `GET /internal/status/{name}`           | live details of a stream published here (`RemoteStreamStatus`) |
//! | `POST`/`DELETE /internal/drain`         | start/stop draining (`/healthz` 503, no new claims/relays) |
//! | `GET /internal/files/{kind}/{*path}`    | `recordings`, `waiting`, `vod` files with Range (storage not shared) |
//! | `POST /internal/whep/delete/{session}?name=` | close a WHEP session of this node (200; 404 unknown) |

use std::net::SocketAddr;
use std::path::{Component, Path as FsPath};
use std::time::Duration;

use anyhow::Context;
use axum::body::Body;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use tower_http::services::ServeFile;

use super::RemoteStreamStatus;
use crate::api::auth::{extract_token, token_matches};
use crate::db::broadcasts::is_valid_name;
use crate::egress::FlvSubscriber;
use crate::error::ApiError;
use crate::state::AppState;

/// Upper bound of the database probe of `/healthz`.
pub const HEALTH_DB_TIMEOUT: Duration = Duration::from_secs(2);
/// Response header naming the node that answered (cluster mode).
pub const NODE_HEADER: &str = "x-rms-node";

/// `GET /healthz` body.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Health {
    pub node_id: String,
    /// Cluster mode enabled.
    pub cluster: bool,
    /// The database answered `SELECT 1` within [`HEALTH_DB_TIMEOUT`].
    pub db: bool,
    /// Streams published on this node (relays excluded).
    pub live_streams: usize,
    pub version: &'static str,
    /// Draining or shutting down (the status is then 503).
    pub draining: bool,
}

/// `/healthz` (public, no auth), merged at `/` by `main.rs`.
pub fn health_router() -> Router<AppState> {
    Router::new().route("/healthz", get(healthz))
}

/// Liveness for load balancers and container health checks: 200 even when the database
/// is unreachable (`db: false`: the data plane keeps working and an outage must not take
/// every node out of the pool); 503 only while draining or shutting down.
async fn healthz(State(state): State<AppState>) -> Response {
    let draining = state.shutdown.is_cancelled() || state.cluster.is_draining();
    let body = Health {
        node_id: state.cluster.node_id().to_string(),
        cluster: state.cluster.enabled(),
        db: state.db.ping(HEALTH_DB_TIMEOUT).await,
        live_streams: state.hub.local_count(),
        version: env!("CARGO_PKG_VERSION"),
        draining,
    };
    let status = if draining {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::OK
    };
    let mut resp = (status, Json(body)).into_response();
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

/// Add `X-Rms-Node: <node_id>` to every response of `router` (cluster mode: lets a
/// client or a test see which node answered behind the load balancer).
pub fn with_node_header<S: Clone + Send + Sync + 'static>(
    router: Router<S>,
    node_id: &str,
) -> Router<S> {
    let Ok(value) = HeaderValue::from_str(node_id) else {
        return router;
    };
    router.layer(middleware::map_response(move |mut resp: Response| {
        let value = value.clone();
        async move {
            resp.headers_mut().insert(NODE_HEADER, value);
            resp
        }
    }))
}

/// `GET /internal/ping` body.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Pong {
    pub node_id: String,
    pub version: String,
    pub draining: bool,
}

/// Router of the internal listener.
pub fn internal_router(state: AppState, secret: String) -> Router {
    let secret = std::sync::Arc::new(secret);
    let node_id = state.cluster.node_id().to_string();
    let router = Router::new()
        .route("/internal/ping", get(ping))
        .route("/internal/relay/{file}", get(relay))
        .route("/internal/kick/{name}", post(kick))
        .route("/internal/status/{name}", get(status))
        .route("/internal/drain", post(drain).delete(undrain))
        .route("/internal/files/{kind}/{*path}", get(files))
        .route("/internal/whep/delete/{session}", post(whep_delete))
        .route_layer(middleware::from_fn(move |req: Request, next: Next| {
            let secret = std::sync::Arc::clone(&secret);
            async move { require_secret(&secret, req, next).await }
        }))
        .fallback(|| async { ApiError::not_found("not found").into_response() })
        .with_state(state);
    with_node_header(router, &node_id)
}

async fn require_secret(secret: &str, req: Request, next: Next) -> Response {
    match extract_bearer(&req) {
        Some(t) if token_matches(secret, &t) => next.run(req).await,
        _ => ApiError::Unauthorized.into_response(),
    }
}

/// Only the `Authorization: Bearer` header (no cookie on the internal routes).
fn extract_bearer(req: &Request) -> Option<String> {
    req.headers().get(header::AUTHORIZATION)?;
    let mut headers = req.headers().clone();
    headers.remove(header::COOKIE);
    extract_token(&headers)
}

async fn ping(State(state): State<AppState>) -> Json<Pong> {
    Json(Pong {
        node_id: state.cluster.node_id().to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        draining: state.cluster.is_draining(),
    })
}

/// A stream published on this node (never a relay: no chains, no loops).
fn local_stream(
    state: &AppState,
    name: &str,
) -> Result<std::sync::Arc<crate::hub::LiveStream>, ApiError> {
    if !is_valid_name(name) {
        return Err(ApiError::broadcast_not_found());
    }
    state
        .hub
        .get(name)
        .filter(|s| s.origin.is_local() && !s.is_ended())
        .ok_or_else(|| ApiError::not_found("stream not published on this node"))
}

/// `GET /internal/relay/{name}.flv`: the same bytes as `/live/{name}.flv` without a
/// viewer session (the edge counts its own viewers).
async fn relay(
    State(state): State<AppState>,
    Path(file): Path<String>,
) -> Result<Response, ApiError> {
    let name = file
        .strip_suffix(".flv")
        .ok_or_else(ApiError::broadcast_not_found)?;
    let stream = local_stream(&state, name)?;
    let mut sub = FlvSubscriber::new(&stream, state.shutdown.clone());
    drop(stream);
    tracing::info!(broadcast = %name, "relay subscriber connected");
    let label = name.to_string();
    let body = async_stream::stream! {
        while let Some(chunk) = sub.next_chunk().await {
            yield Ok::<Bytes, std::io::Error>(chunk);
        }
        tracing::info!(broadcast = %label, "relay subscriber finished");
    };
    let mut resp = Response::new(Body::from_stream(body));
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("video/x-flv"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(resp)
}

/// `POST /internal/kick/{name}`: another node takes the stream over.
async fn kick(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    let stream = local_stream(&state, &name)?;
    tracing::info!(broadcast = %name, "publisher kicked on request of another node");
    stream.kick();
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /internal/status/{name}`.
async fn status(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<RemoteStreamStatus>, ApiError> {
    let s = local_stream(&state, &name)?;
    let info = s.info();
    Ok(Json(RemoteStreamStatus {
        name: s.name.clone(),
        node_id: state.cluster.node_id().to_string(),
        started_at: s.started_at,
        publisher_ip: s.publisher_addr.map(|a| a.ip().to_string()),
        video_codec: info.video_codec,
        audio_codec: info.audio_codec,
        width: info.width,
        height: info.height,
        fps: info.fps,
        bytes_in: s.bytes_in(),
        recording: state.recorder.current(&name),
        packager: state.packager.status(&name),
    }))
}

#[derive(Debug, Serialize)]
struct DrainState {
    node_id: String,
    draining: bool,
}

/// `POST /internal/drain`: `/healthz` answers 503 (the load balancer stops sending new
/// requests), no new claims or relays; publishers and viewers keep running.
async fn drain(State(state): State<AppState>) -> Json<DrainState> {
    state.cluster.set_draining(true);
    tracing::warn!("draining on request (POST /internal/drain)");
    Json(DrainState {
        node_id: state.cluster.node_id().to_string(),
        draining: true,
    })
}

/// `DELETE /internal/drain`: back in the pool.
async fn undrain(State(state): State<AppState>) -> Json<DrainState> {
    if !state.shutdown.is_cancelled() {
        state.cluster.set_draining(false);
        tracing::info!("drain cancelled (DELETE /internal/drain)");
    }
    Json(DrainState {
        node_id: state.cluster.node_id().to_string(),
        draining: state.cluster.is_draining(),
    })
}

/// Plain relative path (no `..`, no root, no empty components).
pub fn safe_relative(path: &str) -> Option<&FsPath> {
    let p = FsPath::new(path);
    let ok = !path.is_empty()
        && !path.contains('\\')
        && p.components().all(|c| matches!(c, Component::Normal(_)));
    ok.then_some(p)
}

/// `GET /internal/files/{kind}/{*path}` (`recordings` | `waiting` | `vod`), Range
/// supported (`ServeFile`).
async fn files(
    State(state): State<AppState>,
    Path((kind, path)): Path<(String, String)>,
    req: Request,
) -> Result<Response, ApiError> {
    let root = match kind.as_str() {
        "recordings" => state.config.recordings_dir(),
        "waiting" => state.config.waiting_dir(),
        "vod" => state.config.vod_dir(),
        _ => return Err(ApiError::not_found("unknown file kind")),
    };
    let rel = safe_relative(&path).ok_or_else(|| ApiError::not_found("file not found"))?;
    let full = root.join(rel);
    if !full.is_file() {
        return Err(ApiError::not_found("file not found"));
    }
    let resp = match ServeFile::new(&full).oneshot(req).await {
        Ok(r) => r,
        Err(never) => match never {},
    };
    Ok(resp.map(Body::new))
}

#[derive(Debug, Default, Deserialize)]
struct WhepDeleteQuery {
    name: Option<String>,
}

/// `POST /internal/whep/delete/{session}?name=<broadcast>`: a `DELETE /whep/...` that
/// reached another node.
async fn whep_delete(
    State(state): State<AppState>,
    Path(session): Path<String>,
    q: Result<Query<WhepDeleteQuery>, QueryRejection>,
) -> Result<StatusCode, ApiError> {
    let Query(q) = q?;
    let name = q.name.unwrap_or_default();
    if state.webrtc.delete_session(&name, &session) {
        Ok(StatusCode::OK)
    } else {
        Err(ApiError::not_found("session not found"))
    }
}

/// Turn a response of another node's internal file route into a response for the
/// client: status and the content headers are copied, the body is streamed.
pub fn proxied_file_response(resp: reqwest::Response) -> Response {
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut headers = axum::http::HeaderMap::new();
    for h in [
        header::CONTENT_TYPE,
        header::CONTENT_LENGTH,
        header::CONTENT_RANGE,
        header::ACCEPT_RANGES,
        header::LAST_MODIFIED,
        header::ETAG,
    ] {
        if let Some(v) = resp.headers().get(h.as_str())
            && let Ok(v) = HeaderValue::from_bytes(v.as_bytes())
        {
            headers.insert(h, v);
        }
    }
    let body = Body::from_stream(resp.bytes_stream());
    let mut out = Response::new(body);
    *out.status_mut() = status;
    *out.headers_mut() = headers;
    out
}

/// Bind `cluster.internal_bind` and serve the internal routes until `shutdown`.
pub async fn serve_internal(
    state: AppState,
    bind: &str,
    secret: String,
    shutdown: CancellationToken,
) -> anyhow::Result<tokio::task::JoinHandle<()>> {
    let addr: SocketAddr = bind
        .parse()
        .with_context(|| format!("invalid cluster.internal_bind {bind:?}"))?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("cannot bind the internal listener on {addr}"))?;
    tracing::info!(addr = %addr, "internal cluster listener ready (/internal/*)");
    let app = internal_router(state, secret);
    Ok(tokio::spawn(async move {
        let server = axum::serve(listener, app.into_make_service())
            .with_graceful_shutdown(async move { shutdown.cancelled().await });
        if let Err(e) = server.await {
            tracing::warn!(error = %e, "internal cluster listener failed");
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::{MediaPacket, PublishInfo, StreamOrigin};
    use axum::body::Body;

    #[tokio::test]
    async fn healthz_is_liveness() {
        let (state, dir) = crate::api::waiting::tests::test_state(false).await;
        let app = health_router().with_state(state.clone());
        let get = |app: Router| async move {
            let resp = app
                .oneshot(
                    Request::builder()
                        .uri("/healthz")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = resp.status();
            assert_eq!(resp.headers()[header::CACHE_CONTROL], "no-store");
            assert!(resp.headers().get(NODE_HEADER).is_none(), "single node");
            let body = axum::body::to_bytes(resp.into_body(), 1 << 16)
                .await
                .unwrap();
            (
                status,
                serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            )
        };
        let (status, v) = get(app.clone()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(v["node_id"], state.cluster.node_id());
        assert_eq!(v["cluster"], false);
        assert_eq!(v["db"], true);
        assert_eq!(v["live_streams"], 0);
        assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(v["draining"], false);
        // Draining and shutdown answer 503.
        state.cluster.set_draining(true);
        assert_eq!(get(app.clone()).await.0, StatusCode::SERVICE_UNAVAILABLE);
        state.cluster.set_draining(false);
        state.shutdown.cancel();
        let (status, v) = get(app).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(v["draining"], true);
        let _ = std::fs::remove_dir_all(dir);
    }

    async fn call(
        app: &Router,
        method: &str,
        auth: Option<&str>,
        uri: &str,
    ) -> (StatusCode, axum::http::HeaderMap, Bytes) {
        let mut req = Request::builder().method(method).uri(uri);
        if let Some(a) = auth {
            req = req.header(header::AUTHORIZATION, a);
        }
        let resp = app
            .clone()
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let (parts, body) = resp.into_parts();
        // Streaming bodies (relay) never end: read until 300 ms without data.
        let mut stream = body.into_data_stream();
        let mut bytes = Vec::new();
        while let Ok(Some(Ok(chunk))) = tokio::time::timeout(
            Duration::from_millis(300),
            futures::StreamExt::next(&mut stream),
        )
        .await
        {
            bytes.extend_from_slice(&chunk);
        }
        (parts.status, parts.headers, Bytes::from(bytes))
    }

    #[tokio::test]
    async fn internal_routes() {
        let (state, dir) = crate::api::waiting::tests::test_state(false).await;
        let secret = "x".repeat(32);
        let bearer = format!("Bearer {secret}");
        let auth = Some(bearer.as_str());
        let app = internal_router(state.clone(), secret.clone());

        // Secret required; the node header is always there.
        let (s, h, _) = call(&app, "GET", None, "/internal/ping").await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        assert_eq!(h[NODE_HEADER], state.cluster.node_id());
        assert_eq!(
            call(&app, "GET", Some("Bearer nope"), "/internal/ping")
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
        let (s, _, body) = call(&app, "GET", auth, "/internal/ping").await;
        assert_eq!(s, StatusCode::OK);
        let pong: Pong = serde_json::from_slice(&body).unwrap();
        assert_eq!(pong.node_id, state.cluster.node_id());
        assert!(!pong.draining);
        assert_eq!(
            call(&app, "GET", auth, "/internal/nope").await.0,
            StatusCode::NOT_FOUND
        );

        // Nothing live: relay, status and kick are 404.
        for (m, uri) in [
            ("GET", "/internal/relay/show.flv"),
            ("GET", "/internal/status/show"),
            ("POST", "/internal/kick/show"),
        ] {
            assert_eq!(
                call(&app, m, auth, uri).await.0,
                StatusCode::NOT_FOUND,
                "{uri}"
            );
        }

        // A relayed stream is never served again (no chains).
        let mut relayed = PublishInfo::new("id");
        relayed.origin = StreamOrigin::Remote("n9".into());
        let r = state.hub.publish_start("show", relayed).unwrap();
        assert_eq!(
            call(&app, "GET", auth, "/internal/relay/show.flv").await.0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            call(&app, "POST", auth, "/internal/kick/show").await.0,
            StatusCode::NOT_FOUND
        );
        state.hub.publish_end_stream(&r);

        // A local stream: FLV bytes, status, kick.
        let s = state
            .hub
            .publish_start("show", PublishInfo::new("id"))
            .unwrap();
        s.push(MediaPacket::video(
            0,
            Bytes::from_static(&[0x17, 0x00, 0, 0, 0, 1, 0x64, 0, 0x1f]),
        ));
        s.push(MediaPacket::video(
            0,
            Bytes::from_static(&[0x17, 0x01, 0, 0, 0, 9]),
        ));
        let (st, h, body) = call(&app, "GET", auth, "/internal/relay/show.flv").await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(h[header::CONTENT_TYPE], "video/x-flv");
        assert_eq!(&body[..3], b"FLV");
        let mut parser = crate::hub::flv_parser::FlvStreamParser::new();
        parser.push(&body);
        let mut n = 0;
        while parser.next_packet().unwrap().is_some() {
            n += 1;
        }
        assert_eq!(n, 2, "sequence header + keyframe");
        assert_eq!(
            state.tracker.counts("show").total(),
            0,
            "no viewer session for relays"
        );
        let (st, _, body) = call(&app, "GET", auth, "/internal/status/show").await;
        assert_eq!(st, StatusCode::OK);
        let rs: RemoteStreamStatus = serde_json::from_slice(&body).unwrap();
        assert_eq!(rs.video_codec.as_deref(), Some("h264"));
        assert_eq!(rs.node_id, state.cluster.node_id());
        assert_eq!(
            call(&app, "POST", auth, "/internal/kick/show").await.0,
            StatusCode::NO_CONTENT
        );
        assert!(s.is_kicked());

        // Drain.
        let (st, _, _) = call(&app, "POST", auth, "/internal/drain").await;
        assert_eq!(st, StatusCode::OK);
        assert!(state.cluster.is_draining());
        call(&app, "DELETE", auth, "/internal/drain").await;
        assert!(!state.cluster.is_draining());

        // Files: kinds, traversal, Range.
        let rec = state.config.recordings_dir().join("show");
        std::fs::create_dir_all(&rec).unwrap();
        std::fs::write(rec.join("a b.flv"), b"0123456789").unwrap();
        let (st, _, body) = call(
            &app,
            "GET",
            auth,
            "/internal/files/recordings/show/a%20b.flv",
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(&body[..], b"0123456789");
        let req = Request::builder()
            .uri("/internal/files/recordings/show/a%20b.flv")
            .header(header::AUTHORIZATION, &bearer)
            .header(header::RANGE, "bytes=2-4")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
        let body = axum::body::to_bytes(resp.into_body(), 1 << 16)
            .await
            .unwrap();
        assert_eq!(&body[..], b"234");
        for uri in [
            "/internal/files/recordings/../admin_token",
            "/internal/files/recordings/%2E%2E/admin_token",
            "/internal/files/other/show/a%20b.flv",
            "/internal/files/recordings/show/missing.flv",
            "/internal/files/recordings/show",
        ] {
            assert_eq!(
                call(&app, "GET", auth, uri).await.0,
                StatusCode::NOT_FOUND,
                "{uri}"
            );
        }

        // WHEP delete of an unknown session.
        assert_eq!(
            call(&app, "POST", auth, "/internal/whep/delete/n1-abc?name=show")
                .await
                .0,
            StatusCode::NOT_FOUND
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn relative_paths() {
        assert!(safe_relative("show/a.flv").is_some());
        for bad in ["", "/etc/passwd", "../x", "a/../../x", "a\\..\\x", "./a"] {
            assert!(safe_relative(bad).is_none(), "{bad}");
        }
    }
}
