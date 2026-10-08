//! PackagerManager: one CMAF packager task per live stream, feeding LL-HLS and LL-DASH
//! (ARCHITECTURE.md §6.3–6.5).
//!
//! - [`cmaf`]: fMP4/CMAF box writer (init segments, `moof`+`mdat` chunks) and FLV tag
//!   classification (H.264 + AAC only).
//! - [`segmenter`]: FLV packets → chunks/parts and GOP-aligned segments.
//! - [`store`]: `SegmentStore`, the shared in-memory window with blocking reads.
//! - [`hls_playlist`] / [`dash_mpd`]: playlist and MPD rendering.
//!
//! On `HubEvent::StreamStarted` (and for streams already live when the manager starts)
//! a task subscribes to the `LiveStream`, runs a [`segmenter::Segmenter`] until the
//! stream's `ended_token()` fires, marks the store ended (`#EXT-X-ENDLIST`), keeps it
//! served for `hls.linger_secs`, then drops it. A new publishing session replaces a
//! lingering one immediately. HTTP routes live in `egress::hls` / `egress::dash` and
//! are mounted by [`router`].

pub mod cmaf;
pub mod dash_mpd;
pub mod hls_playlist;
pub mod segmenter;
pub mod store;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::{Duration, Instant};

use axum::Router;
use serde::Serialize;
use tokio::sync::broadcast::error::RecvError;
use tokio_util::sync::CancellationToken;
use tower_http::cors::CorsLayer;

use crate::config::Config;
use crate::hub::{Hub, HubEvent, LiveStream, PacketKind, StreamInfo};
use crate::metrics::ViewerTracker;
use crate::state::AppState;
use segmenter::{Segmenter, SegmenterConfig, SegmenterStatus};
use store::{SegmentStore, StoreParams};

/// How long cached per-broadcast `enable_hls` / `enable_dash` flags stay valid
/// (also invalidated by `HubEvent::BroadcastUpdated`).
const FLAGS_TTL: Duration = Duration::from_secs(5);

/// Packaging status of a broadcast, shown in `BroadcastView.live.packager`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PackagerStatus {
    /// False when the codecs cannot be packaged (only H.264 + AAC are supported).
    pub supported: bool,
    /// Why the stream is unsupported (or what the packager is waiting for).
    pub reason: Option<String>,
    pub hls_ready: bool,
    pub dash_ready: bool,
    /// The publisher left; the last window is served until the linger time expires.
    pub ended: bool,
    /// Segments / parts produced since the stream started.
    pub segments: u64,
    pub parts: u64,
    /// Average encoder keyframe interval (the dashboard warns when it exceeds 2–3 s).
    pub measured_gop_secs: Option<f64>,
    pub last_segment_secs: Option<f64>,
    pub part_target_secs: f64,
    pub target_duration_secs: u32,
    /// Expected LL-HLS latency added by the server (PART-HOLD-BACK + one part).
    pub latency_hint_secs: f64,
}

#[derive(Debug, Clone, Default)]
struct PackageState {
    unsupported: Option<String>,
    probing: bool,
    ended: bool,
    /// Stream info captured when the stream ended (the `LiveStream` is gone then).
    final_info: Option<StreamInfo>,
}

/// The packaging state of one publishing session (live, or lingering after the end).
pub struct StreamPackage {
    pub id: u64,
    pub name: String,
    pub store: Arc<SegmentStore>,
    stream: Weak<LiveStream>,
    state: Mutex<PackageState>,
    flags: Mutex<Option<(bool, bool, Instant)>>,
    /// Cancelled when a newer session replaces this one (ends the linger early).
    superseded: CancellationToken,
}

impl std::fmt::Debug for StreamPackage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamPackage")
            .field("id", &self.id)
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl StreamPackage {
    fn lock(&self) -> std::sync::MutexGuard<'_, PackageState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn is_for(&self, stream: &Arc<LiveStream>) -> bool {
        std::ptr::eq(self.stream.as_ptr(), Arc::as_ptr(stream))
    }

    /// `Some(reason)` when the stream's codecs cannot be packaged.
    pub fn unsupported_reason(&self) -> Option<String> {
        self.lock().unsupported.clone()
    }

    pub fn is_ended(&self) -> bool {
        self.lock().ended
    }

    /// Current stream info (codecs, fps, declared bitrates).
    pub fn stream_info(&self) -> StreamInfo {
        if let Some(s) = self.stream.upgrade() {
            return s.info();
        }
        self.lock().final_info.clone().unwrap_or_default()
    }

    /// Cached `(enable_hls, enable_dash)` of the broadcast, if still fresh.
    pub fn cached_flags(&self) -> Option<(bool, bool)> {
        let f = self.flags.lock().unwrap_or_else(|e| e.into_inner());
        f.filter(|(_, _, at)| at.elapsed() < FLAGS_TTL)
            .map(|(h, d, _)| (h, d))
    }

    pub fn set_flags(&self, enable_hls: bool, enable_dash: bool) {
        *self.flags.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((enable_hls, enable_dash, Instant::now()));
    }

    fn invalidate_flags(&self) {
        *self.flags.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    fn set_segmenter_status(&self, status: &SegmenterStatus) {
        let mut st = self.lock();
        st.probing = *status == SegmenterStatus::Probing;
        if let SegmenterStatus::Unsupported(r) = status {
            st.unsupported = Some(r.clone());
        }
    }
}

pub struct PackagerManager {
    config: Arc<Config>,
    hub: Arc<Hub>,
    #[allow(dead_code)]
    tracker: Arc<ViewerTracker>,
    entries: RwLock<HashMap<String, Arc<StreamPackage>>>,
    next_id: AtomicU64,
}

impl PackagerManager {
    pub fn new(config: Arc<Config>, hub: Arc<Hub>, tracker: Arc<ViewerTracker>) -> Arc<Self> {
        Arc::new(Self {
            config,
            hub,
            tracker,
            entries: RwLock::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        })
    }

    fn enabled(&self) -> bool {
        self.config.hls.enabled || self.config.dash.enabled
    }

    /// Spawn the manager task: starts one packager per live stream (streams already
    /// live included) and invalidates cached broadcast flags on updates.
    pub fn spawn(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let mut events = self.hub.subscribe_events();
        let this = Arc::clone(self);
        tokio::spawn(async move {
            if !this.enabled() {
                tracing::info!(
                    task = "packager-manager",
                    "LL-HLS and LL-DASH disabled; packager not running"
                );
                return;
            }
            tracing::debug!(task = "packager-manager", "started");
            for s in this.hub.list() {
                this.start_stream(s);
            }
            loop {
                match events.recv().await {
                    Ok(HubEvent::StreamStarted { name }) => {
                        if let Some(s) = this.hub.get(&name) {
                            this.start_stream(s);
                        }
                    }
                    Ok(HubEvent::BroadcastUpdated { name }) => {
                        if let Some(p) = this.get(&name) {
                            p.invalidate_flags();
                        }
                    }
                    Ok(_) => {}
                    Err(RecvError::Lagged(n)) => {
                        tracing::warn!(
                            task = "packager-manager",
                            skipped = n,
                            "lagged on hub events; rescanning live streams"
                        );
                        for s in this.hub.list() {
                            this.start_stream(s);
                        }
                        for p in this.all() {
                            p.invalidate_flags();
                        }
                    }
                    Err(RecvError::Closed) => break,
                }
            }
        })
    }

    /// The package of `name` (live or lingering), if any.
    pub fn get(&self, name: &str) -> Option<Arc<StreamPackage>> {
        self.entries
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .cloned()
    }

    fn all(&self) -> Vec<Arc<StreamPackage>> {
        self.entries
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect()
    }

    /// Packaging status of `name` (`None` when nothing is packaged for it).
    pub fn status(&self, name: &str) -> Option<PackagerStatus> {
        let pkg = self.get(name)?;
        let st = pkg.lock().clone();
        let part_target = self.config.hls.part_duration;
        let mut status = pkg.store.with(|s| PackagerStatus {
            supported: st.unsupported.is_none(),
            reason: st.unsupported.clone(),
            hls_ready: self.config.hls.enabled && s.hls_ready(),
            dash_ready: self.config.dash.enabled && s.hls_ready(),
            ended: st.ended,
            segments: s.segments_total,
            parts: s.parts_total,
            measured_gop_secs: s.measured_gop_ms.map(|ms| (ms / 10.0).round() / 100.0),
            last_segment_secs: s.last_segment_secs(),
            part_target_secs: s.part_target_secs(),
            target_duration_secs: hls_playlist::target_duration(s),
            latency_hint_secs: hls_playlist::part_hold_back(s) + part_target,
        });
        if status.supported && st.probing && !st.ended {
            status.reason = Some("waiting for H.264/AAC sequence headers and a keyframe".into());
        }
        if !status.supported {
            status.hls_ready = false;
            status.dash_ready = false;
        }
        Some(status)
    }

    fn store_params(&self) -> StoreParams {
        StoreParams {
            part_target_ms: ms(self.config.hls.part_duration),
            window_segments: self.config.hls.window_segments.max(3) as usize,
            nominal_segment_ms: ms(self.config.dash.segment_duration),
        }
    }

    fn segmenter_config(&self) -> SegmenterConfig {
        let part = ms(self.config.hls.part_duration);
        let max = ms(self.config.hls.max_segment_duration).max(part);
        SegmenterConfig {
            part_target_ms: part,
            max_segment_ms: max,
            min_segment_ms: part.max(1000).min(max),
            probe_audio_ms: 1000,
        }
    }

    /// Start packaging `stream` unless it is already being packaged.
    fn start_stream(self: &Arc<Self>, stream: Arc<LiveStream>) {
        if stream.is_ended() {
            return;
        }
        let pkg = {
            let mut map = self.entries.write().unwrap_or_else(|e| e.into_inner());
            if map.get(&stream.name).is_some_and(|p| p.is_for(&stream)) {
                return;
            }
            let pkg = Arc::new(StreamPackage {
                id: self.next_id.fetch_add(1, Ordering::Relaxed),
                name: stream.name.clone(),
                store: SegmentStore::new(self.store_params()),
                stream: Arc::downgrade(&stream),
                state: Mutex::new(PackageState {
                    probing: true,
                    ..Default::default()
                }),
                flags: Mutex::new(None),
                superseded: CancellationToken::new(),
            });
            if let Some(old) = map.insert(stream.name.clone(), Arc::clone(&pkg)) {
                // A lingering session of the same broadcast: replace it now.
                old.superseded.cancel();
                old.store.end();
            }
            pkg
        };
        let this = Arc::clone(self);
        tokio::spawn(async move { this.run(pkg, stream).await });
    }

    /// Packager task of one publishing session.
    async fn run(self: Arc<Self>, pkg: Arc<StreamPackage>, stream: Arc<LiveStream>) {
        let name = stream.name.clone();
        tracing::debug!(task = "packager", broadcast = %name, "started");
        let (snapshot, mut rx) = stream.subscribe();
        let ended = stream.ended_token();
        let mut seg = Segmenter::new(self.segmenter_config(), Arc::clone(&pkg.store), &name);
        if let Some(p) = snapshot
            .iter()
            .rev()
            .find(|p| p.kind != PacketKind::Script && !p.is_sequence_header)
        {
            seg.hint_latest_timestamp(p.timestamp);
        }
        for p in &snapshot {
            seg.push(p);
        }
        drop(snapshot);
        let mut last_status = seg.status().clone();
        pkg.set_segmenter_status(&last_status);

        loop {
            if matches!(last_status, SegmenterStatus::Unsupported(_)) {
                // Stop consuming packets; keep the status until the stream ends.
                drop(rx);
                ended.cancelled().await;
                break;
            }
            tokio::select! {
                biased;
                _ = ended.cancelled() => {
                    // Package what was sent right before the end.
                    while let Ok(p) = rx.try_recv() {
                        seg.push(&p);
                    }
                    break;
                }
                r = rx.recv() => match r {
                    Ok(p) => seg.push(&p),
                    Err(RecvError::Lagged(n)) => seg.on_lag(n),
                    Err(RecvError::Closed) => break,
                },
            }
            if *seg.status() != last_status {
                last_status = seg.status().clone();
                pkg.set_segmenter_status(&last_status);
            }
        }

        seg.finish();
        {
            let mut st = pkg.lock();
            st.ended = true;
            st.final_info = Some(stream.info());
        }
        drop(stream);
        let linger = Duration::from_secs(self.config.hls.linger_secs);
        tracing::debug!(task = "packager", broadcast = %name, linger_secs = linger.as_secs(), "stream ended; lingering");
        tokio::select! {
            _ = tokio::time::sleep(linger) => {}
            _ = pkg.superseded.cancelled() => {}
        }
        let removed = {
            let mut map = self.entries.write().unwrap_or_else(|e| e.into_inner());
            match map.get(&name) {
                Some(cur) if cur.id == pkg.id => map.remove(&name).is_some(),
                _ => false,
            }
        };
        tracing::debug!(task = "packager", broadcast = %name, removed, "packager finished");
    }
}

fn ms(secs: f64) -> u32 {
    (secs * 1000.0).round().clamp(1.0, u32::MAX as f64) as u32
}

/// HLS/DASH routes (`/hls/{name}/...`, `/dash/{name}/...`, `/dash/time`), merged at `/`
/// by `main.rs`. CORS is permissive (playback from any origin).
pub fn router() -> Router<AppState> {
    Router::new()
        .merge(crate::egress::hls::routes())
        .merge(crate::egress::dash::routes())
        .layer(CorsLayer::permissive())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::{MediaPacket, PublishInfo};
    use bytes::Bytes;

    fn config(linger: u64) -> Arc<Config> {
        let mut c = Config::default();
        c.hls.linger_secs = linger;
        Arc::new(c)
    }

    async fn wait_for(mut f: impl FnMut() -> bool) {
        for _ in 0..300 {
            if f() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("condition not reached");
    }

    fn feed(stream: &LiveStream, from_ms: u32, frames: u32) {
        for i in 0..frames {
            let ts = from_ms + i * 33;
            let key = i % 60 == 0;
            let b0 = if key { 0x17 } else { 0x27 };
            stream.push(MediaPacket::video(
                ts,
                Bytes::from(vec![b0, 1, 0, 0, 0, 0, 0, 0, 1, 0x41]),
            ));
            stream.push(MediaPacket::audio(
                ts,
                Bytes::from_static(&[0xAF, 0x01, 0x21, 0x10]),
            ));
        }
    }

    fn headers(stream: &LiveStream) {
        let sps: [u8; 26] = [
            0x67, 0x64, 0x00, 0x1f, 0xac, 0xd9, 0x40, 0x50, 0x05, 0xbb, 0x01, 0x10, 0x00, 0x00,
            0x03, 0x00, 0x10, 0x00, 0x00, 0x03, 0x03, 0xc0, 0xf1, 0x83, 0x19, 0x60,
        ];
        let mut body = vec![0x17, 0x00, 0, 0, 0, 1, 0x64, 0x00, 0x1f, 0xff, 0xe1];
        body.extend_from_slice(&(sps.len() as u16).to_be_bytes());
        body.extend_from_slice(&sps);
        body.extend_from_slice(&[1, 0, 4, 0x68, 0xeb, 0xe3, 0xcb]);
        stream.push(MediaPacket::video(0, Bytes::from(body)));
        stream.push(MediaPacket::audio(
            0,
            Bytes::from_static(&[0xAF, 0x00, 0x11, 0x90]),
        ));
    }

    #[tokio::test]
    async fn lifecycle_linger_and_restart() {
        let hub = Hub::new();
        let cfg = config(1);
        let tracker = ViewerTracker::new(&cfg, None, hub.events());
        let mgr = PackagerManager::new(cfg, Arc::clone(&hub), tracker);
        // A stream live before the manager starts is picked up too.
        let s1 = hub.publish_start("a", PublishInfo::new("id")).unwrap();
        mgr.spawn();
        wait_for(|| mgr.get("a").is_some()).await;
        headers(&s1);
        feed(&s1, 0, 130);
        wait_for(|| mgr.status("a").is_some_and(|s| s.hls_ready)).await;
        let st = mgr.status("a").unwrap();
        assert!(st.supported && !st.ended);
        assert!(st.segments >= 1);
        assert!(st.reason.is_none());
        assert_eq!(st.latency_hint_secs, 2.0);
        let first_id = mgr.get("a").unwrap().id;

        hub.publish_end("a");
        wait_for(|| mgr.status("a").is_some_and(|s| s.ended)).await;
        assert!(mgr.get("a").unwrap().store.is_ended());
        assert_eq!(mgr.status("a").unwrap().segments, 3);
        // Removed after the linger period.
        tokio::time::sleep(Duration::from_millis(1000)).await;
        wait_for(|| mgr.get("a").is_none()).await;

        // A new session gets a fresh package.
        let s2 = hub.publish_start("a", PublishInfo::new("id")).unwrap();
        wait_for(|| mgr.get("a").is_some()).await;
        assert_ne!(mgr.get("a").unwrap().id, first_id);
        assert!(mgr.status("a").unwrap().reason.is_some(), "probing");
        hub.publish_end_stream(&s2);
    }

    #[tokio::test]
    async fn new_session_replaces_lingering_one() {
        let hub = Hub::new();
        let cfg = config(60);
        let tracker = ViewerTracker::new(&cfg, None, hub.events());
        let mgr = PackagerManager::new(cfg, Arc::clone(&hub), tracker);
        mgr.spawn();
        tokio::task::yield_now().await;
        let s1 = hub.publish_start("b", PublishInfo::new("id")).unwrap();
        wait_for(|| mgr.get("b").is_some()).await;
        headers(&s1);
        feed(&s1, 0, 70);
        hub.publish_end("b");
        wait_for(|| mgr.status("b").is_some_and(|s| s.ended)).await;
        let old = mgr.get("b").unwrap();
        let _s2 = hub.publish_start("b", PublishInfo::new("id")).unwrap();
        wait_for(|| mgr.get("b").is_some_and(|p| p.id != old.id)).await;
        assert!(old.superseded.is_cancelled());
        assert!(!mgr.get("b").unwrap().is_ended());
    }

    #[tokio::test]
    async fn unsupported_codec_status() {
        let hub = Hub::new();
        let cfg = config(1);
        let tracker = ViewerTracker::new(&cfg, None, hub.events());
        let mgr = PackagerManager::new(cfg, Arc::clone(&hub), tracker);
        mgr.spawn();
        tokio::task::yield_now().await;
        let s = hub.publish_start("c", PublishInfo::new("id")).unwrap();
        wait_for(|| mgr.get("c").is_some()).await;
        s.push(MediaPacket::video(
            0,
            Bytes::from_static(&[0x90, b'h', b'v', b'c', b'1', 1, 2, 3]),
        ));
        wait_for(|| mgr.status("c").is_some_and(|s| !s.supported)).await;
        let st = mgr.status("c").unwrap();
        assert!(st.reason.unwrap().contains("hevc"));
        assert!(!st.hls_ready && !st.dash_ready);
        // The packager stopped consuming packets.
        wait_for(|| s.subscriber_count() == 0).await;
    }
}
