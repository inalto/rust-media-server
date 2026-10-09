//! RecorderManager: FLV writer per live stream (auto-record flag or manual start/stop),
//! remuxed to MP4 with ffmpeg when available (ARCHITECTURE.md §1.5, §8.1).
//!
//! A recording is a `recordings` row plus a task subscribed to the [`LiveStream`]:
//!
//! - status `recording`: packets are written to
//!   `data/recordings/<broadcast>/<YYYYMMDD-HHMMSS>.flv` (UTC) by [`FlvWriter`]
//!   (timestamps rebased to start at 0); `size_bytes`/`duration_secs` are refreshed
//!   every 5 s. On channel lag the writer skips video until the next keyframe.
//! - the recording ends when the stream ends (`ended_token`), on
//!   [`RecorderManager::stop_manual`] or at shutdown; the file is flushed and fsynced.
//! - status `processing`: `ffmpeg -c copy -movflags +faststart` to `.mp4` (10 min
//!   timeout) when ffmpeg is available and `recording.remux_to_mp4` is set.
//! - status `ready`: MP4 (FLV deleted unless `recording.keep_flv`), or FLV when ffmpeg
//!   is unavailable / remux is disabled / the remux failed (`error` = ffmpeg stderr tail).
//! - status `failed`: no media was received, a write error occurred, or the file
//!   vanished (crash recovery).
//!
//! Auto-record: on `HubEvent::StreamStarted` (and for streams already live when the
//! manager starts) a recording starts when the broadcast has `record = true`; in
//! cluster mode only on the owner node (relayed streams are never recorded). A manual
//! stop only ends the current file: auto-record is evaluated once per live period.
//!
//! Crash safety: at startup rows left in `recording`/`processing` are finalized (the
//! FLV is repaired — truncated to the last complete tag — and remuxed) or marked
//! `failed` when the file is missing.
//!
//! `HubEvent::BroadcastUpdated` is emitted on start, stop and finalization so the
//! dashboard (SSE) refreshes.

pub mod flv_writer;
pub mod remux;

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use anyhow::anyhow;
use chrono::{DateTime, Utc};
use tokio::sync::broadcast::{self, error::RecvError};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::db::recordings::{
    self as repo, FORMAT_FLV, FORMAT_MP4, STATUS_FAILED, STATUS_PROCESSING, STATUS_READY,
    STATUS_RECORDING,
};
use crate::db::{self, Db, Recording};
use crate::hub::{Hub, HubEvent, LiveStream, MediaPacket, PacketKind};
pub use flv_writer::{FlvStats, FlvWriter, TimestampRebaser};

/// The in-progress recording of a broadcast (a `recordings` row, status "recording").
pub type RecordingInfo = crate::db::Recording;

#[derive(Debug, thiserror::Error)]
pub enum RecorderError {
    #[error("broadcast is not live")]
    NotLive,
    #[error("broadcast is already being recorded")]
    AlreadyRecording,
    #[error("broadcast is not being recorded")]
    NotRecording,
    #[error("recording not found")]
    NotFound,
    #[error("recording is still in progress or being processed")]
    Busy,
    /// Cluster mode: only the owner node records (this node only relays the stream).
    #[error("the stream is published on node {0}: recordings are made by that node")]
    Remote(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// How often `size_bytes`/`duration_secs` of an active recording are refreshed.
const SIZE_UPDATE_INTERVAL: Duration = Duration::from_secs(5);
/// How long `stop_manual` waits for the file to be closed.
const STOP_TIMEOUT: Duration = Duration::from_secs(15);
/// How long `shutdown` waits for active recordings to close their files.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

pub struct RecorderManager {
    shared: Arc<Shared>,
}

struct Shared {
    config: Arc<Config>,
    db: Db,
    hub: Arc<Hub>,
    /// Resolved ffmpeg executable (`None`: recordings stay FLV).
    ffmpeg: Option<PathBuf>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// In-progress recording per broadcast name.
    active: HashMap<String, Active>,
    /// Ids of recordings being written or post-processed by this process.
    busy: HashSet<String>,
    /// Live streams for which auto-record was already evaluated (one decision per
    /// live period, so a manual stop is not undone).
    auto_checked: HashMap<String, Weak<LiveStream>>,
}

struct Active {
    id: String,
    stream: Weak<LiveStream>,
    stop: CancellationToken,
    rec: watch::Receiver<Recording>,
}

impl Active {
    /// The live period this recording belongs to is still going on.
    fn is_current(&self) -> bool {
        self.stream.upgrade().is_some_and(|s| !s.is_ended())
    }
}

impl RecorderManager {
    pub fn new(config: Arc<Config>, db: Db, hub: Arc<Hub>) -> Arc<Self> {
        let ffmpeg = config.ffmpeg_path();
        Arc::new(Self {
            shared: Arc::new(Shared {
                config,
                db,
                hub,
                ffmpeg,
                state: Mutex::new(State::default()),
            }),
        })
    }

    /// Spawn the manager: crash recovery of unfinished rows, then auto-record on
    /// `StreamStarted` (and for streams already live).
    pub fn spawn(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let shared = Arc::clone(&self.shared);
        let events = shared.hub.subscribe_events();
        let recovery = Arc::clone(&shared);
        tokio::spawn(async move {
            recovery.recover().await;
        });
        tokio::spawn(shared.event_loop(events))
    }

    /// Start a manual recording of the live broadcast `name`.
    pub async fn start_manual(&self, name: &str) -> Result<RecordingInfo, RecorderError> {
        let stream = self.shared.hub.get(name).ok_or(RecorderError::NotLive)?;
        if let Some(node) = stream.origin.remote_node() {
            return Err(RecorderError::Remote(node.to_string()));
        }
        self.shared.start(stream).await
    }

    /// Stop the current recording of `name`: the file is closed and the row moves to
    /// `processing` (remux pending) or `ready`/`failed`. Returns the row in that state.
    pub async fn stop_manual(&self, name: &str) -> Result<RecordingInfo, RecorderError> {
        let (stop, mut rx) = {
            let st = self.shared.lock();
            match st.active.get(name) {
                Some(a) if a.is_current() => (a.stop.clone(), a.rec.clone()),
                _ => return Err(RecorderError::NotRecording),
            }
        };
        stop.cancel();
        let waited =
            tokio::time::timeout(STOP_TIMEOUT, rx.wait_for(|r| r.status != STATUS_RECORDING))
                .await
                .ok()
                .and_then(|r| r.ok().map(|r| r.clone()));
        // Sender gone or still flushing: report the latest known state.
        Ok(waited.unwrap_or_else(|| rx.borrow().clone()))
    }

    /// The in-progress recording of `name`, if any.
    pub fn current(&self, name: &str) -> Option<RecordingInfo> {
        let st = self.shared.lock();
        let a = st.active.get(name)?;
        if !a.is_current() {
            return None;
        }
        let r = a.rec.borrow().clone();
        (r.status == STATUS_RECORDING).then_some(r)
    }

    /// Delete a finished recording: row and file(s). `Busy` while it is still being
    /// recorded or processed.
    pub async fn delete(&self, id: &str) -> Result<RecordingInfo, RecorderError> {
        self.shared.delete(id).await
    }

    /// Absolute path of a recording's file (`None` for a malformed relative path).
    pub fn file_path(&self, rec: &Recording) -> Option<PathBuf> {
        self.shared.abs(&rec.path)
    }

    /// Graceful shutdown: end every active recording and wait (bounded) until their
    /// files are closed. Pending remuxes are resumed by crash recovery at next start.
    pub async fn shutdown(&self) {
        let waits: Vec<watch::Receiver<Recording>> = {
            let st = self.shared.lock();
            st.active
                .values()
                .map(|a| {
                    a.stop.cancel();
                    a.rec.clone()
                })
                .collect()
        };
        if waits.is_empty() {
            return;
        }
        tracing::info!(count = waits.len(), "finalizing active recordings");
        let all = futures::future::join_all(waits.into_iter().map(|mut rx| async move {
            let _ = rx.wait_for(|r| r.status != STATUS_RECORDING).await;
        }));
        if tokio::time::timeout(SHUTDOWN_TIMEOUT, all).await.is_err() {
            tracing::warn!("recordings did not close in time");
        }
    }
}

/// Seconds (millisecond precision) from milliseconds.
fn ms_to_secs(ms: u32) -> f64 {
    ms as f64 / 1000.0
}

fn now_ms() -> DateTime<Utc> {
    db::parse_ts(&db::fmt_ts(&Utc::now()))
}

/// `<stem>.<ext>` for a relative path `<stem>.<anything>`.
fn with_extension(rel: &str, ext: &str) -> String {
    match rel.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() && !stem.ends_with('/') => format!("{stem}.{ext}"),
        _ => format!("{rel}.{ext}"),
    }
}

/// Broadcast name of a recording path (`<name>/<file>`).
fn name_of(rel: &str) -> &str {
    rel.split('/').next().unwrap_or(rel)
}

/// Drops video until a keyframe (start and after lag); once the stream is known to
/// carry video, audio before the first keyframe is dropped as well so the file starts
/// cleanly.
#[derive(Debug, Default)]
struct KeyframeGate {
    expect_video: bool,
    seen_keyframe: bool,
    skipping: bool,
}

impl KeyframeGate {
    fn new(expect_video: bool) -> Self {
        Self {
            expect_video,
            ..Self::default()
        }
    }

    fn accept(&mut self, p: &MediaPacket) -> bool {
        if p.kind == PacketKind::Script || p.is_sequence_header {
            if p.is_video() {
                self.expect_video = true;
            }
            return true;
        }
        if p.is_video() {
            self.expect_video = true;
            if p.is_keyframe {
                self.seen_keyframe = true;
                self.skipping = false;
                return true;
            }
            return self.seen_keyframe && !self.skipping;
        }
        // Audio.
        !self.expect_video || self.seen_keyframe
    }

    fn lagged(&mut self) {
        if self.expect_video {
            self.skipping = true;
        }
    }
}

/// Everything the writer task needs.
struct Job {
    name: String,
    rec: Recording,
    writer: FlvWriter,
    snapshot: Vec<Arc<MediaPacket>>,
    rx: broadcast::Receiver<Arc<MediaPacket>>,
    ended: CancellationToken,
    stop: CancellationToken,
    tx: watch::Sender<Recording>,
    expect_video: bool,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn emit(&self, name: &str) {
        self.hub.emit_shared(HubEvent::BroadcastUpdated {
            name: name.to_string(),
        });
    }

    fn remux_enabled(&self) -> bool {
        self.config.recording.remux_to_mp4 && self.ffmpeg.is_some()
    }

    /// Absolute path under `data/recordings/` (only plain relative components accepted).
    fn abs(&self, rel: &str) -> Option<PathBuf> {
        let p = Path::new(rel);
        let ok = !rel.is_empty() && p.components().all(|c| matches!(c, Component::Normal(_)));
        ok.then(|| self.config.recordings_dir().join(p))
    }

    async fn event_loop(self: Arc<Self>, mut events: broadcast::Receiver<HubEvent>) {
        tracing::debug!(task = "recorder-manager", "started");
        self.check_live_streams();
        loop {
            match events.recv().await {
                Ok(HubEvent::StreamStarted { name }) => Arc::clone(&self).spawn_auto(name),
                Ok(_) => {}
                Err(RecvError::Lagged(n)) => {
                    tracing::warn!(
                        task = "recorder-manager",
                        skipped = n,
                        "lagged on hub events"
                    );
                    self.check_live_streams();
                }
                Err(RecvError::Closed) => break,
            }
        }
        tracing::debug!(task = "recorder-manager", "stopped");
    }

    /// Evaluate auto-record for every live stream not evaluated yet.
    fn check_live_streams(self: &Arc<Self>) {
        for s in self.hub.list() {
            Arc::clone(self).spawn_auto(s.name.clone());
        }
    }

    /// Record the evaluation of auto-record for this live period; false if already done.
    fn mark_auto_checked(&self, stream: &Arc<LiveStream>) -> bool {
        let mut st = self.lock();
        st.auto_checked
            .retain(|_, w| w.upgrade().is_some_and(|s| !s.is_ended()));
        if let Some(prev) = st.auto_checked.get(&stream.name)
            && prev.upgrade().is_some_and(|p| Arc::ptr_eq(&p, stream))
        {
            return false;
        }
        st.auto_checked
            .insert(stream.name.clone(), Arc::downgrade(stream));
        true
    }

    fn spawn_auto(self: Arc<Self>, name: String) {
        tokio::spawn(async move {
            // Relays never record: the owner node does (docs/CLUSTER.md §3.7).
            let Some(stream) = self.hub.get(&name).filter(|s| s.origin.is_local()) else {
                return;
            };
            if !self.mark_auto_checked(&stream) {
                return;
            }
            match db::broadcasts::find_by_name(&self.db, &name).await {
                Ok(Some(b)) if b.record => match self.start(stream).await {
                    Ok(_) | Err(RecorderError::AlreadyRecording) | Err(RecorderError::NotLive) => {}
                    Err(e) => {
                        tracing::error!(task = "recorder-manager", broadcast = %name, error = %e, "cannot start automatic recording")
                    }
                },
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(task = "recorder-manager", broadcast = %name, error = %e, "cannot load broadcast for auto-record")
                }
            }
        });
    }

    /// Start recording `stream` (manual or automatic).
    async fn start(self: &Arc<Self>, stream: Arc<LiveStream>) -> Result<Recording, RecorderError> {
        if let Some(node) = stream.origin.remote_node() {
            return Err(RecorderError::Remote(node.to_string()));
        }
        let name = stream.name.clone();
        let mut rec = Recording {
            id: uuid::Uuid::new_v4().to_string(),
            broadcast_id: stream.broadcast_id.clone(),
            stream_session_id: stream.stream_session_id.clone(),
            path: String::new(),
            format: FORMAT_FLV.into(),
            status: STATUS_RECORDING.into(),
            started_at: now_ms(),
            ended_at: None,
            size_bytes: 0,
            duration_secs: None,
            error: None,
        };
        let stop = CancellationToken::new();
        let (tx, rx) = watch::channel(rec.clone());
        {
            let mut st = self.lock();
            if stream.is_ended() {
                return Err(RecorderError::NotLive);
            }
            if st.active.get(&name).is_some_and(Active::is_current) {
                return Err(RecorderError::AlreadyRecording);
            }
            st.active.insert(
                name.clone(),
                Active {
                    id: rec.id.clone(),
                    stream: Arc::downgrade(&stream),
                    stop: stop.clone(),
                    rec: rx,
                },
            );
            st.busy.insert(rec.id.clone());
        }
        let writer = match self.create_file(&stream, &mut rec).await {
            Ok(w) => w,
            Err(e) => {
                self.remove_active(&name, &rec.id);
                self.release(&rec.id);
                return Err(e);
            }
        };
        let (snapshot, packets) = stream.subscribe();
        let info = stream.info();
        let expect_video = info.has_video || snapshot.iter().any(|p| p.is_video());
        tx.send_replace(rec.clone());
        let job = Job {
            name: name.clone(),
            rec: rec.clone(),
            writer,
            snapshot,
            rx: packets,
            ended: stream.ended_token(),
            stop,
            tx,
            expect_video,
        };
        drop(stream);
        tokio::spawn(Arc::clone(self).run(job));
        self.emit(&name);
        Ok(rec)
    }

    /// Create `data/recordings/<name>/<stamp>[-n].flv` and its database row.
    async fn create_file(
        &self,
        stream: &LiveStream,
        rec: &mut Recording,
    ) -> Result<FlvWriter, RecorderError> {
        let dir = self.config.recordings_dir().join(&stream.name);
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| anyhow!("cannot create {}: {e}", dir.display()))?;
        let info = stream.info();
        let (snap_video, snap_audio) = stream.has_headers();
        let (mut has_video, mut has_audio) =
            (info.has_video || snap_video, info.has_audio || snap_audio);
        if !has_video && !has_audio {
            // Nothing known yet (stream just started): announce both.
            (has_video, has_audio) = (true, true);
        }
        let stamp = rec.started_at.format("%Y%m%d-%H%M%S").to_string();
        let mut created = None;
        for n in 1..1000 {
            let stem = if n == 1 {
                stamp.clone()
            } else {
                format!("{stamp}-{n}")
            };
            if dir.join(format!("{stem}.mp4")).exists() {
                continue;
            }
            let file = format!("{stem}.flv");
            match FlvWriter::create(&dir.join(&file), has_video, has_audio).await {
                Ok(w) => {
                    created = Some((w, file));
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => {
                    return Err(
                        anyhow!("cannot create recording file in {}: {e}", dir.display()).into(),
                    );
                }
            }
        }
        let Some((writer, file)) = created else {
            return Err(anyhow!(
                "cannot find a free recording file name in {}",
                dir.display()
            )
            .into());
        };
        rec.path = format!("{}/{}", stream.name, file);
        rec.size_bytes = writer.bytes();
        if let Err(e) = repo::insert(&self.db, rec).await {
            let _ = tokio::fs::remove_file(writer.path()).await;
            return Err(anyhow!("cannot insert recording row: {e}").into());
        }
        Ok(writer)
    }

    fn remove_active(&self, name: &str, id: &str) {
        let mut st = self.lock();
        if st.active.get(name).is_some_and(|a| a.id == id) {
            st.active.remove(name);
        }
    }

    fn release(&self, id: &str) {
        self.lock().busy.remove(id);
    }

    /// Writer task of one recording.
    async fn run(self: Arc<Self>, job: Job) {
        let Job {
            name,
            mut rec,
            mut writer,
            snapshot,
            mut rx,
            ended,
            stop,
            tx,
            expect_video,
        } = job;
        tracing::info!(task = "recorder", broadcast = %name, id = %rec.id, path = %rec.path, "recording started");
        let mut gate = KeyframeGate::new(expect_video);
        let mut write_error: Option<String> = None;
        for p in &snapshot {
            if gate.accept(p)
                && let Err(e) = writer.write(p).await
            {
                write_error = Some(e.to_string());
                break;
            }
        }
        drop(snapshot);
        let mut tick = tokio::time::interval_at(
            tokio::time::Instant::now() + SIZE_UPDATE_INTERVAL,
            SIZE_UPDATE_INTERVAL,
        );
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut reason = "stream ended";
        while write_error.is_none() {
            tokio::select! {
                biased;
                _ = stop.cancelled() => {
                    reason = "stopped";
                    break;
                }
                // Before `ended`: packets still queued when the stream ends are written.
                r = rx.recv() => match r {
                    Ok(p) => {
                        if gate.accept(&p) && let Err(e) = writer.write(&p).await {
                            write_error = Some(e.to_string());
                        }
                    }
                    Err(RecvError::Lagged(n)) => {
                        tracing::warn!(task = "recorder", broadcast = %name, skipped = n, "recording lagged; skipping video until the next keyframe");
                        gate.lagged();
                    }
                    Err(RecvError::Closed) => break,
                },
                _ = ended.cancelled() => break,
                _ = tick.tick() => {
                    if let Err(e) = writer.flush().await {
                        write_error = Some(e.to_string());
                        continue;
                    }
                    rec.size_bytes = writer.bytes();
                    rec.duration_secs = Some(ms_to_secs(writer.duration_ms()));
                    if let Err(e) = repo::update(&self.db, &rec).await {
                        tracing::warn!(task = "recorder", broadcast = %name, error = %e, "cannot update recording row");
                    }
                    tx.send_replace(rec.clone());
                }
            }
        }
        drop(rx);
        tracing::debug!(task = "recorder", broadcast = %name, id = %rec.id, reason, "recording loop ended");
        if let Some(e) = &write_error {
            tracing::error!(task = "recorder", broadcast = %name, id = %rec.id, error = %e, "recording write error");
            reason = "write error";
        }
        self.finish(name, rec, writer, write_error, reason, tx)
            .await;
    }

    /// Close the file and finalize the row (remux when enabled).
    async fn finish(
        self: Arc<Self>,
        name: String,
        mut rec: Recording,
        writer: FlvWriter,
        mut write_error: Option<String>,
        reason: &str,
        tx: watch::Sender<Recording>,
    ) {
        let id = rec.id.clone();
        let flv = writer.path().to_path_buf();
        let media = writer.media_packets();
        let closing = std::time::Instant::now();
        let closed = writer.finish().await;
        let close_ms = closing.elapsed().as_millis() as u64;
        if close_ms > 2000 {
            tracing::warn!(task = "recorder", broadcast = %name, id = %id, close_ms, "closing the recording file was slow (disk I/O congestion?)");
        } else {
            tracing::debug!(task = "recorder", broadcast = %name, id = %id, close_ms, "recording file closed");
        }
        match closed {
            Ok(stats) => {
                rec.size_bytes = stats.bytes;
                rec.duration_secs = Some(ms_to_secs(stats.duration_ms));
            }
            Err(e) => {
                write_error.get_or_insert(e.to_string());
                rec.size_bytes = tokio::fs::metadata(&flv)
                    .await
                    .map(|m| m.len())
                    .unwrap_or(0);
            }
        }
        rec.ended_at = Some(now_ms());
        // From now on this is no longer the in-progress recording of the broadcast.
        self.remove_active(&name, &rec.id);
        if media == 0 {
            rec.status = STATUS_FAILED.into();
            rec.error = Some("no media received".into());
            rec.size_bytes = 0;
            rec.duration_secs = None;
            let _ = tokio::fs::remove_file(&flv).await;
        } else if let Some(e) = write_error {
            rec.status = STATUS_FAILED.into();
            rec.error = Some(format!("write error: {e}"));
        } else if self.remux_enabled() {
            rec.status = STATUS_PROCESSING.into();
        } else {
            rec.status = STATUS_READY.into();
        }
        self.save(&rec).await;
        tx.send_replace(rec.clone());
        self.emit(&name);
        tracing::info!(
            task = "recorder",
            broadcast = %name,
            id = %rec.id,
            reason,
            status = %rec.status,
            size_bytes = rec.size_bytes,
            duration_secs = rec.duration_secs.unwrap_or(0.0),
            "recording stopped"
        );
        if rec.status == STATUS_PROCESSING {
            let rec = self.remux(rec, &flv).await;
            self.save(&rec).await;
            self.emit(&name);
        }
        self.release(&id);
    }

    async fn save(&self, rec: &Recording) {
        match repo::update(&self.db, rec).await {
            Ok(true) => {}
            Ok(false) => tracing::warn!(id = %rec.id, "recording row vanished"),
            Err(e) => tracing::error!(id = %rec.id, error = %e, "cannot update recording row"),
        }
    }

    /// FLV → MP4 (status `processing` → `ready`); on failure the FLV is kept.
    async fn remux(&self, mut rec: Recording, flv: &Path) -> Recording {
        let Some(ffmpeg) = self.ffmpeg.clone() else {
            rec.status = STATUS_READY.into();
            return rec;
        };
        let mp4_rel = with_extension(&rec.path, "mp4");
        let Some(mp4) = self.abs(&mp4_rel) else {
            rec.status = STATUS_READY.into();
            return rec;
        };
        let started = std::time::Instant::now();
        match remux::remux_to_mp4(&ffmpeg, flv, &mp4, remux::REMUX_TIMEOUT).await {
            Ok(()) => {
                rec.size_bytes = tokio::fs::metadata(&mp4)
                    .await
                    .map(|m| m.len())
                    .unwrap_or(0);
                if let Some(probe) = remux::ffprobe_for(&ffmpeg)
                    && let Some(d) = remux::probe_duration(&probe, &mp4).await
                {
                    rec.duration_secs = Some((d * 1000.0).round() / 1000.0);
                }
                if !self.config.recording.keep_flv
                    && let Err(e) = tokio::fs::remove_file(flv).await
                {
                    tracing::warn!(path = %flv.display(), error = %e, "cannot delete remuxed FLV");
                }
                tracing::info!(
                    task = "recorder",
                    id = %rec.id,
                    path = %mp4_rel,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "recording remuxed to MP4"
                );
                rec.path = mp4_rel;
                rec.format = FORMAT_MP4.into();
                rec.error = None;
            }
            Err(e) => {
                tracing::warn!(task = "recorder", id = %rec.id, error = %e, "MP4 remux failed; keeping FLV");
                rec.format = FORMAT_FLV.into();
                rec.error = Some(e);
            }
        }
        rec.status = STATUS_READY.into();
        rec
    }

    /// Crash recovery: finalize rows left `recording`/`processing` by a previous run.
    async fn recover(self: Arc<Self>) {
        let rows = match repo::list_unfinished(&self.db).await {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(task = "recorder-recovery", error = %e, "cannot list unfinished recordings");
                return;
            }
        };
        for rec in rows {
            {
                let mut st = self.lock();
                if !st.busy.insert(rec.id.clone()) {
                    continue; // started by this process
                }
            }
            let id = rec.id.clone();
            tracing::warn!(task = "recorder-recovery", id = %id, path = %rec.path, status = %rec.status, "finalizing recording left unfinished by a previous run");
            let rec = self.recover_one(rec).await;
            self.save(&rec).await;
            self.emit(name_of(&rec.path));
            self.release(&id);
        }
    }

    async fn recover_one(&self, mut rec: Recording) -> Recording {
        let fail = |mut rec: Recording, msg: String| {
            rec.status = STATUS_FAILED.into();
            rec.error = Some(msg);
            rec.ended_at = rec.ended_at.or(Some(now_ms()));
            rec
        };
        // A completed MP4 whose row was not updated in time.
        if rec.path.ends_with(".mp4") {
            return match self.abs(&rec.path).filter(|p| p.is_file()) {
                Some(p) => {
                    rec.size_bytes = std::fs::metadata(&p)
                        .map(|m| m.len())
                        .unwrap_or(rec.size_bytes);
                    rec.status = STATUS_READY.into();
                    rec.format = FORMAT_MP4.into();
                    rec.ended_at = rec.ended_at.or(Some(now_ms()));
                    rec
                }
                None => fail(rec, "recording file missing after restart".into()),
            };
        }
        let Some(flv) = self.abs(&rec.path).filter(|p| p.is_file()) else {
            return fail(rec, "recording file missing after restart".into());
        };
        let path = flv.clone();
        let scan = tokio::task::spawn_blocking(move || flv_writer::repair_flv(&path)).await;
        let scan = match scan {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => return fail(rec, format!("unreadable recording file: {e}")),
            Err(e) => return fail(rec, format!("recording scan failed: {e}")),
        };
        if scan.media_tags == 0 {
            let _ = tokio::fs::remove_file(&flv).await;
            rec.size_bytes = 0;
            return fail(rec, "no media received".into());
        }
        rec.size_bytes = scan.file_len;
        rec.duration_secs = Some(ms_to_secs(scan.duration_ms));
        if rec.ended_at.is_none() {
            let mtime = std::fs::metadata(&flv)
                .and_then(|m| m.modified())
                .map(DateTime::<Utc>::from)
                .unwrap_or_else(|_| Utc::now());
            rec.ended_at = Some(db::parse_ts(&db::fmt_ts(&mtime)));
        }
        rec.format = FORMAT_FLV.into();
        rec.error = None;
        if self.remux_enabled() {
            rec.status = STATUS_PROCESSING.into();
            self.save(&rec).await;
            self.emit(name_of(&rec.path));
            self.remux(rec, &flv).await
        } else {
            rec.status = STATUS_READY.into();
            rec
        }
    }

    async fn delete(&self, id: &str) -> Result<Recording, RecorderError> {
        if self.lock().busy.contains(id) {
            return Err(RecorderError::Busy);
        }
        let rec = repo::get(&self.db, id)
            .await
            .map_err(|e| anyhow!("database: {e}"))?
            .ok_or(RecorderError::NotFound)?;
        if rec.is_busy() {
            return Err(RecorderError::Busy);
        }
        let rec = repo::delete(&self.db, id)
            .await
            .map_err(|e| anyhow!("database: {e}"))?
            .ok_or(RecorderError::NotFound)?;
        // The main file plus possible siblings (kept FLV, leftover remux temp file).
        let mut files: Vec<String> = vec![rec.path.clone()];
        for ext in ["flv", "mp4", "mp4.tmp"] {
            let sibling = with_extension(&rec.path, ext);
            if !files.contains(&sibling) {
                files.push(sibling);
            }
        }
        for rel in files {
            let Some(p) = self.abs(&rel) else { continue };
            match tokio::fs::remove_file(&p).await {
                Ok(()) => tracing::info!(path = %p.display(), "recording file deleted"),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    tracing::warn!(path = %p.display(), error = %e, "cannot delete recording file")
                }
            }
        }
        tracing::info!(id = %rec.id, path = %rec.path, "recording deleted");
        Ok(rec)
    }
}

#[cfg(test)]
mod tests;
