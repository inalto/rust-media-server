//! Video on demand with multi-bitrate (ABR) delivery (ARCHITECTURE.md §19).
//!
//! A video (`db::vod::VodItem`) lives in `data/vod/<id>/`:
//!
//! ```text
//! data/vod/<id>/source.<ext>   original file (upload or inbox), kept unless vod.keep_source = false
//! data/vod/<id>/poster.jpg     frame at 10 % of the duration, at most 1280 px wide
//! data/vod/<id>/abr/           CMAF output of one ffmpeg pass: manifest.mpd, master.m3u8,
//!                              media_<n>.m3u8, init-<n>.m4s, chunk-<n>-<00001>.m4s
//! data/vod/<id>/abr.tmp/       output being written (swapped in when complete)
//! ```
//!
//! - [`VodManager`] (`AppState.vod`): paths, import of a received/inbox file, job
//!   bookkeeping (cancellation), status, events (`HubEvent::VodChanged`).
//! - [`job`]: the runner task claiming `queued` items atomically (at most
//!   `vod.max_parallel_jobs` at a time) and the job itself (probe → poster → transcode
//!   with progress → HLS/MPD post-processing → `ready`).
//! - [`ffmpeg`]: probe, rendition selection, command line, progress, process runner.
//! - [`hls`]: HLS master rebuild, media playlist and MPD rewrite.
//! - [`inbox`]: the watched folder scanner.
//! - [`serve`]: public playback routes (`/vod/{id}/hls/*`, `/vod/{id}/dash/*`,
//!   `/vod/{id}/poster.jpg`).

pub mod ffmpeg;
pub mod hls;
pub mod inbox;
pub mod job;
pub mod serve;

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use tokio::sync::{Notify, broadcast};
use tokio_util::sync::CancellationToken;

pub use serve::router;

use crate::config::Config;
use crate::db::vod::{self as repo, VodItem};
use crate::db::{Db, DbError};
use crate::hub::HubEvent;

/// Container extensions accepted for video on demand (upload and inbox).
pub const VOD_EXTENSIONS: &[&str] = &[
    "mp4", "mov", "m4v", "mkv", "webm", "avi", "mts", "m2ts", "ts", "mxf", "flv", "mpg", "mpeg",
    "wmv",
];

/// MIME types accepted when an upload's file name has no usable extension.
const VOD_MIME_EXTENSIONS: &[(&str, &str)] = &[
    ("video/mp4", "mp4"),
    ("video/quicktime", "mov"),
    ("video/x-m4v", "m4v"),
    ("video/x-matroska", "mkv"),
    ("video/matroska", "mkv"),
    ("video/webm", "webm"),
    ("video/x-msvideo", "avi"),
    ("video/msvideo", "avi"),
    ("video/avi", "avi"),
    ("video/mp2t", "ts"),
    ("video/vnd.dlna.mpeg-tts", "mts"),
    ("application/mxf", "mxf"),
    ("video/mxf", "mxf"),
    ("video/x-flv", "flv"),
    ("video/mpeg", "mpg"),
    ("video/x-ms-wmv", "wmv"),
];

/// Container type of a VOD file: its extension when accepted, else (uploads) the MIME type.
pub fn vod_extension(file_name: Option<&str>, content_type: Option<&str>) -> Option<&'static str> {
    let from_name = file_name
        .and_then(|n| n.rsplit_once('.'))
        .map(|(_, ext)| ext.trim().to_ascii_lowercase())
        .and_then(|ext| VOD_EXTENSIONS.iter().copied().find(|e| *e == ext));
    if from_name.is_some() {
        return from_name;
    }
    let mime = content_type?
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    VOD_MIME_EXTENSIONS
        .iter()
        .find(|(m, _)| *m == mime)
        .map(|(_, e)| *e)
}

/// Title from a file name: no extension, `_` → space, trimmed (max 200 characters).
pub fn title_from_file_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let stem = match base.rsplit_once('.') {
        Some((s, _)) if !s.is_empty() => s,
        _ => base,
    };
    let t: String = stem
        .replace('_', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let t: String = t.chars().take(200).collect();
    if t.is_empty() { "Video".to_string() } else { t }
}

/// Canonical item id (lowercase hyphenated UUID), `None` for anything else.
pub fn parse_id(s: &str) -> Option<String> {
    uuid::Uuid::parse_str(s.trim())
        .ok()
        .map(|u| u.hyphenated().to_string())
}

/// `GET /api/v1/system` → `vod`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct VodStatus {
    pub enabled: bool,
    /// Enabled and ffmpeg/ffprobe found.
    pub available: bool,
    pub inbox_dir: String,
    pub jobs_running: usize,
    pub queued: u64,
    pub max_parallel_jobs: usize,
    pub encoder: String,
}

/// What [`VodManager::import`] needs besides the file.
#[derive(Debug, Clone)]
pub struct ImportRequest {
    pub original_name: String,
    pub ext: String,
    pub title: String,
    pub description: String,
    pub ladder: String,
    pub origin: String,
    pub probe: ffmpeg::SourceInfo,
}

pub struct VodManager {
    pub config: Arc<Config>,
    db: Db,
    events: broadcast::Sender<HubEvent>,
    ffmpeg: Option<PathBuf>,
    ffprobe: Option<PathBuf>,
    /// Wakes the job runner (new item queued, job finished).
    wake: Notify,
    /// Running jobs: item id → cancellation token.
    jobs: Mutex<HashMap<String, CancellationToken>>,
}

impl VodManager {
    pub fn new(
        config: Arc<Config>,
        db: Db,
        events: broadcast::Sender<HubEvent>,
        ffmpeg: Option<PathBuf>,
    ) -> Arc<Self> {
        let ffprobe = ffmpeg
            .as_deref()
            .and_then(crate::recorder::remux::ffprobe_for);
        Arc::new(Self {
            config,
            db,
            events,
            ffmpeg,
            ffprobe,
            wake: Notify::new(),
            jobs: Mutex::new(HashMap::new()),
        })
    }

    pub fn enabled(&self) -> bool {
        self.config.vod.enabled
    }

    /// Enabled, with ffmpeg and ffprobe.
    pub fn available(&self) -> bool {
        self.enabled() && self.ffmpeg.is_some() && self.ffprobe.is_some()
    }

    pub fn ffmpeg(&self) -> Option<&Path> {
        self.ffmpeg.as_deref()
    }

    pub fn ffprobe(&self) -> Option<&Path> {
        self.ffprobe.as_deref()
    }

    pub fn db(&self) -> &Db {
        &self.db
    }

    /// `data/vod`.
    pub fn dir(&self) -> PathBuf {
        self.config.vod_dir()
    }

    pub fn item_dir(&self, id: &str) -> PathBuf {
        self.dir().join(id)
    }

    pub fn abr_dir(&self, id: &str) -> PathBuf {
        self.item_dir(id).join("abr")
    }

    pub fn poster_path(&self, id: &str) -> PathBuf {
        self.item_dir(id).join("poster.jpg")
    }

    /// Absolute path of the source file (`None` for an unsafe stored path).
    pub fn source_file(&self, item: &VodItem) -> Option<PathBuf> {
        let rel = Path::new(&item.source_path);
        let safe = !item.source_path.is_empty()
            && rel.components().all(|c| matches!(c, Component::Normal(_)));
        safe.then(|| self.dir().join(rel))
    }

    /// Emit `HubEvent::VodChanged` (admin SSE `vod` / `vod_deleted`).
    pub fn changed(&self, id: &str) {
        let _ = self
            .events
            .send(HubEvent::VodChanged { id: id.to_string() });
    }

    /// Wake the job runner (an item was queued).
    pub fn wake(&self) {
        self.wake.notify_one();
    }

    fn lock_jobs(&self) -> std::sync::MutexGuard<'_, HashMap<String, CancellationToken>> {
        self.jobs.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn is_running(&self, id: &str) -> bool {
        self.lock_jobs().contains_key(id)
    }

    pub fn jobs_running(&self) -> usize {
        self.lock_jobs().len()
    }

    /// Cancel the running job of `id` (forced delete). Returns false when none runs.
    pub fn cancel(&self, id: &str) -> bool {
        match self.lock_jobs().get(id) {
            Some(t) => {
                t.cancel();
                true
            }
            None => false,
        }
    }

    /// Shutdown: the jobs see the cancelled shutdown token (their tokens are children of
    /// it); give them up to `timeout` to kill ffmpeg, clean up and put their item back
    /// in the queue (startup re-queues anything still `processing` anyway).
    pub async fn shutdown(&self, timeout: Duration) {
        let deadline = tokio::time::Instant::now() + timeout;
        while self.jobs_running() > 0 {
            if tokio::time::Instant::now() >= deadline {
                tracing::warn!(jobs = self.jobs_running(), "VOD jobs did not stop in time");
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Wait (up to `timeout`) until the job of `id` has stopped.
    pub async fn wait_stopped(&self, id: &str, timeout: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        while self.is_running(id) {
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        true
    }

    pub async fn status(&self) -> VodStatus {
        let queued = match repo::pending_counts(&self.db).await {
            Ok((q, _)) => q,
            Err(e) => {
                tracing::warn!(error = %e, "cannot count queued videos");
                0
            }
        };
        VodStatus {
            enabled: self.enabled(),
            available: self.available(),
            inbox_dir: self.config.vod_inbox_dir().display().to_string(),
            jobs_running: self.jobs_running(),
            queued,
            max_parallel_jobs: self.config.vod.max_parallel_jobs,
            encoder: self.config.vod.encoder.clone(),
        }
    }

    /// Create a `queued` item from `file` (an upload temp file or an inbox file), moved
    /// to `data/vod/<id>/source.<ext>`. On error nothing is left under `data/vod/`; the
    /// file itself stays where it was unless the move happened.
    pub async fn import(&self, file: &Path, req: ImportRequest) -> Result<VodItem, String> {
        let id = uuid::Uuid::new_v4().hyphenated().to_string();
        let dir = self.item_dir(&id);
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        let rel = format!("{id}/source.{}", req.ext);
        let dest = self.dir().join(&rel);
        if let Err(e) = inbox::move_file(file, &dest).await {
            let _ = tokio::fs::remove_dir_all(&dir).await;
            return Err(format!("cannot move the file into {}: {e}", dir.display()));
        }
        let size = tokio::fs::metadata(&dest)
            .await
            .map(|m| m.len())
            .unwrap_or(0);
        let mut item = VodItem::new(&id, &req.title, &rel, &req.ladder, &req.origin);
        item.description = req.description;
        item.source_name = req.original_name;
        item.source_size = size;
        item.source_duration_secs = req.probe.duration_secs;
        if let Some(v) = &req.probe.video {
            item.source_width = Some(v.width);
            item.source_height = Some(v.height);
            item.source_codec = Some(v.codec.clone());
        }
        if let Err(e) = repo::insert(&self.db, &item).await {
            // Give the file back (inbox) / drop it (upload temp) and clean up.
            let _ = inbox::move_file(&dest, file).await;
            let _ = tokio::fs::remove_dir_all(&dir).await;
            return Err(format!("database: {e}"));
        }
        tracing::info!(id = %id, title = %item.title, ladder = %item.ladder, origin = %item.origin, bytes = size, "video on demand queued");
        self.changed(&id);
        self.wake();
        Ok(item)
    }

    /// Remove the transcoded output (abr, abr.tmp, poster), e.g. before a re-transcode.
    pub async fn clear_output(&self, id: &str) {
        let dir = self.item_dir(id);
        for sub in ["abr", "abr.tmp"] {
            let p = dir.join(sub);
            if p.exists()
                && let Err(e) = tokio::fs::remove_dir_all(&p).await
            {
                tracing::warn!(path = %p.display(), error = %e, "cannot remove VOD output");
            }
        }
        let _ = tokio::fs::remove_file(self.poster_path(id)).await;
    }

    /// Remove `data/vod/<id>/` entirely.
    pub async fn remove_files(&self, id: &str) {
        let dir = self.item_dir(id);
        if dir.exists()
            && let Err(e) = tokio::fs::remove_dir_all(&dir).await
        {
            tracing::warn!(path = %dir.display(), error = %e, "cannot remove VOD directory");
        }
    }

    /// Startup: create the folders, re-queue interrupted jobs, remove orphan
    /// directories; then the job runner and the inbox scanner (when available).
    pub fn spawn(self: &Arc<Self>, shutdown: CancellationToken) {
        let mgr = Arc::clone(self);
        tokio::spawn(async move {
            mgr.startup().await;
            if !mgr.enabled() {
                tracing::info!("video on demand disabled (vod.enabled = false)");
                return;
            }
            if !mgr.available() {
                tracing::warn!("ffmpeg/ffprobe not found: video on demand transcoding disabled");
                return;
            }
            tokio::spawn(inbox::run(Arc::clone(&mgr), shutdown.clone()));
            job::runner(mgr, shutdown).await;
        });
    }

    async fn startup(&self) {
        if let Err(e) = tokio::fs::create_dir_all(self.dir()).await {
            tracing::warn!(dir = %self.dir().display(), error = %e, "cannot create the VOD directory");
        }
        match repo::requeue_interrupted(&self.db).await {
            Ok(0) => {}
            Ok(n) => tracing::warn!(
                count = n,
                "re-queued videos left processing by a previous run"
            ),
            Err(e) => tracing::warn!(error = %e, "cannot re-queue interrupted videos"),
        }
        if let Err(e) = self.sweep_orphans().await {
            tracing::warn!(error = %e, "VOD orphan sweep failed");
        }
    }

    /// Remove `data/vod/<uuid>/` directories without a database row (crash between the
    /// move and the insert) older than an hour.
    async fn sweep_orphans(&self) -> Result<(), DbError> {
        let ids: std::collections::HashSet<String> =
            repo::all_ids(&self.db).await?.into_iter().collect();
        let Ok(mut rd) = tokio::fs::read_dir(self.dir()).await else {
            return Ok(());
        };
        while let Ok(Some(entry)) = rd.next_entry().await {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(id) = parse_id(&name).filter(|id| *id == name) else {
                continue;
            };
            if ids.contains(&id) {
                continue;
            }
            let old = entry
                .metadata()
                .await
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age > Duration::from_secs(3600));
            if old {
                tracing::warn!(dir = %entry.path().display(), "removing orphan VOD directory");
                let _ = tokio::fs::remove_dir_all(entry.path()).await;
            }
        }
        Ok(())
    }

    fn register_job(&self, id: &str, token: CancellationToken) {
        self.lock_jobs().insert(id.to_string(), token);
    }

    fn unregister_job(&self, id: &str) {
        self.lock_jobs().remove(id);
        self.wake.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions_titles_ids() {
        assert_eq!(vod_extension(Some("a.MXF"), None), Some("mxf"));
        assert_eq!(vod_extension(Some("clip.mts"), None), Some("mts"));
        assert_eq!(vod_extension(Some("x.mpeg"), None), Some("mpeg"));
        assert_eq!(vod_extension(Some("x.wmv"), None), Some("wmv"));
        assert_eq!(vod_extension(Some("blob"), Some("video/mp2t")), Some("ts"));
        assert_eq!(
            vod_extension(Some("blob"), Some("video/x-flv")),
            Some("flv")
        );
        assert_eq!(
            vod_extension(Some("doc.pdf"), Some("application/pdf")),
            None
        );
        assert_eq!(vod_extension(None, None), None);

        assert_eq!(
            title_from_file_name("Il_mio_video_2026.mp4"),
            "Il mio video 2026"
        );
        assert_eq!(
            title_from_file_name("dir/My__clip.final.mov"),
            "My clip.final"
        );
        assert_eq!(title_from_file_name("___.mp4"), "Video");
        assert_eq!(title_from_file_name("noext"), "noext");

        let id = uuid::Uuid::new_v4();
        assert_eq!(parse_id(&id.to_string()), Some(id.hyphenated().to_string()));
        assert_eq!(
            parse_id(&id.to_string().to_uppercase()),
            Some(id.hyphenated().to_string())
        );
        assert_eq!(parse_id("../etc"), None);
        assert_eq!(parse_id("inbox"), None);
    }
}
