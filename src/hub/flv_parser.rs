//! Incremental FLV byte-stream parser (the inverse of [`super::packet::flv_tag_bytes`]),
//! used by the cluster relay to turn `GET /internal/relay/{name}.flv` back into
//! [`MediaPacket`]s.
//!
//! Tolerant by design: bytes arrive in arbitrary chunks (partial headers and tags are
//! buffered), the header length field is honoured (anything between the 9-byte header
//! and `DataOffset` is skipped), the extended timestamp byte is applied, the
//! `PreviousTagSize` fields are skipped without being trusted, and tags of unknown types
//! (or encrypted ones) are dropped. Memory is bounded: one tag body (at most 16 MiB, the
//! 24-bit size field) plus the unparsed remainder of the last chunk.

use bytes::{Buf, BytesMut};

use super::MediaPacket;
use super::packet::{FLV_TAG_AUDIO, FLV_TAG_HEADER_LEN, FLV_TAG_SCRIPT, FLV_TAG_VIDEO};

/// Upper bound of `DataOffset` in the FLV header (the spec says 9; be lenient).
const MAX_HEADER_LEN: u32 = 1024;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FlvParseError {
    #[error("not an FLV stream (bad signature)")]
    BadSignature,
    #[error("invalid FLV header length {0}")]
    BadHeaderLength(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Waiting for the 9-byte header.
    Header,
    /// Skipping the rest of the header (`DataOffset - 9`) plus `PreviousTagSize0`.
    Skip(usize),
    /// Waiting for a tag header (11 bytes), its body and its `PreviousTagSize`.
    Tags,
}

/// Feed bytes with [`Self::push`], then drain packets with [`Self::next_packet`].
#[derive(Debug)]
pub struct FlvStreamParser {
    buf: BytesMut,
    state: State,
    has_video: bool,
    has_audio: bool,
    /// Tags dropped (unknown type, encrypted, empty body).
    skipped: u64,
}

impl Default for FlvStreamParser {
    fn default() -> Self {
        Self::new()
    }
}

impl FlvStreamParser {
    pub fn new() -> Self {
        Self {
            buf: BytesMut::with_capacity(64 * 1024),
            state: State::Header,
            has_video: false,
            has_audio: false,
            skipped: 0,
        }
    }

    /// Append received bytes.
    pub fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// Header flags (`(video, audio)`), known once the header was parsed.
    pub fn header_flags(&self) -> (bool, bool) {
        (self.has_video, self.has_audio)
    }

    /// Tags dropped so far.
    pub fn skipped(&self) -> u64 {
        self.skipped
    }

    /// Bytes buffered but not parsed yet.
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }

    /// The next complete packet, `Ok(None)` when more bytes are needed.
    pub fn next_packet(&mut self) -> Result<Option<MediaPacket>, FlvParseError> {
        loop {
            match self.state {
                State::Header => {
                    if self.buf.len() < 9 {
                        return Ok(None);
                    }
                    if &self.buf[..3] != b"FLV" {
                        return Err(FlvParseError::BadSignature);
                    }
                    let flags = self.buf[4];
                    self.has_audio = flags & 0x04 != 0;
                    self.has_video = flags & 0x01 != 0;
                    let offset =
                        u32::from_be_bytes([self.buf[5], self.buf[6], self.buf[7], self.buf[8]]);
                    if !(9..=MAX_HEADER_LEN).contains(&offset) {
                        return Err(FlvParseError::BadHeaderLength(offset));
                    }
                    self.buf.advance(9);
                    // Rest of the header, then PreviousTagSize0.
                    self.state = State::Skip(offset as usize - 9 + 4);
                }
                State::Skip(n) => {
                    let take = n.min(self.buf.len());
                    self.buf.advance(take);
                    if take < n {
                        self.state = State::Skip(n - take);
                        return Ok(None);
                    }
                    self.state = State::Tags;
                }
                State::Tags => {
                    if self.buf.len() < FLV_TAG_HEADER_LEN {
                        return Ok(None);
                    }
                    let h = &self.buf[..FLV_TAG_HEADER_LEN];
                    // Bits 7-6 reserved, bit 5 = filter (encrypted), bits 4-0 = type.
                    let filtered = h[0] & 0x20 != 0;
                    let tag_type = h[0] & 0x1F;
                    let size = u32::from_be_bytes([0, h[1], h[2], h[3]]) as usize;
                    let timestamp = u32::from_be_bytes([h[7], h[4], h[5], h[6]]);
                    let total = FLV_TAG_HEADER_LEN + size + 4;
                    if self.buf.len() < total {
                        // Make room for the whole tag at once (bounded by 16 MiB + 15).
                        self.buf.reserve(total - self.buf.len());
                        return Ok(None);
                    }
                    self.buf.advance(FLV_TAG_HEADER_LEN);
                    let body = self.buf.split_to(size).freeze();
                    // PreviousTagSize: skipped, never trusted.
                    self.buf.advance(4);
                    if filtered || body.is_empty() {
                        self.skipped += 1;
                        continue;
                    }
                    let packet = match tag_type {
                        FLV_TAG_VIDEO => MediaPacket::video(timestamp, body),
                        FLV_TAG_AUDIO => MediaPacket::audio(timestamp, body),
                        FLV_TAG_SCRIPT => MediaPacket::script(timestamp, body),
                        _ => {
                            self.skipped += 1;
                            continue;
                        }
                    };
                    return Ok(Some(packet));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::packet::{PacketKind, flv_file_header, flv_tag_bytes};
    use bytes::{BufMut, Bytes};

    fn sample() -> Vec<MediaPacket> {
        vec![
            MediaPacket::script(0, Bytes::from_static(&[2, 0, 10, b'o', b'n'])),
            MediaPacket::video(
                0,
                Bytes::from_static(&[0x17, 0x00, 0, 0, 0, 1, 0x64, 0, 0x1f]),
            ),
            MediaPacket::audio(0, Bytes::from_static(&[0xAF, 0x00, 0x11, 0x90])),
            MediaPacket::video(40, Bytes::from_static(&[0x17, 0x01, 0, 0, 0, 9, 9, 9])),
            MediaPacket::audio(45, Bytes::from_static(&[0xAF, 0x01, 0x21, 0x22])),
            // Extended timestamp byte (> 2^24 ms ≈ 4.66 h).
            MediaPacket::video(0x0123_4567, Bytes::from_static(&[0x27, 0x01, 0, 0, 0, 7])),
        ]
    }

    fn stream_bytes(packets: &[MediaPacket]) -> Vec<u8> {
        let mut out = flv_file_header(true, true).to_vec();
        for p in packets {
            out.extend_from_slice(&flv_tag_bytes(p));
        }
        out
    }

    fn assert_same(a: &MediaPacket, b: &MediaPacket) {
        assert_eq!(a.kind, b.kind);
        assert_eq!(a.timestamp, b.timestamp);
        assert_eq!(a.data, b.data);
        assert_eq!(a.is_keyframe, b.is_keyframe);
        assert_eq!(a.is_sequence_header, b.is_sequence_header);
    }

    #[test]
    fn round_trip_with_flv_tag_bytes() {
        let packets = sample();
        let bytes = stream_bytes(&packets);
        let mut p = FlvStreamParser::new();
        p.push(&bytes);
        let mut out = Vec::new();
        while let Some(pkt) = p.next_packet().unwrap() {
            out.push(pkt);
        }
        assert_eq!(p.header_flags(), (true, true));
        assert_eq!(out.len(), packets.len());
        for (a, b) in out.iter().zip(&packets) {
            assert_same(a, b);
        }
        assert_eq!(out[0].kind, PacketKind::Script);
        assert!(out[1].is_sequence_header && out[3].is_keyframe);
        assert_eq!(out[5].timestamp, 0x0123_4567);
        assert_eq!(p.buffered(), 0);
    }

    #[test]
    fn partial_reads_byte_by_byte_and_random_chunks() {
        let packets = sample();
        let bytes = stream_bytes(&packets);
        // One byte at a time.
        let mut p = FlvStreamParser::new();
        let mut out = Vec::new();
        for b in &bytes {
            p.push(std::slice::from_ref(b));
            while let Some(pkt) = p.next_packet().unwrap() {
                out.push(pkt);
            }
        }
        assert_eq!(out.len(), packets.len());
        // Uneven chunks.
        for chunk in [2usize, 7, 13, 64] {
            let mut p = FlvStreamParser::new();
            let mut n = 0;
            for c in bytes.chunks(chunk) {
                p.push(c);
                while let Some(pkt) = p.next_packet().unwrap() {
                    assert_same(&pkt, &packets[n]);
                    n += 1;
                }
            }
            assert_eq!(n, packets.len(), "chunk {chunk}");
        }
    }

    #[test]
    fn tolerant_header_and_tags() {
        // DataOffset 12 (3 extra header bytes), a bogus PreviousTagSize, an unknown tag
        // type and an encrypted tag are all skipped.
        let mut b = BytesMut::new();
        b.put_slice(b"FLV");
        b.put_u8(1);
        b.put_u8(0x01);
        b.put_u32(12);
        b.put_slice(&[0xAA, 0xBB, 0xCC]);
        b.put_u32(0xDEAD_BEEF); // PreviousTagSize0 should be 0: ignored
        let unknown = [
            7u8, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0x55, 0x55, 0xFF, 0xFF, 0xFF, 0xFF,
        ];
        b.put_slice(&unknown);
        let mut encrypted = flv_tag_bytes(&MediaPacket::video(
            1,
            Bytes::from_static(&[0x17, 0x01, 0, 0, 0]),
        ))
        .to_vec();
        encrypted[0] |= 0x20;
        b.put_slice(&encrypted);
        let real = MediaPacket::audio(5, Bytes::from_static(&[0xAF, 0x01, 1]));
        let mut tag = flv_tag_bytes(&real).to_vec();
        let n = tag.len();
        tag[n - 4..].copy_from_slice(&[0, 0, 0, 1]); // wrong PreviousTagSize
        b.put_slice(&tag);
        let mut p = FlvStreamParser::new();
        p.push(&b);
        let pkt = p.next_packet().unwrap().unwrap();
        assert_same(&pkt, &real);
        assert!(p.next_packet().unwrap().is_none());
        assert_eq!(p.skipped(), 2);
        assert_eq!(p.header_flags(), (true, false));
    }

    #[test]
    fn rejects_garbage() {
        let mut p = FlvStreamParser::new();
        p.push(b"<html>not flv</html>");
        assert!(matches!(p.next_packet(), Err(FlvParseError::BadSignature)));
        let mut p = FlvStreamParser::new();
        p.push(&[b'F', b'L', b'V', 1, 5, 0, 0, 0, 3]);
        assert!(matches!(
            p.next_packet(),
            Err(FlvParseError::BadHeaderLength(3))
        ));
        let mut p = FlvStreamParser::new();
        p.push(b"FL");
        assert!(matches!(p.next_packet(), Ok(None)));
    }
}
