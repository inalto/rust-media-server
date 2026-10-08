//! HTTP-FLV egress: `GET /live/{name}.flv` (ARCHITECTURE.md §6.1).
//!
//! Streaming (chunked) body: FLV header, metadata, sequence headers, GOP cache, then
//! live tags with timestamps passed through as-is. One `ViewerSession` (Protocol::Flv)
//! per response, closed when the client disconnects (body dropped).

use std::net::SocketAddr;

use axum::body::Body;
use axum::extract::{ConnectInfo, Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::Response;
use bytes::Bytes;

use super::{FlvSubscriber, resolve_flv};
use crate::error::ApiError;
use crate::metrics::{Protocol, client_ip, user_agent};
use crate::state::AppState;

pub async fn handler(
    State(state): State<AppState>,
    Path(file): Path<String>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let (broadcast, stream) = resolve_flv(&state, &file).await?;
    let ip = client_ip(&headers, peer, state.config.server.trust_proxy);
    let session = state
        .tracker
        .open(&broadcast.name, Protocol::Flv, ip, &user_agent(&headers));
    let mut sub = FlvSubscriber::new(&stream, state.shutdown.clone());
    drop(stream);

    let body = async_stream::stream! {
        // Keep the viewer session alive exactly as long as the response body.
        let _session = session;
        while let Some(chunk) = sub.next_chunk().await {
            yield Ok::<Bytes, std::io::Error>(chunk);
        }
    };

    let mut resp = Response::new(Body::from_stream(body));
    *resp.status_mut() = StatusCode::OK;
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("video/x-flv"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    h.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    // Disable proxy buffering (nginx) for low latency.
    h.insert("x-accel-buffering", HeaderValue::from_static("no"));
    Ok(resp)
}
