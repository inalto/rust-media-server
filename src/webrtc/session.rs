//! One WHEP session: an `rtc` (sans-I/O, webrtc-rs 0.21 core) peer connection, the
//! negotiation of its SDP answer, and the task that drives it.
//!
//! The task owns the `RTCPeerConnection` and multiplexes: datagrams routed to it by the
//! shared [`UdpMux`], media chunks from the stream's [`WhepPublisher`], protocol timers,
//! and cancellation (DELETE, broadcast disabled, stream ended, idle publisher, shutdown).
//! Outgoing datagrams are sent from the shared UDP port. Media flows only once the peer
//! connection is `Connected` (DTLS-SRTP up); video starts at an IDR access unit and
//! resynchronises at the next IDR after a lag. Inbound RTCP (receiver reports, NACK,
//! PLI) is consumed by the interceptors (NACK responder, sender reports) and otherwise
//! drained: a keyframe cannot be forced upstream, viewers wait for the next GOP.

use std::net::{IpAddr, SocketAddr};
use std::pin::pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rtc::ice::mdns::MulticastDnsMode;
use rtc::interceptor::Registry;
use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::configuration::RTCConfigurationBuilder;
use rtc::peer_connection::configuration::interceptor_registry::register_default_interceptors;
use rtc::peer_connection::configuration::media_engine::{
    MIME_TYPE_H264, MIME_TYPE_OPUS, MediaEngine,
};
use rtc::peer_connection::configuration::setting_engine::SettingEngineBuilder;
use rtc::peer_connection::event::RTCPeerConnectionEvent;
use rtc::peer_connection::sdp::RTCSessionDescription;
use rtc::peer_connection::state::{RTCIceConnectionState, RTCPeerConnectionState};
use rtc::peer_connection::transport::{
    CandidateConfig, CandidateHostConfig, RTCIceCandidate, RTCIceCandidateInit,
};
use rtc::peer_connection::{RTCPeerConnection, RTCPeerConnectionBuilder};
use rtc::rtp;
use rtc::rtp_transceiver::RTCRtpSenderId;
use rtc::rtp_transceiver::rtp_sender::{
    RTCPFeedback, RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters,
    RTCRtpEncodingParameters, RtpCodecKind,
};
use rtc::sansio::Protocol;
use rtc::shared::{TaggedBytesMut, TransportContext, TransportProtocol};
use tokio::sync::broadcast::{self, error::RecvError};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::audio::OPUS_CLOCK_RATE;
use super::h264::choose_h264_payload_type;
use super::mux::{Inbound, UdpMux};
use super::publisher::{ChunkKind, MediaChunk, SessionSlot, VIDEO_CLOCK_RATE, VideoProfile};
use crate::metrics::{Protocol as ViewerProtocol, ViewerSession, ViewerTracker};

/// A session that is not `Connected` within this time is closed.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// A session `Disconnected` for longer than this is closed.
const DISCONNECTED_GRACE: Duration = Duration::from_secs(10);
/// Upper bound of a single protocol-timer sleep.
const MAX_SLEEP: Duration = Duration::from_secs(1);

/// H.264 (packetization-mode=1) profiles the answer can accept, with the payload types
/// used when they are not dictated by the offer.
const H264_PROFILES: [(u8, &str); 4] = [
    (102, "42001f"),
    (125, "42e01f"),
    (116, "4d001f"),
    (123, "64001f"),
];

/// Why negotiation failed (mapped to 400 / 415 / 500 by the HTTP layer).
#[derive(Debug, thiserror::Error)]
pub enum NegotiateError {
    #[error("invalid SDP offer: {0}")]
    BadOffer(String),
    #[error("{0}")]
    Unsupported(String),
    #[error("WebRTC negotiation failed: {0}")]
    Internal(String),
}

/// Inputs of [`negotiate`].
pub struct NegotiateOptions<'a> {
    pub ice_lite: bool,
    pub ufrag: &'a str,
    pub pwd: &'a str,
    /// Advertised host candidates (`ip:udp_port`).
    pub candidates: &'a [SocketAddr],
    pub audio: bool,
    pub profile: Option<VideoProfile>,
    /// MediaStream id announced in the answer (`a=msid`).
    pub stream_id: &'a str,
}

/// A sender of the session: its id in the peer connection, SSRC, negotiated payload
/// type and the session's own RTP sequence counter.
#[derive(Debug, Clone, Copy)]
pub struct SenderInfo {
    pub id: RTCRtpSenderId,
    pub ssrc: u32,
    pub payload_type: u8,
    seq: u16,
}

impl SenderInfo {
    fn stamp(&mut self, mut p: rtp::Packet) -> rtp::Packet {
        p.header.ssrc = self.ssrc;
        p.header.payload_type = self.payload_type;
        p.header.sequence_number = self.seq;
        self.seq = self.seq.wrapping_add(1);
        p
    }
}

/// A negotiated (not yet connected) peer connection and its SDP answer.
pub struct Negotiated {
    pub pc: RTCPeerConnection,
    pub answer: String,
    pub video: SenderInfo,
    pub audio: Option<SenderInfo>,
}

fn internal(e: impl std::fmt::Display) -> NegotiateError {
    NegotiateError::Internal(e.to_string())
}

fn media_engine(audio: bool) -> Result<MediaEngine, NegotiateError> {
    let mut me = MediaEngine::default();
    let feedback = vec![
        RTCPFeedback {
            typ: "ccm".into(),
            parameter: "fir".into(),
        },
        RTCPFeedback {
            typ: "nack".into(),
            parameter: String::new(),
        },
        RTCPFeedback {
            typ: "nack".into(),
            parameter: "pli".into(),
        },
    ];
    for (pt, plid) in H264_PROFILES {
        me.register_codec(
            RTCRtpCodecParameters {
                rtp_codec: RTCRtpCodec {
                    mime_type: MIME_TYPE_H264.into(),
                    clock_rate: VIDEO_CLOCK_RATE,
                    channels: 0,
                    sdp_fmtp_line: format!(
                        "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id={plid}"
                    ),
                    rtcp_feedback: feedback.clone(),
                },
                payload_type: pt,
            },
            RtpCodecKind::Video,
        )
        .map_err(internal)?;
    }
    if audio {
        me.register_codec(
            RTCRtpCodecParameters {
                rtp_codec: RTCRtpCodec {
                    mime_type: MIME_TYPE_OPUS.into(),
                    clock_rate: OPUS_CLOCK_RATE,
                    channels: 2,
                    sdp_fmtp_line: "minptime=10;useinbandfec=1".into(),
                    rtcp_feedback: vec![],
                },
                payload_type: super::audio::OPUS_PT,
            },
            RtpCodecKind::Audio,
        )
        .map_err(internal)?;
    }
    Ok(me)
}

fn track(stream_id: &str, kind: RtpCodecKind, ssrc: u32) -> MediaStreamTrack {
    let label = if kind == RtpCodecKind::Video {
        "video"
    } else {
        "audio"
    };
    MediaStreamTrack::new(
        stream_id.to_string(),
        format!("{stream_id}-{label}"),
        label.to_string(),
        kind,
        vec![RTCRtpEncodingParameters {
            rtp_coding_parameters: RTCRtpCodingParameters {
                ssrc: Some(ssrc),
                ..Default::default()
            },
            ..Default::default()
        }],
    )
}

/// Negotiated `(payload type, fmtp)` of a sender's codecs with MIME type `mime`.
fn sender_codecs(pc: &mut RTCPeerConnection, id: RTCRtpSenderId, mime: &str) -> Vec<(u8, String)> {
    pc.rtp_sender(id)
        .map(|mut s| {
            s.get_parameters()
                .rtp_parameters
                .codecs
                .iter()
                .filter(|c| c.rtp_codec.mime_type.eq_ignore_ascii_case(mime))
                .map(|c| (c.payload_type, c.rtp_codec.sdp_fmtp_line.clone()))
                .collect()
        })
        .unwrap_or_default()
}

/// Build a peer connection for a WHEP `offer`: H.264 video (+ Opus audio), the given
/// ICE credentials and host candidates, ICE-lite as configured. Returns the answer.
pub fn negotiate(offer: &str, o: &NegotiateOptions<'_>) -> Result<Negotiated, NegotiateError> {
    let now = Instant::now();
    let mut me = media_engine(o.audio)?;
    let registry = register_default_interceptors(Registry::new(), &mut me).map_err(internal)?;
    let settings = SettingEngineBuilder::new()
        .with_lite(o.ice_lite)
        .with_ice_credentials(o.ufrag.to_string(), o.pwd.to_string())
        .with_multicast_dns_mode(MulticastDnsMode::Disabled)
        .with_ice_timeouts(
            Some(Duration::from_secs(5)),
            Some(Duration::from_secs(25)),
            Some(Duration::from_secs(2)),
        )
        .build();
    let mut pc = RTCPeerConnectionBuilder::new()
        .with_configuration(RTCConfigurationBuilder::new().build())
        .with_setting_engine(settings)
        .with_media_engine(me)
        .with_interceptor_registry(registry)
        .build(now)
        .map_err(internal)?;

    let video_ssrc = rand::random::<u32>();
    let video_id = pc
        .add_track(track(o.stream_id, RtpCodecKind::Video, video_ssrc))
        .map_err(internal)?;
    let audio_ssrc = rand::random::<u32>();
    let audio_id = if o.audio {
        Some(
            pc.add_track(track(o.stream_id, RtpCodecKind::Audio, audio_ssrc))
                .map_err(internal)?,
        )
    } else {
        None
    };

    let desc = RTCSessionDescription::offer(offer.to_string())
        .map_err(|e| NegotiateError::BadOffer(e.to_string()))?;
    pc.set_remote_description(now, desc)
        .map_err(|e| NegotiateError::BadOffer(e.to_string()))?;

    for addr in o.candidates {
        let candidate = CandidateHostConfig {
            base_config: CandidateConfig {
                network: "udp".to_owned(),
                address: addr.ip().to_string(),
                port: addr.port(),
                component: 1,
                ..Default::default()
            },
            ..Default::default()
        }
        .new_candidate_host()
        .map_err(internal)?;
        let init = RTCIceCandidate::from(&candidate)
            .to_json()
            .map_err(internal)?;
        pc.add_local_candidate(init).map_err(internal)?;
    }
    // End of candidates (gathering complete): the answer is final.
    pc.add_local_candidate(RTCIceCandidateInit::default())
        .map_err(internal)?;

    let answer = pc
        .create_answer(None)
        .map_err(|e| NegotiateError::BadOffer(e.to_string()))?;
    pc.set_local_description(now, answer).map_err(internal)?;
    let answer = pc
        .local_description()
        .map(|d| d.sdp)
        .ok_or_else(|| internal("no local description"))?;

    let video_codecs = sender_codecs(&mut pc, video_id, MIME_TYPE_H264);
    let profile = o.profile.map(|p| (p.profile_idc, p.constraints));
    let video_pt = choose_h264_payload_type(&video_codecs, profile).ok_or_else(|| {
        NegotiateError::Unsupported("the offer has no H.264 video with packetization-mode=1".into())
    })?;
    let audio = match audio_id {
        Some(id) => sender_codecs(&mut pc, id, MIME_TYPE_OPUS)
            .first()
            .map(|(pt, _)| SenderInfo {
                id,
                ssrc: audio_ssrc,
                payload_type: *pt,
                seq: rand::random(),
            }),
        None => None,
    };
    Ok(Negotiated {
        pc,
        answer,
        video: SenderInfo {
            id: video_id,
            ssrc: video_ssrc,
            payload_type: video_pt,
            seq: rand::random(),
        },
        audio,
    })
}

/// Everything a running session needs.
pub struct SessionTask {
    pub id: String,
    pub mux_id: u64,
    pub name: String,
    pub negotiated: Negotiated,
    pub inbound: mpsc::Receiver<Inbound>,
    pub media: broadcast::Receiver<Arc<MediaChunk>>,
    pub mux: Arc<UdpMux>,
    /// Local address the inbound datagrams are attributed to (first host candidate).
    pub local_addr: SocketAddr,
    /// DELETE, broadcast disabled, server shutdown.
    pub cancel: CancellationToken,
    /// The publisher stopped (stream ended or idle).
    pub publisher_stop: CancellationToken,
    pub slot: SessionSlot,
    pub tracker: Arc<ViewerTracker>,
    pub ip: IpAddr,
    pub user_agent: String,
}

/// Why a session ended (logged).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndReason {
    Cancelled,
    StreamEnded,
    Failed,
    Closed,
    Disconnected,
    ConnectTimeout,
}

struct Driver {
    pc: RTCPeerConnection,
    mux: Arc<UdpMux>,
    local_addr: SocketAddr,
    connected: bool,
    connected_at: Option<Instant>,
    disconnected_since: Option<Instant>,
    end: Option<EndReason>,
    video: SenderInfo,
    audio: Option<SenderInfo>,
    video_started: bool,
    viewer: Option<ViewerSession>,
    bytes_out: u64,
    name: String,
    id: String,
}

impl Driver {
    /// Send every queued datagram.
    async fn flush(&mut self) {
        while let Some(msg) = self.pc.poll_write() {
            match self.mux.send(&msg.message, msg.transport.peer_addr).await {
                Ok(n) => self.bytes_out += n as u64,
                Err(e) => {
                    tracing::debug!(task = "webrtc-session", session = %self.id, error = %e, "UDP send failed")
                }
            }
        }
    }

    /// Handle state changes; drain application messages (inbound RTCP).
    fn drain_events(&mut self, tracker: &Arc<ViewerTracker>, ip: IpAddr, ua: &str) {
        while let Some(event) = self.pc.poll_event() {
            match event {
                RTCPeerConnectionEvent::OnConnectionStateChangeEvent(state) => {
                    tracing::debug!(task = "webrtc-session", session = %self.id, %state, "peer connection state");
                    match state {
                        RTCPeerConnectionState::Connected => {
                            self.disconnected_since = None;
                            if !self.connected {
                                self.connected = true;
                                self.connected_at = Some(Instant::now());
                                self.viewer =
                                    Some(tracker.open(&self.name, ViewerProtocol::WebRtc, ip, ua));
                                tracing::info!(task = "webrtc-session", broadcast = %self.name, session = %self.id, "WebRTC viewer connected");
                            }
                        }
                        RTCPeerConnectionState::Disconnected => {
                            self.disconnected_since.get_or_insert_with(Instant::now);
                        }
                        RTCPeerConnectionState::Failed => self.end = Some(EndReason::Failed),
                        RTCPeerConnectionState::Closed => self.end = Some(EndReason::Closed),
                        _ => {}
                    }
                }
                RTCPeerConnectionEvent::OnIceConnectionStateChangeEvent(state) => {
                    tracing::debug!(task = "webrtc-session", session = %self.id, %state, "ICE state");
                    match state {
                        RTCIceConnectionState::Failed => self.end = Some(EndReason::Failed),
                        RTCIceConnectionState::Closed => self.end = Some(EndReason::Closed),
                        RTCIceConnectionState::Disconnected => {
                            self.disconnected_since.get_or_insert_with(Instant::now);
                        }
                        RTCIceConnectionState::Connected | RTCIceConnectionState::Completed => {
                            self.disconnected_since = None;
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        // RTCP from the browser (receiver reports, NACK/PLI) is handled by the
        // interceptors; nothing to do with what reaches the application.
        while self.pc.poll_read().is_some() {}
    }

    fn write_chunk(&mut self, chunk: &MediaChunk) {
        let now = Instant::now();
        let sender = match chunk.kind {
            ChunkKind::Video => {
                if !self.video_started {
                    if !chunk.idr {
                        return;
                    }
                    self.video_started = true;
                }
                &mut self.video
            }
            ChunkKind::Audio => match self.audio.as_mut() {
                Some(a) => a,
                None => return,
            },
        };
        let id = sender.id;
        let packets: Vec<rtp::Packet> = chunk
            .packets
            .iter()
            .map(|p| sender.stamp(p.clone()))
            .collect();
        let Some(mut rtp_sender) = self.pc.rtp_sender(id) else {
            return;
        };
        for p in packets {
            if let Err(e) = rtp_sender.write_rtp(now, p) {
                tracing::trace!(task = "webrtc-session", session = %self.id, error = %e, "write_rtp failed");
            }
        }
    }
}

/// Drive the session until it ends; returns why.
pub async fn run(t: SessionTask) -> EndReason {
    let SessionTask {
        id,
        mux_id,
        name,
        negotiated,
        mut inbound,
        mut media,
        mux,
        local_addr,
        cancel,
        publisher_stop,
        slot,
        tracker,
        ip,
        user_agent,
    } = t;
    let started = Instant::now();
    let mut d = Driver {
        pc: negotiated.pc,
        mux: Arc::clone(&mux),
        local_addr,
        connected: false,
        connected_at: None,
        disconnected_since: None,
        end: None,
        video: negotiated.video,
        audio: negotiated.audio,
        video_started: false,
        viewer: None,
        bytes_out: 0,
        name: name.clone(),
        id: id.clone(),
    };
    let mut lagged: u64 = 0;
    let reason = loop {
        d.flush().await;
        d.drain_events(&tracker, ip, &user_agent);
        d.flush().await;
        if let Some(r) = d.end {
            break r;
        }
        let now = Instant::now();
        if !d.connected && now.duration_since(started) > CONNECT_TIMEOUT {
            break EndReason::ConnectTimeout;
        }
        if let Some(since) = d.disconnected_since
            && now.duration_since(since) > DISCONNECTED_GRACE
        {
            break EndReason::Disconnected;
        }
        let deadline =
            d.pc.poll_timeout()
                .map_or(now + MAX_SLEEP, |t| t.min(now + MAX_SLEEP));
        if deadline <= now {
            if let Err(e) = d.pc.handle_timeout(now) {
                tracing::debug!(task = "webrtc-session", session = %id, error = %e, "timeout handling failed");
            }
            continue;
        }
        let sleep = pin!(tokio::time::sleep_until(deadline.into()));
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break EndReason::Cancelled,
            _ = publisher_stop.cancelled() => break EndReason::StreamEnded,
            msg = inbound.recv() => {
                let Some(msg) = msg else { break EndReason::Cancelled };
                let tagged = TaggedBytesMut {
                    now: msg.at,
                    transport: TransportContext {
                        local_addr: d.local_addr,
                        peer_addr: msg.peer,
                        ecn: None,
                        transport_protocol: TransportProtocol::UDP,
                    },
                    message: msg.data,
                };
                if let Err(e) = d.pc.handle_read(tagged) {
                    tracing::trace!(task = "webrtc-session", session = %id, error = %e, "datagram rejected");
                }
            }
            chunk = media.recv() => match chunk {
                Ok(chunk) => {
                    if d.connected {
                        d.write_chunk(&chunk);
                    }
                }
                Err(RecvError::Lagged(n)) => {
                    lagged += n;
                    // Video resumes at the next IDR; audio just continues.
                    d.video_started = false;
                    tracing::warn!(task = "webrtc-session", broadcast = %name, session = %id, skipped = n, "WebRTC viewer lagging; waiting for the next keyframe");
                }
                Err(RecvError::Closed) => break EndReason::StreamEnded,
            },
            _ = sleep => {
                if let Err(e) = d.pc.handle_timeout(Instant::now()) {
                    tracing::debug!(task = "webrtc-session", session = %id, error = %e, "timeout handling failed");
                }
            }
        }
    };
    let _ = d.pc.close();
    d.flush().await;
    mux.unregister(mux_id);
    let connected_secs = d.connected_at.map(|t| t.elapsed().as_secs());
    tracing::info!(
        task = "webrtc-session",
        broadcast = %name,
        session = %id,
        reason = ?reason,
        connected_secs = ?connected_secs,
        bytes_out = d.bytes_out,
        lagged,
        "WebRTC session closed"
    );
    drop(d.viewer.take());
    drop(slot);
    reason
}
