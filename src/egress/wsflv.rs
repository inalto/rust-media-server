//! WebSocket-FLV egress: `GET /ws/live/{name}.flv` (ARCHITECTURE.md §6.2).
//!
//! Same byte stream as HTTP-FLV, as binary WebSocket messages: the first message holds
//! the FLV header plus the catch-up tags, then one message per tag. Playable with
//! mpegts.js (`type: 'flv'`, `url: 'ws://...'`). 404/403 are returned before upgrading.

use std::net::SocketAddr;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, Path, State};
use axum::http::HeaderMap;
use axum::response::Response;
use futures::{SinkExt, StreamExt};

use super::{FlvSubscriber, resolve_flv};
use crate::error::ApiError;
use crate::metrics::{Protocol, ViewerSession, client_ip, user_agent};
use crate::state::AppState;

pub async fn handler(
    State(state): State<AppState>,
    Path(file): Path<String>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    let (broadcast, stream) = resolve_flv(&state, &file).await?;
    let ip = client_ip(&headers, peer, state.config.server.trust_proxy);
    let ua = user_agent(&headers);
    let name = broadcast.name.clone();
    let tracker = state.tracker.clone();
    let shutdown = state.shutdown.clone();
    Ok(ws
        .max_message_size(64 * 1024)
        .on_upgrade(move |socket| async move {
            // Subscribe only once the upgrade succeeded.
            let session = tracker.open(&name, Protocol::WsFlv, ip, &ua);
            let sub = FlvSubscriber::new(&stream, shutdown);
            drop(stream);
            serve(socket, sub, session).await;
        }))
}

async fn serve(socket: WebSocket, mut sub: FlvSubscriber, session: ViewerSession) {
    let (mut sink, mut incoming) = socket.split();
    loop {
        tokio::select! {
            chunk = sub.next_chunk() => match chunk {
                Some(bytes) => {
                    if sink.send(Message::Binary(bytes)).await.is_err() {
                        break;
                    }
                }
                None => {
                    let _ = sink.send(Message::Close(None)).await;
                    break;
                }
            },
            msg = incoming.next() => match msg {
                // Pings are answered automatically; other client messages are ignored.
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => {}
            },
        }
    }
    drop(session);
}
