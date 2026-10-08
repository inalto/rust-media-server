//! Playback endpoints. Phase 1: HTTP-FLV (`/live/{name}.flv`) and WebSocket-FLV
//! (`/ws/live/{name}.flv`). LL-HLS / LL-DASH (`hls.rs`, `dash.rs`) come with the
//! packager branch and are mounted by `packager::router()`.

pub mod httpflv;
pub mod wsflv;

use std::sync::Arc;

use axum::Router;
use axum::routing::get;
use bytes::{Bytes, BytesMut};
use tokio::sync::broadcast::{self, error::RecvError};
use tokio_util::sync::CancellationToken;
use tower_http::cors::CorsLayer;

use crate::db::{self, Broadcast, broadcasts::is_valid_name};
use crate::error::ApiError;
use crate::hub::packet::{flv_file_header, write_flv_tag};
use crate::hub::{LiveStream, MediaPacket, PacketKind};
use crate::state::AppState;

/// `/live/{name}.flv` and `/ws/live/{name}.flv`, with `Access-Control-Allow-Origin: *`.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/live/{file}", get(httpflv::handler))
        .route("/ws/live/{file}", get(wsflv::handler))
        .layer(CorsLayer::permissive())
}

/// Resolve `{name}.flv` to the broadcast and its live stream (§6.1 semantics):
/// 404 unknown broadcast, 403 when `enable_flv = false`, 404 "stream offline".
pub(crate) async fn resolve_flv(
    state: &AppState,
    file: &str,
) -> Result<(Broadcast, Arc<LiveStream>), ApiError> {
    let name = file
        .strip_suffix(".flv")
        .filter(|n| is_valid_name(n))
        .ok_or_else(ApiError::broadcast_not_found)?;
    let broadcast = db::broadcasts::find_by_name(&state.db, name)
        .await?
        .ok_or_else(ApiError::broadcast_not_found)?;
    if !broadcast.enable_flv {
        return Err(ApiError::forbidden(
            "FLV playback is disabled for this broadcast",
        ));
    }
    let stream = state
        .hub
        .get(name)
        .ok_or_else(|| ApiError::not_found("stream offline"))?;
    Ok((broadcast, stream))
}

/// FLV byte stream for one viewer: the first chunk is the FLV header plus the
/// catch-up tags (metadata, sequence headers, current GOP); each following chunk is
/// one live tag.
///
/// Slow consumers: on `RecvError::Lagged` everything except headers is dropped until
/// the next video keyframe (audio-only streams just continue). The publisher is never
/// blocked; memory is bounded by the hub channel capacity.
pub struct FlvSubscriber {
    name: String,
    rx: broadcast::Receiver<Arc<MediaPacket>>,
    ended: CancellationToken,
    shutdown: CancellationToken,
    first: Option<Bytes>,
    /// No video keyframe sent yet: drop video inter frames.
    need_keyframe: bool,
    /// Recovering from lag: drop audio and video until a keyframe.
    skipping: bool,
    has_video: bool,
}

impl FlvSubscriber {
    pub fn new(stream: &LiveStream, shutdown: CancellationToken) -> Self {
        let (snapshot, rx) = stream.subscribe();
        let info = stream.info();
        let snap_video = snapshot.iter().any(|p| p.is_video());
        let snap_audio = snapshot.iter().any(|p| p.is_audio());
        let mut has_video = info.has_video || snap_video;
        let mut has_audio = info.has_audio || snap_audio;
        if !has_video && !has_audio {
            // Nothing known yet (stream just started): announce both.
            has_video = true;
            has_audio = true;
        }
        let mut buf =
            BytesMut::with_capacity(13 + snapshot.iter().map(|p| p.flv_size()).sum::<usize>());
        buf.extend_from_slice(&flv_file_header(has_video, has_audio));
        let mut need_keyframe = true;
        for p in &snapshot {
            if p.is_video() && p.is_keyframe && !p.is_sequence_header {
                need_keyframe = false;
            }
            write_flv_tag(&mut buf, p);
        }
        Self {
            name: stream.name.clone(),
            rx,
            ended: stream.ended_token(),
            shutdown,
            first: Some(buf.freeze()),
            need_keyframe,
            skipping: false,
            has_video,
        }
    }

    fn accept(&mut self, p: &MediaPacket) -> bool {
        if p.kind == PacketKind::Script || p.is_sequence_header {
            return true;
        }
        if p.is_video() {
            if p.is_keyframe {
                self.need_keyframe = false;
                self.skipping = false;
                return true;
            }
            return !(self.need_keyframe || self.skipping);
        }
        !self.skipping
    }

    /// Next chunk of FLV bytes; `None` when the stream ended or the server shuts down.
    /// Cancel-safe.
    pub async fn next_chunk(&mut self) -> Option<Bytes> {
        if let Some(first) = self.first.take() {
            return Some(first);
        }
        loop {
            let received = tokio::select! {
                biased;
                _ = self.shutdown.cancelled() => return None,
                r = self.rx.recv() => r,
                _ = self.ended.cancelled() => return None,
            };
            match received {
                Ok(p) => {
                    if self.accept(&p) {
                        let mut b = BytesMut::with_capacity(p.flv_size());
                        write_flv_tag(&mut b, &p);
                        return Some(b.freeze());
                    }
                }
                Err(RecvError::Lagged(n)) => {
                    tracing::warn!(broadcast = %self.name, skipped = n, "FLV viewer lagging; dropping until next keyframe");
                    if self.has_video {
                        self.skipping = true;
                    }
                }
                Err(RecvError::Closed) => return None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::{Hub, PublishInfo};

    #[tokio::test]
    async fn subscriber_first_chunk_and_lag_recovery() {
        let hub = Hub::new();
        let s = hub.publish_start("x", PublishInfo::new("id")).unwrap();
        s.push(MediaPacket::video(
            0,
            Bytes::from_static(&[0x17, 0x00, 0, 0, 0, 1, 0x64, 0, 0x1f]),
        ));
        s.push(MediaPacket::audio(
            0,
            Bytes::from_static(&[0xAF, 0x00, 0x11, 0x90]),
        ));
        s.push(MediaPacket::video(
            0,
            Bytes::from_static(&[0x17, 0x01, 0, 0, 0, 9]),
        ));
        let mut sub = FlvSubscriber::new(&s, CancellationToken::new());
        let first = sub.next_chunk().await.unwrap();
        assert_eq!(&first[..3], b"FLV");
        assert_eq!(first[4], 5);
        // header 13 + 3 tags
        assert_eq!(first.len(), 13 + (11 + 9 + 4) + (11 + 4 + 4) + (11 + 6 + 4));

        // Overflow the channel: subscriber must skip until the next keyframe.
        for i in 0..(crate::hub::stream::STREAM_CHANNEL_CAPACITY as u32 + 10) {
            s.push(MediaPacket::video(
                i,
                Bytes::from_static(&[0x27, 0x01, 0, 0, 0]),
            ));
        }
        s.push(MediaPacket::audio(
            99_000,
            Bytes::from_static(&[0xAF, 0x01, 0x21]),
        ));
        s.push(MediaPacket::video(
            99_999,
            Bytes::from_static(&[0x17, 0x01, 0, 0, 0]),
        ));
        let next = sub.next_chunk().await.unwrap();
        assert_eq!(next[0], 9, "first chunk after lag must be video");
        assert_eq!(next[11] >> 4, 1, "first chunk after lag must be a keyframe");

        hub.publish_end("x");
        assert!(sub.next_chunk().await.is_none());
    }
}
