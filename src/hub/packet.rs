//! `MediaPacket` (ARCHITECTURE.md §4.3) plus FLV serialization and codec-config helpers.
//!
//! A `MediaPacket` carries the FLV tag *body* exactly as received over RTMP; the
//! 11-byte FLV tag header is only produced on output by [`flv_tag_bytes`].

use std::collections::HashMap;

use bytes::{BufMut, Bytes, BytesMut};
use rtmp_rs::amf::AmfValue;
use serde::Serialize;

pub const FLV_TAG_AUDIO: u8 = 8;
pub const FLV_TAG_VIDEO: u8 = 9;
pub const FLV_TAG_SCRIPT: u8 = 18;

/// FLV header (9 bytes) + PreviousTagSize0 (4 bytes).
pub const FLV_HEADER_LEN: usize = 13;
/// FLV tag header length.
pub const FLV_TAG_HEADER_LEN: usize = 11;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PacketKind {
    Video,
    Audio,
    Script,
}

impl PacketKind {
    pub fn flv_tag_type(self) -> u8 {
        match self {
            PacketKind::Video => FLV_TAG_VIDEO,
            PacketKind::Audio => FLV_TAG_AUDIO,
            PacketKind::Script => FLV_TAG_SCRIPT,
        }
    }
}

#[derive(Debug, Clone)]
pub struct MediaPacket {
    pub kind: PacketKind,
    /// FLV timestamp in milliseconds, as received (monotonic-ish).
    pub timestamp: u32,
    /// FLV tag body (no 11-byte tag header), shared across subscribers.
    pub data: Bytes,
    /// Video only: frame type 1 (keyframe).
    pub is_keyframe: bool,
    /// AVC/HEVC/AAC (or enhanced RTMP) sequence header / decoder config record.
    pub is_sequence_header: bool,
}

impl MediaPacket {
    /// Video packet; keyframe / sequence-header flags derived from the tag body.
    pub fn video(timestamp: u32, data: Bytes) -> Self {
        Self {
            kind: PacketKind::Video,
            timestamp,
            is_keyframe: video_is_keyframe(&data),
            is_sequence_header: video_is_sequence_header(&data),
            data,
        }
    }

    /// Audio packet; sequence-header flag derived from the tag body.
    pub fn audio(timestamp: u32, data: Bytes) -> Self {
        Self {
            kind: PacketKind::Audio,
            timestamp,
            is_keyframe: false,
            is_sequence_header: audio_is_sequence_header(&data),
            data,
        }
    }

    /// Script data (AMF0, typically `onMetaData`).
    pub fn script(timestamp: u32, data: Bytes) -> Self {
        Self {
            kind: PacketKind::Script,
            timestamp,
            is_keyframe: false,
            is_sequence_header: false,
            data,
        }
    }

    pub fn is_video(&self) -> bool {
        self.kind == PacketKind::Video
    }

    pub fn is_audio(&self) -> bool {
        self.kind == PacketKind::Audio
    }

    /// Size of this packet once serialized as an FLV tag (header + body + PreviousTagSize).
    pub fn flv_size(&self) -> usize {
        FLV_TAG_HEADER_LEN + self.data.len() + 4
    }
}

// ---------------------------------------------------------------------------
// Flag detection (legacy FLV and Enhanced RTMP)
// ---------------------------------------------------------------------------

/// Enhanced RTMP video: `IsExHeader` bit set in the first byte.
pub fn video_is_enhanced(data: &[u8]) -> bool {
    data.first().is_some_and(|b| b & 0x80 != 0)
}

/// Video frame type 1 (keyframe). Legacy: `(b >> 4) == 1`; enhanced: `((b >> 4) & 7) == 1`.
pub fn video_is_keyframe(data: &[u8]) -> bool {
    let Some(&b) = data.first() else { return false };
    if b & 0x80 != 0 {
        (b >> 4) & 0x07 == 1
    } else {
        b >> 4 == 1
    }
}

/// AVC / HEVC (legacy codec id 7 / 12) with packet type 0, or enhanced RTMP
/// `PacketTypeSequenceStart` (0).
pub fn video_is_sequence_header(data: &[u8]) -> bool {
    let Some(&b) = data.first() else { return false };
    if b & 0x80 != 0 {
        return b & 0x0F == 0;
    }
    let codec = b & 0x0F;
    (codec == 7 || codec == 12) && data.get(1) == Some(&0)
}

/// AAC (sound format 10) with packet type 0, or enhanced audio (sound format 9)
/// `PacketTypeSequenceStart` (0).
pub fn audio_is_sequence_header(data: &[u8]) -> bool {
    let Some(&b) = data.first() else { return false };
    match b >> 4 {
        10 => data.get(1) == Some(&0),
        9 => b & 0x0F == 0,
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// FLV serialization
// ---------------------------------------------------------------------------

/// FLV file header (9 bytes) followed by PreviousTagSize0 = 0 (4 bytes).
pub fn flv_file_header(has_video: bool, has_audio: bool) -> Bytes {
    let mut flags = 0u8;
    if has_audio {
        flags |= 0x04;
    }
    if has_video {
        flags |= 0x01;
    }
    let mut b = BytesMut::with_capacity(FLV_HEADER_LEN);
    b.put_slice(b"FLV");
    b.put_u8(1);
    b.put_u8(flags);
    b.put_u32(9);
    b.put_u32(0);
    b.freeze()
}

/// Append one FLV tag (11-byte header + body + PreviousTagSize) to `buf`.
///
/// Bodies larger than the 24-bit FLV size field (16 MiB) cannot be represented; they
/// are truncated to keep the stream parseable (RTMP messages are limited to 24 bits too,
/// so this never happens with real input).
pub fn write_flv_tag(buf: &mut BytesMut, p: &MediaPacket) {
    let body = if p.data.len() > 0x00FF_FFFF {
        &p.data[..0x00FF_FFFF]
    } else {
        &p.data[..]
    };
    let size = body.len() as u32;
    buf.reserve(FLV_TAG_HEADER_LEN + body.len() + 4);
    buf.put_u8(p.kind.flv_tag_type());
    buf.put_slice(&size.to_be_bytes()[1..]);
    let ts = p.timestamp;
    buf.put_slice(&ts.to_be_bytes()[1..]); // lower 24 bits
    buf.put_u8((ts >> 24) as u8); // extended byte
    buf.put_slice(&[0, 0, 0]); // stream id
    buf.put_slice(body);
    buf.put_u32(FLV_TAG_HEADER_LEN as u32 + size);
}

/// Serialize one packet as a standalone FLV tag.
pub fn flv_tag_bytes(p: &MediaPacket) -> Bytes {
    let mut b = BytesMut::with_capacity(p.flv_size());
    write_flv_tag(&mut b, p);
    b.freeze()
}

/// Build the body of an `onMetaData` script tag (AMF0: string `"onMetaData"` + ECMA array).
pub fn on_metadata_body(metadata: &HashMap<String, AmfValue>) -> Bytes {
    rtmp_rs::amf::amf0::encode_all(&[
        AmfValue::String("onMetaData".into()),
        AmfValue::EcmaArray(metadata.clone()),
    ])
}

// ---------------------------------------------------------------------------
// Codec configuration parsing
// ---------------------------------------------------------------------------

/// Video codec name for a tag body ("h264", "hevc", "av1", "vp9", ...).
pub fn video_codec_name(data: &[u8]) -> Option<&'static str> {
    let &b = data.first()?;
    if b & 0x80 != 0 {
        return match data.get(1..5)? {
            b"avc1" => Some("h264"),
            b"hvc1" => Some("hevc"),
            b"av01" => Some("av1"),
            b"vp09" => Some("vp9"),
            b"vp08" => Some("vp8"),
            _ => None,
        };
    }
    match b & 0x0F {
        2 => Some("h263"),
        3 => Some("screenvideo"),
        4 | 5 => Some("vp6"),
        7 => Some("h264"),
        12 => Some("hevc"),
        13 => Some("av1"),
        _ => None,
    }
}

/// Audio codec name for a tag body ("aac", "mp3", "opus", ...).
pub fn audio_codec_name(data: &[u8]) -> Option<&'static str> {
    let &b = data.first()?;
    match b >> 4 {
        10 => Some("aac"),
        2 | 14 => Some("mp3"),
        11 => Some("speex"),
        7 => Some("pcma"),
        8 => Some("pcmu"),
        0 | 3 => Some("pcm"),
        4..=6 => Some("nellymoser"),
        9 => match data.get(1..5)? {
            b"mp4a" => Some("aac"),
            b"Opus" => Some("opus"),
            b"fLaC" => Some("flac"),
            b"ac-3" => Some("ac3"),
            b"ec-3" => Some("eac3"),
            b".mp3" => Some("mp3"),
            _ => None,
        },
        _ => None,
    }
}

/// Information extracted from an AVC sequence header.
#[derive(Debug, Clone, PartialEq)]
pub struct AvcInfo {
    pub profile: u8,
    pub compatibility: u8,
    pub level: u8,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// RFC 6381 codec string, e.g. `avc1.64001f`.
    pub codec_string: String,
}

/// Parse an AVC sequence header tag body (legacy `0x17 0x00 cts(3) AVCDecoderConfigurationRecord`
/// or enhanced `ExHeader 'avc1' record`).
pub fn parse_avc_sequence_header(data: &[u8]) -> Option<AvcInfo> {
    if !video_is_sequence_header(data) {
        return None;
    }
    let record = if video_is_enhanced(data) {
        if data.get(1..5)? != b"avc1" {
            return None;
        }
        data.get(5..)?
    } else {
        if data[0] & 0x0F != 7 {
            return None;
        }
        data.get(5..)?
    };
    // AVCDecoderConfigurationRecord
    if record.len() < 7 || record[0] != 1 {
        return None;
    }
    let (profile, compatibility, level) = (record[1], record[2], record[3]);
    let num_sps = (record[5] & 0x1F) as usize;
    let mut dims = None;
    if num_sps > 0 {
        let len = u16::from_be_bytes([*record.get(6)?, *record.get(7)?]) as usize;
        let sps = record.get(8..8 + len)?;
        dims = parse_h264_sps_dimensions(sps);
    }
    Some(AvcInfo {
        profile,
        compatibility,
        level,
        width: dims.map(|d| d.0),
        height: dims.map(|d| d.1),
        codec_string: format!("avc1.{profile:02x}{compatibility:02x}{level:02x}"),
    })
}

/// Information extracted from an AAC sequence header (AudioSpecificConfig).
#[derive(Debug, Clone, PartialEq)]
pub struct AacInfo {
    pub object_type: u8,
    pub sample_rate: u32,
    pub channels: u8,
    /// RFC 6381 codec string, e.g. `mp4a.40.2`.
    pub codec_string: String,
}

const AAC_SAMPLE_RATES: [u32; 13] = [
    96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
];

/// Parse a legacy AAC sequence header tag body (`0xAF 0x00 AudioSpecificConfig`).
pub fn parse_aac_sequence_header(data: &[u8]) -> Option<AacInfo> {
    if data.len() < 4 || data[0] >> 4 != 10 || data[1] != 0 {
        return None;
    }
    let (b0, b1) = (data[2], data[3]);
    let object_type = b0 >> 3;
    let sfi = (((b0 & 0x07) << 1) | (b1 >> 7)) as usize;
    let sample_rate = if sfi == 0x0F {
        // Explicit 24-bit frequency follows; rare, read it if present.
        let b2 = *data.get(4)? as u32;
        let b3 = *data.get(5)? as u32;
        let b4 = *data.get(6)? as u32;
        ((b1 as u32 & 0x7F) << 17) | (b2 << 9) | (b3 << 1) | (b4 >> 7)
    } else {
        *AAC_SAMPLE_RATES.get(sfi)?
    };
    let channels = if sfi == 0x0F {
        (data.get(6)? >> 3) & 0x0F
    } else {
        (b1 >> 3) & 0x0F
    };
    Some(AacInfo {
        object_type,
        sample_rate,
        channels,
        codec_string: format!("mp4a.40.{object_type}"),
    })
}

/// MSB-first bit reader over an RBSP (emulation prevention bytes already removed).
struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn bit(&mut self) -> Option<u32> {
        let byte = *self.data.get(self.pos / 8)?;
        let v = (byte >> (7 - (self.pos % 8))) & 1;
        self.pos += 1;
        Some(v as u32)
    }

    fn bits(&mut self, n: u32) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | self.bit()?;
        }
        Some(v)
    }

    /// Unsigned Exp-Golomb.
    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        let rest = self.bits(zeros)?;
        Some(((1u64 << zeros) - 1 + rest as u64).min(u32::MAX as u64) as u32)
    }

    /// Signed Exp-Golomb.
    fn se(&mut self) -> Option<i32> {
        let k = self.ue()? as i64;
        Some(if k % 2 == 1 { (k + 1) / 2 } else { -(k / 2) } as i32)
    }
}

fn strip_emulation_prevention(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

/// Decode width/height (after cropping) from an H.264 SPS NAL unit (with its 1-byte NAL header).
pub fn parse_h264_sps_dimensions(sps_nal: &[u8]) -> Option<(u32, u32)> {
    if sps_nal.len() < 4 || sps_nal[0] & 0x1F != 7 {
        return None;
    }
    let rbsp = strip_emulation_prevention(&sps_nal[1..]);
    let mut r = BitReader::new(&rbsp);
    let profile_idc = r.bits(8)?;
    r.bits(8)?; // constraint flags
    r.bits(8)?; // level_idc
    r.ue()?; // seq_parameter_set_id
    let mut chroma_format_idc = 1;
    let mut separate_colour_plane = 0;
    if matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        chroma_format_idc = r.ue()?;
        if chroma_format_idc == 3 {
            separate_colour_plane = r.bit()?;
        }
        r.ue()?; // bit_depth_luma_minus8
        r.ue()?; // bit_depth_chroma_minus8
        r.bit()?; // qpprime_y_zero_transform_bypass_flag
        if r.bit()? == 1 {
            // seq_scaling_matrix_present_flag
            let count = if chroma_format_idc == 3 { 12 } else { 8 };
            for i in 0..count {
                if r.bit()? == 1 {
                    let size = if i < 6 { 16 } else { 64 };
                    let (mut last, mut next) = (8i32, 8i32);
                    for _ in 0..size {
                        if next != 0 {
                            let delta = r.se()?;
                            next = (last + delta + 256).rem_euclid(256);
                        }
                        if next != 0 {
                            last = next;
                        }
                    }
                }
            }
        }
    }
    r.ue()?; // log2_max_frame_num_minus4
    let poc_type = r.ue()?;
    if poc_type == 0 {
        r.ue()?; // log2_max_pic_order_cnt_lsb_minus4
    } else if poc_type == 1 {
        r.bit()?; // delta_pic_order_always_zero_flag
        r.se()?; // offset_for_non_ref_pic
        r.se()?; // offset_for_top_to_bottom_field
        let n = r.ue()?;
        if n > 255 {
            return None;
        }
        for _ in 0..n {
            r.se()?;
        }
    }
    r.ue()?; // max_num_ref_frames
    r.bit()?; // gaps_in_frame_num_value_allowed_flag
    let width_mbs = r.ue()?.checked_add(1)?;
    let height_map_units = r.ue()?.checked_add(1)?;
    let frame_mbs_only = r.bit()?;
    if frame_mbs_only == 0 {
        r.bit()?; // mb_adaptive_frame_field_flag
    }
    r.bit()?; // direct_8x8_inference_flag
    let (mut cl, mut cr, mut ct, mut cb) = (0, 0, 0, 0);
    if r.bit()? == 1 {
        cl = r.ue()?;
        cr = r.ue()?;
        ct = r.ue()?;
        cb = r.ue()?;
    }
    let chroma_array_type = if separate_colour_plane == 1 {
        0
    } else {
        chroma_format_idc
    };
    let (crop_x, crop_y) = if chroma_array_type == 0 {
        (1, 2 - frame_mbs_only)
    } else {
        let sub_w = if chroma_array_type == 3 { 1 } else { 2 };
        let sub_h = if chroma_array_type == 1 { 2 } else { 1 };
        (sub_w, sub_h * (2 - frame_mbs_only))
    };
    let width = (width_mbs * 16).checked_sub(crop_x * (cl + cr))?;
    let height = ((2 - frame_mbs_only) * height_map_units * 16).checked_sub(crop_y * (ct + cb))?;
    (width > 0 && height > 0 && width <= 16384 && height <= 16384).then_some((width, height))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flv_header_bytes() {
        let h = flv_file_header(true, true);
        assert_eq!(&h[..], &[b'F', b'L', b'V', 1, 5, 0, 0, 0, 9, 0, 0, 0, 0]);
        assert_eq!(flv_file_header(true, false)[4], 1);
        assert_eq!(flv_file_header(false, true)[4], 4);
        assert_eq!(h.len(), FLV_HEADER_LEN);
    }

    #[test]
    fn flv_tag_layout() {
        let p = MediaPacket::video(
            0x0123_4567,
            Bytes::from_static(&[0x17, 0x01, 0, 0, 0, 0xAA, 0xBB]),
        );
        let t = flv_tag_bytes(&p);
        assert_eq!(t.len(), 11 + 7 + 4);
        assert_eq!(t[0], 9); // video
        assert_eq!(&t[1..4], &[0, 0, 7]); // size
        assert_eq!(&t[4..7], &[0x23, 0x45, 0x67]); // ts lower 24 bits
        assert_eq!(t[7], 0x01); // ts extended
        assert_eq!(&t[8..11], &[0, 0, 0]); // stream id
        assert_eq!(&t[11..18], &p.data[..]);
        assert_eq!(&t[18..22], &(18u32).to_be_bytes()); // PreviousTagSize = 11 + 7
        assert_eq!(p.flv_size(), t.len());

        let a = flv_tag_bytes(&MediaPacket::audio(
            5,
            Bytes::from_static(&[0xAF, 0x01, 0x21]),
        ));
        assert_eq!(a[0], 8);
        let s = flv_tag_bytes(&MediaPacket::script(0, Bytes::from_static(&[2, 0, 0])));
        assert_eq!(s[0], 18);
    }

    #[test]
    fn flag_detection() {
        // Legacy AVC
        assert!(video_is_keyframe(&[0x17, 0x01]));
        assert!(!video_is_keyframe(&[0x27, 0x01]));
        assert!(video_is_sequence_header(&[0x17, 0x00]));
        assert!(!video_is_sequence_header(&[0x17, 0x01]));
        assert!(video_is_sequence_header(&[0x1C, 0x00])); // legacy HEVC id 12
        assert!(!video_is_sequence_header(&[0x12, 0x00])); // H.263
        // Enhanced RTMP: 0x90 = ExHeader | keyframe | SequenceStart
        assert!(video_is_keyframe(&[0x90, b'h', b'v', b'c', b'1']));
        assert!(video_is_sequence_header(&[0x90, b'h', b'v', b'c', b'1']));
        assert!(!video_is_sequence_header(&[0x91, b'h', b'v', b'c', b'1']));
        assert!(!video_is_keyframe(&[0xA1, b'h', b'v', b'c', b'1']));
        // AAC
        assert!(audio_is_sequence_header(&[0xAF, 0x00, 0x12, 0x10]));
        assert!(!audio_is_sequence_header(&[0xAF, 0x01, 0x21]));
        assert!(!audio_is_sequence_header(&[0x2F, 0x00])); // MP3
        assert!(audio_is_sequence_header(&[0x90, b'O', b'p', b'u', b's'])); // enhanced
        // Empty bodies never panic
        assert!(!video_is_keyframe(&[]));
        assert!(!video_is_sequence_header(&[]));
        assert!(!audio_is_sequence_header(&[]));
        assert_eq!(video_codec_name(&[0x17]), Some("h264"));
        assert_eq!(
            video_codec_name(&[0x90, b'h', b'v', b'c', b'1']),
            Some("hevc")
        );
        assert_eq!(audio_codec_name(&[0xAF]), Some("aac"));
    }

    #[test]
    fn metadata_amf0() {
        let mut m = HashMap::new();
        m.insert("width".to_string(), AmfValue::Number(1280.0));
        let body = on_metadata_body(&m);
        let decoded = rtmp_rs::amf::amf0::decode_all(&body).unwrap();
        assert_eq!(decoded[0], AmfValue::String("onMetaData".into()));
        let map = match &decoded[1] {
            AmfValue::EcmaArray(m) | AmfValue::Object(m) => m.clone(),
            other => panic!("unexpected {other:?}"),
        };
        assert_eq!(map.get("width"), Some(&AmfValue::Number(1280.0)));
    }

    // Real SPS NAL units produced by ffmpeg/libx264 (High profile).
    const SPS_720P: [u8; 26] = [
        0x67, 0x64, 0x00, 0x1f, 0xac, 0xd9, 0x40, 0x50, 0x05, 0xbb, 0x01, 0x10, 0x00, 0x00, 0x03,
        0x00, 0x10, 0x00, 0x00, 0x03, 0x03, 0xc0, 0xf1, 0x83, 0x19, 0x60,
    ];
    const SPS_1080P: [u8; 27] = [
        0x67, 0x64, 0x00, 0x28, 0xac, 0xd9, 0x40, 0x78, 0x02, 0x27, 0xe5, 0xc0, 0x44, 0x00, 0x00,
        0x03, 0x00, 0x04, 0x00, 0x00, 0x03, 0x00, 0xf0, 0x3c, 0x60, 0xc6, 0x58,
    ];
    const SPS_854X480: [u8; 26] = [
        0x67, 0x64, 0x00, 0x1f, 0xac, 0xd9, 0x40, 0xd8, 0x3d, 0xe6, 0xf0, 0x11, 0x00, 0x00, 0x03,
        0x00, 0x01, 0x00, 0x00, 0x03, 0x00, 0x3c, 0x0f, 0x18, 0x31, 0x96,
    ];

    #[test]
    fn sps_dimensions() {
        assert_eq!(parse_h264_sps_dimensions(&SPS_720P), Some((1280, 720)));
        assert_eq!(parse_h264_sps_dimensions(&SPS_1080P), Some((1920, 1080)));
        assert_eq!(parse_h264_sps_dimensions(&SPS_854X480), Some((854, 480)));
        assert_eq!(parse_h264_sps_dimensions(&[0x67]), None);
        assert_eq!(parse_h264_sps_dimensions(&[0x68, 1, 2, 3, 4]), None);
        // Truncations never panic.
        for n in 0..SPS_1080P.len() {
            let _ = parse_h264_sps_dimensions(&SPS_1080P[..n]);
        }
    }

    #[test]
    fn avc_and_aac_headers() {
        let mut body = vec![0x17, 0x00, 0, 0, 0, 1, 0x64, 0x00, 0x1f, 0xff, 0xe1];
        body.extend_from_slice(&(SPS_720P.len() as u16).to_be_bytes());
        body.extend_from_slice(&SPS_720P);
        body.extend_from_slice(&[1, 0, 4, 0x68, 0xeb, 0xe3, 0xcb]);
        let info = parse_avc_sequence_header(&body).unwrap();
        assert_eq!(info.codec_string, "avc1.64001f");
        assert_eq!((info.width, info.height), (Some(1280), Some(720)));
        // Truncated record never panics.
        assert!(parse_avc_sequence_header(&body[..9]).is_none());

        // AAC-LC 48 kHz stereo: ASC 0x11 0x90
        let aac = parse_aac_sequence_header(&[0xAF, 0x00, 0x11, 0x90]).unwrap();
        assert_eq!(aac.object_type, 2);
        assert_eq!(aac.sample_rate, 48000);
        assert_eq!(aac.channels, 2);
        assert_eq!(aac.codec_string, "mp4a.40.2");
        assert!(parse_aac_sequence_header(&[0xAF, 0x00]).is_none());
    }
}
