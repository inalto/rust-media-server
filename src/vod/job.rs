//! Transcoding jobs: the runner task and one job.
//!
//! The runner wakes up when an item is queued (or a job ends, or every 15 s for items
//! queued by another instance sharing the database), takes the oldest `queued` items
//! while a slot of `vod.max_parallel_jobs` is free and claims each one atomically
//! (`UPDATE … SET status='processing' WHERE id=? AND status='queued'`, one row or
//! nothing). A job: probe → renditions → poster → one ffmpeg pass with progress (≥ 1 %
//! steps, at most every 0.5 s) → playlist/MPD post-processing → swap
//! `abr.tmp/` into `abr/` → `ready`. Failures keep the source and set `failed` with the
//! error (tail of ffmpeg's stderr); a re-transcode re-queues the item. A forced delete
//! cancels the job (ffmpeg killed); on shutdown the item goes back to `queued`.

use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::FutureExt;
use tokio::sync::{Semaphore, watch};
use tokio_util::sync::CancellationToken;

use super::VodManager;
use super::ffmpeg::{self, RunError, TranscodeSpec};
use super::hls;
use crate::db::vod as repo;

/// Safety poll of the queue (items queued by another process).
const POLL: Duration = Duration::from_secs(15);
/// Minimum transcode timeout.
const MIN_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// Progress events (DB write + SSE): only for a step of at least 1 % (the stored value is
/// an integer percentage) and at most one per `PROGRESS_MIN_INTERVAL` (short videos
/// encode several percent per second).
const PROGRESS_MIN_INTERVAL: Duration = Duration::from_millis(500);

/// Timeout of a transcode: max(30 min, 10 × duration).
pub fn timeout_for(duration_secs: Option<f64>) -> Duration {
    let d = duration_secs
        .filter(|d| d.is_finite() && *d > 0.0)
        .unwrap_or(0.0);
    MIN_TIMEOUT.max(Duration::from_secs_f64((d * 10.0).min(7.0 * 86400.0)))
}

/// The job runner (one per process).
pub async fn runner(mgr: Arc<VodManager>, shutdown: CancellationToken) {
    let slots = Arc::new(Semaphore::new(mgr.config.vod.max_parallel_jobs.max(1)));
    tracing::info!(
        task = "vod-runner",
        slots = mgr.config.vod.max_parallel_jobs,
        "started"
    );
    loop {
        while let Ok(permit) = Arc::clone(&slots).try_acquire_owned() {
            let id = match next(&mgr).await {
                Ok(Some(id)) => id,
                Ok(None) => break,
                Err(e) => {
                    tracing::warn!(task = "vod-runner", error = %e, "cannot read the queue");
                    break;
                }
            };
            let token = shutdown.child_token();
            mgr.register_job(&id, token.clone());
            let mgr2 = Arc::clone(&mgr);
            tokio::spawn(async move {
                let _permit = permit;
                let fut = AssertUnwindSafe(run(&mgr2, &id, &token)).catch_unwind();
                if fut.await.is_err() {
                    tracing::error!(id = %id, "VOD job panicked");
                    let _ =
                        repo::mark_failed(mgr2.db(), &id, "internal error (job panicked)").await;
                    mgr2.changed(&id);
                }
                mgr2.unregister_job(&id);
            });
        }
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = mgr.wake.notified() => {}
            _ = tokio::time::sleep(POLL) => {}
        }
    }
    tracing::info!(task = "vod-runner", "stopped");
}

/// Claim the oldest `queued` item not already running here.
async fn next(mgr: &VodManager) -> Result<Option<String>, crate::db::DbError> {
    for id in repo::queued_ids(mgr.db(), 16).await? {
        if mgr.is_running(&id) {
            continue;
        }
        if repo::claim(mgr.db(), &id).await? {
            return Ok(Some(id));
        }
    }
    Ok(None)
}

enum JobError {
    Cancelled,
    Failed(String),
}

impl From<String> for JobError {
    fn from(s: String) -> Self {
        JobError::Failed(s)
    }
}

/// Run one claimed job to completion (never returns an error: the outcome is in the DB).
pub async fn run(mgr: &VodManager, id: &str, cancel: &CancellationToken) {
    let started = Instant::now();
    tracing::info!(id = %id, "VOD transcode started");
    match process(mgr, id, cancel).await {
        Ok(()) => {
            tracing::info!(id = %id, secs = format!("{:.1}", started.elapsed().as_secs_f64()), "VOD transcode finished");
        }
        Err(JobError::Cancelled) => {
            let _ = tokio::fs::remove_dir_all(mgr.item_dir(id).join("abr.tmp")).await;
            // Shutdown: back to the queue. Forced delete: the row is gone (no-op).
            if matches!(repo::unclaim(mgr.db(), id).await, Ok(true)) {
                tracing::info!(id = %id, "VOD transcode interrupted; re-queued");
            } else {
                tracing::info!(id = %id, "VOD transcode cancelled");
            }
            mgr.changed(id);
        }
        Err(JobError::Failed(msg)) => {
            tracing::warn!(id = %id, error = %msg, "VOD transcode failed");
            let _ = tokio::fs::remove_dir_all(mgr.item_dir(id).join("abr.tmp")).await;
            if let Err(e) = repo::mark_failed(mgr.db(), id, &msg).await {
                tracing::warn!(id = %id, error = %e, "cannot mark the video as failed");
            }
            mgr.changed(id);
        }
    }
}

async fn process(mgr: &VodManager, id: &str, cancel: &CancellationToken) -> Result<(), JobError> {
    let (Some(ffmpeg_bin), Some(ffprobe_bin)) = (mgr.ffmpeg(), mgr.ffprobe()) else {
        return Err("ffmpeg/ffprobe not available".to_string().into());
    };
    let db = mgr.db();
    let Some(item) = repo::get(db, id)
        .await
        .map_err(|e| format!("database: {e}"))?
    else {
        return Err(JobError::Cancelled); // deleted meanwhile
    };
    let ladder = mgr
        .config
        .vod
        .ladder(&item.ladder)
        .ok_or_else(|| {
            format!(
                "unknown ladder \"{}\" (not in the configuration)",
                item.ladder
            )
        })?
        .clone();
    let source = mgr
        .source_file(&item)
        .filter(|p| p.is_file())
        .ok_or_else(|| "source file not found".to_string())?;

    // Probe (fps, channels and rotation are not stored).
    let info = ffmpeg::probe(ffprobe_bin, &source).await?;
    let video = info
        .video
        .clone()
        .ok_or_else(|| "the file has no video stream".to_string())?;
    let _ = repo::set_source_info(
        db,
        id,
        info.duration_secs,
        Some(video.width),
        Some(video.height),
        Some(video.codec.clone()),
    )
    .await;
    let renditions = ffmpeg::select_renditions(&ladder.renditions, video.width, video.height);
    if renditions.is_empty() {
        return Err("the ladder has no renditions".to_string().into());
    }

    let item_dir = mgr.item_dir(id);
    let tmp = item_dir.join("abr.tmp");
    if tmp.exists() {
        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }
    tokio::fs::create_dir_all(&tmp)
        .await
        .map_err(|e| format!("cannot create {}: {e}", tmp.display()))?;

    // Poster first (fast): the dashboard shows it while the transcode runs.
    make_poster(
        mgr,
        id,
        ffmpeg_bin,
        &source,
        &video,
        info.duration_secs,
        cancel,
    )
    .await?;
    mgr.changed(id);

    let spec = TranscodeSpec {
        source: source.clone(),
        out_dir: tmp.clone(),
        video_index: video.index,
        audio_index: info.audio.as_ref().map(|a| a.index),
        channels: info.audio.as_ref().map(|a| a.channels).unwrap_or(2),
        fps: video.fps,
        renditions: renditions.clone(),
        segment_duration: mgr.config.vod.segment_duration,
        encoder: mgr.config.vod.encoder.clone(),
        preset: mgr.config.vod.preset.clone(),
        encoder_args: mgr.config.vod.encoder_args.clone(),
        hwaccel_args: mgr.config.vod.hwaccel_args.clone(),
    };
    let args = ffmpeg::transcode_args(&spec);
    tracing::debug!(id = %id, args = ?args, "ffmpeg");
    let timeout = timeout_for(info.duration_secs);
    let (tx, rx) = watch::channel(0u64);
    let transcode = async {
        let tx = tx;
        let r = ffmpeg::run(ffmpeg_bin, &args, timeout, cancel, Some(&tx)).await;
        drop(tx);
        r
    };
    let (result, ()) = tokio::join!(transcode, report_progress(mgr, id, info.duration_secs, rx));
    match result {
        Ok(()) => {}
        Err(RunError::Cancelled) => return Err(JobError::Cancelled),
        Err(RunError::Timeout) => {
            return Err(
                format!("transcoding timed out after {} min", timeout.as_secs() / 60).into(),
            );
        }
        Err(e) => return Err(e.to_string().into()),
    }
    if cancel.is_cancelled() {
        return Err(JobError::Cancelled);
    }

    // Playlists, MPD, measured bandwidth.
    let version = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let has_audio = info.audio.is_some();
    let fps = video.fps;
    let (dir, sel) = (tmp.clone(), renditions.clone());
    let produced =
        tokio::task::spawn_blocking(move || hls::finalize(&dir, &sel, has_audio, fps, &version))
            .await
            .map_err(|e| format!("post-processing task failed: {e}"))??;

    // Swap the new output in.
    let abr = item_dir.join("abr");
    if abr.exists() {
        tokio::fs::remove_dir_all(&abr)
            .await
            .map_err(|e| format!("cannot replace the previous output: {e}"))?;
    }
    tokio::fs::rename(&tmp, &abr)
        .await
        .map_err(|e| format!("cannot move the output into place: {e}"))?;
    match repo::mark_ready(db, id, &produced).await {
        Ok(true) => {}
        Ok(false) => {
            // Deleted while finishing: nothing references the files any more.
            if repo::get(db, id).await.ok().flatten().is_none() {
                mgr.remove_files(id).await;
            }
            return Err(JobError::Cancelled);
        }
        Err(e) => return Err(format!("database: {e}").into()),
    }
    if !mgr.config.vod.keep_source {
        match tokio::fs::remove_file(&source).await {
            Ok(()) => tracing::info!(id = %id, "source removed (vod.keep_source = false)"),
            Err(e) => tracing::warn!(id = %id, error = %e, "cannot remove the source"),
        }
    }
    mgr.changed(id);
    Ok(())
}

/// Poster at 10 % of the duration (1 s when unknown; 0 s as a fallback), at most
/// 1280 px wide. A failed poster is not fatal.
async fn make_poster(
    mgr: &VodManager,
    id: &str,
    ffmpeg_bin: &Path,
    source: &Path,
    video: &ffmpeg::VideoInfo,
    duration: Option<f64>,
    cancel: &CancellationToken,
) -> Result<(), JobError> {
    let poster = mgr.poster_path(id);
    let tmp = poster.with_file_name("poster.tmp.jpg");
    let size = ffmpeg::poster_size(video.width, video.height);
    let first = duration.map(|d| d * 0.1).unwrap_or(1.0);
    for seek in [first, 0.0] {
        let args = ffmpeg::poster_args(source, video.index, seek, size, &tmp);
        match ffmpeg::run(ffmpeg_bin, &args, ffmpeg::POSTER_TIMEOUT, cancel, None).await {
            Err(RunError::Cancelled) => return Err(JobError::Cancelled),
            Ok(()) if tokio::fs::metadata(&tmp).await.is_ok_and(|m| m.len() > 0) => {
                if tokio::fs::rename(&tmp, &poster).await.is_ok() {
                    return Ok(());
                }
            }
            Ok(()) => {}
            Err(e) => tracing::debug!(error = %e, seek, "poster attempt failed"),
        }
    }
    let _ = tokio::fs::remove_file(&tmp).await;
    tracing::warn!(source = %source.display(), "could not extract a poster frame");
    Ok(())
}

/// Throttled progress updates (DB + SSE) until the sender is dropped.
async fn report_progress(
    mgr: &VodManager,
    id: &str,
    duration: Option<f64>,
    mut rx: watch::Receiver<u64>,
) {
    let mut last_pct = 0u32;
    let mut last_at = Instant::now();
    while rx.changed().await.is_ok() {
        let pct = ffmpeg::progress_percent(*rx.borrow_and_update(), duration);
        if pct <= last_pct || last_at.elapsed() < PROGRESS_MIN_INTERVAL {
            continue;
        }
        last_pct = pct;
        last_at = Instant::now();
        match repo::set_progress(mgr.db(), id, pct).await {
            Ok(true) => mgr.changed(id),
            Ok(false) => {}
            Err(e) => tracing::warn!(id = %id, error = %e, "cannot store VOD progress"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeouts() {
        assert_eq!(timeout_for(None), MIN_TIMEOUT);
        assert_eq!(timeout_for(Some(30.0)), MIN_TIMEOUT);
        assert_eq!(timeout_for(Some(3600.0)), Duration::from_secs(36_000));
        assert_eq!(timeout_for(Some(f64::NAN)), MIN_TIMEOUT);
    }
}
