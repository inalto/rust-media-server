//! `WhepPublisher`: the per-broadcast WebRTC media source shared by every WHEP session
//! of one live stream.
//!
//! - Video: FLV H.264 frames → Annex-B access units (SPS/PPS injected before IDRs, see
//!   [`super::h264`]) → RTP packets (RFC 6184 payloader, MTU 1200), packetized **once**
//!   per stream. RTP timestamps follow the FLV decode timestamps on a 90 kHz clock
//!   (frames go out in decode order: B-frames are not reordered, so encoders should
//!   not produce them — `tune=zerolatency` / OBS "bframes=0").
//! - Audio: Opus RTP packets produced by ffmpeg ([`super::audio`]).
//!
//! Packets are fanned out as [`MediaChunk`]s (one per video access unit, one per audio
//! packet) through a bounded `broadcast` channel; each session stamps its own SSRC,
//! payload type and sequence numbers. The webrtc-rs 0.21 track objects bind to a single
//! peer connection, which is why the sharing happens here, before the peer connections.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use bytes::Bytes;
use rtc::rtp;
use rtc::rtp::codec::h264::H264Payloader;
use rtc::rtp::packetizer::Payloader;
use tokio::sync::{broadcast, watch};
use tokio_util::sync::CancellationToken;

use super::h264::{ParameterSets, avcc_to_annexb, parse_avcc};
use crate::hub::{LiveStream, MediaPacket};
use crate::packager::cmaf::{VideoTag, parse_video_tag};

/// RTP payload size limit (fits in a 1280-byte IPv6 minimum MTU with SRTP overhead).
pub const RTP_MTU: usize = 1200;
/// Video RTP clock rate.
pub const VIDEO_CLOCK_RATE: u32 = 90_000;
/// Capacity of the per-publisher chunk channel (≈ 10 s of 30 fps video + Opus).
pub const CHUNK_CHANNEL_CAPACITY: usize = 1024;
/// Frame timestamp steps larger than this (or negative) are encoder discontinuities.
const MAX_FRAME_STEP_MS: u32 = 10_000;
const DEFAULT_FRAME_MS: u32 = 33;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkKind {
    Video,
    Audio,
}

/// RTP packets of one video access unit or one audio frame. Header fields that are
/// per session (SSRC, payload type, sequence number) are placeholders.
#[derive(Debug, Clone)]
pub struct MediaChunk {
    pub kind: ChunkKind,
    /// Video access unit with an IDR slice (a session may start here).
    pub idr: bool,
    pub packets: Vec<rtp::Packet>,
}

impl MediaChunk {
    pub fn audio(packet: rtp::Packet) -> Self {
        Self {
            kind: ChunkKind::Audio,
            idr: false,
            packets: vec![packet],
        }
    }
}

/// H.264 profile of the stream (from the AVC sequence header).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoProfile {
    pub profile_idc: u8,
    pub constraints: u8,
    pub level_idc: u8,
}

impl From<&ParameterSets> for VideoProfile {
    fn from(p: &ParameterSets) -> Self {
        Self {
            profile_idc: p.profile_idc,
            constraints: p.constraints,
            level_idc: p.level_idc,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum PublisherState {
    /// Waiting for the H.264 sequence header.
    Starting,
    Ready(VideoProfile),
    /// The stream's video cannot be sent over WebRTC (HEVC, AV1...).
    Unsupported(String),
    /// The stream ended or the publisher was stopped.
    Ended,
}

/// Maps FLV decode timestamps (ms, possibly wrapping or jumping) to a continuous
/// millisecond clock. Discontinuities advance by the last frame duration.
#[derive(Debug, Default)]
pub struct MediaClock {
    last: Option<u32>,
    ext_ms: u64,
    last_step: u32,
}

impl MediaClock {
    pub fn advance(&mut self, ts: u32) -> u64 {
        if let Some(prev) = self.last {
            let d = ts.wrapping_sub(prev) as i32;
            let step = if d > 0 && d as u32 <= MAX_FRAME_STEP_MS {
                self.last_step = d as u32;
                d as u32
            } else if d == 0 {
                1 // never give two frames the same RTP timestamp
            } else if self.last_step > 0 {
                self.last_step
            } else {
                DEFAULT_FRAME_MS
            };
            self.ext_ms += step as u64;
        }
        self.last = Some(ts);
        self.ext_ms
    }
}

/// Result of feeding one FLV packet to the [`VideoPacketizer`].
#[derive(Debug)]
pub enum VideoEvent {
    Nothing,
    Config(VideoProfile),
    Chunk(MediaChunk),
    Unsupported(String),
}

/// FLV H.264 → RTP, shared by all sessions of a stream.
pub struct VideoPacketizer {
    params: Option<ParameterSets>,
    payloader: H264Payloader,
    clock: MediaClock,
    ts_base: u32,
    seq: u16,
    need_idr: bool,
}

impl Default for VideoPacketizer {
    fn default() -> Self {
        Self::new()
    }
}

impl VideoPacketizer {
    pub fn new() -> Self {
        Self {
            params: None,
            payloader: H264Payloader::default(),
            clock: MediaClock::default(),
            ts_base: rand::random(),
            seq: rand::random(),
            need_idr: true,
        }
    }

    /// After a lag (packets lost): drop frames until the next IDR.
    pub fn on_lag(&mut self) {
        self.need_idr = true;
    }

    pub fn push(&mut self, p: &MediaPacket) -> VideoEvent {
        if !p.is_video() {
            return VideoEvent::Nothing;
        }
        match parse_video_tag(&p.data) {
            VideoTag::Config(vc) => match parse_avcc(&vc.avcc) {
                Some(ps) => {
                    let profile = VideoProfile::from(&ps);
                    if self.params.as_ref() != Some(&ps) {
                        // New decoder configuration: restart at its first IDR.
                        self.need_idr = true;
                        self.params = Some(ps);
                    }
                    VideoEvent::Config(profile)
                }
                None => VideoEvent::Nothing,
            },
            VideoTag::Frame { keyframe, data, .. } => {
                let Some(params) = &self.params else {
                    return VideoEvent::Nothing;
                };
                let ms = self.clock.advance(p.timestamp);
                let Some(au) = avcc_to_annexb(&data, params, keyframe) else {
                    return VideoEvent::Nothing;
                };
                if self.need_idr && !au.idr {
                    return VideoEvent::Nothing;
                }
                self.need_idr = false;
                let timestamp = self
                    .ts_base
                    .wrapping_add((ms * (VIDEO_CLOCK_RATE as u64 / 1000)) as u32);
                match self.packetize(&au.data, timestamp) {
                    Some(packets) => VideoEvent::Chunk(MediaChunk {
                        kind: ChunkKind::Video,
                        idr: au.idr,
                        packets,
                    }),
                    None => VideoEvent::Nothing,
                }
            }
            VideoTag::Ignore => VideoEvent::Nothing,
            VideoTag::Unsupported(codec) => VideoEvent::Unsupported(codec),
        }
    }

    fn packetize(&mut self, annexb: &Bytes, timestamp: u32) -> Option<Vec<rtp::Packet>> {
        let payloads = self.payloader.payload(RTP_MTU, annexb).ok()?;
        if payloads.is_empty() {
            return None;
        }
        let last = payloads.len() - 1;
        Some(
            payloads
                .into_iter()
                .enumerate()
                .map(|(i, payload)| {
                    let seq = self.seq;
                    self.seq = self.seq.wrapping_add(1);
                    rtp::Packet {
                        header: rtp::header::Header {
                            version: 2,
                            marker: i == last,
                            sequence_number: seq,
                            timestamp,
                            ..Default::default()
                        },
                        payload,
                    }
                })
                .collect(),
        )
    }
}

/// The shared media source of one live stream (see the module docs).
pub struct WhepPublisher {
    pub id: u64,
    pub name: String,
    stream: Weak<LiveStream>,
    tx: broadcast::Sender<Arc<MediaChunk>>,
    state: watch::Sender<PublisherState>,
    /// Cancelled when the publisher stops (stream ended, idle, shutdown).
    pub stop: CancellationToken,
    sessions: AtomicUsize,
    audio: AtomicBool,
}

impl std::fmt::Debug for WhepPublisher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WhepPublisher")
            .field("id", &self.id)
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl WhepPublisher {
    pub fn new(id: u64, stream: &Arc<LiveStream>, audio: bool) -> Arc<Self> {
        let (tx, _) = broadcast::channel(CHUNK_CHANNEL_CAPACITY);
        let (state, _) = watch::channel(PublisherState::Starting);
        Arc::new(Self {
            id,
            name: stream.name.clone(),
            stream: Arc::downgrade(stream),
            tx,
            state,
            stop: CancellationToken::new(),
            sessions: AtomicUsize::new(0),
            audio: AtomicBool::new(audio),
        })
    }

    /// Whether this publisher serves this publishing session of the broadcast.
    pub fn is_for(&self, stream: &Arc<LiveStream>) -> bool {
        std::ptr::eq(self.stream.as_ptr(), Arc::as_ptr(stream))
    }

    pub fn sender(&self) -> broadcast::Sender<Arc<MediaChunk>> {
        self.tx.clone()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Arc<MediaChunk>> {
        self.tx.subscribe()
    }

    pub fn state(&self) -> PublisherState {
        self.state.borrow().clone()
    }

    pub fn set_state(&self, s: PublisherState) {
        self.state.send_if_modified(|cur| {
            // Ended is final.
            if *cur == s || *cur == PublisherState::Ended {
                return false;
            }
            *cur = s;
            true
        });
    }

    /// Wait (at most `timeout`) until the publisher is no longer `Starting`.
    pub async fn wait_ready(&self, timeout: Duration) -> PublisherState {
        let mut rx = self.state.subscribe();
        let _ =
            tokio::time::timeout(timeout, rx.wait_for(|s| *s != PublisherState::Starting)).await;
        self.state()
    }

    /// Whether new sessions get an Opus track.
    pub fn has_audio(&self) -> bool {
        self.audio.load(Ordering::Relaxed)
    }

    /// The Opus encoder is running: offer audio to new sessions.
    pub fn enable_audio(&self) {
        self.audio.store(true, Ordering::Relaxed);
    }

    pub fn sessions(&self) -> usize {
        self.sessions.load(Ordering::Relaxed)
    }

    /// Count a session (reserved before negotiation, released by the slot's drop).
    pub fn reserve(self: &Arc<Self>) -> SessionSlot {
        self.sessions.fetch_add(1, Ordering::Relaxed);
        SessionSlot(Arc::clone(self))
    }
}

/// A session's claim on a publisher (keeps it from idling out).
pub struct SessionSlot(Arc<WhepPublisher>);

impl SessionSlot {
    pub fn publisher(&self) -> &Arc<WhepPublisher> {
        &self.0
    }
}

impl Drop for SessionSlot {
    fn drop(&mut self) {
        self.0.sessions.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FLV AVC sequence header (libx264 High profile 720p SPS + PPS).
    fn avc_seq_header() -> Bytes {
        const SPS: [u8; 26] = [
            0x67, 0x64, 0x00, 0x1f, 0xac, 0xd9, 0x40, 0x50, 0x05, 0xbb, 0x01, 0x10, 0x00, 0x00,
            0x03, 0x00, 0x10, 0x00, 0x00, 0x03, 0x03, 0xc0, 0xf1, 0x83, 0x19, 0x60,
        ];
        let mut body = vec![0x17, 0x00, 0, 0, 0, 1, 0x64, 0x00, 0x1f, 0xff, 0xe1];
        body.extend_from_slice(&(SPS.len() as u16).to_be_bytes());
        body.extend_from_slice(&SPS);
        body.extend_from_slice(&[1, 0, 4, 0x68, 0xeb, 0xe3, 0xcb]);
        Bytes::from(body)
    }

    fn frame(ts: u32, key: bool, nals: &[&[u8]]) -> MediaPacket {
        let mut body = vec![if key { 0x17 } else { 0x27 }, 1, 0, 0, 0];
        for n in nals {
            body.extend_from_slice(&(n.len() as u32).to_be_bytes());
            body.extend_from_slice(n);
        }
        MediaPacket::video(ts, Bytes::from(body))
    }

    #[test]
    fn clock_absorbs_discontinuities() {
        let mut c = MediaClock::default();
        assert_eq!(c.advance(1000), 0);
        assert_eq!(c.advance(1033), 33);
        assert_eq!(c.advance(1066), 66);
        assert_eq!(c.advance(1066), 67, "equal timestamps still advance");
        assert_eq!(c.advance(500), 100, "backwards jump: last step");
        assert_eq!(c.advance(533), 133);
        assert_eq!(c.advance(90_000), 166, "forward jump: last step");
        // 32-bit wrap is a normal step.
        let mut c = MediaClock::default();
        c.advance(u32::MAX - 10);
        assert_eq!(c.advance(22), 33);
    }

    #[test]
    fn packetizer_waits_for_idr_and_injects_parameter_sets() {
        let mut v = VideoPacketizer::new();
        // No config yet: frames are dropped.
        assert!(matches!(
            v.push(&frame(0, true, &[&[0x65, 1, 2]])),
            VideoEvent::Nothing
        ));
        let VideoEvent::Config(profile) = v.push(&MediaPacket::video(0, avc_seq_header())) else {
            panic!("config expected")
        };
        assert_eq!(profile.profile_idc, 0x64);
        // Inter frame before the first IDR: dropped.
        assert!(matches!(
            v.push(&frame(10, false, &[&[0x41, 1]])),
            VideoEvent::Nothing
        ));
        let VideoEvent::Chunk(idr) = v.push(&frame(33, true, &[&[0x65, 1, 2, 3]])) else {
            panic!("chunk expected")
        };
        assert!(idr.idr);
        // SPS+PPS aggregated in a STAP-A, then the IDR slice; marker on the last packet.
        assert_eq!(idr.packets.len(), 2);
        assert_eq!(idr.packets[0].payload[0] & 0x1F, 24, "STAP-A");
        assert_eq!(idr.packets[1].payload[0] & 0x1F, 5);
        assert!(!idr.packets[0].header.marker && idr.packets[1].header.marker);
        let ts0 = idr.packets[0].header.timestamp;
        let VideoEvent::Chunk(p) = v.push(&frame(66, false, &[&[0x41, 7]])) else {
            panic!("chunk expected")
        };
        assert!(!p.idr);
        assert_eq!(p.packets[0].header.timestamp.wrapping_sub(ts0), 33 * 90);
        assert_eq!(
            p.packets[0].header.sequence_number,
            idr.packets[1].header.sequence_number.wrapping_add(1)
        );
        // Large frame: FU-A fragments, all with the same timestamp.
        let big = vec![0x41; 5000];
        let VideoEvent::Chunk(p) = v.push(&frame(99, false, &[&big])) else {
            panic!("chunk expected")
        };
        assert!(p.packets.len() >= 5);
        assert!(p.packets.iter().all(|x| x.payload.len() <= RTP_MTU));
        assert!(
            p.packets
                .iter()
                .all(|x| x.header.timestamp == ts0.wrapping_add(66 * 90))
        );
        assert_eq!(p.packets[0].payload[0] & 0x1F, 28, "FU-A");
        // After a lag, inter frames are dropped until the next IDR.
        v.on_lag();
        assert!(matches!(
            v.push(&frame(132, false, &[&[0x41, 8]])),
            VideoEvent::Nothing
        ));
        assert!(matches!(
            v.push(&frame(165, true, &[&[0x65, 9]])),
            VideoEvent::Chunk(MediaChunk { idr: true, .. })
        ));
    }

    #[test]
    fn packetizer_reports_unsupported_codecs() {
        let mut v = VideoPacketizer::new();
        let hevc = MediaPacket::video(0, Bytes::from_static(&[0x90, b'h', b'v', b'c', b'1', 1]));
        assert!(matches!(v.push(&hevc), VideoEvent::Unsupported(c) if c == "hevc"));
        let audio = MediaPacket::audio(0, Bytes::from_static(&[0xAF, 1, 2]));
        assert!(matches!(v.push(&audio), VideoEvent::Nothing));
    }

    #[tokio::test]
    async fn publisher_state_and_slots() {
        let hub = crate::hub::Hub::new();
        let s = hub
            .publish_start("p", crate::hub::PublishInfo::new("id"))
            .unwrap();
        let p = WhepPublisher::new(1, &s, true);
        assert!(p.is_for(&s));
        assert_eq!(p.state(), PublisherState::Starting);
        let st = p.wait_ready(Duration::from_millis(20)).await;
        assert_eq!(st, PublisherState::Starting);
        let profile = VideoProfile {
            profile_idc: 0x42,
            constraints: 0xe0,
            level_idc: 0x1f,
        };
        p.set_state(PublisherState::Ready(profile));
        assert_eq!(
            p.wait_ready(Duration::from_secs(1)).await,
            PublisherState::Ready(profile)
        );
        p.set_state(PublisherState::Ended);
        p.set_state(PublisherState::Ready(profile));
        assert_eq!(p.state(), PublisherState::Ended, "ended is final");
        let a = p.reserve();
        let b = p.reserve();
        assert_eq!(p.sessions(), 2);
        drop(a);
        assert_eq!(b.publisher().sessions(), 1);
        drop(b);
        assert_eq!(p.sessions(), 0);
        hub.publish_end("p");
    }
}
