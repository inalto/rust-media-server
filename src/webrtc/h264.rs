//! H.264 helpers for WebRTC egress: AVCDecoderConfigurationRecord parameter sets,
//! AVCC (length-prefixed) → Annex-B access units with SPS/PPS injection on keyframes,
//! and the choice of the negotiated H.264 payload type.
//!
//! FLV carries H.264 as AVCC: every NAL unit is prefixed by its length on
//! `lengthSizeMinusOne + 1` bytes, and SPS/PPS only travel in the sequence header.
//! The RTP payloader (RFC 6184) takes Annex-B access units; WebRTC receivers need
//! SPS/PPS in-band before every IDR (a viewer may join at any keyframe), so they are
//! prepended to keyframe access units that do not already carry them.

use bytes::{BufMut, Bytes, BytesMut};

/// NAL unit types used here.
pub const NAL_IDR: u8 = 5;
pub const NAL_SPS: u8 = 7;
pub const NAL_PPS: u8 = 8;

const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// SPS/PPS and NAL length size from an AVCDecoderConfigurationRecord.
#[derive(Debug, Clone, PartialEq)]
pub struct ParameterSets {
    /// Bytes of the NAL length prefix in frames (1, 2, 3 or 4).
    pub length_size: usize,
    pub sps: Vec<Bytes>,
    pub pps: Vec<Bytes>,
    pub profile_idc: u8,
    /// `constraint_set` flags byte (profile compatibility).
    pub constraints: u8,
    pub level_idc: u8,
}

impl ParameterSets {
    /// RFC 6184 `profile-level-id` (6 hex digits), e.g. `64001f`.
    pub fn profile_level_id(&self) -> String {
        format!(
            "{:02x}{:02x}{:02x}",
            self.profile_idc, self.constraints, self.level_idc
        )
    }
}

/// Parse an AVCDecoderConfigurationRecord (the FLV sequence header payload after the
/// 5-byte video tag header). `None` when malformed or without SPS/PPS.
pub fn parse_avcc(record: &[u8]) -> Option<ParameterSets> {
    if record.len() < 7 || record[0] != 1 {
        return None;
    }
    let length_size = (record[4] & 0x03) as usize + 1;
    let mut pos = 5;
    let read_sets = |count: usize, pos: &mut usize| -> Option<Vec<Bytes>> {
        let mut out = Vec::with_capacity(count);
        for _ in 0..count {
            let len = u16::from_be_bytes([*record.get(*pos)?, *record.get(*pos + 1)?]) as usize;
            *pos += 2;
            let nal = record.get(*pos..*pos + len)?;
            *pos += len;
            if !nal.is_empty() {
                out.push(Bytes::copy_from_slice(nal));
            }
        }
        Some(out)
    };
    let num_sps = (record[pos] & 0x1F) as usize;
    pos += 1;
    let sps = read_sets(num_sps, &mut pos)?;
    let num_pps = *record.get(pos)? as usize;
    pos += 1;
    let pps = read_sets(num_pps, &mut pos)?;
    if sps.is_empty() || pps.is_empty() {
        return None;
    }
    Some(ParameterSets {
        length_size,
        sps,
        pps,
        profile_idc: record[1],
        constraints: record[2],
        level_idc: record[3],
    })
}

/// Split an AVCC frame into its NAL units. `None` if a length prefix overruns the
/// buffer (malformed frame). Zero-length NAL units are skipped.
pub fn split_avcc(data: &[u8], length_size: usize) -> Option<Vec<&[u8]>> {
    if !(1..=4).contains(&length_size) {
        return None;
    }
    let mut nals = Vec::new();
    let mut pos = 0;
    while pos < data.len() {
        let prefix = data.get(pos..pos + length_size)?;
        let len = prefix
            .iter()
            .fold(0usize, |acc, b| (acc << 8) | *b as usize);
        pos += length_size;
        let nal = data.get(pos..pos + len)?;
        pos += len;
        if !nal.is_empty() {
            nals.push(nal);
        }
    }
    Some(nals)
}

/// One Annex-B access unit ready for the RTP payloader.
#[derive(Debug, Clone, PartialEq)]
pub struct AccessUnit {
    pub data: Bytes,
    /// Contains an IDR slice (a viewer can start decoding here).
    pub idr: bool,
}

/// Convert an AVCC frame to an Annex-B access unit (4-byte start codes). When
/// `keyframe` is set and the frame carries no SPS of its own, the SPS and PPS of
/// `params` are prepended. `None` for malformed or empty frames.
pub fn avcc_to_annexb(data: &[u8], params: &ParameterSets, keyframe: bool) -> Option<AccessUnit> {
    let nals = split_avcc(data, params.length_size)?;
    if nals.is_empty() {
        return None;
    }
    let nal_type = |n: &[u8]| n[0] & 0x1F;
    let idr = nals.iter().any(|n| nal_type(n) == NAL_IDR);
    let inject = (keyframe || idr) && !nals.iter().any(|n| nal_type(n) == NAL_SPS);
    let mut size: usize = nals.iter().map(|n| n.len() + 4).sum();
    if inject {
        size += params
            .sps
            .iter()
            .chain(params.pps.iter())
            .map(|n| n.len() + 4)
            .sum::<usize>();
    }
    let mut out = BytesMut::with_capacity(size);
    if inject {
        for nal in params.sps.iter().chain(params.pps.iter()) {
            out.put_slice(&START_CODE);
            out.put_slice(nal);
        }
    }
    for nal in nals {
        out.put_slice(&START_CODE);
        out.put_slice(nal);
    }
    Some(AccessUnit {
        data: out.freeze(),
        idr,
    })
}

/// `key=value` pairs of an SDP `a=fmtp` line (keys lowercased).
fn fmtp_param<'a>(fmtp: &'a str, key: &str) -> Option<&'a str> {
    fmtp.split(';').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        k.trim().eq_ignore_ascii_case(key).then(|| v.trim())
    })
}

/// Pick the payload type to send H.264 with among the negotiated `(payload type,
/// fmtp)` codecs. Only `packetization-mode=1` is usable (FU-A/STAP-A). Preference:
/// same profile_idc + constraint byte as the stream, then same profile_idc, then
/// Constrained Baseline (`42e0..`, decodable by every WebRTC browser), then the
/// first remaining one. Browsers decode any profile they support regardless of
/// the label, so a mismatch only matters for strict receivers.
pub fn choose_h264_payload_type(codecs: &[(u8, String)], profile: Option<(u8, u8)>) -> Option<u8> {
    let mut best: Option<(u8, u8)> = None; // (score, pt)
    for (pt, fmtp) in codecs {
        if fmtp_param(fmtp, "packetization-mode") != Some("1") {
            continue;
        }
        let plid = fmtp_param(fmtp, "profile-level-id")
            .filter(|v| v.len() == 6)
            .and_then(|v| u32::from_str_radix(v, 16).ok());
        let (p_idc, p_iop) = match plid {
            Some(v) => (((v >> 16) & 0xFF) as u8, ((v >> 8) & 0xFF) as u8),
            None => (0x42, 0x00), // RFC 6184 default: Baseline
        };
        let score = match profile {
            Some((idc, iop)) if idc == p_idc && iop == p_iop => 4,
            Some((idc, _)) if idc == p_idc => 3,
            _ if p_idc == 0x42 && p_iop & 0x40 != 0 => 2,
            _ => 1,
        };
        if best.is_none_or(|(s, _)| score > s) {
            best = Some((score, *pt));
        }
    }
    best.map(|(_, pt)| pt)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPS: [u8; 26] = [
        0x67, 0x64, 0x00, 0x1f, 0xac, 0xd9, 0x40, 0x50, 0x05, 0xbb, 0x01, 0x10, 0x00, 0x00, 0x03,
        0x00, 0x10, 0x00, 0x00, 0x03, 0x03, 0xc0, 0xf1, 0x83, 0x19, 0x60,
    ];
    const PPS: [u8; 4] = [0x68, 0xeb, 0xe3, 0xcb];

    fn record(length_size_minus_one: u8) -> Vec<u8> {
        let mut r = vec![1, 0x64, 0x00, 0x1f, 0xfc | length_size_minus_one, 0xe1];
        r.extend_from_slice(&(SPS.len() as u16).to_be_bytes());
        r.extend_from_slice(&SPS);
        r.push(1);
        r.extend_from_slice(&(PPS.len() as u16).to_be_bytes());
        r.extend_from_slice(&PPS);
        r
    }

    fn avcc(length_size: usize, nals: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for n in nals {
            let len = (n.len() as u32).to_be_bytes();
            out.extend_from_slice(&len[4 - length_size..]);
            out.extend_from_slice(n);
        }
        out
    }

    fn annexb(nals: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for n in nals {
            out.extend_from_slice(&START_CODE);
            out.extend_from_slice(n);
        }
        out
    }

    #[test]
    fn parses_avcc_record() {
        let ps = parse_avcc(&record(3)).unwrap();
        assert_eq!(ps.length_size, 4);
        assert_eq!(ps.sps, vec![Bytes::copy_from_slice(&SPS)]);
        assert_eq!(ps.pps, vec![Bytes::copy_from_slice(&PPS)]);
        assert_eq!(ps.profile_level_id(), "64001f");
        assert_eq!(parse_avcc(&record(1)).unwrap().length_size, 2);
        // Truncated / wrong version / no PPS.
        let r = record(3);
        for n in 0..r.len() {
            assert!(parse_avcc(&r[..n]).is_none(), "prefix {n}");
        }
        let mut bad = r.clone();
        bad[0] = 0;
        assert!(parse_avcc(&bad).is_none());
    }

    #[test]
    fn keyframe_gets_sps_pps_injected() {
        let ps = parse_avcc(&record(3)).unwrap();
        let sei: &[u8] = &[0x06, 0x05, 0x01, 0x80];
        let idr: &[u8] = &[0x65, 0x88, 0x84, 0x00];
        let au = avcc_to_annexb(&avcc(4, &[sei, idr]), &ps, true).unwrap();
        assert!(au.idr);
        assert_eq!(&au.data[..], &annexb(&[&SPS, &PPS, sei, idr])[..]);
    }

    #[test]
    fn inter_frames_are_converted_without_injection() {
        let ps = parse_avcc(&record(3)).unwrap();
        // Multi-slice access unit.
        let s1: &[u8] = &[0x41, 0x9a, 0x01];
        let s2: &[u8] = &[0x41, 0x9a, 0x02, 0x03];
        let au = avcc_to_annexb(&avcc(4, &[s1, s2]), &ps, false).unwrap();
        assert!(!au.idr);
        assert_eq!(&au.data[..], &annexb(&[s1, s2])[..]);
    }

    #[test]
    fn two_byte_length_prefixes() {
        let ps = parse_avcc(&record(1)).unwrap();
        let idr: &[u8] = &[0x65, 1, 2, 3, 4, 5];
        let p: &[u8] = &[0x41, 9];
        let au = avcc_to_annexb(&avcc(2, &[idr, p]), &ps, true).unwrap();
        assert_eq!(&au.data[..], &annexb(&[&SPS, &PPS, idr, p])[..]);
        // A 4-byte-prefixed frame read with 2-byte prefixes is malformed or garbage,
        // never a panic.
        let _ = avcc_to_annexb(&avcc(4, &[idr]), &ps, true);
    }

    #[test]
    fn in_band_parameter_sets_are_not_duplicated() {
        let ps = parse_avcc(&record(3)).unwrap();
        let idr: &[u8] = &[0x65, 0x88];
        let au = avcc_to_annexb(&avcc(4, &[&SPS, &PPS, idr]), &ps, true).unwrap();
        assert_eq!(&au.data[..], &annexb(&[&SPS, &PPS, idr])[..]);
        // An IDR not flagged as keyframe by FLV still gets the parameter sets.
        let au = avcc_to_annexb(&avcc(4, &[idr]), &ps, false).unwrap();
        assert!(au.idr);
        assert!(au.data.starts_with(&annexb(&[&SPS])));
    }

    #[test]
    fn malformed_frames_are_rejected() {
        let ps = parse_avcc(&record(3)).unwrap();
        assert!(avcc_to_annexb(&[0, 0, 0, 9, 0x65], &ps, true).is_none());
        assert!(avcc_to_annexb(&[0, 0], &ps, true).is_none());
        assert!(avcc_to_annexb(&[], &ps, true).is_none());
        assert!(
            avcc_to_annexb(&[0, 0, 0, 0], &ps, true).is_none(),
            "only empty NALs"
        );
        assert!(split_avcc(&[1, 0x41], 5).is_none());
    }

    #[test]
    fn payload_type_choice() {
        let offer = vec![
            (
                102,
                "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42001f"
                    .to_string(),
            ),
            (
                127,
                "level-asymmetry-allowed=1;packetization-mode=0;profile-level-id=42001f"
                    .to_string(),
            ),
            (
                125,
                "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
                    .to_string(),
            ),
            (
                112,
                "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=64001f"
                    .to_string(),
            ),
        ];
        // High profile stream → the High profile payload type.
        assert_eq!(
            choose_h264_payload_type(&offer, Some((0x64, 0x00))),
            Some(112)
        );
        // Main profile not offered → Constrained Baseline.
        assert_eq!(
            choose_h264_payload_type(&offer, Some((0x4d, 0x40))),
            Some(125)
        );
        assert_eq!(choose_h264_payload_type(&offer, None), Some(125));
        assert_eq!(
            choose_h264_payload_type(&offer, Some((0x42, 0xe0))),
            Some(125)
        );
        // packetization-mode=0 only → unusable.
        assert_eq!(choose_h264_payload_type(&offer[1..2], None), None);
        assert_eq!(choose_h264_payload_type(&[], None), None);
    }
}
