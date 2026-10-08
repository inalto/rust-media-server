//! RTMP ingest (ARCHITECTURE.md §5): rtmp-rs handler → hub.
//!
//! Publish flow: `on_publish` looks up the broadcast by stream key (the RTMP stream
//! name, any `?query` suffix ignored), rejects unknown keys and broadcasts that are
//! already live, opens a `stream_sessions` row and calls [`Hub::publish_start`].
//! Raw FLV tags (`on_media_tag`) are pushed to the [`LiveStream`]; `onMetaData`
//! (`on_metadata`, which rtmp-rs delivers as a decoded map, never as a raw script tag)
//! is re-serialized as an AMF0 script tag so FLV outputs can replay it.
//! `on_unpublish` / `on_disconnect` end the stream and close the DB row.
//!
//! ## Accept loop and kicking publishers
//!
//! rtmp-rs ignores the return value of `on_media_tag` and offers no API to close a
//! session from the handler. Instead of `RtmpServer::run`, this module runs its own
//! accept loop around the public `rtmp_rs::server::connection::Connection` type, so
//! every connection task can be cancelled through a `CancellationToken`. That token is
//! handed to the hub as [`PublishInfo::kick`]; [`Hub::kick`] (API
//! `POST /broadcasts/{name}/disconnect`) cancels it, the connection future is dropped
//! (closing the TCP socket) and the session is finalized here. As a second line of
//! defence, tags from a kicked session are ignored.
//!
//! A publisher that stopped sending data for more than [`STALE_PUBLISHER_MS`] (e.g. a
//! dead TCP connection not yet detected) is replaced when a new publish with the same
//! key arrives, so OBS reconnects are not rejected as "already live".
//!
//! RTMP playback (`play`) is rejected: playback goes through HTTP-FLV/WS-FLV/HLS/DASH.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Utc;
use rtmp_rs::amf::AmfValue;
use rtmp_rs::media::{FlvTag, FlvTagType};
use rtmp_rs::protocol::message::{ConnectParams, PlayParams, PublishParams};
use rtmp_rs::server::connection::Connection;
use rtmp_rs::server::handler::{AuthResult, MediaDeliveryMode, RtmpHandler};
use rtmp_rs::session::{SessionContext, StreamContext};
use rtmp_rs::{ServerConfig, StreamKey, StreamRegistry};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::config::Config;
use crate::db::{self, Db};
use crate::hub::packet::on_metadata_body;
use crate::hub::{Hub, LiveStream, MediaPacket, PublishInfo};
use crate::metrics::ViewerTracker;

/// A live publisher silent for longer than this can be replaced by a new publish.
pub const STALE_PUBLISHER_MS: i64 = 5_000;
/// rtmp-rs idle timeout (no data received) for every connection.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// Mask a stream key for logs: first 4 chars only.
fn mask_key(key: &str) -> String {
    let prefix: String = key.chars().take(4).collect();
    format!("{prefix}…")
}

/// The stream key as configured in OBS may carry a `?query` (some services use it);
/// only the part before `?` is matched against the database.
fn normalize_key(raw: &str) -> &str {
    raw.split('?').next().unwrap_or("").trim()
}

struct Publisher {
    session_id: u64,
    stream: Arc<LiveStream>,
    /// rtmp-rs registry key (`app` + raw stream name) for manual unregistration.
    registry_key: StreamKey,
    finished: AtomicBool,
}

/// The rtmp-rs handler. Shared by all connections.
pub struct RtmpIngest {
    config: Arc<Config>,
    db: Db,
    hub: Arc<Hub>,
    tracker: Arc<ViewerTracker>,
    registry: Arc<StreamRegistry>,
    /// Active publishers by rtmp-rs session id.
    publishers: Mutex<HashMap<u64, Arc<Publisher>>>,
    /// Cancellation token of every open connection, by session id.
    connections: Mutex<HashMap<u64, CancellationToken>>,
}

impl RtmpIngest {
    fn publisher(&self, session_id: u64) -> Option<Arc<Publisher>> {
        self.publishers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&session_id)
            .cloned()
    }

    fn take_publisher(&self, session_id: u64) -> Option<Arc<Publisher>> {
        self.publishers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&session_id)
    }

    fn connection_token(&self, session_id: u64) -> CancellationToken {
        self.connections
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&session_id)
            .cloned()
            .unwrap_or_default()
    }

    /// End the live period of a publisher (idempotent): hub, DB row, logs.
    async fn finish_publisher(&self, p: &Publisher, reason: &str) {
        if p.finished.swap(true, Ordering::SeqCst) {
            return;
        }
        let stream = &p.stream;
        self.hub.publish_end_stream(stream);
        let peak = self.tracker.take_peak(&stream.name);
        let info = stream.info();
        tracing::info!(
            broadcast = %stream.name,
            session_id = p.session_id,
            reason,
            bytes_in = stream.bytes_in(),
            peak_viewers = peak,
            "publish ended"
        );
        if let Some(id) = &stream.stream_session_id {
            let end = db::stream_sessions::StreamSessionEnd {
                ended_at: Utc::now(),
                video_codec: info.video_codec,
                audio_codec: info.audio_codec,
                width: info.width,
                height: info.height,
                fps: info.fps,
                bytes_in: stream.bytes_in(),
                peak_viewers: peak,
            };
            if let Err(e) = db::stream_sessions::close(&self.db, id, end).await {
                tracing::error!(error = %e, broadcast = %stream.name, "failed to close stream session row");
            }
        }
    }

    /// Called when a connection is gone (normally, kicked or at shutdown).
    /// `cancelled`: the connection future was dropped, so rtmp-rs did not clean up its
    /// registry entry and we must do it.
    async fn finish_session(&self, session_id: u64, cancelled: bool, reason: &str) {
        if let Some(p) = self.take_publisher(session_id) {
            if cancelled {
                self.registry
                    .unregister_publisher(&p.registry_key, session_id)
                    .await;
            }
            self.finish_publisher(&p, reason).await;
        }
    }

    /// Replace a stale publisher of `existing` (see [`STALE_PUBLISHER_MS`]).
    async fn take_over(&self, existing: &Arc<LiveStream>) {
        let old = {
            let mut map = self.publishers.lock().unwrap_or_else(|e| e.into_inner());
            let id = map
                .iter()
                .find(|(_, p)| Arc::ptr_eq(&p.stream, existing))
                .map(|(id, _)| *id);
            id.and_then(|id| map.remove(&id))
        };
        existing.kick();
        match old {
            Some(p) => {
                self.registry
                    .unregister_publisher(&p.registry_key, p.session_id)
                    .await;
                self.finish_publisher(&p, "replaced by new publisher").await;
            }
            None => {
                self.hub.publish_end_stream(existing);
            }
        }
    }
}

impl RtmpHandler for RtmpIngest {
    async fn on_connect(&self, ctx: &SessionContext, params: &ConnectParams) -> AuthResult {
        tracing::debug!(
            session_id = ctx.session_id,
            peer = %ctx.peer_addr,
            app = %params.app,
            flash_ver = ?params.flash_ver,
            "RTMP connect"
        );
        AuthResult::Accept
    }

    async fn on_fc_publish(&self, ctx: &SessionContext, stream_key: &str) -> AuthResult {
        // Validation happens in on_publish so the encoder receives a proper
        // NetStream.Publish.BadName status instead of a dropped connection.
        tracing::debug!(session_id = ctx.session_id, key = %mask_key(stream_key), "FCPublish");
        AuthResult::Accept
    }

    async fn on_publish(&self, ctx: &SessionContext, params: &PublishParams) -> AuthResult {
        let key = normalize_key(&params.stream_key);
        let peer = ctx.peer_addr;
        if key.is_empty() {
            tracing::warn!(%peer, "publish rejected: empty stream key");
            return AuthResult::Reject("missing stream key".into());
        }
        if self.publisher(ctx.session_id).is_some() {
            return AuthResult::Reject("this connection is already publishing".into());
        }
        let broadcast = match db::broadcasts::find_by_key(&self.db, key).await {
            Ok(Some(b)) => b,
            Ok(None) => {
                tracing::warn!(%peer, key = %mask_key(key), app = %ctx.app, "publish rejected: unknown stream key");
                return AuthResult::Reject("invalid stream key".into());
            }
            Err(e) => {
                tracing::error!(error = %e, "publish rejected: database error");
                return AuthResult::Reject("internal error".into());
            }
        };
        let name = broadcast.name.clone();
        if ctx.app != self.config.rtmp.app {
            tracing::info!(broadcast = %name, app = %ctx.app, expected = %self.config.rtmp.app, "publisher uses a different RTMP app name; accepting anyway");
        }

        if let Some(existing) = self.hub.get(&name) {
            let idle = existing.idle_ms();
            if idle < STALE_PUBLISHER_MS {
                tracing::warn!(broadcast = %name, %peer, "publish rejected: broadcast already live");
                return AuthResult::Reject("broadcast already live".into());
            }
            tracing::warn!(broadcast = %name, %peer, idle_ms = idle, "replacing stale publisher");
            self.take_over(&existing).await;
        }

        let started_at = Utc::now();
        let stream_session_id = match db::stream_sessions::open(
            &self.db,
            &broadcast.id,
            started_at,
            Some(peer.ip().to_string()),
        )
        .await
        {
            Ok(id) => Some(id),
            Err(e) => {
                tracing::error!(error = %e, broadcast = %name, "failed to open stream session row");
                None
            }
        };
        let info = PublishInfo {
            broadcast_id: broadcast.id.clone(),
            publisher_addr: Some(peer),
            stream_session_id: stream_session_id.clone(),
            kick: self.connection_token(ctx.session_id),
        };
        let stream = match self.hub.publish_start(&name, info) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(broadcast = %name, %peer, error = %e, "publish rejected");
                if let Some(id) = stream_session_id {
                    let end = db::stream_sessions::StreamSessionEnd {
                        ended_at: Utc::now(),
                        ..Default::default()
                    };
                    let _ = db::stream_sessions::close(&self.db, &id, end).await;
                }
                return AuthResult::Reject("broadcast already live".into());
            }
        };
        self.tracker.reset_peak(&name);
        let publisher = Arc::new(Publisher {
            session_id: ctx.session_id,
            stream,
            registry_key: StreamKey::new(ctx.app.clone(), params.stream_key.clone()),
            finished: AtomicBool::new(false),
        });
        self.publishers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(ctx.session_id, publisher);
        tracing::info!(
            broadcast = %name,
            session_id = ctx.session_id,
            %peer,
            app = %ctx.app,
            encoder = ?ctx.encoder_type,
            "publish accepted"
        );
        AuthResult::Accept
    }

    async fn on_play(&self, ctx: &SessionContext, _params: &PlayParams) -> AuthResult {
        tracing::debug!(session_id = ctx.session_id, "RTMP play rejected");
        AuthResult::Reject("RTMP playback is not supported: use HTTP-FLV, HLS or DASH".into())
    }

    async fn on_metadata(&self, ctx: &StreamContext, metadata: &HashMap<String, AmfValue>) {
        let Some(p) = self.publisher(ctx.session.session_id) else {
            return;
        };
        if p.stream.is_kicked() {
            return;
        }
        p.stream.apply_metadata(metadata);
        p.stream
            .push(MediaPacket::script(0, on_metadata_body(metadata)));
        tracing::debug!(broadcast = %p.stream.name, keys = metadata.len(), "onMetaData received");
    }

    async fn on_media_tag(&self, ctx: &StreamContext, tag: &FlvTag) -> bool {
        let Some(p) = self.publisher(ctx.session.session_id) else {
            return false;
        };
        if p.stream.is_kicked() || p.finished.load(Ordering::Relaxed) {
            return false;
        }
        let packet = match tag.tag_type {
            FlvTagType::Video => MediaPacket::video(tag.timestamp, tag.data.clone()),
            FlvTagType::Audio => MediaPacket::audio(tag.timestamp, tag.data.clone()),
            FlvTagType::Script => MediaPacket::script(tag.timestamp, tag.data.clone()),
        };
        p.stream.push(packet);
        true
    }

    async fn on_unpublish(&self, ctx: &StreamContext) {
        self.finish_session(ctx.session.session_id, false, "unpublish")
            .await;
    }

    async fn on_disconnect(&self, ctx: &SessionContext) {
        self.finish_session(ctx.session_id, false, "disconnect")
            .await;
    }

    fn media_delivery_mode(&self) -> MediaDeliveryMode {
        MediaDeliveryMode::RawFlv
    }
}

/// Bound RTMP listener, ready to [`RtmpServer::run`].
pub struct RtmpServer {
    listener: TcpListener,
    server_config: ServerConfig,
    handler: Arc<RtmpIngest>,
    next_session_id: AtomicU64,
}

impl RtmpServer {
    /// Bind `rtmp.bind`. Fails early (at startup) if the address is unavailable.
    pub async fn bind(
        config: Arc<Config>,
        db: Db,
        hub: Arc<Hub>,
        tracker: Arc<ViewerTracker>,
    ) -> anyhow::Result<Self> {
        let addr = config.rtmp_bind_addr()?;
        let listener = TcpListener::bind(addr)
            .await
            .map_err(|e| anyhow::anyhow!("cannot bind RTMP listener on {addr}: {e}"))?;
        let server_config = ServerConfig::default()
            .bind(addr)
            .idle_timeout(IDLE_TIMEOUT);
        let handler = Arc::new(RtmpIngest {
            config,
            db,
            hub,
            tracker,
            registry: Arc::new(StreamRegistry::new()),
            publishers: Mutex::new(HashMap::new()),
            connections: Mutex::new(HashMap::new()),
        });
        Ok(Self {
            listener,
            server_config,
            handler,
            next_session_id: AtomicU64::new(1),
        })
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Accept loop. Returns after `shutdown` fires and open connections are finalized
    /// (bounded wait).
    pub async fn run(self, shutdown: CancellationToken) {
        let addr = self.local_addr().ok();
        tracing::info!(task = "rtmp-ingest", addr = ?addr, "RTMP ingest listening");
        let cleanup = self.handler.registry.spawn_cleanup_task();
        let tasks = TaskTracker::new();
        let limiter = Arc::new(Semaphore::new(
            self.handler.config.rtmp.max_connections.max(1),
        ));
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                accepted = self.listener.accept() => match accepted {
                    Ok((socket, peer)) => match Arc::clone(&limiter).try_acquire_owned() {
                        Ok(permit) => self.spawn_connection(&tasks, socket, peer, &shutdown, permit),
                        Err(_) => {
                            tracing::warn!(task = "rtmp-ingest", %peer, limit = self.handler.config.rtmp.max_connections, "RTMP connection limit reached; closing connection");
                            drop(socket);
                        }
                    },
                    Err(e) => {
                        // e.g. EMFILE: back off instead of spinning.
                        tracing::warn!(task = "rtmp-ingest", error = %e, "accept failed");
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                }
            }
        }
        tasks.close();
        if tokio::time::timeout(Duration::from_secs(5), tasks.wait())
            .await
            .is_err()
        {
            tracing::warn!(
                task = "rtmp-ingest",
                "timed out waiting for RTMP connections to close"
            );
        }
        cleanup.abort();
        tracing::info!(task = "rtmp-ingest", "RTMP ingest stopped");
    }

    fn spawn_connection(
        &self,
        tasks: &TaskTracker,
        socket: TcpStream,
        peer: SocketAddr,
        shutdown: &CancellationToken,
        permit: OwnedSemaphorePermit,
    ) {
        if let Err(e) = socket.set_nodelay(true) {
            tracing::debug!(error = %e, "set_nodelay failed");
        }
        let session_id = self.next_session_id.fetch_add(1, Ordering::Relaxed);
        let token = shutdown.child_token();
        self.handler
            .connections
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(session_id, token.clone());
        let handler = Arc::clone(&self.handler);
        let config = self.server_config.clone();
        tracing::debug!(session_id, %peer, "RTMP connection accepted");
        tasks.spawn(async move {
            // Released when this task ends (connection closed).
            let _permit = permit;
            let registry = Arc::clone(&handler.registry);
            let mut conn = Connection::new(session_id, socket, peer, config, Arc::clone(&handler), registry);
            let cancelled = tokio::select! {
                result = conn.run() => {
                    if let Err(e) = result {
                        tracing::debug!(session_id, %peer, error = %e, "RTMP connection ended with error");
                    }
                    false
                }
                _ = token.cancelled() => true,
            };
            // Dropping the connection closes the socket.
            drop(conn);
            if cancelled {
                tracing::info!(session_id, %peer, "RTMP connection closed by server (kick or shutdown)");
            }
            handler
                .finish_session(session_id, cancelled, if cancelled { "kicked" } else { "connection closed" })
                .await;
            handler
                .connections
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&session_id);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_normalization() {
        assert_eq!(normalize_key("abc"), "abc");
        assert_eq!(normalize_key("abc?token=1"), "abc");
        assert_eq!(normalize_key(" abc "), "abc");
        assert_eq!(normalize_key("?x"), "");
        assert_eq!(mask_key("0123456789"), "0123…");
    }
}
