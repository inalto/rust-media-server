//! AAC → Opus for WebRTC: one ffmpeg process per publisher.
//!
//! Browsers only guarantee Opus (and G.711) for WebRTC audio, so the stream's audio is
//! fed to ffmpeg as an audio-only FLV on stdin (header, AAC sequence header, audio tags
//! rebased to 0) and re-encoded to Opus 48 kHz stereo, 20 ms frames, which ffmpeg sends
//! as RTP (payload type [`OPUS_PT`]) to a UDP socket bound on 127.0.0.1. The packets keep
//! ffmpeg's 48 kHz timestamps; each session only rewrites SSRC, payload type and sequence
//! numbers. ffmpeg's RTCP goes to the same socket and is ignored.
//!
//! The process is restarted with exponential backoff if it exits while the stream is
//! live. Writes to its stdin are async (`tokio::process`) and the hub receiver absorbs
//! a stalled ffmpeg by lagging (no unbounded buffering).

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use rtc::rtp;
use rtc::shared::marshal::Unmarshal;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UdpSocket;
use tokio::sync::broadcast::{self, error::RecvError};
use tokio_util::sync::CancellationToken;

use super::publisher::MediaChunk;
use crate::hub::packet::{audio_codec_name, flv_file_header, write_flv_tag};
use crate::hub::{LiveStream, MediaPacket, PacketKind};

/// RTP payload type ffmpeg is told to use (rewritten per session to the negotiated one).
pub const OPUS_PT: u8 = 111;
/// Opus RTP clock rate.
pub const OPUS_CLOCK_RATE: u32 = 48_000;
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// A run longer than this resets the restart backoff.
const STABLE_RUN: Duration = Duration::from_secs(30);
/// ffmpeg stderr lines logged per run (the rest is discarded).
const MAX_LOG_LINES: usize = 20;

/// ffmpeg command line (after the executable) for an RTP/Opus output to `port`.
pub fn ffmpeg_args(port: u16, bitrate_kbps: u32) -> Vec<String> {
    [
        "-hide_banner",
        "-loglevel",
        "warning",
        "-fflags",
        "+nobuffer",
        "-analyzeduration",
        "0",
        // Minimal probing: every probed byte (32 KB ≈ 2 s of AAC) would become
        // permanent extra latency; the FLV header and AAC config are enough.
        "-probesize",
        "32",
        "-f",
        "flv",
        "-i",
        "pipe:0",
        "-vn",
        "-c:a",
        "libopus",
        "-application",
        "lowdelay",
        "-frame_duration",
        "20",
        "-b:a",
    ]
    .iter()
    .map(|s| s.to_string())
    .chain([
        format!("{bitrate_kbps}k"),
        "-ar".into(),
        "48000".into(),
        "-ac".into(),
        "2".into(),
        "-f".into(),
        "rtp".into(),
        "-payload_type".into(),
        OPUS_PT.to_string(),
        format!("rtp://127.0.0.1:{port}?rtcpport={port}&pkt_size=1200"),
    ])
    .collect()
}

/// Parse one datagram from ffmpeg: an RTP packet with payload type `pt` (RTCP and
/// anything else yields `None`).
pub fn parse_rtp(buf: &[u8], pt: u8) -> Option<rtp::Packet> {
    if buf.len() < 12 || buf[0] >> 6 != 2 || (200..=206).contains(&buf[1]) {
        return None;
    }
    let mut b = buf;
    let p = rtp::Packet::unmarshal(&mut b).ok()?;
    (p.header.payload_type == pt && !p.payload.is_empty()).then_some(p)
}

/// Builds the audio-only FLV fed to ffmpeg: file header, then the AAC sequence header
/// and audio frames with timestamps rebased to the first frame.
#[derive(Debug, Default)]
pub struct FlvAudioFeed {
    started: bool,
    base: Option<u32>,
}

impl FlvAudioFeed {
    /// FLV bytes for `p` (`None` for non-audio packets). The first call also emits
    /// the FLV file header.
    pub fn bytes_for(&mut self, p: &MediaPacket) -> Option<Bytes> {
        if p.kind != PacketKind::Audio {
            return None;
        }
        let mut buf = BytesMut::with_capacity(p.flv_size() + 13);
        if !self.started {
            self.started = true;
            buf.extend_from_slice(&flv_file_header(false, true));
        }
        let timestamp = if p.is_sequence_header {
            0
        } else {
            let base = *self.base.get_or_insert(p.timestamp);
            p.timestamp.saturating_sub(base)
        };
        let rebased = MediaPacket {
            timestamp,
            ..p.clone()
        };
        write_flv_tag(&mut buf, &rebased);
        Some(buf.freeze())
    }
}

/// Keep an Opus encoder running for `stream` until it ends or `stop` fires.
pub async fn run(
    stream: Arc<LiveStream>,
    ffmpeg: PathBuf,
    bitrate_kbps: u32,
    tx: broadcast::Sender<Arc<MediaChunk>>,
    stop: CancellationToken,
) {
    let ended = stream.ended_token();
    let name = stream.name.clone();
    let mut backoff = Duration::from_secs(1);
    loop {
        if ended.is_cancelled() || stop.is_cancelled() {
            break;
        }
        let started = Instant::now();
        match run_once(&stream, &ffmpeg, bitrate_kbps, &tx, &stop, &ended).await {
            Ok(()) => {}
            Err(e) => {
                tracing::warn!(task = "webrtc-audio", broadcast = %name, error = %e, "Opus encoder failed")
            }
        }
        if ended.is_cancelled() || stop.is_cancelled() {
            break;
        }
        if started.elapsed() > STABLE_RUN {
            backoff = Duration::from_secs(1);
        }
        tracing::warn!(task = "webrtc-audio", broadcast = %name, retry_in_secs = backoff.as_secs(), "ffmpeg exited; restarting");
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            _ = ended.cancelled() => break,
            _ = stop.cancelled() => break,
        }
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
    tracing::debug!(task = "webrtc-audio", broadcast = %name, "stopped");
}

/// One ffmpeg run: returns when ffmpeg exits, the stream ends or `stop` fires.
async fn run_once(
    stream: &Arc<LiveStream>,
    ffmpeg: &Path,
    bitrate_kbps: u32,
    tx: &broadcast::Sender<Arc<MediaChunk>>,
    stop: &CancellationToken,
    ended: &CancellationToken,
) -> anyhow::Result<()> {
    let socket = UdpSocket::bind("127.0.0.1:0").await?;
    let port = socket.local_addr()?.port();
    let mut child = tokio::process::Command::new(ffmpeg)
        .args(ffmpeg_args(port, bitrate_kbps))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("ffmpeg stdin unavailable"))?;
    if let Some(stderr) = child.stderr.take() {
        let name = stream.name.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            let mut logged = 0;
            while let Ok(Some(line)) = lines.next_line().await {
                if logged < MAX_LOG_LINES {
                    logged += 1;
                    let line: String = line.chars().take(300).collect();
                    tracing::debug!(task = "webrtc-audio", broadcast = %name, "ffmpeg: {line}");
                }
            }
        });
    }
    tracing::debug!(task = "webrtc-audio", broadcast = %stream.name, port, "ffmpeg Opus encoder started");

    let (snapshot, mut rx) = stream.subscribe();
    let mut feed = FlvAudioFeed::default();
    // Only the AAC sequence header from the catch-up: old frames would only add delay.
    let mut header_sent = false;
    for p in snapshot
        .iter()
        .filter(|p| p.is_audio() && p.is_sequence_header)
    {
        if let Some(b) = feed.bytes_for(p) {
            stdin.write_all(&b).await?;
            header_sent = true;
        }
    }
    drop(snapshot);

    let mut buf = vec![0u8; 2048];
    let mut packets: u64 = 0;
    let result = loop {
        tokio::select! {
            biased;
            _ = stop.cancelled() => break Ok(()),
            _ = ended.cancelled() => break Ok(()),
            status = child.wait() => {
                break Err(anyhow::anyhow!("ffmpeg exited ({})", status.map(|s| s.to_string()).unwrap_or_else(|e| e.to_string())));
            }
            r = socket.recv_from(&mut buf) => {
                let Ok((n, from)) = r else { continue };
                if !from.ip().is_loopback() {
                    continue;
                }
                if let Some(p) = parse_rtp(&buf[..n], OPUS_PT) {
                    packets += 1;
                    let _ = tx.send(Arc::new(MediaChunk::audio(p)));
                }
            }
            r = rx.recv() => match r {
                Ok(p) => {
                    if !p.is_audio() {
                        continue;
                    }
                    // AAC frames are useless to ffmpeg before the AudioSpecificConfig.
                    let aac = audio_codec_name(&p.data) == Some("aac");
                    if aac && !p.is_sequence_header && !header_sent {
                        continue;
                    }
                    header_sent |= p.is_sequence_header;
                    if let Some(b) = feed.bytes_for(&p)
                        && let Err(e) = stdin.write_all(&b).await
                    {
                        break Err(anyhow::anyhow!("writing to ffmpeg: {e}"));
                    }
                }
                Err(RecvError::Lagged(n)) => {
                    tracing::warn!(task = "webrtc-audio", broadcast = %stream.name, skipped = n, "Opus encoder lagging; audio packets dropped");
                }
                Err(RecvError::Closed) => break Ok(()),
            },
        }
    };
    drop(stdin);
    let _ = child.start_kill();
    let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
    tracing::debug!(task = "webrtc-audio", broadcast = %stream.name, packets, "ffmpeg Opus encoder stopped");
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use rtc::shared::marshal::Marshal;

    #[test]
    fn ffmpeg_command_line() {
        let a = ffmpeg_args(40000, 96);
        let s = a.join(" ");
        assert!(s.contains("-analyzeduration 0 -probesize 32 -f flv"));
        assert!(s.contains("-f flv -i pipe:0 -vn -c:a libopus -application lowdelay -frame_duration 20 -b:a 96k -ar 48000 -ac 2"));
        assert!(s.ends_with(
            "-f rtp -payload_type 111 rtp://127.0.0.1:40000?rtcpport=40000&pkt_size=1200"
        ));
    }

    #[test]
    fn rtp_parsing_filters_payload_type_and_rtcp() {
        let pkt = rtp::Packet {
            header: rtp::header::Header {
                version: 2,
                marker: true,
                payload_type: OPUS_PT,
                sequence_number: 4242,
                timestamp: 960_000,
                ssrc: 0x1234_5678,
                ..Default::default()
            },
            payload: Bytes::from_static(&[0xfc, 0xff, 0xfe]),
        };
        let raw = pkt.marshal().unwrap();
        let parsed = parse_rtp(&raw, OPUS_PT).unwrap();
        assert_eq!(parsed.header.sequence_number, 4242);
        assert_eq!(parsed.header.timestamp, 960_000);
        assert_eq!(parsed.header.ssrc, 0x1234_5678);
        assert_eq!(&parsed.payload[..], &[0xfc, 0xff, 0xfe]);
        // Other payload type, RTCP sender report, garbage, truncated.
        assert!(parse_rtp(&raw, 96).is_none());
        let sr = [0x80, 200, 0, 6, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0];
        assert!(parse_rtp(&sr, OPUS_PT).is_none());
        assert!(parse_rtp(&[0x00; 20], OPUS_PT).is_none());
        for n in 0..raw.len() {
            let _ = parse_rtp(&raw[..n], OPUS_PT);
        }
    }

    #[test]
    fn flv_feed_rebases_audio() {
        let mut feed = FlvAudioFeed::default();
        assert!(
            feed.bytes_for(&MediaPacket::video(
                0,
                Bytes::from_static(&[0x17, 1, 0, 0, 0])
            ))
            .is_none()
        );
        let header = feed
            .bytes_for(&MediaPacket::audio(
                5000,
                Bytes::from_static(&[0xAF, 0, 0x11, 0x90]),
            ))
            .unwrap();
        assert_eq!(&header[..3], b"FLV");
        assert_eq!(header[4], 0x04, "audio-only FLV header");
        // Sequence header tag at timestamp 0.
        assert_eq!(&header[13 + 4..13 + 8], &[0, 0, 0, 0]);
        let f1 = feed
            .bytes_for(&MediaPacket::audio(
                5021,
                Bytes::from_static(&[0xAF, 1, 0x21]),
            ))
            .unwrap();
        assert_eq!(f1[0], 8);
        assert_eq!(&f1[4..7], &[0, 0, 0], "first frame rebased to 0");
        let f2 = feed
            .bytes_for(&MediaPacket::audio(
                5042,
                Bytes::from_static(&[0xAF, 1, 0x21]),
            ))
            .unwrap();
        assert_eq!(&f2[4..7], &[0, 0, 21]);
        // Out-of-order timestamp before the base saturates at 0.
        let f3 = feed
            .bytes_for(&MediaPacket::audio(
                10,
                Bytes::from_static(&[0xAF, 1, 0x21]),
            ))
            .unwrap();
        assert_eq!(&f3[4..7], &[0, 0, 0]);
    }
}
