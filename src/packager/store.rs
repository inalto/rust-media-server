//! `SegmentStore` (ARCHITECTURE.md §6.5): the in-memory segments/parts of one stream,
//! shared by the LL-HLS and LL-DASH egress.
//!
//! The packager task is the only writer ([`SegmentStore::set_init`],
//! [`SegmentStore::open_segment`], [`SegmentStore::push_chunk`],
//! [`SegmentStore::complete_segment`], [`SegmentStore::end`]). HTTP handlers read
//! consistent snapshots under a short `std::sync::Mutex` ([`SegmentStore::with`]) and
//! block on a `tokio::sync::watch` version counter that is bumped on every change
//! ([`SegmentStore::wait`], [`SegmentStore::wait_for_part`],
//! [`SegmentStore::chunk_stream`]).
//!
//! Every chunk is stored in two layouts produced from the same samples: the muxed HLS
//! fragment (one `moof` with a video and an audio `traf`, one `mdat`) and one
//! single-track fragment per DASH Representation.
//!
//! Time base: video times are 90 kHz ticks **since the Unix epoch** (media time 0 =
//! 1970-01-01T00:00:00Z, see `segmenter.rs`), audio times use the AAC sample rate on
//! the same epoch. This makes segment numbers, `$Time$` values and therefore every
//! cacheable URL unique across publishing sessions of the same broadcast.
//!
//! Memory is bounded: `window_segments` complete segments + the in-progress one
//! (whose length the segmenter caps at `hls.max_segment_duration`).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures::Stream;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::cmaf::{self, AudioConfig, VIDEO_TIMESCALE, VideoConfig};

/// Number of keyframe intervals averaged for [`StoreState::measured_gop_ms`].
const GOP_AVERAGE_WINDOW: usize = 8;

/// Static parameters of a store.
#[derive(Debug, Clone, PartialEq)]
pub struct StoreParams {
    /// Advertised part target duration (milliseconds).
    pub part_target_ms: u32,
    /// Complete segments kept (the in-progress one comes on top).
    pub window_segments: usize,
    /// Initial guess of the segment duration before anything was measured (ms).
    pub nominal_segment_ms: u32,
}

impl Default for StoreParams {
    fn default() -> Self {
        Self {
            part_target_ms: 500,
            window_segments: 6,
            nominal_segment_ms: 2000,
        }
    }
}

/// Initialization segments of one codec configuration ("generation").
#[derive(Debug, Clone)]
pub struct InitSegments {
    /// 0 for the first configuration; bumped when the encoder sends a different
    /// sequence header mid-stream (HLS discontinuity / new DASH period).
    pub generation: u32,
    /// HLS `init.mp4` (video + audio tracks).
    pub hls: Bytes,
    /// DASH `init-video.m4s`.
    pub video: Bytes,
    /// DASH `init-audio.m4s`.
    pub audio: Option<Bytes>,
    pub video_config: VideoConfig,
    pub audio_config: Option<AudioConfig>,
}

impl InitSegments {
    pub fn new(generation: u32, video: VideoConfig, audio: Option<AudioConfig>) -> Self {
        Self {
            generation,
            hls: cmaf::init_segment(Some(&video), audio.as_ref()),
            video: cmaf::init_segment(Some(&video), None),
            audio: audio.as_ref().map(|a| cmaf::init_segment(None, Some(a))),
            video_config: video,
            audio_config: audio,
        }
    }

    /// `avc1.64001f,mp4a.40.2`
    pub fn codecs(&self) -> String {
        match &self.audio_config {
            Some(a) => format!("{},{}", self.video_config.codec, a.codec),
            None => self.video_config.codec.clone(),
        }
    }

    pub fn audio_timescale(&self) -> Option<u32> {
        self.audio_config.as_ref().map(|a| a.sample_rate)
    }
}

/// Payload of a chunk produced by the segmenter (see [`Chunk`]).
#[derive(Debug, Clone, Default)]
pub struct ChunkData {
    /// Video decode time of the first sample (90 kHz).
    pub start: u64,
    /// Video duration (90 kHz).
    pub duration: u64,
    /// Starts with a keyframe.
    pub independent: bool,
    pub hls: Bytes,
    pub dash_video: Bytes,
    pub dash_audio: Option<Bytes>,
    /// Decode time of the first audio sample (audio timescale), if any.
    pub audio_start: Option<u64>,
    /// Sum of the audio sample durations (audio timescale).
    pub audio_duration: u64,
}

/// One CMAF chunk = one LL-HLS part = one piece of a chunked DASH segment.
#[derive(Debug, Clone, PartialEq)]
pub struct Chunk {
    /// Media sequence number of the parent segment.
    pub seq: u64,
    pub part_index: u32,
    pub start: u64,
    pub duration: u64,
    pub independent: bool,
    pub hls: Bytes,
    pub dash_video: Bytes,
    pub dash_audio: Option<Bytes>,
    pub audio_start: Option<u64>,
    pub audio_duration: u64,
}

impl Chunk {
    pub fn duration_ms(&self) -> f64 {
        self.duration as f64 * 1000.0 / VIDEO_TIMESCALE as f64
    }

    pub fn duration_secs(&self) -> f64 {
        self.duration as f64 / VIDEO_TIMESCALE as f64
    }
}

/// One segment (normally one GOP).
#[derive(Debug, Clone)]
pub struct Segment {
    pub seq: u64,
    /// Codec configuration generation (see [`InitSegments::generation`]).
    pub generation: u32,
    /// Video decode time of the first sample (90 kHz since the epoch).
    pub start: u64,
    /// Video duration so far (complete: final), 90 kHz.
    pub duration: u64,
    /// Decode time of the first audio sample (audio timescale).
    pub audio_start: Option<u64>,
    pub audio_duration: u64,
    /// Wall-clock time of `start`.
    pub program_date_time: DateTime<Utc>,
    pub chunks: Vec<Arc<Chunk>>,
    pub complete: bool,
    /// Starts with a keyframe (false when split because of `max_segment_duration`).
    pub independent: bool,
    pub bytes: usize,
}

impl Segment {
    pub fn duration_secs(&self) -> f64 {
        self.duration as f64 / VIDEO_TIMESCALE as f64
    }

    /// Concatenation of the HLS parts (the full `seg-N.m4s`).
    pub fn hls_bytes(&self) -> Bytes {
        concat(self.chunks.iter().map(|c| &c.hls))
    }
}

fn concat<'a>(parts: impl Iterator<Item = &'a Bytes> + Clone) -> Bytes {
    let len = parts.clone().map(Bytes::len).sum();
    let mut out = Vec::with_capacity(len);
    for p in parts {
        out.extend_from_slice(p);
    }
    Bytes::from(out)
}

/// Everything readers can see, behind the store mutex.
#[derive(Debug, Default)]
pub struct StoreState {
    pub params: StoreParams,
    /// Init segments of every generation still referenced (oldest first).
    pub inits: Vec<Arc<InitSegments>>,
    /// Complete segments (oldest first) followed by at most one in-progress segment.
    pub segments: VecDeque<Segment>,
    /// The stream ended (`#EXT-X-ENDLIST`); nothing will be added any more.
    pub ended: bool,
    /// HLS `EXT-X-TARGETDURATION` (seconds); never decreases.
    pub target_duration: u32,
    /// Longest complete segment / part ever produced (90 kHz).
    pub max_segment: u64,
    pub max_part: u64,
    /// Average keyframe interval (milliseconds).
    pub measured_gop_ms: Option<f64>,
    gop_samples: VecDeque<f64>,
    pub segments_total: u64,
    pub parts_total: u64,
    pub bytes_total: u64,
    /// Number of `EXT-X-DISCONTINUITY` tags evicted from the window.
    pub discontinuity_sequence: u64,
    next_seq: Option<u64>,
}

impl StoreState {
    /// Current (newest) init generation.
    pub fn init(&self) -> Option<&Arc<InitSegments>> {
        self.inits.last()
    }

    pub fn init_gen(&self, generation: u32) -> Option<&Arc<InitSegments>> {
        self.inits.iter().find(|i| i.generation == generation)
    }

    pub fn complete_segments(&self) -> impl DoubleEndedIterator<Item = &Segment> {
        self.segments.iter().filter(|s| s.complete)
    }

    pub fn complete_count(&self) -> usize {
        self.complete_segments().count()
    }

    /// The in-progress segment (if any).
    pub fn in_progress(&self) -> Option<&Segment> {
        self.segments.back().filter(|s| !s.complete)
    }

    pub fn segment(&self, seq: u64) -> Option<&Segment> {
        let first = self.segments.front()?.seq;
        let idx = usize::try_from(seq.checked_sub(first)?).ok()?;
        self.segments.get(idx).filter(|s| s.seq == seq)
    }

    pub fn first_seq(&self) -> Option<u64> {
        self.segments.front().map(|s| s.seq)
    }

    /// Sequence number of the newest segment (complete or in progress).
    pub fn last_seq(&self) -> Option<u64> {
        self.segments.back().map(|s| s.seq)
    }

    /// Ready for HLS: an init segment and at least one complete segment.
    pub fn hls_ready(&self) -> bool {
        !self.inits.is_empty() && self.segments.iter().any(|s| s.complete)
    }

    /// Duration (seconds) of the newest complete segment.
    pub fn last_segment_secs(&self) -> Option<f64> {
        self.complete_segments()
            .next_back()
            .map(Segment::duration_secs)
    }

    /// Expected (upper bound of the) duration of the in-progress segment, 90 kHz: the
    /// longest complete segment in the window, else the measured GOP, else the
    /// configured nominal duration.
    ///
    /// An upper bound on purpose: dash.js locates the segment after the one it just
    /// requested *by time* (`start + announced duration`), so over-estimating lands in
    /// the next segment while under-estimating would request the same segment again.
    pub fn nominal_segment(&self) -> u64 {
        let ms = self
            .complete_segments()
            .map(|s| s.duration)
            .max()
            .map(|d| d as f64 / 90.0)
            .or(self.measured_gop_ms)
            .unwrap_or(self.params.nominal_segment_ms as f64);
        (ms.max(1.0) * 90.0).round() as u64
    }

    /// Video bandwidth estimate (bits/s): peak over the complete segments.
    pub fn peak_bitrate(&self) -> Option<u64> {
        self.complete_segments()
            .filter(|s| s.duration > 0)
            .map(|s| (s.bytes as f64 * 8.0 / s.duration_secs()) as u64)
            .max()
    }

    pub fn part_target_secs(&self) -> f64 {
        self.params.part_target_ms as f64 / 1000.0
    }

    fn set_target_duration(&mut self, segment_ticks: u64) {
        // RFC 8216bis: EXTINF rounded to the nearest integer must be <= target.
        let rounded = ((segment_ticks as f64 / VIDEO_TIMESCALE as f64).round() as u32).max(1);
        self.target_duration = self.target_duration.max(rounded);
    }
}

/// Outcome of a blocking lookup.
#[derive(Debug, Clone, PartialEq)]
pub enum Wait<T> {
    Ready(T),
    /// Will never exist (evicted, beyond the end, stream ended...).
    NotFound,
    /// Timed out (or the server is shutting down).
    Timeout,
}

pub struct SegmentStore {
    state: Mutex<StoreState>,
    version: watch::Sender<u64>,
}

impl std::fmt::Debug for SegmentStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SegmentStore").finish_non_exhaustive()
    }
}

impl SegmentStore {
    pub fn new(params: StoreParams) -> Arc<Self> {
        let (version, _) = watch::channel(0);
        let mut params = params;
        params.window_segments = params.window_segments.max(3);
        params.part_target_ms = params.part_target_ms.max(1);
        Arc::new(Self {
            state: Mutex::new(StoreState {
                params,
                ..Default::default()
            }),
            version,
        })
    }

    fn lock(&self) -> MutexGuard<'_, StoreState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Run `f` on a consistent snapshot (keep it short: it holds the store mutex).
    pub fn with<R>(&self, f: impl FnOnce(&StoreState) -> R) -> R {
        f(&self.lock())
    }

    fn update<R>(&self, f: impl FnOnce(&mut StoreState) -> R) -> R {
        let r = f(&mut self.lock());
        self.version.send_modify(|v| *v += 1);
        r
    }

    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.version.subscribe()
    }

    // -- writer side ---------------------------------------------------------

    /// Install a new codec configuration; returns its generation.
    pub fn set_init(&self, video: VideoConfig, audio: Option<AudioConfig>) -> Arc<InitSegments> {
        self.update(|s| {
            let generation = s.inits.last().map_or(0, |i| i.generation + 1);
            let init = Arc::new(InitSegments::new(generation, video, audio));
            s.inits.push(Arc::clone(&init));
            prune_inits(s);
            init
        })
    }

    /// Start a new segment at video decode time `start` (completing the previous one).
    /// The first segment number is the Unix second of `start`, so numbers never
    /// repeat across publishing sessions. Returns the sequence number.
    pub fn open_segment(&self, start: u64, independent: bool) -> u64 {
        self.update(|s| {
            complete_last(s);
            let seq = s.next_seq.unwrap_or_else(|| start / VIDEO_TIMESCALE as u64);
            s.next_seq = Some(seq + 1);
            let generation = s.inits.last().map_or(0, |i| i.generation);
            let millis = (start as i128 * 1000 / VIDEO_TIMESCALE as i128) as i64;
            s.segments.push_back(Segment {
                seq,
                generation,
                start,
                duration: 0,
                audio_start: None,
                audio_duration: 0,
                program_date_time: DateTime::from_timestamp_millis(millis).unwrap_or_default(),
                chunks: Vec::new(),
                complete: false,
                independent,
                bytes: 0,
            });
            seq
        })
    }

    /// Append a chunk to the in-progress segment (ignored if there is none).
    pub fn push_chunk(&self, data: ChunkData) {
        self.update(|s| {
            let Some(seg) = s.segments.back_mut().filter(|g| !g.complete) else {
                return;
            };
            let chunk = Chunk {
                seq: seg.seq,
                part_index: seg.chunks.len() as u32,
                start: data.start,
                duration: data.duration,
                independent: data.independent,
                hls: data.hls,
                dash_video: data.dash_video,
                dash_audio: data.dash_audio,
                audio_start: data.audio_start,
                audio_duration: data.audio_duration,
            };
            seg.duration += chunk.duration;
            if seg.audio_start.is_none() {
                seg.audio_start = chunk.audio_start;
            }
            seg.audio_duration += chunk.audio_duration;
            seg.bytes += chunk.hls.len();
            let (dur, len) = (chunk.duration, chunk.hls.len());
            seg.chunks.push(Arc::new(chunk));
            s.max_part = s.max_part.max(dur);
            s.parts_total += 1;
            s.bytes_total += len as u64;
        })
    }

    /// Mark the in-progress segment complete and evict old segments.
    pub fn complete_segment(&self) {
        self.update(complete_last);
    }

    /// Record one keyframe interval (milliseconds) for the measured GOP.
    pub fn record_keyframe_interval(&self, ms: f64) {
        if !(ms.is_finite() && ms > 0.0) {
            return;
        }
        let mut s = self.lock();
        s.gop_samples.push_back(ms);
        while s.gop_samples.len() > GOP_AVERAGE_WINDOW {
            s.gop_samples.pop_front();
        }
        s.measured_gop_ms = Some(s.gop_samples.iter().sum::<f64>() / s.gop_samples.len() as f64);
    }

    /// The stream ended: complete the last segment and wake every waiter.
    pub fn end(&self) {
        self.update(|s| {
            complete_last(s);
            s.ended = true;
        });
    }

    pub fn is_ended(&self) -> bool {
        self.lock().ended
    }

    // -- reader side ---------------------------------------------------------

    /// Wait until `check` returns `Some` (evaluated under the lock after every
    /// change), the timeout expires or `cancel` fires. `check` must itself return
    /// `Some` for conditions that can no longer change (e.g. stream ended).
    pub async fn wait<R>(
        &self,
        timeout: Duration,
        cancel: &CancellationToken,
        mut check: impl FnMut(&StoreState) -> Option<R>,
    ) -> Option<R> {
        let mut rx = self.version.subscribe();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Some(r) = check(&self.lock()) {
                return Some(r);
            }
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return None,
                changed = rx.changed() => {
                    if changed.is_err() {
                        // Sender dropped: the store is gone; one last check.
                        return check(&self.lock());
                    }
                }
                _ = tokio::time::sleep_until(deadline) => return None,
            }
        }
    }

    /// Block until part `part` of segment `seq` is complete.
    pub async fn wait_for_part(
        &self,
        seq: u64,
        part: u32,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Wait<Arc<Chunk>> {
        let r = self
            .wait(timeout, cancel, |s| {
                if let Some(seg) = s.segment(seq) {
                    if let Some(c) = seg.chunks.get(part as usize) {
                        return Some(Wait::Ready(Arc::clone(c)));
                    }
                    if seg.complete {
                        return Some(Wait::NotFound);
                    }
                    return s.ended.then_some(Wait::NotFound);
                }
                // Not in the window: evicted, or not started yet.
                match s.last_seq() {
                    Some(last) if seq == last + 1 && !s.ended => None,
                    None if !s.ended => None,
                    _ => Some(Wait::NotFound),
                }
            })
            .await;
        r.unwrap_or(Wait::Timeout)
    }

    /// Block until segment `seq` is complete; returns its full HLS bytes.
    pub async fn wait_for_segment_complete(
        &self,
        seq: u64,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Wait<Bytes> {
        let r = self
            .wait(timeout, cancel, |s| match s.segment(seq) {
                Some(seg) if seg.complete => Some(Wait::Ready(seg.hls_bytes())),
                Some(_) => None,
                None => match s.last_seq() {
                    Some(last) if seq == last + 1 && !s.ended => None,
                    _ => Some(Wait::NotFound),
                },
            })
            .await;
        r.unwrap_or(Wait::Timeout)
    }

    /// Stream the chunks of segment `seq` as they are produced: existing chunks first,
    /// then new ones until the segment is complete. `pick` selects the bytes of a
    /// chunk (HLS fragment, DASH video or audio fragment; `None` skips the chunk).
    /// Ends early on `cancel`, eviction, or when no chunk arrives within `idle`.
    pub fn chunk_stream(
        self: Arc<Self>,
        seq: u64,
        pick: fn(&Chunk) -> Option<Bytes>,
        idle: Duration,
        cancel: CancellationToken,
    ) -> impl Stream<Item = Bytes> + Send + 'static {
        async_stream::stream! {
            let mut next = 0usize;
            loop {
                let step = self
                    .wait(idle, &cancel, |s| {
                        let Some(seg) = s.segment(seq) else {
                            return Some((Vec::new(), true));
                        };
                        if seg.chunks.len() > next {
                            let new: Vec<Arc<Chunk>> = seg.chunks[next..].to_vec();
                            return Some((new, seg.complete));
                        }
                        (seg.complete || s.ended).then(|| (Vec::new(), true))
                    })
                    .await;
                let Some((chunks, done)) = step else { break };
                next += chunks.len();
                for c in chunks {
                    if let Some(b) = pick(&c) {
                        yield b;
                    }
                }
                if done {
                    break;
                }
            }
        }
    }
}

/// Complete the in-progress segment (dropping it if empty), update statistics and
/// evict segments beyond the window.
fn complete_last(s: &mut StoreState) {
    let Some(last) = s.segments.back_mut() else {
        return;
    };
    if last.complete {
        return;
    }
    if last.chunks.is_empty() {
        s.segments.pop_back();
        // Reuse the number: nothing was ever published under it.
        if let Some(n) = s.next_seq.as_mut() {
            *n -= 1;
        }
        return;
    }
    last.complete = true;
    let dur = last.duration;
    s.max_segment = s.max_segment.max(dur);
    s.set_target_duration(dur);
    s.segments_total += 1;
    let window = s.params.window_segments;
    while s.complete_count() > window {
        if let Some(old) = s.segments.pop_front() {
            // A discontinuity sits before the first segment of a new generation.
            if s.segments
                .front()
                .is_some_and(|n| n.generation != old.generation)
            {
                s.discontinuity_sequence += 1;
            }
        }
    }
    prune_inits(s);
}

/// Drop init generations no longer referenced by any segment (keep the newest).
fn prune_inits(s: &mut StoreState) {
    let Some(newest) = s.inits.last().map(|i| i.generation) else {
        return;
    };
    let oldest_used = s.segments.front().map_or(newest, |g| g.generation);
    s.inits
        .retain(|i| i.generation >= oldest_used || i.generation == newest);
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use futures::StreamExt;

    pub(crate) fn test_video() -> VideoConfig {
        VideoConfig {
            avcc: Bytes::from_static(&[1, 0x64, 0, 0x1f, 0xff, 0xe0, 0]),
            width: 1280,
            height: 720,
            codec: "avc1.64001f".into(),
        }
    }

    pub(crate) fn test_audio() -> AudioConfig {
        AudioConfig {
            asc: Bytes::from_static(&[0x11, 0x90]),
            object_type: 2,
            sample_rate: 48000,
            channels: 2,
            codec: "mp4a.40.2".into(),
        }
    }

    /// 90 kHz ticks for `secs` seconds after 2026-01-01T00:00:00Z.
    pub(crate) fn t(secs: f64) -> u64 {
        (1_767_225_600.0 * 90_000.0 + secs * 90_000.0).round() as u64
    }

    pub(crate) fn chunk(start: u64, ms: u64, independent: bool) -> ChunkData {
        ChunkData {
            start,
            duration: ms * 90,
            independent,
            hls: Bytes::from(vec![0u8; 100]),
            dash_video: Bytes::from(vec![1u8; 60]),
            dash_audio: Some(Bytes::from(vec![2u8; 40])),
            audio_start: Some(start / 90 * 48),
            audio_duration: ms * 48,
        }
    }

    /// Push `n` complete 2 s segments of four 500 ms parts starting at `t(from)`.
    pub(crate) fn fill(store: &SegmentStore, from: f64, n: usize) {
        for i in 0..n {
            let start = t(from + 2.0 * i as f64);
            store.open_segment(start, true);
            for p in 0..4u64 {
                store.push_chunk(chunk(start + p * 45_000, 500, p == 0));
            }
        }
        store.complete_segment();
    }

    fn params(window: usize) -> StoreParams {
        StoreParams {
            window_segments: window,
            ..Default::default()
        }
    }

    #[test]
    fn numbering_window_and_stats() {
        let store = SegmentStore::new(params(3));
        store.set_init(test_video(), Some(test_audio()));
        fill(&store, 0.0, 5);
        store.with(|s| {
            assert_eq!(s.segments.len(), 3, "window of 3 complete segments");
            assert!(s.segments.iter().all(|g| g.complete));
            assert_eq!(s.first_seq(), Some(1_767_225_600 + 2));
            assert_eq!(s.last_seq(), Some(1_767_225_600 + 4));
            assert_eq!(s.segments_total, 5);
            assert_eq!(s.parts_total, 20);
            assert_eq!(s.target_duration, 2);
            assert_eq!(s.max_part, 45_000);
            assert_eq!(s.last_segment_secs(), Some(2.0));
            let seg = s.segment(1_767_225_600 + 3).unwrap();
            assert_eq!(seg.chunks.len(), 4);
            assert_eq!(seg.chunks[2].part_index, 2);
            assert_eq!(seg.duration, 180_000);
            assert_eq!(seg.audio_duration, 96_000);
            assert_eq!(
                seg.program_date_time.to_rfc3339(),
                "2026-01-01T00:00:06+00:00"
            );
            assert_eq!(seg.hls_bytes().len(), 400);
            assert_eq!(s.peak_bitrate(), Some(400 * 8 / 2));
            assert!(s.hls_ready());
            assert_eq!(s.nominal_segment(), 180_000);
        });
        assert!(store.with(|s| s.segment(1_767_225_600).is_none()));
    }

    #[test]
    fn empty_segment_is_dropped_and_number_reused() {
        let store = SegmentStore::new(params(6));
        store.set_init(test_video(), None);
        let a = store.open_segment(t(0.0), true);
        store.push_chunk(chunk(t(0.0), 500, true));
        let b = store.open_segment(t(0.5), true);
        assert_eq!(b, a + 1);
        // b gets no chunk: completing it drops it, its number is reused.
        let c = store.open_segment(t(0.6), true);
        assert_eq!(c, b);
        store.with(|s| assert_eq!(s.segments.len(), 2));
    }

    #[test]
    fn target_duration_rounds_and_never_decreases() {
        let store = SegmentStore::new(params(6));
        store.set_init(test_video(), None);
        store.open_segment(t(0.0), true);
        store.push_chunk(chunk(t(0.0), 2_400, true));
        store.open_segment(t(2.4), true);
        store.push_chunk(chunk(t(2.4), 3_600, true));
        store.open_segment(t(6.0), true);
        store.push_chunk(chunk(t(6.0), 1_000, true));
        store.complete_segment();
        store.with(|s| assert_eq!(s.target_duration, 4));
    }

    #[test]
    fn generations_and_discontinuity_sequence() {
        let store = SegmentStore::new(params(3));
        store.set_init(test_video(), Some(test_audio()));
        fill(&store, 0.0, 2);
        let g1 = store.set_init(test_video(), None);
        assert_eq!(g1.generation, 1);
        fill(&store, 4.0, 2);
        store.with(|s| {
            assert_eq!(s.inits.len(), 2);
            assert_eq!(s.segments.front().unwrap().generation, 0);
            assert_eq!(s.discontinuity_sequence, 0);
        });
        fill(&store, 8.0, 1);
        store.with(|s| {
            // The last generation-0 segment was evicted together with its discontinuity.
            assert!(s.segments.iter().all(|g| g.generation == 1));
            assert_eq!(s.discontinuity_sequence, 1);
            assert_eq!(s.inits.len(), 1);
            assert_eq!(s.init().unwrap().generation, 1);
            assert_eq!(s.init().unwrap().codecs(), "avc1.64001f");
        });
    }

    #[tokio::test]
    async fn wait_for_part_blocks_until_published() {
        let store = SegmentStore::new(params(6));
        store.set_init(test_video(), Some(test_audio()));
        let seq = store.open_segment(t(0.0), true);
        store.push_chunk(chunk(t(0.0), 500, true));
        let cancel = CancellationToken::new();

        // Existing part: immediate.
        let r = store
            .wait_for_part(seq, 0, Duration::from_secs(1), &cancel)
            .await;
        assert!(matches!(r, Wait::Ready(c) if c.part_index == 0));

        // Next part: blocks until pushed.
        let s2 = Arc::clone(&store);
        let waiter = tokio::spawn(async move {
            s2.wait_for_part(seq, 1, Duration::from_secs(5), &CancellationToken::new())
                .await
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!waiter.is_finished());
        store.push_chunk(chunk(t(0.5), 500, false));
        assert!(matches!(waiter.await.unwrap(), Wait::Ready(c) if c.part_index == 1));

        // First part of the next segment (preload hint after completion) blocks too.
        let s3 = Arc::clone(&store);
        let waiter = tokio::spawn(async move {
            s3.wait_for_part(
                seq + 1,
                0,
                Duration::from_secs(5),
                &CancellationToken::new(),
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        store.open_segment(t(1.0), true);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!waiter.is_finished());
        store.push_chunk(chunk(t(1.0), 500, true));
        assert!(matches!(waiter.await.unwrap(), Wait::Ready(c) if c.seq == seq + 1));

        // Timeout.
        let r = store
            .wait_for_part(seq + 1, 3, Duration::from_millis(100), &cancel)
            .await;
        assert_eq!(r, Wait::Timeout);
        // Part beyond a complete segment / far future / evicted: not found.
        assert_eq!(
            store
                .wait_for_part(seq, 7, Duration::from_secs(1), &cancel)
                .await,
            Wait::NotFound
        );
        assert_eq!(
            store
                .wait_for_part(seq + 5, 0, Duration::from_secs(1), &cancel)
                .await,
            Wait::NotFound
        );
        assert_eq!(
            store
                .wait_for_part(seq - 1, 0, Duration::from_secs(1), &cancel)
                .await,
            Wait::NotFound
        );
        // Cancellation ends the wait.
        let c2 = CancellationToken::new();
        c2.cancel();
        assert_eq!(
            store
                .wait_for_part(seq + 1, 3, Duration::from_secs(60), &c2)
                .await,
            Wait::Timeout
        );
        // Stream end wakes waiters with NotFound.
        let s4 = Arc::clone(&store);
        let waiter = tokio::spawn(async move {
            s4.wait_for_part(
                seq + 1,
                1,
                Duration::from_secs(60),
                &CancellationToken::new(),
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(10)).await;
        store.end();
        assert_eq!(waiter.await.unwrap(), Wait::NotFound);
    }

    #[tokio::test]
    async fn segment_complete_wait_and_chunk_stream() {
        let store = SegmentStore::new(params(6));
        store.set_init(test_video(), Some(test_audio()));
        let seq = store.open_segment(t(0.0), true);
        store.push_chunk(chunk(t(0.0), 500, true));
        let stream = Arc::clone(&store).chunk_stream(
            seq,
            |c| c.dash_audio.clone(),
            Duration::from_secs(5),
            CancellationToken::new(),
        );
        let s2 = Arc::clone(&store);
        let full = tokio::spawn(async move {
            s2.wait_for_segment_complete(seq, Duration::from_secs(10), &CancellationToken::new())
                .await
        });
        let producer = {
            let store = Arc::clone(&store);
            tokio::spawn(async move {
                for p in 1..4u64 {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    store.push_chunk(chunk(t(0.5 * p as f64), 500, false));
                }
                store.open_segment(t(2.0), true);
            })
        };
        let got: Vec<Bytes> = stream.collect().await;
        producer.await.unwrap();
        assert_eq!(got.len(), 4);
        assert!(got.iter().all(|b| b.len() == 40 && b[0] == 2));
        assert!(matches!(full.await.unwrap(), Wait::Ready(b) if b.len() == 400));

        // A stream on an evicted / unknown segment ends immediately.
        let none: Vec<Bytes> = Arc::clone(&store)
            .chunk_stream(
                seq + 10,
                |c| Some(c.hls.clone()),
                Duration::from_secs(1),
                CancellationToken::new(),
            )
            .collect()
            .await;
        assert!(none.is_empty());
    }

    #[test]
    fn gop_average() {
        let store = SegmentStore::new(params(6));
        store.record_keyframe_interval(2000.0);
        store.record_keyframe_interval(1000.0);
        store.record_keyframe_interval(f64::NAN);
        store.with(|s| assert_eq!(s.measured_gop_ms, Some(1500.0)));
        for _ in 0..20 {
            store.record_keyframe_interval(4000.0);
        }
        store.with(|s| assert_eq!(s.measured_gop_ms, Some(4000.0)));
    }
}
