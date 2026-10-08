//! Segmenter: turns the FLV packets of one live stream into CMAF chunks and segments
//! in a [`SegmentStore`] (ARCHITECTURE.md §6.3).
//!
//! Rules:
//! - A **segment** starts at every video keyframe (one GOP per segment), unless the
//!   current segment is shorter than `min_segment_ms` (very short GOPs are merged).
//!   A GOP longer than `max_segment_ms` is split at the next part boundary anyway
//!   (the new segment does not start with a keyframe; warned once per stream).
//! - A **chunk/part** is closed at the frame boundary closest to `part_target_ms`
//!   without ever exceeding it (pending samples are re-split after timestamp gaps),
//!   and always at keyframes. A chunk is published only once complete, i.e. when the
//!   video frame that follows it arrives, so every sample duration is exact.
//! - Audio samples are assigned to the chunk whose video range contains their decode
//!   time; audio that arrives late simply joins the next chunk.
//!
//! Time base (see `store.rs`): media time 0 is the Unix epoch. The first keyframe's
//! FLV timestamp is anchored to the wall-clock time at which the newest known packet
//! was received, so that `media time = wall-clock time of capture` (as seen by the
//! server). Video: 90 kHz ticks, `dts = media_ms * 90`, composition offset = FLV CTS.
//! Audio: AAC sample rate; decode times count 1024 samples per frame starting from the
//! first frame's timestamp, resynchronised to the FLV timestamp when they drift apart
//! by more than one frame.

use std::sync::Arc;

use bytes::Bytes;
use chrono::Utc;

use super::cmaf::{
    self, AAC_FRAME_SAMPLES, AUDIO_TRACK_ID, AudioConfig, AudioTag, Sample, TrackRun,
    VIDEO_TRACK_ID, VideoConfig, VideoTag,
};
use super::store::{ChunkData, SegmentStore};
use crate::hub::{MediaPacket, PacketKind};

const TICKS_PER_MS: u64 = (cmaf::VIDEO_TIMESCALE / 1000) as u64;
/// Default video frame duration before one is measured (30 fps).
const DEFAULT_FRAME_TICKS: u64 = 3000;
/// Sample durations are clamped to this (10 s) to survive timestamp jumps.
const MAX_SAMPLE_TICKS: u64 = 10 * cmaf::VIDEO_TIMESCALE as u64;
/// Bound of audio samples waiting for the next video frame (~11 s at 48 kHz).
const MAX_PENDING_AUDIO: usize = 512;
/// Audio received for this long without any video config → "audio-only".
const AUDIO_ONLY_TIMEOUT_MS: i64 = 5000;
/// Audio frames kept while waiting for a keyframe (see [`Segmenter::pre_audio`]).
const MAX_PRE_AUDIO: usize = 64;

#[derive(Debug, Clone, PartialEq)]
pub struct SegmenterConfig {
    pub part_target_ms: u32,
    pub max_segment_ms: u32,
    /// Keyframes closer than this to the segment start do not start a new segment.
    pub min_segment_ms: u32,
    /// How long (media time) to wait for an audio sequence header after the first
    /// keyframe before packaging video only.
    pub probe_audio_ms: u32,
}

impl Default for SegmenterConfig {
    fn default() -> Self {
        Self {
            part_target_ms: 500,
            max_segment_ms: 6000,
            min_segment_ms: 1000,
            probe_audio_ms: 1000,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum SegmenterStatus {
    /// Waiting for sequence headers / the first keyframe.
    Probing,
    Running,
    Unsupported(String),
}

/// FLV timestamps are 32-bit milliseconds; extend them to i64 using signed deltas.
#[derive(Debug, Default)]
struct TsUnwrap {
    last_raw: Option<u32>,
    last_ext: i64,
}

impl TsUnwrap {
    fn unwrap(&mut self, raw: u32) -> i64 {
        match self.last_raw {
            None => self.last_ext = raw as i64,
            Some(prev) => self.last_ext += raw.wrapping_sub(prev) as i32 as i64,
        }
        self.last_raw = Some(raw);
        self.last_ext
    }

    /// Extended value of `raw` relative to the last one, without updating state.
    fn peek(&self, raw: u32) -> i64 {
        match self.last_raw {
            None => raw as i64,
            Some(prev) => self.last_ext + raw.wrapping_sub(prev) as i32 as i64,
        }
    }
}

#[derive(Debug, Clone)]
struct PendingVideo {
    dts: u64,
    cts: i32,
    sync: bool,
    data: Bytes,
}

#[derive(Debug, Clone)]
struct PendingAudio {
    dts: u64,
    data: Bytes,
}

/// Packets buffered between the first keyframe and the start of packaging.
#[derive(Debug, Default)]
struct Probe {
    packets: Vec<(i64, VideoOrAudio)>,
    first_keyframe_ts: Option<i64>,
    first_audio_ts: Option<i64>,
}

#[derive(Debug, Clone)]
enum VideoOrAudio {
    Video {
        keyframe: bool,
        cts_ms: i32,
        data: Bytes,
    },
    Audio(Bytes),
}

pub struct Segmenter {
    cfg: SegmenterConfig,
    store: Arc<SegmentStore>,
    name: String,
    clock: fn() -> i64,
    status: SegmenterStatus,
    ts: TsUnwrap,
    /// Newest raw timestamp known to the caller (GOP-cache snapshot tail).
    hint_latest_raw: Option<u32>,
    latest_ts: Option<i64>,

    // Codec configurations: latest received / currently packaged.
    video_cfg: Option<VideoConfig>,
    audio_cfg: Option<AudioConfig>,
    active_video: Option<VideoConfig>,
    active_audio: Option<AudioConfig>,
    probe: Probe,

    // Time base.
    base_ts: i64,
    anchor_ms: i64,

    // Cutting state.
    need_keyframe: bool,
    seg_start: u64,
    chunk_start: u64,
    pending_video: Vec<PendingVideo>,
    pending_audio: Vec<PendingAudio>,
    last_video_dts: Option<u64>,
    last_video_duration: Option<u64>,
    last_keyframe_dts: Option<u64>,
    audio_next: Option<u64>,
    last_audio_dts: Option<u64>,
    /// Audio received while waiting for a keyframe: frames with the keyframe's
    /// timestamp (or later) often arrive just before it.
    pre_audio: std::collections::VecDeque<(i64, Bytes)>,
    frag_seq: u32,

    warned_long_gop: bool,
    warned_ts: bool,
    warned_audio_backlog: bool,
}

fn wall_clock_ms() -> i64 {
    Utc::now().timestamp_millis()
}

impl Segmenter {
    pub fn new(cfg: SegmenterConfig, store: Arc<SegmentStore>, name: impl Into<String>) -> Self {
        Self {
            cfg,
            store,
            name: name.into(),
            clock: wall_clock_ms,
            status: SegmenterStatus::Probing,
            ts: TsUnwrap::default(),
            hint_latest_raw: None,
            latest_ts: None,
            video_cfg: None,
            audio_cfg: None,
            active_video: None,
            active_audio: None,
            probe: Probe::default(),
            base_ts: 0,
            anchor_ms: 0,
            need_keyframe: true,
            seg_start: 0,
            chunk_start: 0,
            pending_video: Vec::new(),
            pending_audio: Vec::new(),
            last_video_dts: None,
            last_video_duration: None,
            last_keyframe_dts: None,
            audio_next: None,
            last_audio_dts: None,
            pre_audio: std::collections::VecDeque::new(),
            frag_seq: 0,
            warned_long_gop: false,
            warned_ts: false,
            warned_audio_backlog: false,
        }
    }

    /// Replace the wall clock (Unix milliseconds); tests only.
    pub fn with_clock(mut self, clock: fn() -> i64) -> Self {
        self.clock = clock;
        self
    }

    /// Newest FLV timestamp of the catch-up snapshot (anchors media time to the wall
    /// clock as if that packet was received now).
    pub fn hint_latest_timestamp(&mut self, raw: u32) {
        self.hint_latest_raw = Some(raw);
    }

    pub fn status(&self) -> &SegmenterStatus {
        &self.status
    }

    pub fn store(&self) -> &Arc<SegmentStore> {
        &self.store
    }

    fn part_ticks(&self) -> u64 {
        self.cfg.part_target_ms as u64 * TICKS_PER_MS
    }

    fn set_unsupported(&mut self, reason: String) {
        if matches!(self.status, SegmenterStatus::Unsupported(_)) {
            return;
        }
        tracing::warn!(broadcast = %self.name, %reason, "stream cannot be packaged for LL-HLS/LL-DASH");
        if self.status == SegmenterStatus::Running {
            self.flush(None);
            self.store.end();
        }
        self.status = SegmenterStatus::Unsupported(reason);
        self.probe = Probe::default();
    }

    /// Feed one packet from the hub.
    pub fn push(&mut self, p: &MediaPacket) {
        if matches!(self.status, SegmenterStatus::Unsupported(_)) {
            return;
        }
        match p.kind {
            PacketKind::Script => {}
            PacketKind::Video => match cmaf::parse_video_tag(&p.data) {
                VideoTag::Config(c) => self.on_video_config(c),
                VideoTag::Frame {
                    keyframe,
                    cts_ms,
                    data,
                } => {
                    let ts = self.ts.unwrap(p.timestamp);
                    self.latest_ts = Some(self.latest_ts.map_or(ts, |l| l.max(ts)));
                    self.on_frame(
                        ts,
                        VideoOrAudio::Video {
                            keyframe,
                            cts_ms,
                            data,
                        },
                    );
                }
                VideoTag::Ignore => {}
                VideoTag::Unsupported(codec) => self.set_unsupported(format!(
                    "video codec {codec} is not supported for LL-HLS/LL-DASH (H.264 only)"
                )),
            },
            PacketKind::Audio => match cmaf::parse_audio_tag(&p.data) {
                AudioTag::Config(c) => self.on_audio_config(c),
                AudioTag::Frame(data) => {
                    let ts = self.ts.unwrap(p.timestamp);
                    self.latest_ts = Some(self.latest_ts.map_or(ts, |l| l.max(ts)));
                    self.on_frame(ts, VideoOrAudio::Audio(data));
                }
                AudioTag::Ignore => {}
                AudioTag::Unsupported(codec) => self.set_unsupported(format!(
                    "audio codec {codec} is not supported for LL-HLS/LL-DASH (AAC only)"
                )),
            },
        }
    }

    fn on_video_config(&mut self, c: VideoConfig) {
        if self.video_cfg.as_ref() == Some(&c) {
            return; // repeated header
        }
        self.video_cfg = Some(c);
        self.maybe_reconfigure();
    }

    fn on_audio_config(&mut self, c: AudioConfig) {
        if self.audio_cfg.as_ref() == Some(&c) {
            return;
        }
        self.audio_cfg = Some(c);
        if self.status == SegmenterStatus::Probing && self.probe.first_keyframe_ts.is_some() {
            self.start();
        } else {
            self.maybe_reconfigure();
        }
    }

    /// A different sequence header arrived while packaging: close the current segment
    /// and start a new init generation at the next keyframe.
    fn maybe_reconfigure(&mut self) {
        if self.status != SegmenterStatus::Running {
            return;
        }
        let (Some(v), a) = (self.video_cfg.clone(), self.audio_cfg.clone()) else {
            return;
        };
        if self.active_video.as_ref() == Some(&v) && self.active_audio == a {
            return;
        }
        tracing::warn!(
            broadcast = %self.name,
            "codec configuration changed mid-stream; starting a new init segment (discontinuity)"
        );
        self.flush(None);
        self.store.complete_segment();
        self.store.set_init(v.clone(), a.clone());
        self.active_video = Some(v);
        self.active_audio = a;
        self.need_keyframe = true;
        self.audio_next = None;
    }

    fn on_frame(&mut self, ts: i64, f: VideoOrAudio) {
        match self.status {
            SegmenterStatus::Running => self.run_frame(ts, f),
            SegmenterStatus::Probing => self.probe_frame(ts, f),
            SegmenterStatus::Unsupported(_) => {}
        }
    }

    fn probe_frame(&mut self, ts: i64, f: VideoOrAudio) {
        match &f {
            VideoOrAudio::Video { keyframe, .. } => {
                if self.video_cfg.is_none() {
                    return; // undecodable without a config
                }
                if self.probe.first_keyframe_ts.is_none() {
                    if !keyframe {
                        return;
                    }
                    self.probe.first_keyframe_ts = Some(ts);
                }
                self.probe.packets.push((ts, f));
                if self.audio_cfg.is_some() {
                    self.start();
                } else if let Some(first) = self.probe.first_keyframe_ts
                    && ts - first >= self.cfg.probe_audio_ms as i64
                {
                    tracing::info!(broadcast = %self.name, "no audio sequence header; packaging video only");
                    self.start();
                }
            }
            VideoOrAudio::Audio(_) => {
                if self.video_cfg.is_none() {
                    let first = *self.probe.first_audio_ts.get_or_insert(ts);
                    if ts - first > AUDIO_ONLY_TIMEOUT_MS {
                        self.set_unsupported(
                            "audio-only streams are not supported for LL-HLS/LL-DASH".into(),
                        );
                    }
                    return;
                }
                if self.probe.first_keyframe_ts.is_some() {
                    self.probe.packets.push((ts, f));
                } else if let VideoOrAudio::Audio(data) = f {
                    self.stash_audio(ts, data);
                }
            }
        }
    }

    /// Install the init segments, anchor the time base and replay the probe buffer.
    fn start(&mut self) {
        let Some(video) = self.video_cfg.clone() else {
            return;
        };
        let Some(base) = self.probe.first_keyframe_ts else {
            return;
        };
        let audio = self.audio_cfg.clone();
        self.store.set_init(video.clone(), audio.clone());
        self.active_video = Some(video);
        self.active_audio = audio;
        let mut latest = self.latest_ts.unwrap_or(base).max(base);
        if let Some(raw) = self.hint_latest_raw {
            latest = latest.max(self.ts.peek(raw));
        }
        self.base_ts = base;
        self.anchor_ms = (self.clock)() - (latest - base);
        self.status = SegmenterStatus::Running;
        self.need_keyframe = true;
        tracing::info!(
            broadcast = %self.name,
            codecs = %self.store.with(|s| s.init().map(|i| i.codecs()).unwrap_or_default()),
            "LL-HLS/LL-DASH packaging started"
        );
        for (ts, f) in std::mem::take(&mut self.probe.packets) {
            self.run_frame(ts, f);
        }
    }

    /// Media time (Unix ms) of an extended FLV timestamp; `None` before the base.
    fn media_ms(&self, ts: i64) -> Option<i64> {
        (ts >= self.base_ts).then(|| self.anchor_ms + (ts - self.base_ts))
    }

    fn run_frame(&mut self, ts: i64, f: VideoOrAudio) {
        if self.need_keyframe
            && let VideoOrAudio::Audio(data) = f
        {
            self.stash_audio(ts, data);
            return;
        }
        let Some(ms) = self.media_ms(ts) else {
            return; // before the first keyframe
        };
        let ms = ms.max(0) as u64;
        match f {
            VideoOrAudio::Video {
                keyframe,
                cts_ms,
                data,
            } => self.video_frame(ms * TICKS_PER_MS, keyframe, cts_ms, data),
            VideoOrAudio::Audio(data) => self.audio_frame(ms, data),
        }
    }

    fn stash_audio(&mut self, ts: i64, data: Bytes) {
        self.pre_audio.push_back((ts, data));
        while self.pre_audio.len() > MAX_PRE_AUDIO {
            self.pre_audio.pop_front();
        }
    }

    /// A segment just opened at `from_dts`: package the stashed audio at or after it.
    fn drain_pre_audio(&mut self, from_dts: u64) {
        for (ts, data) in std::mem::take(&mut self.pre_audio) {
            if let Some(ms) = self.media_ms(ts)
                && ms.max(0) as u64 * TICKS_PER_MS >= from_dts
            {
                self.audio_frame(ms.max(0) as u64, data);
            }
        }
    }

    fn video_frame(&mut self, mut dts: u64, keyframe: bool, cts_ms: i32, data: Bytes) {
        if let Some(last) = self.last_video_dts
            && dts <= last
        {
            if !self.warned_ts {
                self.warned_ts = true;
                tracing::warn!(broadcast = %self.name, "non-increasing video timestamps; adjusting");
            }
            dts = last + 1;
        }
        let sample = PendingVideo {
            dts,
            cts: cts_ms.saturating_mul(TICKS_PER_MS as i32),
            sync: keyframe,
            data,
        };

        if self.need_keyframe {
            if !keyframe {
                return;
            }
            self.need_keyframe = false;
            self.store.open_segment(dts, true);
            self.seg_start = dts;
            self.chunk_start = dts;
            self.note_video(dts, true);
            self.pending_video.push(sample);
            self.drain_pre_audio(dts);
            return;
        }

        if keyframe {
            if dts - self.seg_start >= self.cfg.min_segment_ms as u64 * TICKS_PER_MS {
                self.close_chunk(dts);
                self.store.open_segment(dts, true);
                self.seg_start = dts;
            } else {
                // Short GOP: an independent part inside the current segment.
                self.close_chunk(dts);
            }
            self.chunk_start = dts;
            self.note_video(dts, true);
            self.pending_video.push(sample);
            return;
        }

        let part = self.part_ticks();
        // Timestamp gap: the pending frames already exceed the target; publish all
        // but the last one so parts never exceed PART-TARGET.
        if dts - self.chunk_start > part && self.pending_video.len() > 1 {
            let last = self.pending_video.pop().expect("len > 1");
            let boundary = last.dts;
            self.close_chunk(boundary);
            self.after_part_boundary(boundary);
            self.pending_video.push(last);
        }
        let elapsed = dts - self.chunk_start;
        let est = self.last_video_duration.unwrap_or(DEFAULT_FRAME_TICKS);
        if elapsed + est / 2 >= part || elapsed > part {
            self.close_chunk(dts);
            self.after_part_boundary(dts);
        }
        self.note_video(dts, false);
        self.pending_video.push(sample);
    }

    /// After a part closed at `boundary` (not a keyframe): split the segment if it
    /// would otherwise exceed `max_segment_ms`.
    fn after_part_boundary(&mut self, boundary: u64) {
        self.chunk_start = boundary;
        let max = self.cfg.max_segment_ms as u64 * TICKS_PER_MS;
        if boundary - self.seg_start + self.part_ticks() > max {
            if !self.warned_long_gop {
                self.warned_long_gop = true;
                tracing::warn!(
                    broadcast = %self.name,
                    max_segment_ms = self.cfg.max_segment_ms,
                    "keyframe interval longer than hls.max_segment_duration: splitting segments \
                     without keyframes (set the encoder keyframe interval to 1-2 s)"
                );
            }
            self.store.open_segment(boundary, false);
            self.seg_start = boundary;
        }
    }

    fn note_video(&mut self, dts: u64, keyframe: bool) {
        if let Some(prev) = self.last_video_dts {
            self.last_video_duration = Some((dts - prev).clamp(1, MAX_SAMPLE_TICKS));
        }
        self.last_video_dts = Some(dts);
        if keyframe {
            if let Some(prev) = self.last_keyframe_dts {
                self.store
                    .record_keyframe_interval((dts - prev) as f64 / TICKS_PER_MS as f64);
            }
            self.last_keyframe_dts = Some(dts);
        }
    }

    fn audio_frame(&mut self, media_ms: u64, data: Bytes) {
        let Some(sr) = self.active_audio.as_ref().map(|a| a.sample_rate as u64) else {
            return; // packaging video only
        };
        let from_ts = media_ms * sr / 1000;
        let frame = AAC_FRAME_SAMPLES as u64;
        let dts = match self.audio_next {
            Some(next) if from_ts.abs_diff(next) <= frame => next,
            _ => from_ts,
        };
        if let Some(last) = self.last_audio_dts
            && dts <= last
        {
            return; // timestamp went backwards: drop rather than overlap
        }
        self.audio_next = Some(dts + frame);
        self.last_audio_dts = Some(dts);
        self.pending_audio.push(PendingAudio { dts, data });
        if self.pending_audio.len() > MAX_PENDING_AUDIO {
            self.pending_audio.remove(0);
            if !self.warned_audio_backlog {
                self.warned_audio_backlog = true;
                tracing::warn!(broadcast = %self.name, "audio without video frames; dropping old audio");
            }
        }
    }

    /// Publish the pending video frames (and the audio before `boundary`) as one chunk.
    /// `boundary` is the decode time of the next video frame (90 kHz).
    fn close_chunk(&mut self, boundary: u64) {
        if self.pending_video.is_empty() {
            return;
        }
        let video = std::mem::take(&mut self.pending_video);
        let first_dts = video[0].dts;
        let boundary = boundary.max(video[video.len() - 1].dts + 1);
        let v_samples: Vec<Sample> = video
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let next = video.get(i + 1).map_or(boundary, |n| n.dts);
                Sample {
                    duration: (next - s.dts).clamp(1, MAX_SAMPLE_TICKS) as u32,
                    cts: s.cts,
                    sync: s.sync,
                    data: s.data.clone(),
                }
            })
            .collect();

        // Audio whose decode time is before the boundary.
        let mut a_samples = Vec::new();
        let mut audio_start = None;
        let mut audio_duration = 0u64;
        if let Some(sr) = self.active_audio.as_ref().map(|a| a.sample_rate as u128) {
            let split = self
                .pending_audio
                .iter()
                .position(|a| {
                    a.dts as u128 * cmaf::VIDEO_TIMESCALE as u128 >= boundary as u128 * sr
                })
                .unwrap_or(self.pending_audio.len());
            let rest = self.pending_audio.split_off(split);
            let taken = std::mem::replace(&mut self.pending_audio, rest);
            if let Some(first) = taken.first() {
                audio_start = Some(first.dts);
            }
            for (i, s) in taken.iter().enumerate() {
                let next = taken
                    .get(i + 1)
                    .or(self.pending_audio.first())
                    .map_or(s.dts + AAC_FRAME_SAMPLES as u64, |n| n.dts);
                let duration = (next - s.dts).clamp(1, u32::MAX as u64);
                audio_duration += duration;
                a_samples.push(Sample {
                    duration: duration as u32,
                    cts: 0,
                    sync: true,
                    data: s.data.clone(),
                });
            }
        }

        let seq = self.frag_seq;
        self.frag_seq = self.frag_seq.wrapping_add(1);
        let video_run = TrackRun {
            track_id: VIDEO_TRACK_ID,
            base_decode_time: first_dts,
            samples: &v_samples,
            audio: false,
        };
        let audio_run = TrackRun {
            track_id: AUDIO_TRACK_ID,
            base_decode_time: audio_start.unwrap_or(0),
            samples: &a_samples,
            audio: true,
        };
        let data = ChunkData {
            start: first_dts,
            duration: boundary - first_dts,
            independent: video[0].sync,
            hls: cmaf::fragment(seq, &[video_run, audio_run]),
            dash_video: cmaf::fragment(seq, &[video_run]),
            dash_audio: (!a_samples.is_empty()).then(|| cmaf::fragment(seq, &[audio_run])),
            audio_start,
            audio_duration,
        };
        self.store.push_chunk(data);
    }

    /// Publish pending samples using an estimated duration for the last frame.
    fn flush(&mut self, boundary: Option<u64>) {
        if let Some(last) = self.pending_video.last() {
            let est = self.last_video_duration.unwrap_or(DEFAULT_FRAME_TICKS);
            let b = boundary.unwrap_or(last.dts + est);
            self.close_chunk(b);
        }
        self.pending_audio.clear();
    }

    /// The hub receiver lagged: close the current segment and resume at the next
    /// keyframe with a new segment.
    pub fn on_lag(&mut self, skipped: u64) {
        tracing::warn!(broadcast = %self.name, skipped, "packager lagging behind the stream; resyncing at the next keyframe");
        if self.status != SegmenterStatus::Running {
            return;
        }
        self.flush(None);
        self.store.complete_segment();
        self.need_keyframe = true;
        self.audio_next = None;
        self.pre_audio.clear();
        // The gap is not a keyframe interval.
        self.last_keyframe_dts = None;
    }

    /// The stream ended: publish what is pending and mark the store ended.
    pub fn finish(&mut self) {
        if self.status == SegmenterStatus::Running {
            self.flush(None);
        }
        self.store.end();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packager::cmaf::{ParsedBox, parse_boxes};
    use crate::packager::store::StoreParams;

    const NOW_MS: i64 = 1_767_225_600_000; // 2026-01-01T00:00:00Z

    fn clock() -> i64 {
        NOW_MS
    }

    pub(crate) fn avc_header() -> MediaPacket {
        let sps: [u8; 26] = [
            0x67, 0x64, 0x00, 0x1f, 0xac, 0xd9, 0x40, 0x50, 0x05, 0xbb, 0x01, 0x10, 0x00, 0x00,
            0x03, 0x00, 0x10, 0x00, 0x00, 0x03, 0x03, 0xc0, 0xf1, 0x83, 0x19, 0x60,
        ];
        let mut body = vec![0x17, 0x00, 0, 0, 0, 1, 0x64, 0x00, 0x1f, 0xff, 0xe1];
        body.extend_from_slice(&(sps.len() as u16).to_be_bytes());
        body.extend_from_slice(&sps);
        body.extend_from_slice(&[1, 0, 4, 0x68, 0xeb, 0xe3, 0xcb]);
        MediaPacket::video(0, Bytes::from(body))
    }

    pub(crate) fn aac_header() -> MediaPacket {
        MediaPacket::audio(0, Bytes::from_static(&[0xAF, 0x00, 0x11, 0x90]))
    }

    fn vframe(ts: u32, key: bool, cts: u8) -> MediaPacket {
        let b0 = if key { 0x17 } else { 0x27 };
        let nal = if key { 0x65 } else { 0x41 };
        MediaPacket::video(
            ts,
            Bytes::from(vec![b0, 0x01, 0, 0, cts, 0, 0, 0, 2, nal, 0x80]),
        )
    }

    fn aframe(ts: u32) -> MediaPacket {
        MediaPacket::audio(ts, Bytes::from_static(&[0xAF, 0x01, 0x21, 0x10, 0x04]))
    }

    /// Interleaved 30 fps video (keyframe every `gop` frames) + 48 kHz AAC, in
    /// timestamp order, for `frames` video frames starting at `t0` ms.
    fn av_packets(t0: u32, frames: u32, gop: u32) -> Vec<MediaPacket> {
        let mut out = Vec::new();
        let mut a = 0u32;
        for i in 0..frames {
            let vts = t0 + (i as f64 * 1000.0 / 30.0).round() as u32;
            loop {
                let ats = t0 + (a as f64 * 1024.0 * 1000.0 / 48000.0).round() as u32;
                if ats > vts {
                    break;
                }
                out.push(aframe(ats));
                a += 1;
            }
            out.push(vframe(vts, i % gop == 0, 0));
        }
        out
    }

    fn segmenter(cfg: SegmenterConfig, window: usize) -> Segmenter {
        let store = SegmentStore::new(StoreParams {
            part_target_ms: cfg.part_target_ms,
            window_segments: window,
            nominal_segment_ms: 2000,
        });
        Segmenter::new(cfg, store, "test").with_clock(clock)
    }

    fn u32_at(b: &[u8], i: usize) -> u32 {
        u32::from_be_bytes(b[i..i + 4].try_into().unwrap())
    }

    fn tfdt(traf: &ParsedBox) -> u64 {
        u64::from_be_bytes(
            traf.find(b"tfdt").unwrap().payload[4..12]
                .try_into()
                .unwrap(),
        )
    }

    #[test]
    fn cuts_gop_segments_and_parts() {
        let mut s = segmenter(SegmenterConfig::default(), 10);
        s.push(&avc_header());
        s.push(&aac_header());
        for p in av_packets(1000, 30 * 8 + 1, 60) {
            s.push(&p);
        }
        assert_eq!(s.status(), &SegmenterStatus::Running);
        let store = Arc::clone(s.store());
        store.with(|st| {
            assert_eq!(st.complete_count(), 4, "4 complete 2 s segments");
            let first = st.segments.front().unwrap();
            // Anchor: packaging started at the first keyframe, received "now".
            assert_eq!(first.start, NOW_MS as u64 * 90);
            assert_eq!(first.seq, NOW_MS as u64 / 1000);
            for seg in st.complete_segments() {
                assert!(seg.independent);
                assert_eq!(seg.duration, 180_000, "2 s GOP");
                assert_eq!(seg.chunks.len(), 4);
                assert!(seg.chunks[0].independent);
                assert!(seg.chunks[1..].iter().all(|c| !c.independent));
                for c in &seg.chunks {
                    assert_eq!(c.duration, 45_000, "500 ms parts");
                    assert!(c.dash_audio.is_some());
                }
                // Audio: 2 s at 48 kHz = 93.75 frames -> 93 or 94 frames.
                let frames = seg.audio_duration / 1024;
                assert!((93..=94).contains(&frames), "{frames}");
            }
            // Segments are contiguous in both tracks.
            let segs: Vec<_> = st.complete_segments().collect();
            for w in segs.windows(2) {
                assert_eq!(w[0].start + w[0].duration, w[1].start);
                assert_eq!(
                    w[0].audio_start.unwrap() + w[0].audio_duration,
                    w[1].audio_start.unwrap()
                );
                assert_eq!(w[0].seq + 1, w[1].seq);
            }
            assert_eq!(st.max_part, 45_000);
            assert_eq!(st.measured_gop_ms, Some(2000.0));
            assert_eq!(st.target_duration, 2);
        });
    }

    #[test]
    fn fragments_are_well_formed_and_av_aligned() {
        let mut s = segmenter(SegmenterConfig::default(), 10);
        s.push(&avc_header());
        s.push(&aac_header());
        for p in av_packets(0, 61, 60) {
            s.push(&p);
        }
        let store = Arc::clone(s.store());
        store.with(|st| {
            let seg = st.segments.front().unwrap();
            let c0 = &seg.chunks[0];
            let boxes = parse_boxes(&c0.hls).unwrap();
            assert_eq!(boxes.len(), 2);
            let mut trafs = Vec::new();
            boxes[0].find_all(b"traf", &mut trafs);
            assert_eq!(trafs.len(), 2);
            let v_tfdt = tfdt(trafs[0]);
            let a_tfdt = tfdt(trafs[1]);
            assert_eq!(v_tfdt, seg.start);
            // Same instant in both timescales.
            assert_eq!(v_tfdt / 90, a_tfdt * 1000 / 48000);
            // 15 video samples in the first 500 ms part.
            let trun = &trafs[0].find(b"trun").unwrap().payload;
            assert_eq!(u32_at(trun, 4), 15);
            let durations: u64 = (0..15).map(|i| u32_at(trun, 12 + i * 16) as u64).sum();
            assert_eq!(durations, 45_000);
            // DASH single-track fragments.
            let v = parse_boxes(&c0.dash_video).unwrap();
            let mut t = Vec::new();
            v[0].find_all(b"traf", &mut t);
            assert_eq!(t.len(), 1);
            assert_eq!(tfdt(t[0]), v_tfdt);
            let a = parse_boxes(c0.dash_audio.as_ref().unwrap()).unwrap();
            let mut t = Vec::new();
            a[0].find_all(b"traf", &mut t);
            assert_eq!(tfdt(t[0]), a_tfdt);
            // Video samples keep the AVCC payload (FLV header stripped).
            let mdat = &v[1].payload;
            assert_eq!(&mdat[..6], &[0, 0, 0, 2, 0x65, 0x80]);
        });
    }

    #[test]
    fn part_durations_never_exceed_target_25fps_and_gaps() {
        let mut s = segmenter(SegmenterConfig::default(), 10);
        s.push(&avc_header());
        s.push(&aac_header());
        // 25 fps with a 200 ms gap and 1 ms jitter.
        let mut ts = 0u32;
        for i in 0..200u32 {
            s.push(&vframe(ts, i % 50 == 0, 0));
            ts += if i == 70 {
                200
            } else if i % 2 == 0 {
                41
            } else {
                39
            };
        }
        let store = Arc::clone(s.store());
        store.with(|st| {
            assert!(st.max_part <= 45_000, "max part {}", st.max_part);
            // >= 85 % of the target except the last part of a segment and the part
            // cut right before the timestamp gap.
            let short = st
                .complete_segments()
                .flat_map(|seg| &seg.chunks[..seg.chunks.len() - 1])
                .filter(|c| c.duration < 38_250)
                .count();
            assert_eq!(short, 1);
        });
    }

    #[test]
    fn snapshot_hint_anchors_media_time() {
        let mut s = segmenter(SegmenterConfig::default(), 10);
        // The GOP-cache snapshot ends 1.5 s after its keyframe: that packet is "now".
        s.hint_latest_timestamp(11_500);
        s.push(&avc_header());
        s.push(&aac_header());
        s.push(&vframe(10_000, true, 0));
        s.push(&vframe(10_033, false, 0));
        let start = s.store().with(|st| st.segments[0].start);
        assert_eq!(start, (NOW_MS as u64 - 1500) * 90);
    }

    #[test]
    fn b_frames_composition_offsets() {
        let mut s = segmenter(SegmenterConfig::default(), 10);
        s.push(&avc_header());
        s.push(&aac_header());
        s.push(&vframe(0, true, 66));
        s.push(&vframe(33, false, 100));
        s.push(&vframe(67, false, 0));
        s.push(&vframe(100, true, 66));
        s.finish();
        let store = Arc::clone(s.store());
        store.with(|st| {
            let seg = st.segments.front().unwrap();
            let b = parse_boxes(&seg.chunks[0].hls).unwrap();
            let trun = &b[0].find(b"trun").unwrap().payload;
            assert_eq!(u32_at(trun, 24) as i32, 66 * 90);
            assert_eq!(u32_at(trun, 40) as i32, 100 * 90);
            assert!(st.ended);
        });
    }

    #[test]
    fn long_gop_is_split_at_max_segment_duration() {
        let cfg = SegmenterConfig {
            max_segment_ms: 3000,
            ..Default::default()
        };
        let mut s = segmenter(cfg, 10);
        s.push(&avc_header());
        s.push(&aac_header());
        for p in av_packets(0, 30 * 10 + 1, 300) {
            s.push(&p);
        }
        let store = Arc::clone(s.store());
        store.with(|st| {
            let segs: Vec<_> = st.complete_segments().collect();
            assert!(segs.len() >= 3);
            assert!(segs[0].independent);
            assert!(!segs[1].independent);
            assert!(!segs[1].chunks[0].independent);
            assert!(segs.iter().all(|g| g.duration <= 270_000));
            assert!(st.target_duration <= 3);
        });
    }

    #[test]
    fn short_gops_are_merged() {
        let mut s = segmenter(SegmenterConfig::default(), 10);
        s.push(&avc_header());
        s.push(&aac_header());
        // Keyframe every 9 frames (300 ms).
        for p in av_packets(0, 30 * 4 + 1, 9) {
            s.push(&p);
        }
        let store = Arc::clone(s.store());
        store.with(|st| {
            for seg in st.complete_segments() {
                assert!(seg.duration >= 90_000, "segment {} too short", seg.duration);
                // Every keyframe starts an independent part.
                assert!(seg.chunks.iter().filter(|c| c.independent).count() >= 3);
            }
        });
    }

    #[test]
    fn video_only_after_probe_timeout() {
        let mut s = segmenter(SegmenterConfig::default(), 10);
        s.push(&avc_header());
        for i in 0..90u32 {
            s.push(&vframe(i * 33, i % 60 == 0, 0));
        }
        assert_eq!(s.status(), &SegmenterStatus::Running);
        let store = Arc::clone(s.store());
        store.with(|st| {
            let init = st.init().unwrap();
            assert!(init.audio.is_none());
            assert_eq!(init.codecs(), "avc1.64001f");
            assert_eq!(st.complete_count(), 1);
            // Packaging started after 1 s of probing (frame 31, ts 1023 = "now"); the
            // probe buffer was replayed from the first keyframe.
            assert_eq!(st.segments[0].start, (NOW_MS as u64 - 31 * 33) * 90);
        });
    }

    #[test]
    fn unsupported_codecs() {
        let mut s = segmenter(SegmenterConfig::default(), 10);
        s.push(&MediaPacket::video(
            0,
            Bytes::from_static(&[0x90, b'h', b'v', b'c', b'1', 1, 2, 3]),
        ));
        assert!(matches!(s.status(), SegmenterStatus::Unsupported(r) if r.contains("hevc")));

        let mut s = segmenter(SegmenterConfig::default(), 10);
        s.push(&avc_header());
        s.push(&MediaPacket::audio(
            0,
            Bytes::from_static(&[0x2F, 0xFF, 0xFB]),
        ));
        assert!(matches!(s.status(), SegmenterStatus::Unsupported(r) if r.contains("mp3")));

        let mut s = segmenter(SegmenterConfig::default(), 10);
        s.push(&aac_header());
        for i in 0..300u32 {
            s.push(&aframe(i * 21));
        }
        assert!(matches!(s.status(), SegmenterStatus::Unsupported(r) if r.contains("audio-only")));
    }

    #[test]
    fn lag_resyncs_at_next_keyframe() {
        let mut s = segmenter(SegmenterConfig::default(), 10);
        s.push(&avc_header());
        s.push(&aac_header());
        let pkts = av_packets(0, 30 * 6 + 1, 60);
        let (a, b) = pkts.split_at(pkts.len() / 3);
        for p in a {
            s.push(p);
        }
        s.on_lag(100);
        // Skip some packets, resume mid-GOP: frames until the next keyframe are dropped.
        for p in &b[40..] {
            s.push(p);
        }
        let store = Arc::clone(s.store());
        store.with(|st| {
            let segs: Vec<_> = st.complete_segments().collect();
            assert!(segs.len() >= 2);
            assert!(segs.iter().all(|g| g.independent));
            // Gap after the lag: the next segment starts at a later keyframe.
            assert!(segs[1].start > segs[0].start + segs[0].duration);
        });
    }

    #[test]
    fn codec_change_starts_new_generation() {
        let mut s = segmenter(SegmenterConfig::default(), 10);
        s.push(&avc_header());
        s.push(&aac_header());
        for p in av_packets(0, 61, 60) {
            s.push(&p);
        }
        s.push(&avc_header()); // identical: ignored
        assert_eq!(s.store().with(|st| st.inits.len()), 1);
        // 44.1 kHz audio config.
        s.push(&MediaPacket::audio(
            0,
            Bytes::from_static(&[0xAF, 0x00, 0x12, 0x10]),
        ));
        for p in av_packets(2033, 61, 60) {
            s.push(&p);
        }
        let store = Arc::clone(s.store());
        store.with(|st| {
            assert_eq!(st.init().unwrap().generation, 1);
            assert_eq!(st.init().unwrap().audio_timescale(), Some(44100));
            let gens: Vec<u32> = st.segments.iter().map(|g| g.generation).collect();
            assert_eq!(gens.first(), Some(&0));
            assert_eq!(gens.last(), Some(&1));
        });
    }

    #[test]
    fn timestamp_unwrap() {
        let mut u = TsUnwrap::default();
        assert_eq!(u.unwrap(u32::MAX - 10), u32::MAX as i64 - 10);
        assert_eq!(u.unwrap(5), u32::MAX as i64 + 6);
        assert_eq!(u.unwrap(2), u32::MAX as i64 + 3);
        assert_eq!(u.peek(20), u32::MAX as i64 + 21);
    }
}
