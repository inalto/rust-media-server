//! `LiveStream`: one live broadcast inside the hub (ARCHITECTURE.md §4.3).
//!
//! Packets are fanned out through a bounded `tokio::sync::broadcast` channel
//! (capacity [`STREAM_CHANNEL_CAPACITY`]) and kept in a bounded [`GopCache`] so that
//! late joiners can start from the last keyframe. [`LiveStream::subscribe`] takes the
//! catch-up snapshot and the receiver under the same lock as [`LiveStream::push`], so
//! no packet is lost or duplicated between the two.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use chrono::{DateTime, Utc};
use rtmp_rs::amf::AmfValue;
use serde::Serialize;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use super::HubEvent;
use super::packet::{
    MediaPacket, PacketKind, audio_codec_name, parse_aac_sequence_header,
    parse_avc_sequence_header, video_codec_name,
};

/// Capacity of the per-stream packet channel (≈ 30 s of 30 fps video + audio).
pub const STREAM_CHANNEL_CAPACITY: usize = 2048;
/// GOP cache bound: maximum span between first and last cached packet.
pub const GOP_CACHE_MAX_DURATION_MS: u32 = 4_000;
/// GOP cache bound: maximum cached bytes.
pub const GOP_CACHE_MAX_BYTES: usize = 4 * 1024 * 1024;

/// Codec / format information, from `onMetaData` and from sequence headers
/// (sequence headers win for codec and dimensions).
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct StreamInfo {
    /// "h264", "hevc", "av1", ...
    pub video_codec: Option<String>,
    /// "aac", "mp3", "opus", ...
    pub audio_codec: Option<String>,
    /// RFC 6381 codec string, e.g. "avc1.64001f" (from the AVC sequence header).
    pub video_codec_string: Option<String>,
    /// RFC 6381 codec string, e.g. "mp4a.40.2" (from the AAC sequence header).
    pub audio_codec_string: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<f64>,
    /// Declared by the encoder in `onMetaData` (kbit/s).
    pub video_bitrate_kbps: Option<f64>,
    /// Declared by the encoder in `onMetaData` (kbit/s).
    pub audio_bitrate_kbps: Option<f64>,
    pub audio_sample_rate: Option<u32>,
    pub audio_channels: Option<u8>,
    /// Encoder name from `onMetaData` (e.g. "obs-output module (libobs version 30.0.0)").
    pub encoder: Option<String>,
    pub has_video: bool,
    pub has_audio: bool,
}

impl StreamInfo {
    /// Merge `onMetaData` fields. Codec/dimensions already known from sequence headers are kept.
    pub fn apply_metadata(&mut self, m: &HashMap<String, AmfValue>) {
        let num = |k: &str| {
            m.get(k)
                .and_then(AmfValue::as_number)
                .filter(|v| v.is_finite())
        };
        if self.width.is_none() {
            self.width = num("width").filter(|v| *v > 0.0).map(|v| v as u32);
        }
        if self.height.is_none() {
            self.height = num("height").filter(|v| *v > 0.0).map(|v| v as u32);
        }
        if let Some(fps) = num("framerate").or_else(|| num("fps")).filter(|v| *v > 0.0) {
            self.fps = Some(fps);
        }
        if let Some(v) = num("videodatarate").filter(|v| *v > 0.0) {
            self.video_bitrate_kbps = Some(v);
        }
        if let Some(v) = num("audiodatarate").filter(|v| *v > 0.0) {
            self.audio_bitrate_kbps = Some(v);
        }
        if self.audio_sample_rate.is_none() {
            self.audio_sample_rate = num("audiosamplerate")
                .filter(|v| *v > 0.0)
                .map(|v| v as u32);
        }
        if self.audio_channels.is_none() {
            self.audio_channels = num("audiochannels")
                .filter(|v| *v > 0.0)
                .map(|v| v as u8)
                .or_else(|| {
                    m.get("stereo")
                        .and_then(AmfValue::as_bool)
                        .map(|s| if s { 2 } else { 1 })
                });
        }
        if let Some(e) = m.get("encoder").and_then(AmfValue::as_str) {
            self.encoder = Some(e.chars().take(200).collect());
        }
        if self.video_codec.is_none() {
            self.video_codec = m
                .get("videocodecid")
                .and_then(metadata_video_codec)
                .map(str::to_string);
        }
        if self.audio_codec.is_none() {
            self.audio_codec = m
                .get("audiocodecid")
                .and_then(metadata_audio_codec)
                .map(str::to_string);
        }
        if self.video_codec.is_some() || self.width.is_some() {
            self.has_video = true;
        }
        if self.audio_codec.is_some() || self.audio_sample_rate.is_some() {
            self.has_audio = true;
        }
    }

    /// Update from a sequence header packet. Returns true if anything changed.
    pub fn apply_sequence_header(&mut self, p: &MediaPacket) -> bool {
        let before = self.clone();
        match p.kind {
            PacketKind::Video => {
                self.has_video = true;
                if let Some(c) = video_codec_name(&p.data) {
                    self.video_codec = Some(c.to_string());
                }
                if let Some(avc) = parse_avc_sequence_header(&p.data) {
                    self.video_codec_string = Some(avc.codec_string);
                    if avc.width.is_some() {
                        self.width = avc.width;
                        self.height = avc.height;
                    }
                }
            }
            PacketKind::Audio => {
                self.has_audio = true;
                if let Some(c) = audio_codec_name(&p.data) {
                    self.audio_codec = Some(c.to_string());
                }
                if let Some(aac) = parse_aac_sequence_header(&p.data) {
                    self.audio_codec_string = Some(aac.codec_string);
                    self.audio_sample_rate = Some(aac.sample_rate);
                    self.audio_channels = Some(aac.channels);
                }
            }
            PacketKind::Script => {}
        }
        *self != before
    }
}

fn metadata_video_codec(v: &AmfValue) -> Option<&'static str> {
    match v {
        AmfValue::Number(n) => match *n as i64 {
            2 => Some("h263"),
            4 | 5 => Some("vp6"),
            7 => Some("h264"),
            12 => Some("hevc"),
            13 => Some("av1"),
            _ => None,
        },
        AmfValue::String(s) => match s.as_str() {
            "avc1" => Some("h264"),
            "hvc1" | "hev1" => Some("hevc"),
            "av01" => Some("av1"),
            "vp09" => Some("vp9"),
            _ => None,
        },
        _ => None,
    }
}

fn metadata_audio_codec(v: &AmfValue) -> Option<&'static str> {
    match v {
        AmfValue::Number(n) => match *n as i64 {
            2 | 14 => Some("mp3"),
            10 => Some("aac"),
            11 => Some("speex"),
            _ => None,
        },
        AmfValue::String(s) => match s.as_str() {
            "mp4a" => Some("aac"),
            "Opus" | "opus" => Some("opus"),
            ".mp3" => Some("mp3"),
            _ => None,
        },
        _ => None,
    }
}

/// Bounded cache for late joiners: latest script tag, latest video/audio sequence
/// headers and every packet since the last video keyframe.
///
/// For streams with video, the cache always starts at a keyframe; when the GOP grows
/// beyond the duration/byte bounds it is dropped and caching resumes at the next
/// keyframe (new subscribers then wait for that keyframe). Audio-only streams keep a
/// sliding window bounded the same way.
#[derive(Debug)]
pub struct GopCache {
    metadata: Option<Arc<MediaPacket>>,
    video_header: Option<Arc<MediaPacket>>,
    audio_header: Option<Arc<MediaPacket>>,
    packets: VecDeque<Arc<MediaPacket>>,
    bytes: usize,
    has_video: bool,
    max_duration_ms: u32,
    max_bytes: usize,
}

impl Default for GopCache {
    fn default() -> Self {
        Self::new(GOP_CACHE_MAX_DURATION_MS, GOP_CACHE_MAX_BYTES)
    }
}

impl GopCache {
    pub fn new(max_duration_ms: u32, max_bytes: usize) -> Self {
        Self {
            metadata: None,
            video_header: None,
            audio_header: None,
            packets: VecDeque::new(),
            bytes: 0,
            has_video: false,
            max_duration_ms,
            max_bytes,
        }
    }

    fn clear_packets(&mut self) {
        self.packets.clear();
        self.bytes = 0;
    }

    fn append(&mut self, p: &Arc<MediaPacket>) {
        self.bytes += p.data.len();
        self.packets.push_back(Arc::clone(p));
    }

    fn span_ms(&self) -> u32 {
        match (self.packets.front(), self.packets.back()) {
            (Some(first), Some(last)) => {
                let d = last.timestamp.wrapping_sub(first.timestamp) as i32;
                d.max(0) as u32
            }
            _ => 0,
        }
    }

    fn over_limits(&self) -> bool {
        self.bytes > self.max_bytes || self.span_ms() > self.max_duration_ms
    }

    fn gop_started(&self) -> bool {
        self.packets
            .front()
            .is_some_and(|p| p.is_video() && p.is_keyframe)
    }

    pub fn push(&mut self, p: &Arc<MediaPacket>) {
        match p.kind {
            PacketKind::Script => {
                self.metadata = Some(Arc::clone(p));
                return;
            }
            PacketKind::Video if p.is_sequence_header => {
                self.has_video = true;
                // New decoder configuration: frames of the old one are useless.
                self.video_header = Some(Arc::clone(p));
                self.clear_packets();
                return;
            }
            PacketKind::Audio if p.is_sequence_header => {
                self.audio_header = Some(Arc::clone(p));
                return;
            }
            PacketKind::Video => {
                self.has_video = true;
                if p.is_keyframe {
                    self.clear_packets();
                    self.append(p);
                } else if self.gop_started() {
                    self.append(p);
                }
            }
            PacketKind::Audio => {
                if self.has_video {
                    if self.gop_started() {
                        self.append(p);
                    }
                } else {
                    self.append(p);
                    while self.packets.len() > 1 && self.over_limits() {
                        if let Some(old) = self.packets.pop_front() {
                            self.bytes -= old.data.len();
                        }
                    }
                }
            }
        }
        if self.has_video && self.over_limits() {
            // GOP too long/large: drop it, resume at the next keyframe.
            self.clear_packets();
        }
    }

    /// Catch-up packets in send order: script, video header, audio header, GOP.
    pub fn snapshot(&self) -> Vec<Arc<MediaPacket>> {
        let mut out = Vec::with_capacity(self.packets.len() + 3);
        out.extend(self.metadata.iter().cloned());
        out.extend(self.video_header.iter().cloned());
        out.extend(self.audio_header.iter().cloned());
        out.extend(self.packets.iter().cloned());
        out
    }

    pub fn len(&self) -> usize {
        self.packets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.packets.is_empty()
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn has_video_header(&self) -> bool {
        self.video_header.is_some()
    }

    pub fn has_audio_header(&self) -> bool {
        self.audio_header.is_some()
    }
}

/// Where the packets of a [`LiveStream`] come from.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase", tag = "kind", content = "node")]
pub enum StreamOrigin {
    /// A publisher connected to this node (RTMP ingest): this node owns the stream.
    #[default]
    Local,
    /// Relayed from the owner node (cluster mode, `cluster::relay`): no
    /// `stream_sessions` row, no recording, never relayed again.
    Remote(String),
}

impl StreamOrigin {
    pub fn is_local(&self) -> bool {
        matches!(self, StreamOrigin::Local)
    }

    /// The owner node of a relayed stream.
    pub fn remote_node(&self) -> Option<&str> {
        match self {
            StreamOrigin::Local => None,
            StreamOrigin::Remote(node) => Some(node),
        }
    }
}

/// Parameters for [`super::Hub::publish_start`].
#[derive(Debug, Clone)]
pub struct PublishInfo {
    pub broadcast_id: String,
    pub publisher_addr: Option<SocketAddr>,
    /// Id of the `stream_sessions` row of this live period, if any.
    pub stream_session_id: Option<String>,
    /// Cancelled by [`LiveStream::kick`]; the ingest closes the publisher connection
    /// when it fires.
    pub kick: CancellationToken,
    /// [`StreamOrigin::Local`] for publishers, `Remote(node)` for cluster relays.
    pub origin: StreamOrigin,
    /// Cluster mode: the `live_streams.session_id` claimed for this publish.
    pub claim_id: Option<String>,
}

impl PublishInfo {
    pub fn new(broadcast_id: impl Into<String>) -> Self {
        Self {
            broadcast_id: broadcast_id.into(),
            publisher_addr: None,
            stream_session_id: None,
            kick: CancellationToken::new(),
            origin: StreamOrigin::Local,
            claim_id: None,
        }
    }
}

pub struct LiveStream {
    /// Broadcast name (hub key, used in playback URLs).
    pub name: String,
    pub broadcast_id: String,
    pub started_at: DateTime<Utc>,
    pub publisher_addr: Option<SocketAddr>,
    pub stream_session_id: Option<String>,
    /// Publisher on this node, or relay of another node's stream (cluster mode).
    pub origin: StreamOrigin,
    /// Cluster mode: `live_streams.session_id` of this publish.
    pub claim_id: Option<String>,
    tx: broadcast::Sender<Arc<MediaPacket>>,
    gop: Mutex<GopCache>,
    info: RwLock<StreamInfo>,
    bytes_in: AtomicU64,
    packets: AtomicU64,
    last_packet_ms: AtomicI64,
    kick: CancellationToken,
    ended: CancellationToken,
    events: broadcast::Sender<HubEvent>,
}

impl std::fmt::Debug for LiveStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveStream")
            .field("name", &self.name)
            .field("started_at", &self.started_at)
            .field("publisher_addr", &self.publisher_addr)
            .field("origin", &self.origin)
            .finish_non_exhaustive()
    }
}

impl LiveStream {
    pub(crate) fn new(name: &str, info: PublishInfo, events: broadcast::Sender<HubEvent>) -> Self {
        let (tx, _) = broadcast::channel(STREAM_CHANNEL_CAPACITY);
        Self {
            name: name.to_string(),
            broadcast_id: info.broadcast_id,
            started_at: Utc::now(),
            publisher_addr: info.publisher_addr,
            stream_session_id: info.stream_session_id,
            origin: info.origin,
            claim_id: info.claim_id,
            tx,
            gop: Mutex::new(GopCache::default()),
            info: RwLock::new(StreamInfo::default()),
            bytes_in: AtomicU64::new(0),
            packets: AtomicU64::new(0),
            last_packet_ms: AtomicI64::new(Utc::now().timestamp_millis()),
            kick: info.kick,
            ended: CancellationToken::new(),
            events,
        }
    }

    /// Catch-up snapshot (headers first, then the current GOP) plus a receiver for
    /// every packet pushed after the snapshot. Atomic with respect to [`Self::push`].
    pub fn subscribe(&self) -> (Vec<Arc<MediaPacket>>, broadcast::Receiver<Arc<MediaPacket>>) {
        let gop = self.gop.lock().unwrap_or_else(|e| e.into_inner());
        let snapshot = gop.snapshot();
        let rx = self.tx.subscribe();
        drop(gop);
        (snapshot, rx)
    }

    /// Publish a packet to all subscribers (never blocks: slow receivers lag).
    pub fn push(&self, packet: MediaPacket) -> Arc<MediaPacket> {
        let packet = Arc::new(packet);
        self.bytes_in
            .fetch_add(packet.data.len() as u64, Ordering::Relaxed);
        self.packets.fetch_add(1, Ordering::Relaxed);
        self.last_packet_ms
            .store(Utc::now().timestamp_millis(), Ordering::Relaxed);

        if packet.is_sequence_header {
            let changed = {
                let mut info = self.info.write().unwrap_or_else(|e| e.into_inner());
                info.apply_sequence_header(&packet)
            };
            if changed {
                let _ = self.events.send(HubEvent::Metadata {
                    name: self.name.clone(),
                });
            }
        }

        let mut gop = self.gop.lock().unwrap_or_else(|e| e.into_inner());
        gop.push(&packet);
        // Sent under the cache lock so `subscribe` sees each packet exactly once.
        let _ = self.tx.send(Arc::clone(&packet));
        drop(gop);
        packet
    }

    /// Merge `onMetaData` into [`StreamInfo`] and emit `HubEvent::Metadata`.
    pub fn apply_metadata(&self, metadata: &HashMap<String, AmfValue>) {
        {
            let mut info = self.info.write().unwrap_or_else(|e| e.into_inner());
            info.apply_metadata(metadata);
        }
        let _ = self.events.send(HubEvent::Metadata {
            name: self.name.clone(),
        });
    }

    pub fn info(&self) -> StreamInfo {
        self.info.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Whether the cache currently holds video / audio sequence headers.
    pub fn has_headers(&self) -> (bool, bool) {
        let gop = self.gop.lock().unwrap_or_else(|e| e.into_inner());
        (gop.has_video_header(), gop.has_audio_header())
    }

    pub fn bytes_in(&self) -> u64 {
        self.bytes_in.load(Ordering::Relaxed)
    }

    pub fn packets(&self) -> u64 {
        self.packets.load(Ordering::Relaxed)
    }

    pub fn last_packet_at(&self) -> DateTime<Utc> {
        DateTime::from_timestamp_millis(self.last_packet_ms.load(Ordering::Relaxed))
            .unwrap_or_default()
    }

    /// Milliseconds since the last packet (or since start if none yet).
    pub fn idle_ms(&self) -> i64 {
        (Utc::now().timestamp_millis() - self.last_packet_ms.load(Ordering::Relaxed)).max(0)
    }

    /// A relay of another node's stream (cluster mode).
    pub fn is_relay(&self) -> bool {
        !self.origin.is_local()
    }

    /// Number of active packet receivers (FLV viewers, packager, recorder...).
    pub fn subscriber_count(&self) -> usize {
        self.tx.receiver_count()
    }

    /// Ask the ingest to disconnect the publisher.
    pub fn kick(&self) {
        self.kick.cancel();
    }

    pub fn is_kicked(&self) -> bool {
        self.kick.is_cancelled()
    }

    /// Token cancelled when the stream ends (publisher gone). Consumers select on it.
    pub fn ended_token(&self) -> CancellationToken {
        self.ended.clone()
    }

    pub fn is_ended(&self) -> bool {
        self.ended.is_cancelled()
    }

    pub(crate) fn mark_ended(&self) {
        self.ended.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    fn v(ts: u32, key: bool, len: usize) -> MediaPacket {
        let mut d = vec![if key { 0x17 } else { 0x27 }, 0x01, 0, 0, 0];
        d.resize(len.max(5), 0);
        MediaPacket::video(ts, Bytes::from(d))
    }
    fn a(ts: u32) -> MediaPacket {
        MediaPacket::audio(ts, Bytes::from_static(&[0xAF, 0x01, 0x21, 0x00]))
    }
    fn vh() -> MediaPacket {
        MediaPacket::video(
            0,
            Bytes::from_static(&[0x17, 0x00, 0, 0, 0, 1, 0x64, 0, 0x1f]),
        )
    }
    fn ah() -> MediaPacket {
        MediaPacket::audio(0, Bytes::from_static(&[0xAF, 0x00, 0x11, 0x90]))
    }
    fn stream() -> LiveStream {
        let (ev, _) = broadcast::channel(16);
        LiveStream::new("t", PublishInfo::new("id"), ev)
    }
    fn arc(p: MediaPacket) -> Arc<MediaPacket> {
        Arc::new(p)
    }

    #[test]
    fn gop_cache_starts_at_keyframe() {
        let mut c = GopCache::default();
        c.push(&arc(vh()));
        c.push(&arc(ah()));
        c.push(&arc(v(0, false, 10))); // before first keyframe: dropped
        c.push(&arc(a(5))); // audio before first keyframe: dropped
        assert!(c.is_empty());
        c.push(&arc(v(40, true, 10)));
        c.push(&arc(a(45)));
        c.push(&arc(v(73, false, 10)));
        let s = c.snapshot();
        assert_eq!(s.len(), 2 + 3);
        assert!(s[0].is_sequence_header && s[0].is_video());
        assert!(s[1].is_sequence_header && s[1].is_audio());
        assert!(s[2].is_keyframe);
        // A new keyframe resets the GOP.
        c.push(&arc(v(2040, true, 10)));
        assert_eq!(c.len(), 1);
        assert_eq!(c.snapshot().len(), 3);
    }

    #[test]
    fn gop_cache_bounds() {
        let mut c = GopCache::new(4000, 1000);
        c.push(&arc(vh()));
        c.push(&arc(v(0, true, 100)));
        c.push(&arc(v(4001, false, 100))); // exceeds 4 s: dropped
        assert!(c.is_empty());
        c.push(&arc(v(4034, false, 100))); // still waiting for keyframe
        assert!(c.is_empty());
        c.push(&arc(v(4067, true, 100)));
        assert_eq!(c.len(), 1);
        for i in 0..20 {
            c.push(&arc(v(4100 + i, false, 100)));
        }
        assert!(c.bytes() <= 1000);
        assert!(c.is_empty() || c.snapshot()[1].is_keyframe);
    }

    #[test]
    fn gop_cache_audio_only_window() {
        let mut c = GopCache::new(1000, 1 << 20);
        c.push(&arc(ah()));
        for i in 0..100 {
            c.push(&arc(a(i * 23)));
        }
        let s = c.snapshot();
        let span = s.last().unwrap().timestamp - s[1].timestamp;
        assert!(span <= 1000, "span {span}");
        assert!(s[0].is_sequence_header);
    }

    #[test]
    fn script_tag_kept_first() {
        let mut c = GopCache::default();
        c.push(&arc(vh()));
        c.push(&arc(MediaPacket::script(0, Bytes::from_static(&[2, 0, 0]))));
        assert_eq!(c.snapshot()[0].kind, PacketKind::Script);
    }

    #[tokio::test]
    async fn subscribe_snapshot_and_receiver_are_contiguous() {
        let s = stream();
        s.push(vh());
        s.push(ah());
        s.push(v(0, true, 10));
        s.push(a(10));
        let (snap, mut rx) = s.subscribe();
        assert_eq!(snap.len(), 4);
        s.push(v(33, false, 10));
        s.push(a(43));
        let p1 = rx.recv().await.unwrap();
        let p2 = rx.recv().await.unwrap();
        assert_eq!((p1.timestamp, p2.timestamp), (33, 43));
        assert!(rx.try_recv().is_err());
        assert_eq!(s.subscriber_count(), 1);
        assert_eq!(s.packets(), 6);
        // Info from sequence headers.
        let info = s.info();
        assert_eq!(info.video_codec.as_deref(), Some("h264"));
        assert_eq!(info.audio_codec.as_deref(), Some("aac"));
        assert_eq!(info.audio_sample_rate, Some(48000));
    }

    #[tokio::test]
    async fn concurrent_subscribe_never_loses_packets() {
        let s = Arc::new(stream());
        s.push(vh());
        s.push(v(0, true, 10));
        let producer = {
            let s = Arc::clone(&s);
            tokio::task::spawn_blocking(move || {
                for i in 1..2000u32 {
                    s.push(v(i, false, 10));
                }
            })
        };
        let mut checks = Vec::new();
        for _ in 0..50 {
            let (snap, rx) = s.subscribe();
            checks.push((snap, rx));
            tokio::task::yield_now().await;
        }
        producer.await.unwrap();
        for (snap, mut rx) in checks {
            // Last timestamp in the snapshot + 1 must be the first received one.
            let last = snap.last().unwrap().timestamp;
            if last == 1999 {
                assert!(rx.try_recv().is_err());
                continue;
            }
            let first = rx.recv().await.unwrap().timestamp;
            assert_eq!(
                first,
                last + 1,
                "gap or duplicate between snapshot and receiver"
            );
        }
    }

    #[test]
    fn metadata_merge() {
        let mut info = StreamInfo::default();
        let mut m = HashMap::new();
        m.insert("width".into(), AmfValue::Number(1920.0));
        m.insert("height".into(), AmfValue::Number(1080.0));
        m.insert("framerate".into(), AmfValue::Number(30.0));
        m.insert("videocodecid".into(), AmfValue::Number(7.0));
        m.insert("audiocodecid".into(), AmfValue::String("mp4a".into()));
        m.insert("encoder".into(), AmfValue::String("obs".into()));
        info.apply_metadata(&m);
        assert_eq!(
            (info.width, info.height, info.fps),
            (Some(1920), Some(1080), Some(30.0))
        );
        assert_eq!(info.video_codec.as_deref(), Some("h264"));
        assert_eq!(info.audio_codec.as_deref(), Some("aac"));
        assert!(info.has_video && info.has_audio);
    }
}
