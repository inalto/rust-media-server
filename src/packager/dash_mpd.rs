//! LL-DASH MPD generation (ARCHITECTURE.md §6.4, PROTOCOLS.md §3.3).
//!
//! - `type="dynamic"`, `availabilityStartTime="1970-01-01T00:00:00Z"`: media time is
//!   wall-clock time since the epoch (see `store.rs`), so `$Time$` values are unique
//!   across sessions and a segment's availability is simply its end time.
//! - One `AdaptationSet`/`Representation` per track (`video`, `audio`), each with a
//!   `SegmentTemplate` + `SegmentTimeline` (`$Time$` addressing, explicit `t` on every
//!   `S`), `availabilityTimeOffset` = nominal segment − part duration and
//!   `availabilityTimeComplete="false"`: a segment can be requested as soon as its first
//!   chunk exists and is delivered with chunked transfer while being produced.
//! - The in-progress segment is announced as the last `S` once its first chunk exists,
//!   with its expected duration (dash.js only requests segments listed in the timeline;
//!   its `t` is exact, so the URL is always right; the final `d` follows in the next
//!   MPD update).
//! - `ServiceDescription` latency targets, `UTCTiming` (http-xsdate → `/dash/time`),
//!   `minimumUpdatePeriod` (1 s: new segments are only discovered through MPD updates),
//!   `timeShiftBufferDepth` = the window. After the stream ended, the MPD loses
//!   `minimumUpdatePeriod` and gains `mediaPresentationDuration`.
//! - Codec changes mid-stream start a new `Period` (one per init generation).

use std::fmt::Write;

use chrono::{DateTime, SecondsFormat, Utc};

use super::cmaf::VIDEO_TIMESCALE;
use super::store::{InitSegments, Segment, StoreState};

/// Options that do not live in the store.
#[derive(Debug, Clone, PartialEq)]
pub struct MpdOptions {
    /// Absolute URL of `/dash/time`.
    pub utc_timing_url: String,
    pub publish_time: DateTime<Utc>,
    /// `minimumUpdatePeriod` (seconds).
    pub minimum_update_period: f64,
    /// ServiceDescription latency (ms).
    pub latency_target_ms: u32,
    pub latency_min_ms: u32,
    pub latency_max_ms: u32,
    /// Fallback bandwidths before anything was measured (bits/s).
    pub default_video_bandwidth: u64,
    pub default_audio_bandwidth: u64,
    pub frame_rate: Option<f64>,
}

impl Default for MpdOptions {
    fn default() -> Self {
        Self {
            utc_timing_url: "/dash/time".into(),
            publish_time: DateTime::UNIX_EPOCH,
            minimum_update_period: 1.0,
            latency_target_ms: 2000,
            latency_min_ms: 1500,
            latency_max_ms: 6000,
            default_video_bandwidth: 2_500_000,
            default_audio_bandwidth: 128_000,
            frame_rate: None,
        }
    }
}

/// `xs:duration` with millisecond precision: `PT12.345S`.
fn duration(secs: f64) -> String {
    let ms = (secs.max(0.0) * 1000.0).round() as u64;
    if ms.is_multiple_of(1000) {
        format!("PT{}S", ms / 1000)
    } else {
        format!("PT{}.{:03}S", ms / 1000, ms % 1000)
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Convert 90 kHz ticks to `timescale` units.
fn rescale(ticks: u64, timescale: u32) -> u64 {
    (ticks as u128 * timescale as u128 / VIDEO_TIMESCALE as u128) as u64
}

/// `(t, d)` timeline entries of one track for the segments of one generation.
struct Timeline {
    entries: Vec<(u64, u64)>,
}

fn video_timeline(segs: &[&Segment], nominal: u64, part: u64) -> Timeline {
    let entries = segs
        .iter()
        .filter(|s| !s.chunks.is_empty())
        .map(|s| {
            let d = if s.complete {
                s.duration
            } else {
                nominal.max(s.duration + part)
            };
            (s.start, d)
        })
        .collect();
    Timeline { entries }
}

fn audio_timeline(segs: &[&Segment], timescale: u32, nominal: u64, part: u64) -> Timeline {
    let entries = segs
        .iter()
        .filter_map(|s| {
            let t = s.audio_start?;
            let d = if s.complete {
                s.audio_duration
            } else {
                rescale(nominal, timescale).max(s.audio_duration + rescale(part, timescale))
            };
            (d > 0).then_some((t, d))
        })
        .collect();
    Timeline { entries }
}

fn write_timeline(out: &mut String, tl: &Timeline) {
    out.push_str("          <SegmentTimeline>\n");
    for (t, d) in &tl.entries {
        let _ = writeln!(out, "            <S t=\"{t}\" d=\"{d}\"/>");
    }
    out.push_str("          </SegmentTimeline>\n");
}

/// Peak bitrate (bits/s) of one representation over the complete segments.
fn peak_bitrate(segs: &[&Segment], audio: bool) -> Option<u64> {
    segs.iter()
        .filter(|s| s.complete)
        .filter_map(|s| {
            let (bytes, secs) = if audio {
                let b: usize = s
                    .chunks
                    .iter()
                    .filter_map(|c| c.dash_audio.as_ref().map(|a| a.len()))
                    .sum();
                (b, s.duration_secs())
            } else {
                let b: usize = s.chunks.iter().map(|c| c.dash_video.len()).sum();
                (b, s.duration_secs())
            };
            (secs > 0.0 && bytes > 0).then(|| (bytes as f64 * 8.0 / secs) as u64)
        })
        .max()
}

#[allow(clippy::too_many_arguments)]
fn write_period(
    out: &mut String,
    init: &InitSegments,
    segs: &[&Segment],
    first_period: bool,
    period_start: u64,
    nominal: u64,
    part: u64,
    o: &MpdOptions,
) {
    let generation = init.generation;
    let start = if first_period && generation == 0 {
        0
    } else {
        period_start
    };
    let _ = writeln!(
        out,
        "  <Period id=\"p{generation}\" start=\"{}\">",
        duration(start as f64 / VIDEO_TIMESCALE as f64)
    );
    let init_query = if generation == 0 {
        String::new()
    } else {
        format!("?g={generation}")
    };
    let ato = (nominal.saturating_sub(part)).max(1) as f64 / VIDEO_TIMESCALE as f64;

    // Video.
    let v = &init.video_config;
    let vbw = peak_bitrate(segs, false).unwrap_or(o.default_video_bandwidth);
    out.push_str(
        "    <AdaptationSet id=\"0\" contentType=\"video\" mimeType=\"video/mp4\" \
         segmentAlignment=\"true\" startWithSAP=\"1\">\n",
    );
    let _ = write!(
        out,
        "      <Representation id=\"video\" codecs=\"{}\" bandwidth=\"{vbw}\"",
        xml_escape(&v.codec)
    );
    if v.width > 0 && v.height > 0 {
        let _ = write!(out, " width=\"{}\" height=\"{}\"", v.width, v.height);
    }
    if let Some(fps) = o.frame_rate.filter(|f| f.is_finite() && *f > 0.0) {
        let fps = if (fps - fps.round()).abs() < 0.001 {
            format!("{}", fps.round() as u64)
        } else {
            format!("{}/1000", (fps * 1000.0).round() as u64)
        };
        let _ = write!(out, " frameRate=\"{fps}\"");
    }
    out.push_str(">\n");
    let _ = write!(
        out,
        "        <SegmentTemplate timescale=\"{VIDEO_TIMESCALE}\" \
         initialization=\"init-$RepresentationID$.m4s{init_query}\" \
         media=\"chunk-$RepresentationID$-$Time$.m4s\""
    );
    if start > 0 {
        let _ = write!(out, " presentationTimeOffset=\"{start}\"");
    }
    let _ = writeln!(
        out,
        " availabilityTimeOffset=\"{ato:.3}\" availabilityTimeComplete=\"false\">"
    );
    write_timeline(out, &video_timeline(segs, nominal, part));
    out.push_str("        </SegmentTemplate>\n      </Representation>\n    </AdaptationSet>\n");

    // Audio.
    if let (Some(a), true) = (&init.audio_config, init.audio.is_some()) {
        let ts = a.sample_rate;
        let abw = peak_bitrate(segs, true).unwrap_or(o.default_audio_bandwidth);
        out.push_str(
            "    <AdaptationSet id=\"1\" contentType=\"audio\" mimeType=\"audio/mp4\" lang=\"und\" \
             segmentAlignment=\"true\" startWithSAP=\"1\">\n",
        );
        let _ = writeln!(
            out,
            "      <Representation id=\"audio\" codecs=\"{}\" bandwidth=\"{abw}\" audioSamplingRate=\"{ts}\">",
            xml_escape(&a.codec)
        );
        let channels = if a.channels == 0 { 2 } else { a.channels };
        let _ = writeln!(
            out,
            "        <AudioChannelConfiguration schemeIdUri=\"urn:mpeg:dash:23003:3:audio_channel_configuration:2011\" value=\"{channels}\"/>"
        );
        let _ = write!(
            out,
            "        <SegmentTemplate timescale=\"{ts}\" \
             initialization=\"init-$RepresentationID$.m4s{init_query}\" \
             media=\"chunk-$RepresentationID$-$Time$.m4s\""
        );
        if start > 0 {
            let _ = write!(out, " presentationTimeOffset=\"{}\"", rescale(start, ts));
        }
        let _ = writeln!(
            out,
            " availabilityTimeOffset=\"{ato:.3}\" availabilityTimeComplete=\"false\">"
        );
        write_timeline(out, &audio_timeline(segs, ts, nominal, part));
        out.push_str("        </SegmentTemplate>\n      </Representation>\n    </AdaptationSet>\n");
    }
    out.push_str("  </Period>\n");
}

/// The MPD (`manifest.mpd`).
pub fn mpd(s: &StoreState, o: &MpdOptions) -> String {
    let nominal = s.nominal_segment();
    let part = s.params.part_target_ms as u64 * 90;
    let window: f64 = s.complete_segments().map(Segment::duration_secs).sum();
    let max_seg = (s.max_segment.max(nominal)) as f64 / VIDEO_TIMESCALE as f64;

    let mut out = String::with_capacity(4096);
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    let _ = write!(
        out,
        "<MPD xmlns=\"urn:mpeg:dash:schema:mpd:2011\" \
         profiles=\"urn:mpeg:dash:profile:isoff-live:2011\" type=\"dynamic\" \
         availabilityStartTime=\"1970-01-01T00:00:00Z\" publishTime=\"{}\"",
        o.publish_time.to_rfc3339_opts(SecondsFormat::Millis, true)
    );
    if s.ended {
        let end = s.segments.back().map_or(0.0, |g| {
            (g.start + g.duration) as f64 / VIDEO_TIMESCALE as f64
        });
        let _ = write!(out, " mediaPresentationDuration=\"{}\"", duration(end));
    } else {
        let _ = write!(
            out,
            " minimumUpdatePeriod=\"{}\"",
            duration(o.minimum_update_period)
        );
    }
    let _ = writeln!(
        out,
        " minBufferTime=\"PT1S\" timeShiftBufferDepth=\"{}\" maxSegmentDuration=\"{}\" \
         suggestedPresentationDelay=\"{}\">",
        duration(window),
        duration(max_seg),
        duration(o.latency_target_ms as f64 / 1000.0)
    );
    let _ = writeln!(
        out,
        "  <ServiceDescription id=\"0\">\n    <Latency referenceId=\"0\" target=\"{}\" min=\"{}\" max=\"{}\"/>\n    \
         <PlaybackRate min=\"0.96\" max=\"1.04\"/>\n  </ServiceDescription>",
        o.latency_target_ms, o.latency_min_ms, o.latency_max_ms
    );

    // One Period per init generation present in the window.
    let mut first = true;
    for init in &s.inits {
        let segs: Vec<&Segment> = s
            .segments
            .iter()
            .filter(|g| g.generation == init.generation)
            .collect();
        let Some(period_start) = segs.first().map(|g| g.start) else {
            if init.generation == s.init().map_or(0, |i| i.generation) && first {
                // Nothing produced yet: announce the (empty) period.
                write_period(&mut out, init, &segs, true, 0, nominal, part, o);
                first = false;
            }
            continue;
        };
        write_period(&mut out, init, &segs, first, period_start, nominal, part, o);
        first = false;
    }
    let _ = writeln!(
        out,
        "  <UTCTiming schemeIdUri=\"urn:mpeg:dash:utc:http-xsdate:2014\" value=\"{}\"/>",
        xml_escape(&o.utc_timing_url)
    );
    out.push_str("</MPD>\n");
    out
}

/// Find the segment whose `$Time$` for representation `audio`/video is `time`.
pub fn segment_by_time(s: &StoreState, audio: bool, time: u64) -> Option<u64> {
    s.segments
        .iter()
        .find(|g| {
            if audio {
                g.audio_start == Some(time)
            } else {
                g.start == time && !g.chunks.is_empty()
            }
        })
        .map(|g| g.seq)
}

/// Whether `time` may still become a segment start of the representation (it lies
/// after the newest known segment start, within a few segment durations).
pub fn time_in_near_future(s: &StoreState, audio: bool, time: u64) -> bool {
    if s.ended {
        return false;
    }
    let Some(last) = s.segments.back() else {
        return false;
    };
    let horizon = 3 * s.nominal_segment().max(s.max_segment);
    if audio {
        let Some(ts) = s.init().and_then(|i| i.audio_timescale()) else {
            return false;
        };
        let newest = last.audio_start.unwrap_or_else(|| rescale(last.start, ts));
        time > newest && time <= newest + rescale(horizon, ts)
    } else {
        time >= last.start && time <= last.start + horizon
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packager::store::tests::{chunk, fill, t, test_audio, test_video};
    use crate::packager::store::{SegmentStore, StoreParams};

    fn store() -> std::sync::Arc<SegmentStore> {
        let s = SegmentStore::new(StoreParams {
            window_segments: 3,
            ..Default::default()
        });
        s.set_init(test_video(), Some(test_audio()));
        s
    }

    fn opts() -> MpdOptions {
        MpdOptions {
            utc_timing_url: "http://example.com/dash/time".into(),
            publish_time: DateTime::from_timestamp_millis(1_767_225_610_500).unwrap(),
            frame_rate: Some(30.0),
            ..Default::default()
        }
    }

    #[test]
    fn live_mpd_golden() {
        let s = store();
        fill(&s, 0.0, 4);
        s.open_segment(t(8.0), true);
        s.push_chunk(chunk(t(8.0), 500, true));
        let text = s.with(|st| mpd(st, &opts()));
        let v0 = t(2.0);
        let a0 = v0 / 90 * 48;
        let expected = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" profiles="urn:mpeg:dash:profile:isoff-live:2011" type="dynamic" availabilityStartTime="1970-01-01T00:00:00Z" publishTime="2026-01-01T00:00:10.500Z" minimumUpdatePeriod="PT1S" minBufferTime="PT1S" timeShiftBufferDepth="PT6S" maxSegmentDuration="PT2S" suggestedPresentationDelay="PT2S">
  <ServiceDescription id="0">
    <Latency referenceId="0" target="2000" min="1500" max="6000"/>
    <PlaybackRate min="0.96" max="1.04"/>
  </ServiceDescription>
  <Period id="p0" start="PT0S">
    <AdaptationSet id="0" contentType="video" mimeType="video/mp4" segmentAlignment="true" startWithSAP="1">
      <Representation id="video" codecs="avc1.64001f" bandwidth="960" width="1280" height="720" frameRate="30">
        <SegmentTemplate timescale="90000" initialization="init-$RepresentationID$.m4s" media="chunk-$RepresentationID$-$Time$.m4s" availabilityTimeOffset="1.500" availabilityTimeComplete="false">
          <SegmentTimeline>
            <S t="{v0}" d="180000"/>
            <S t="{v1}" d="180000"/>
            <S t="{v2}" d="180000"/>
            <S t="{v3}" d="180000"/>
          </SegmentTimeline>
        </SegmentTemplate>
      </Representation>
    </AdaptationSet>
    <AdaptationSet id="1" contentType="audio" mimeType="audio/mp4" lang="und" segmentAlignment="true" startWithSAP="1">
      <Representation id="audio" codecs="mp4a.40.2" bandwidth="640" audioSamplingRate="48000">
        <AudioChannelConfiguration schemeIdUri="urn:mpeg:dash:23003:3:audio_channel_configuration:2011" value="2"/>
        <SegmentTemplate timescale="48000" initialization="init-$RepresentationID$.m4s" media="chunk-$RepresentationID$-$Time$.m4s" availabilityTimeOffset="1.500" availabilityTimeComplete="false">
          <SegmentTimeline>
            <S t="{a0}" d="96000"/>
            <S t="{a1}" d="96000"/>
            <S t="{a2}" d="96000"/>
            <S t="{a3}" d="96000"/>
          </SegmentTimeline>
        </SegmentTemplate>
      </Representation>
    </AdaptationSet>
  </Period>
  <UTCTiming schemeIdUri="urn:mpeg:dash:utc:http-xsdate:2014" value="http://example.com/dash/time"/>
</MPD>
"#,
            v1 = v0 + 180_000,
            v2 = v0 + 360_000,
            v3 = v0 + 540_000,
            a1 = a0 + 96_000,
            a2 = a0 + 192_000,
            a3 = a0 + 288_000,
        );
        assert_eq!(text, expected);
        // The in-progress segment is resolvable by its exact $Time$.
        s.with(|st| {
            assert_eq!(segment_by_time(st, false, v0 + 540_000), st.last_seq());
            assert_eq!(segment_by_time(st, true, a0 + 288_000), st.last_seq());
            assert_eq!(segment_by_time(st, false, v0 + 1), None);
            assert!(time_in_near_future(st, false, v0 + 720_000));
            assert!(!time_in_near_future(st, false, v0 + 90_000_000));
            assert!(!time_in_near_future(st, false, v0));
        });
    }

    #[test]
    fn in_progress_duration_grows_past_nominal() {
        let s = store();
        fill(&s, 0.0, 1);
        s.open_segment(t(2.0), true);
        for i in 0..6 {
            s.push_chunk(chunk(t(2.0 + 0.5 * i as f64), 500, i == 0));
        }
        let text = s.with(|st| mpd(st, &opts()));
        // 3 s produced + one part ahead.
        assert!(
            text.contains(&format!("<S t=\"{}\" d=\"315000\"/>", t(2.0))),
            "{text}"
        );
    }

    #[test]
    fn in_progress_prediction_uses_longest_window_segment() {
        // Long GOP split into 6 s + 2.4 s segments: the in-progress segment after a
        // short one must still be announced with 6 s (dash.js finds the next segment at
        // `start + announced duration`).
        let s = store();
        s.open_segment(t(0.0), true);
        s.push_chunk(chunk(t(0.0), 6000, true));
        s.open_segment(t(6.0), false);
        s.push_chunk(chunk(t(6.0), 2400, false));
        s.open_segment(t(8.4), true);
        s.push_chunk(chunk(t(8.4), 500, true));
        let text = s.with(|st| mpd(st, &opts()));
        assert!(
            text.contains(&format!("<S t=\"{}\" d=\"216000\"/>", t(6.0))),
            "{text}"
        );
        assert!(
            text.contains(&format!("<S t=\"{}\" d=\"540000\"/>", t(8.4))),
            "{text}"
        );
        assert!(text.contains("availabilityTimeOffset=\"5.500\""), "{text}");
        assert!(text.contains("maxSegmentDuration=\"PT6S\""), "{text}");
    }

    #[test]
    fn ended_mpd_and_empty_store() {
        let s = store();
        let text = s.with(|st| mpd(st, &opts()));
        assert!(text.contains("<Period id=\"p0\" start=\"PT0S\">"));
        assert_eq!(text.matches("<S ").count(), 0);
        fill(&s, 0.0, 1);
        s.end();
        let text = s.with(|st| mpd(st, &opts()));
        assert!(!text.contains("minimumUpdatePeriod"));
        assert!(text.contains("mediaPresentationDuration=\"PT1767225602S\""));
    }

    #[test]
    fn new_generation_starts_new_period() {
        let s = store();
        fill(&s, 0.0, 1);
        s.set_init(test_video(), None);
        fill(&s, 2.0, 1);
        let text = s.with(|st| mpd(st, &opts()));
        assert_eq!(text.matches("<Period ").count(), 2);
        assert!(text.contains("<Period id=\"p1\" start=\"PT1767225602S\">"));
        assert!(text.contains(&format!("presentationTimeOffset=\"{}\"", t(2.0))));
        assert!(text.contains("init-$RepresentationID$.m4s?g=1"));
        // Video-only generation: one AdaptationSet in the second period.
        let p1 = text.split("<Period id=\"p1\"").nth(1).unwrap();
        assert_eq!(p1.matches("<AdaptationSet").count(), 1);
    }

    #[test]
    fn durations() {
        assert_eq!(duration(1.0), "PT1S");
        assert_eq!(duration(1.5), "PT1.500S");
        assert_eq!(duration(0.0), "PT0S");
        assert_eq!(duration(12.3456), "PT12.346S");
    }
}
