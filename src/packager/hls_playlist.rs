//! LL-HLS playlist generation (ARCHITECTURE.md §6.3, PROTOCOLS.md §3.2).
//!
//! Media playlist rules implemented here:
//! - `EXT-X-VERSION:9`, `EXT-X-TARGETDURATION` = longest segment rounded to the
//!   nearest integer (never decreasing during a stream),
//!   `EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=YES,PART-HOLD-BACK=<3 × part target>`
//!   (no `CAN-SKIP-UNTIL`: delta playlists are not implemented),
//!   `EXT-X-PART-INF:PART-TARGET=<configured part duration>` (the segmenter never emits
//!   a longer part except across timestamp gaps).
//! - `EXT-X-MAP` per codec generation, `EXT-X-DISCONTINUITY` (+ new map) when the
//!   encoder changed its sequence headers, `EXT-X-DISCONTINUITY-SEQUENCE` when such a
//!   boundary was evicted from the window.
//! - `EXT-X-PROGRAM-DATE-TIME` on the first segment and after each discontinuity.
//! - `EXT-X-PART` lines for the in-progress segment and the newest complete segments
//!   (at least 3, and at least 3 target durations, as required by RFC 8216bis).
//! - A trailing `EXT-X-PRELOAD-HINT` for the next part while live; `EXT-X-ENDLIST`
//!   once the stream ended.

use std::fmt::Write;

use chrono::{DateTime, SecondsFormat, Utc};

use super::store::{Segment, StoreState};

/// Minimum number of complete segments that keep their `EXT-X-PART` lines.
const PART_SEGMENTS: usize = 3;

/// `EXT-X-MAP` URI of a codec generation (generation 0 is plain `init.mp4`).
pub fn init_uri(generation: u32) -> String {
    if generation == 0 {
        "init.mp4".to_string()
    } else {
        format!("init.mp4?g={generation}")
    }
}

pub fn segment_uri(seq: u64) -> String {
    format!("seg-{seq}.m4s")
}

pub fn part_uri(seq: u64, part: u32) -> String {
    format!("part-{seq}-{part}.m4s")
}

fn pdt(t: &DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// `EXT-X-TARGETDURATION` in effect.
pub fn target_duration(s: &StoreState) -> u32 {
    if s.target_duration > 0 {
        s.target_duration
    } else {
        ((s.nominal_segment() as f64 / 90_000.0).round() as u32).max(1)
    }
}

/// `PART-HOLD-BACK` (seconds): 3 × part target (RFC 8216bis recommendation).
pub fn part_hold_back(s: &StoreState) -> f64 {
    3.0 * s.part_target_secs()
}

/// Next part to be published: `(msn, part index)`.
pub fn next_part(s: &StoreState) -> Option<(u64, u32)> {
    let last = s.segments.back()?;
    Some(if last.complete {
        (last.seq + 1, 0)
    } else {
        (last.seq, last.chunks.len() as u32)
    })
}

/// Index (into `s.segments`) of the first segment whose parts are listed.
fn first_part_segment(s: &StoreState, target: u32) -> usize {
    let mut covered = 0.0;
    let mut count = 0;
    let mut idx = s.segments.len();
    for (i, seg) in s.segments.iter().enumerate().rev() {
        if !seg.complete {
            idx = i;
            continue;
        }
        if count >= PART_SEGMENTS && covered >= 3.0 * target as f64 {
            break;
        }
        covered += seg.duration_secs();
        count += 1;
        idx = i;
    }
    idx
}

fn write_parts(out: &mut String, seg: &Segment) {
    for c in &seg.chunks {
        let _ = write!(
            out,
            "#EXT-X-PART:DURATION={:.3},URI=\"{}\"",
            c.duration_secs(),
            part_uri(seg.seq, c.part_index)
        );
        if c.independent {
            out.push_str(",INDEPENDENT=YES");
        }
        out.push('\n');
    }
}

/// The media playlist (`stream.m3u8`).
pub fn media_playlist(s: &StoreState) -> String {
    let target = target_duration(s);
    let mut out = String::with_capacity(4096);
    out.push_str("#EXTM3U\n#EXT-X-VERSION:9\n");
    let _ = writeln!(out, "#EXT-X-TARGETDURATION:{target}");
    let _ = writeln!(
        out,
        "#EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=YES,PART-HOLD-BACK={:.3}",
        part_hold_back(s)
    );
    let _ = writeln!(
        out,
        "#EXT-X-PART-INF:PART-TARGET={:.3}",
        s.part_target_secs()
    );
    let first_seq = s
        .first_seq()
        .or_else(|| next_part(s).map(|(m, _)| m))
        .unwrap_or(0);
    let _ = writeln!(out, "#EXT-X-MEDIA-SEQUENCE:{first_seq}");
    if s.discontinuity_sequence > 0 {
        let _ = writeln!(
            out,
            "#EXT-X-DISCONTINUITY-SEQUENCE:{}",
            s.discontinuity_sequence
        );
    }
    let parts_from = first_part_segment(s, target);
    let mut prev_gen: Option<u32> = None;
    for (i, seg) in s.segments.iter().enumerate() {
        // An in-progress segment without parts is announced by the preload hint only.
        if !seg.complete && seg.chunks.is_empty() {
            continue;
        }
        if prev_gen != Some(seg.generation) {
            if prev_gen.is_some() {
                out.push_str("#EXT-X-DISCONTINUITY\n");
            }
            let _ = writeln!(out, "#EXT-X-MAP:URI=\"{}\"", init_uri(seg.generation));
            let _ = writeln!(
                out,
                "#EXT-X-PROGRAM-DATE-TIME:{}",
                pdt(&seg.program_date_time)
            );
            prev_gen = Some(seg.generation);
        }
        if i >= parts_from {
            write_parts(&mut out, seg);
        }
        if seg.complete {
            let _ = writeln!(
                out,
                "#EXTINF:{:.3},\n{}",
                seg.duration_secs(),
                segment_uri(seg.seq)
            );
        }
    }
    if prev_gen.is_none()
        && let Some(init) = s.init()
    {
        let _ = writeln!(out, "#EXT-X-MAP:URI=\"{}\"", init_uri(init.generation));
    }
    if s.ended {
        out.push_str("#EXT-X-ENDLIST\n");
    } else if let Some((msn, part)) = next_part(s) {
        let _ = writeln!(
            out,
            "#EXT-X-PRELOAD-HINT:TYPE=PART,URI=\"{}\"",
            part_uri(msn, part)
        );
    }
    out
}

/// Variant information for the master playlist.
#[derive(Debug, Clone, PartialEq)]
pub struct VariantInfo {
    pub bandwidth: u64,
    pub average_bandwidth: Option<u64>,
    pub codecs: String,
    pub resolution: Option<(u32, u32)>,
    pub frame_rate: Option<f64>,
}

/// Peak/average bandwidth of the variant: measured segment bitrates when available,
/// else the encoder's declared bitrates (+10 % container overhead), else 3 Mbit/s.
pub fn variant_bandwidth(s: &StoreState, declared_kbps: Option<f64>) -> (u64, Option<u64>) {
    let declared = declared_kbps
        .filter(|k| k.is_finite() && *k > 0.0)
        .map(|k| (k * 1100.0) as u64);
    let peak = s.peak_bitrate();
    let avg = {
        let (bytes, dur) = s.complete_segments().fold((0usize, 0.0f64), |(b, d), g| {
            (b + g.bytes, d + g.duration_secs())
        });
        (dur > 0.0).then(|| (bytes as f64 * 8.0 / dur) as u64)
    };
    let bandwidth = peak.max(declared).unwrap_or(3_000_000);
    (bandwidth, avg)
}

/// The master playlist (`master.m3u8` / `index.m3u8`) with one variant.
pub fn master_playlist(v: &VariantInfo) -> String {
    let mut out = String::from("#EXTM3U\n#EXT-X-VERSION:9\n");
    let _ = write!(out, "#EXT-X-STREAM-INF:BANDWIDTH={}", v.bandwidth);
    if let Some(avg) = v.average_bandwidth {
        let _ = write!(out, ",AVERAGE-BANDWIDTH={avg}");
    }
    let _ = write!(out, ",CODECS=\"{}\"", v.codecs);
    if let Some((w, h)) = v.resolution.filter(|(w, h)| *w > 0 && *h > 0) {
        let _ = write!(out, ",RESOLUTION={w}x{h}");
    }
    if let Some(fps) = v.frame_rate.filter(|f| f.is_finite() && *f > 0.0) {
        let _ = write!(out, ",FRAME-RATE={fps:.3}");
    }
    out.push_str("\nstream.m3u8\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packager::store::tests::{chunk, fill, t, test_audio, test_video};
    use crate::packager::store::{SegmentStore, StoreParams};

    fn store(window: usize) -> std::sync::Arc<SegmentStore> {
        let s = SegmentStore::new(StoreParams {
            window_segments: window,
            ..Default::default()
        });
        s.set_init(test_video(), Some(test_audio()));
        s
    }

    const SEQ0: u64 = 1_767_225_600;

    #[test]
    fn live_playlist_golden() {
        let s = store(4);
        fill(&s, 0.0, 5); // seq0..seq0+4, window keeps the last 4
        let seq = s.open_segment(t(10.0), true);
        assert_eq!(seq, SEQ0 + 5);
        s.push_chunk(chunk(t(10.0), 500, true));
        s.push_chunk(chunk(t(10.5), 500, false));
        let text = s.with(media_playlist);
        let expected = "\
#EXTM3U
#EXT-X-VERSION:9
#EXT-X-TARGETDURATION:2
#EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=YES,PART-HOLD-BACK=1.500
#EXT-X-PART-INF:PART-TARGET=0.500
#EXT-X-MEDIA-SEQUENCE:1767225601
#EXT-X-MAP:URI=\"init.mp4\"
#EXT-X-PROGRAM-DATE-TIME:2026-01-01T00:00:02.000Z
#EXTINF:2.000,
seg-1767225601.m4s
#EXT-X-PART:DURATION=0.500,URI=\"part-1767225602-0.m4s\",INDEPENDENT=YES
#EXT-X-PART:DURATION=0.500,URI=\"part-1767225602-1.m4s\"
#EXT-X-PART:DURATION=0.500,URI=\"part-1767225602-2.m4s\"
#EXT-X-PART:DURATION=0.500,URI=\"part-1767225602-3.m4s\"
#EXTINF:2.000,
seg-1767225602.m4s
#EXT-X-PART:DURATION=0.500,URI=\"part-1767225603-0.m4s\",INDEPENDENT=YES
#EXT-X-PART:DURATION=0.500,URI=\"part-1767225603-1.m4s\"
#EXT-X-PART:DURATION=0.500,URI=\"part-1767225603-2.m4s\"
#EXT-X-PART:DURATION=0.500,URI=\"part-1767225603-3.m4s\"
#EXTINF:2.000,
seg-1767225603.m4s
#EXT-X-PART:DURATION=0.500,URI=\"part-1767225604-0.m4s\",INDEPENDENT=YES
#EXT-X-PART:DURATION=0.500,URI=\"part-1767225604-1.m4s\"
#EXT-X-PART:DURATION=0.500,URI=\"part-1767225604-2.m4s\"
#EXT-X-PART:DURATION=0.500,URI=\"part-1767225604-3.m4s\"
#EXTINF:2.000,
seg-1767225604.m4s
#EXT-X-PART:DURATION=0.500,URI=\"part-1767225605-0.m4s\",INDEPENDENT=YES
#EXT-X-PART:DURATION=0.500,URI=\"part-1767225605-1.m4s\"
#EXT-X-PRELOAD-HINT:TYPE=PART,URI=\"part-1767225605-2.m4s\"
";
        assert_eq!(text, expected);
    }

    #[test]
    fn preload_hint_after_segment_completion_and_endlist() {
        let s = store(6);
        fill(&s, 0.0, 1);
        let text = s.with(media_playlist);
        assert!(text.ends_with(&format!(
            "#EXTINF:2.000,\nseg-{SEQ0}.m4s\n#EXT-X-PRELOAD-HINT:TYPE=PART,URI=\"part-{}-0.m4s\"\n",
            SEQ0 + 1
        )));
        // Open (empty) in-progress segment: still only the hint for its part 0.
        s.open_segment(t(2.0), true);
        assert_eq!(s.with(media_playlist), text);
        s.end();
        let text = s.with(media_playlist);
        assert!(text.ends_with(&format!("seg-{SEQ0}.m4s\n#EXT-X-ENDLIST\n")));
        assert!(!text.contains("PRELOAD-HINT"));
    }

    #[test]
    fn discontinuity_on_new_generation() {
        let s = store(6);
        fill(&s, 0.0, 1);
        s.set_init(test_video(), None);
        fill(&s, 2.0, 1);
        let text = s.with(media_playlist);
        let expected_tail = format!(
            "#EXTINF:2.000,\nseg-{}.m4s\n#EXT-X-DISCONTINUITY\n#EXT-X-MAP:URI=\"init.mp4?g=1\"\n\
             #EXT-X-PROGRAM-DATE-TIME:2026-01-01T00:00:02.000Z\n",
            SEQ0
        );
        assert!(text.contains(&expected_tail), "{text}");
        assert!(!text.contains("DISCONTINUITY-SEQUENCE"));
    }

    #[test]
    fn parts_kept_for_three_target_durations() {
        let s = store(10);
        // Target duration 2 s; 1 s segments -> parts must cover >= 6 s (6 segments).
        s.open_segment(t(0.0), true);
        s.push_chunk(chunk(t(0.0), 2000, true));
        for i in 0..9 {
            s.open_segment(t(2.0 + i as f64), true);
            s.push_chunk(chunk(t(2.0 + i as f64), 500, true));
            s.push_chunk(chunk(t(2.5 + i as f64), 500, false));
        }
        s.complete_segment();
        let text = s.with(media_playlist);
        let with_parts = text
            .split("#EXTINF")
            .filter(|chunk| chunk.contains("#EXT-X-PART:"))
            .count();
        assert_eq!(with_parts, 6, "{text}");
    }

    #[test]
    fn master_golden() {
        let v = VariantInfo {
            bandwidth: 2_800_000,
            average_bandwidth: Some(2_600_000),
            codecs: "avc1.64001f,mp4a.40.2".into(),
            resolution: Some((1280, 720)),
            frame_rate: Some(30.0),
        };
        assert_eq!(
            master_playlist(&v),
            "#EXTM3U\n#EXT-X-VERSION:9\n#EXT-X-STREAM-INF:BANDWIDTH=2800000,AVERAGE-BANDWIDTH=2600000,\
             CODECS=\"avc1.64001f,mp4a.40.2\",RESOLUTION=1280x720,FRAME-RATE=30.000\nstream.m3u8\n"
        );
        let s = store(6);
        assert_eq!(
            s.with(|st| variant_bandwidth(st, Some(2628.0))).0,
            2_890_800
        );
        assert_eq!(s.with(|st| variant_bandwidth(st, None)), (3_000_000, None));
    }

    #[test]
    fn next_part_and_target_defaults() {
        let s = store(6);
        assert_eq!(s.with(next_part), None);
        assert_eq!(s.with(target_duration), 2);
        let seq = s.open_segment(t(0.0), true);
        assert_eq!(s.with(next_part), Some((seq, 0)));
        s.push_chunk(chunk(t(0.0), 500, true));
        assert_eq!(s.with(next_part), Some((seq, 1)));
    }
}
