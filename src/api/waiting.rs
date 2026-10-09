//! Waiting videos (ARCHITECTURE.md §8.1): per-broadcast upload/removal, plus the
//! upload pipeline shared with the global default (`api/settings.rs`).
//!
//! Pipeline: the multipart `file` field is streamed to a temp file under
//! `data/upload-tmp/` (outside the publicly served `data/waiting/` tree; never buffered
//! in memory, bounded by `server.max_upload_mb`),
//! then:
//! - with ffmpeg: normalized to H.264/AAC MP4 (`+faststart`, max 1920 px wide) and a
//!   poster frame is extracted (`<video stem>.jpg`, see [`super::poster_path_for`]);
//! - without ffmpeg: only MP4 files are accepted and stored as-is (no poster).
//!
//! Videos are stored as `data/waiting/<broadcast-name>/<uuid>.mp4` (global default:
//! `data/waiting/_default/<uuid>.mp4`); the database keeps the path relative to
//! `data/waiting/`. Replacing or deleting a video removes the old file and poster.

use std::path::{Path as FsPath, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime};

use axum::extract::multipart::{Field, MultipartRejection};
use axum::extract::{DefaultBodyLimit, Multipart, Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Semaphore;

use super::broadcasts::{load, remove_waiting_file};
use super::{BaseUrl, BroadcastView, ViewContext, poster_path_for, waiting_url};
use crate::config::Config;
use crate::db::{self, DbError};
use crate::error::ApiError;
use crate::hub::HubEvent;
use crate::state::AppState;

pub fn routes(state: &AppState) -> Router<AppState> {
    Router::new().route(
        "/broadcasts/{name}/waiting-video",
        post(upload_waiting_video)
            .delete(delete_waiting_video)
            .layer(DefaultBodyLimit::max(upload_body_limit(&state.config))),
    )
}

/// Container extensions accepted for upload (normalized to MP4 when ffmpeg is available).
pub const ACCEPTED_EXTENSIONS: &[&str] = &["mp4", "mov", "m4v", "webm", "mkv", "avi"];

/// MIME types accepted when the file name has no usable extension.
const MIME_EXTENSIONS: &[(&str, &str)] = &[
    ("video/mp4", "mp4"),
    ("video/quicktime", "mov"),
    ("video/x-m4v", "m4v"),
    ("video/webm", "webm"),
    ("video/x-matroska", "mkv"),
    ("video/matroska", "mkv"),
    ("video/x-msvideo", "avi"),
    ("video/msvideo", "avi"),
    ("video/avi", "avi"),
];

/// Sub-directory of `data/waiting/` holding the global default waiting video.
pub const DEFAULT_SUBDIR: &str = "_default";
/// Sub-directory of the data dir for in-flight uploads (`.upload-*` files). Kept outside
/// `data/waiting/`, which is served publicly under `/waiting/*`.
pub const TMP_SUBDIR: &str = "upload-tmp";
const TMP_PREFIX: &str = ".upload-";
/// Leftover temp files older than this are removed on the next upload.
const TMP_MAX_AGE: Duration = Duration::from_secs(24 * 3600);
/// ffmpeg normalization timeout.
const NORMALIZE_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const POSTER_TIMEOUT: Duration = Duration::from_secs(60);
/// Keep only the tail of ffmpeg's stderr (error message for the API response/log).
const STDERR_TAIL: usize = 4096;
const MIB: u64 = 1024 * 1024;
/// Allowance for the multipart framing around the file itself.
const MULTIPART_SLACK: u64 = 64 * 1024;

/// At most two concurrent ffmpeg normalizations (CPU bound).
static TRANSCODE_SLOTS: Semaphore = Semaphore::const_new(2);

/// Request body limit of the upload routes: `server.max_upload_mb` MiB + multipart framing.
pub fn upload_body_limit(config: &Config) -> usize {
    let bytes = config
        .server
        .max_upload_mb
        .saturating_mul(MIB)
        .saturating_add(MULTIPART_SLACK);
    usize::try_from(bytes).unwrap_or(usize::MAX)
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Upload handler error: an [`ApiError`] or one of the statuses `ApiError` lacks.
#[derive(Debug)]
pub enum UploadError {
    Api(ApiError),
    /// 415: the file type is not accepted (or ffmpeg is missing and it is not an MP4).
    UnsupportedMediaType(String),
    /// 422: ffmpeg could not decode/convert the file.
    Unprocessable(String),
}

impl From<ApiError> for UploadError {
    fn from(e: ApiError) -> Self {
        UploadError::Api(e)
    }
}

impl From<DbError> for UploadError {
    fn from(e: DbError) -> Self {
        UploadError::Api(e.into())
    }
}

impl IntoResponse for UploadError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            UploadError::Api(e) => return e.into_response(),
            UploadError::UnsupportedMediaType(m) => (StatusCode::UNSUPPORTED_MEDIA_TYPE, m),
            UploadError::Unprocessable(m) => (StatusCode::UNPROCESSABLE_ENTITY, m),
        };
        let mut resp = (status, Json(json!({ "error": message }))).into_response();
        resp.headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        resp
    }
}

fn io_error(context: &str, e: std::io::Error) -> UploadError {
    UploadError::Api(ApiError::internal(format!("{context}: {e}")))
}

pub(crate) fn multipart_error(e: axum::extract::multipart::MultipartError) -> UploadError {
    if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
        UploadError::Api(ApiError::PayloadTooLarge(
            "file is larger than the upload limit".into(),
        ))
    } else {
        UploadError::Api(ApiError::bad_request(format!(
            "invalid multipart body: {}",
            e.body_text()
        )))
    }
}

pub(crate) fn multipart_rejection(e: MultipartRejection) -> UploadError {
    UploadError::Api(ApiError::bad_request(e.body_text()))
}

// ---------------------------------------------------------------------------
// Validation and paths
// ---------------------------------------------------------------------------

/// Container type of an upload: the file name extension when it is an accepted one,
/// otherwise the MIME type. `None` when neither identifies an accepted video.
pub fn upload_extension(
    file_name: Option<&str>,
    content_type: Option<&str>,
) -> Option<&'static str> {
    let from_name = file_name
        .and_then(|n| n.rsplit_once('.'))
        .map(|(_, ext)| ext.trim().to_ascii_lowercase())
        .and_then(|ext| ACCEPTED_EXTENSIONS.iter().copied().find(|e| *e == ext));
    if from_name.is_some() {
        return from_name;
    }
    let mime = content_type?
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    MIME_EXTENSIONS
        .iter()
        .find(|(m, _)| *m == mime)
        .map(|(_, e)| *e)
}

/// Check an upload's type: any accepted video with ffmpeg, only MP4 without.
pub fn check_upload(
    file_name: Option<&str>,
    content_type: Option<&str>,
    ffmpeg_available: bool,
) -> Result<&'static str, UploadError> {
    match upload_extension(file_name, content_type) {
        None => Err(UploadError::UnsupportedMediaType(format!(
            "unsupported file type: upload a video ({})",
            ACCEPTED_EXTENSIONS.join(", ")
        ))),
        Some(ext) if !ffmpeg_available && ext != "mp4" => Err(UploadError::UnsupportedMediaType(
            "ffmpeg not available: upload an MP4 (H.264/AAC)".into(),
        )),
        Some(ext) => Ok(ext),
    }
}

/// Relative path (under `data/waiting/`) of a stored waiting video: `<subdir>/<id>.mp4`.
pub fn video_rel_path(subdir: &str, id: &str) -> String {
    format!("{subdir}/{id}.mp4")
}

/// True when `rel` is a plain relative path (no `..`, no root, not empty).
pub fn is_safe_rel_path(rel: &str) -> bool {
    let p = FsPath::new(rel);
    !rel.is_empty()
        && p.components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
}

/// Public URL of the poster of `video_rel`, if the poster file exists.
pub fn poster_url_if_exists(config: &Config, base: &BaseUrl, video_rel: &str) -> Option<String> {
    let poster = poster_path_for(video_rel);
    (is_safe_rel_path(&poster) && config.waiting_dir().join(&poster).is_file())
        .then(|| waiting_url(base, &poster))
}

/// Remove a waiting video and its poster (best effort, logged).
pub async fn remove_video_files(state: &AppState, rel: &str) {
    remove_waiting_file(state, rel).await;
    remove_waiting_file(state, &poster_path_for(rel)).await;
}

// ---------------------------------------------------------------------------
// Upload pipeline
// ---------------------------------------------------------------------------

/// Temp file removed on drop unless [`TempFile::disarm`]ed.
pub(crate) struct TempFile {
    pub(crate) path: PathBuf,
    armed: bool,
}

impl TempFile {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }
    pub(crate) fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Remove `.upload-*` leftovers (crash, killed connection) older than a day.
async fn sweep_stale_temp_files(dir: &FsPath) {
    let Ok(mut rd) = tokio::fs::read_dir(dir).await else {
        return;
    };
    while let Ok(Some(entry)) = rd.next_entry().await {
        if !entry.file_name().to_string_lossy().starts_with(TMP_PREFIX) {
            continue;
        }
        let old = entry
            .metadata()
            .await
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| SystemTime::now().duration_since(t).ok())
            .is_some_and(|age| age > TMP_MAX_AGE);
        if old {
            let _ = tokio::fs::remove_file(entry.path()).await;
        }
    }
}

/// `data/upload-tmp/` (created; leftovers older than a day removed): where uploads are
/// streamed before being moved into place (waiting videos, video on demand).
pub(crate) async fn upload_tmp_dir(state: &AppState) -> Result<PathBuf, UploadError> {
    let tmp_dir = state.config.server.data_dir.join(TMP_SUBDIR);
    tokio::fs::create_dir_all(&tmp_dir)
        .await
        .map_err(|e| io_error("creating upload directory", e))?;
    sweep_stale_temp_files(&tmp_dir).await;
    Ok(tmp_dir)
}

/// Stream a multipart file field to `<tmp_dir>/.upload-<uuid>.<ext>` (never buffered in
/// memory). 413 over `max_upload_mb` MiB, 400 for an empty file; the temp file is
/// removed on any error (and when the returned guard is dropped armed).
pub(crate) async fn save_field(
    field: &mut Field<'_>,
    tmp_dir: &FsPath,
    ext: &str,
    max_upload_mb: u64,
) -> Result<(TempFile, u64), UploadError> {
    let limit = max_upload_mb.saturating_mul(MIB);
    let tmp = TempFile::new(tmp_dir.join(format!(
        "{TMP_PREFIX}{}.{ext}",
        uuid::Uuid::new_v4().simple()
    )));
    let mut out = tokio::fs::File::create(&tmp.path)
        .await
        .map_err(|e| io_error("creating temp file", e))?;
    let mut written: u64 = 0;
    while let Some(chunk) = field.chunk().await.map_err(multipart_error)? {
        written += chunk.len() as u64;
        if written > limit {
            return Err(UploadError::Api(ApiError::PayloadTooLarge(format!(
                "file is larger than the upload limit ({max_upload_mb} MB)"
            ))));
        }
        out.write_all(&chunk)
            .await
            .map_err(|e| io_error("writing temp file", e))?;
    }
    out.flush()
        .await
        .map_err(|e| io_error("writing temp file", e))?;
    drop(out);
    if written == 0 {
        return Err(UploadError::Api(ApiError::bad_request(
            "the uploaded file is empty",
        )));
    }
    Ok((tmp, written))
}

/// Receive the multipart `file` field and store it as a waiting video under
/// `data/waiting/<subdir>/`. Returns the new path relative to `data/waiting/`.
pub(crate) async fn receive_waiting_video(
    state: &AppState,
    mut multipart: Multipart,
    subdir: &str,
) -> Result<String, UploadError> {
    let waiting_dir = state.config.waiting_dir();
    let tmp_dir = upload_tmp_dir(state).await?;

    while let Some(mut field) = multipart.next_field().await.map_err(multipart_error)? {
        if field.name() != Some("file") {
            continue;
        }
        let ext = check_upload(
            field.file_name(),
            field.content_type(),
            state.ffmpeg.is_some(),
        )?;
        let original = field.file_name().unwrap_or("").to_string();
        let (mut tmp, written) =
            save_field(&mut field, &tmp_dir, ext, state.config.server.max_upload_mb).await?;
        tracing::info!(file = %original, bytes = written, "waiting video received");

        let dest_dir = waiting_dir.join(subdir);
        tokio::fs::create_dir_all(&dest_dir)
            .await
            .map_err(|e| io_error("creating waiting video directory", e))?;
        let rel = video_rel_path(subdir, &uuid::Uuid::new_v4().simple().to_string());
        let dest = waiting_dir.join(&rel);
        match &state.ffmpeg {
            Some(ffmpeg) => {
                let _permit = TRANSCODE_SLOTS
                    .acquire()
                    .await
                    .map_err(|e| UploadError::Api(ApiError::internal(e.to_string())))?;
                // The outputs are removed unless everything succeeds: on errors and
                // also when the client disconnects mid-way (this future is dropped,
                // ffmpeg is killed and a partial MP4 would otherwise stay orphaned
                // under data/waiting/).
                let mut dest_guard = TempFile::new(dest.clone());
                let poster = waiting_dir.join(poster_path_for(&rel));
                let mut poster_guard = TempFile::new(poster.clone());
                normalize(ffmpeg, &tmp.path, &dest).await?;
                if make_poster(ffmpeg, &dest, &poster).await {
                    poster_guard.disarm();
                } else {
                    tracing::warn!(video = %rel, "could not extract a poster frame");
                }
                dest_guard.disarm();
            }
            None => {
                check_mp4_signature(&tmp.path).await?;
                tokio::fs::rename(&tmp.path, &dest)
                    .await
                    .map_err(|e| io_error("storing waiting video", e))?;
                tmp.disarm();
            }
        }
        tracing::info!(video = %rel, normalized = state.ffmpeg.is_some(), "waiting video stored");
        return Ok(rel);
    }
    Err(UploadError::Api(ApiError::bad_request(
        "missing multipart field \"file\"",
    )))
}

/// Without ffmpeg the file is served as-is: require an ISO-BMFF (`ftyp`) header.
async fn check_mp4_signature(path: &FsPath) -> Result<(), UploadError> {
    let mut head = [0u8; 12];
    let mut f = tokio::fs::File::open(path)
        .await
        .map_err(|e| io_error("reading upload", e))?;
    let n = f
        .read(&mut head)
        .await
        .map_err(|e| io_error("reading upload", e))?;
    if n >= 8 && &head[4..8] == b"ftyp" {
        Ok(())
    } else {
        Err(UploadError::UnsupportedMediaType(
            "ffmpeg not available: upload an MP4 (H.264/AAC)".into(),
        ))
    }
}

/// Outcome of an ffmpeg run that did not succeed.
#[derive(Debug)]
enum FfmpegFailure {
    Spawn(std::io::Error),
    Timeout,
    Exit(String),
}

/// Run ffmpeg with `args` (stdin/stdout closed, stderr tail captured), killing it on
/// timeout or when the request is dropped.
async fn run_ffmpeg(
    ffmpeg: &FsPath,
    args: &[&str],
    timeout: Duration,
) -> Result<(), FfmpegFailure> {
    let mut child = tokio::process::Command::new(ffmpeg)
        .args(["-nostdin", "-hide_banner"])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(FfmpegFailure::Spawn)?;
    let mut stderr = child.stderr.take();
    let collect = async move {
        let mut tail: Vec<u8> = Vec::new();
        if let Some(err) = stderr.as_mut() {
            let mut buf = [0u8; 2048];
            while let Ok(n) = err.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                tail.extend_from_slice(&buf[..n]);
                if tail.len() > STDERR_TAIL {
                    tail.drain(..tail.len() - STDERR_TAIL);
                }
            }
        }
        tail
    };
    let run = async { tokio::join!(child.wait(), collect) };
    match tokio::time::timeout(timeout, run).await {
        Err(_) => Err(FfmpegFailure::Timeout),
        Ok((Err(e), _)) => Err(FfmpegFailure::Spawn(e)),
        Ok((Ok(status), _)) if status.success() => Ok(()),
        Ok((Ok(status), tail)) => {
            let text = String::from_utf8_lossy(&tail);
            let last = text
                .lines()
                .map(str::trim)
                .rfind(|l| !l.is_empty())
                .unwrap_or("")
                .to_string();
            Err(FfmpegFailure::Exit(if last.is_empty() {
                format!("ffmpeg exited with {status}")
            } else {
                last
            }))
        }
    }
}

/// Normalize any accepted video to H.264/AAC MP4 with `+faststart` (≤ 1920 px wide).
/// The width is rounded down to an even value (yuv420p/libx264 reject odd sizes) and
/// the height follows the aspect ratio (`-2`, also even).
async fn normalize(ffmpeg: &FsPath, input: &FsPath, output: &FsPath) -> Result<(), UploadError> {
    let (input, output) = (input.to_string_lossy(), output.to_string_lossy());
    let args = [
        "-y",
        "-v",
        "error",
        "-i",
        &input,
        "-map",
        "0:v:0",
        "-map",
        "0:a?",
        "-c:v",
        "libx264",
        "-preset",
        "veryfast",
        "-crf",
        "23",
        "-vf",
        "scale='trunc(min(1920,iw)/2)*2':-2",
        "-pix_fmt",
        "yuv420p",
        "-c:a",
        "aac",
        "-b:a",
        "128k",
        "-movflags",
        "+faststart",
        &output,
    ];
    match run_ffmpeg(ffmpeg, &args, NORMALIZE_TIMEOUT).await {
        Ok(()) => Ok(()),
        Err(FfmpegFailure::Spawn(e)) => Err(io_error("running ffmpeg", e)),
        Err(FfmpegFailure::Timeout) => Err(UploadError::Api(ApiError::unavailable(
            "video processing timed out",
        ))),
        Err(FfmpegFailure::Exit(msg)) => {
            tracing::warn!(error = %msg, "waiting video normalization failed");
            Err(UploadError::Unprocessable(format!(
                "could not process the video: {msg}"
            )))
        }
    }
}

/// Extract a poster frame at 1 s (0 s for shorter videos). Returns `true` on success.
async fn make_poster(ffmpeg: &FsPath, video: &FsPath, poster: &FsPath) -> bool {
    let (video_s, poster_s) = (video.to_string_lossy(), poster.to_string_lossy());
    for seek in ["1", "0"] {
        let args = [
            "-y",
            "-v",
            "error",
            "-ss",
            seek,
            "-i",
            &video_s,
            "-frames:v",
            "1",
            "-q:v",
            "3",
            &poster_s,
        ];
        let ok = run_ffmpeg(ffmpeg, &args, POSTER_TIMEOUT).await.is_ok();
        let non_empty = tokio::fs::metadata(poster)
            .await
            .is_ok_and(|m| m.is_file() && m.len() > 0);
        if ok && non_empty {
            return true;
        }
    }
    let _ = tokio::fs::remove_file(poster).await;
    false
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

fn changed(state: &AppState, name: &str) {
    state.hub.emit_shared(HubEvent::BroadcastUpdated {
        name: name.to_string(),
    });
}

/// `POST /broadcasts/{name}/waiting-video` (multipart `file`) → `BroadcastView`.
async fn upload_waiting_video(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    multipart: Result<Multipart, MultipartRejection>,
) -> Result<Json<BroadcastView>, UploadError> {
    let b = load(&state, &name).await?;
    let multipart = multipart.map_err(multipart_rejection)?;
    let rel = receive_waiting_video(&state, multipart, &b.name).await?;
    // Re-read: another request may have changed the video while this one was processed.
    let previous = db::broadcasts::find_by_name(&state.db, &b.name)
        .await?
        .and_then(|cur| cur.waiting_video);
    let Some(updated) =
        db::broadcasts::set_waiting_video(&state.db, &b.name, Some(rel.clone())).await?
    else {
        remove_video_files(&state, &rel).await;
        return Err(ApiError::broadcast_not_found().into());
    };
    if let Some(prev) = previous.filter(|p| *p != rel) {
        remove_video_files(&state, &prev).await;
    }
    tracing::info!(broadcast = %b.name, video = %rel, "waiting video updated");
    changed(&state, &b.name);
    let ctx = ViewContext::load(&state, &headers).await?;
    Ok(Json(ctx.view(&state, updated).await))
}

/// `DELETE /broadcasts/{name}/waiting-video` → `BroadcastView` (the global default
/// applies again).
async fn delete_waiting_video(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Result<Json<BroadcastView>, UploadError> {
    let b = load(&state, &name).await?;
    let updated = match b.waiting_video.clone() {
        Some(prev) => {
            let updated = db::broadcasts::set_waiting_video(&state.db, &b.name, None)
                .await?
                .ok_or_else(ApiError::broadcast_not_found)?;
            remove_video_files(&state, &prev).await;
            tracing::info!(broadcast = %b.name, "waiting video removed");
            changed(&state, &b.name);
            updated
        }
        None => b,
    };
    let ctx = ViewContext::load(&state, &headers).await?;
    Ok(Json(ctx.view(&state, updated).await))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::Request;
    use tokio_util::sync::CancellationToken;
    use tower::ServiceExt;

    use crate::db::Db;
    use crate::db::broadcasts::NewBroadcast;
    use crate::hub::Hub;
    use crate::metrics::ViewerTracker;
    use crate::packager::PackagerManager;
    use crate::recorder::RecorderManager;
    use crate::state::AppStateParts;

    pub const TOKEN: &str = "test-admin-token";

    #[test]
    fn extension_and_mime_validation() {
        assert_eq!(upload_extension(Some("clip.MP4"), None), Some("mp4"));
        assert_eq!(
            upload_extension(Some("a.b.mov"), Some("text/plain")),
            Some("mov")
        );
        assert_eq!(upload_extension(Some("x.webm"), None), Some("webm"));
        assert_eq!(upload_extension(Some("x.mkv"), None), Some("mkv"));
        assert_eq!(upload_extension(Some("x.avi"), None), Some("avi"));
        assert_eq!(upload_extension(Some("x.m4v"), None), Some("m4v"));
        assert_eq!(
            upload_extension(Some("noext"), Some("video/quicktime")),
            Some("mov")
        );
        assert_eq!(
            upload_extension(Some("blob"), Some("video/mp4; codecs=avc1")),
            Some("mp4")
        );
        assert_eq!(
            upload_extension(None, Some("video/x-matroska")),
            Some("mkv")
        );
        assert_eq!(upload_extension(Some("evil.exe"), None), None);
        assert_eq!(upload_extension(Some("img.jpg"), Some("image/jpeg")), None);
        assert_eq!(upload_extension(None, None), None);

        assert_eq!(check_upload(Some("a.mov"), None, true).ok(), Some("mov"));
        assert_eq!(check_upload(Some("a.mp4"), None, false).ok(), Some("mp4"));
        assert!(matches!(
            check_upload(Some("a.mov"), None, false),
            Err(UploadError::UnsupportedMediaType(m)) if m.contains("ffmpeg not available")
        ));
        assert!(matches!(
            check_upload(Some("a.txt"), Some("text/plain"), true),
            Err(UploadError::UnsupportedMediaType(_))
        ));
    }

    #[test]
    fn path_building() {
        assert_eq!(video_rel_path("show", "abc"), "show/abc.mp4");
        assert_eq!(video_rel_path(DEFAULT_SUBDIR, "abc"), "_default/abc.mp4");
        assert_eq!(
            poster_path_for(&video_rel_path("show", "abc")),
            "show/abc.jpg"
        );
        assert!(is_safe_rel_path("show/abc.mp4"));
        assert!(!is_safe_rel_path("../abc.mp4"));
        assert!(!is_safe_rel_path("/etc/passwd"));
        assert!(!is_safe_rel_path(""));
        let mut cfg = Config::default();
        cfg.server.max_upload_mb = 1;
        assert_eq!(upload_body_limit(&cfg), 1024 * 1024 + 64 * 1024);
        cfg.server.max_upload_mb = u64::MAX;
        assert_eq!(upload_body_limit(&cfg), usize::MAX);
    }

    /// App state over an in-memory DB and a temp data dir. `ffmpeg = false` forces
    /// the "ffmpeg unavailable" mode.
    pub async fn test_state(ffmpeg: bool) -> (AppState, PathBuf) {
        let dir = std::env::temp_dir().join(format!("rms-waiting-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("waiting")).unwrap();
        let mut config = Config::default();
        config.server.data_dir = dir.clone();
        config.server.admin_token = TOKEN.into();
        config.server.max_upload_mb = 8;
        if !ffmpeg {
            config.ffmpeg.path = "/nonexistent/ffmpeg".into();
        }
        let config = Arc::new(config);
        let db = Db::open_in_memory().unwrap();
        let hub = Hub::new();
        let tracker = ViewerTracker::new(&config, None, hub.events());
        let packager = PackagerManager::new(config.clone(), hub.clone(), tracker.clone());
        let recorder = RecorderManager::new(config.clone(), db.clone(), hub.clone());
        let webrtc =
            crate::webrtc::WebRtcManager::new(config.clone(), hub.clone(), tracker.clone());
        let state = AppState::new(AppStateParts {
            config,
            db,
            hub,
            tracker,
            packager,
            recorder,
            webrtc,
            admin_token: TOKEN.into(),
            shutdown: CancellationToken::new(),
        });
        (state, dir)
    }

    pub fn multipart_request(
        method: &str,
        uri: &str,
        file_name: &str,
        mime: &str,
        data: &[u8],
    ) -> Request<Body> {
        let boundary = "rms-test-boundary-7d1f";
        let mut body = Vec::new();
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"note\"\r\n\r\nignored\r\n\
                 --{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{file_name}\"\r\n\
                 Content-Type: {mime}\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(data);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        Request::builder()
            .method(method)
            .uri(uri)
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .header(
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(Body::from(body))
            .unwrap()
    }

    pub async fn send(state: &AppState, req: Request<Body>) -> (StatusCode, serde_json::Value) {
        let app = crate::api::router(state).with_state(state.clone());
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, json)
    }

    pub fn delete_request(uri: &str) -> Request<Body> {
        Request::builder()
            .method("DELETE")
            .uri(uri)
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .body(Body::empty())
            .unwrap()
    }

    /// Tiny real video (1.5 s, odd 161x91 size, 4:4:4, with audio) generated with
    /// ffmpeg, if available: normalization must cope with odd dimensions.
    pub fn tiny_mp4(dir: &FsPath) -> Option<Vec<u8>> {
        let ffmpeg = crate::config::find_in_path("ffmpeg")?;
        let out = dir.join("tiny-src.mov");
        let status = std::process::Command::new(ffmpeg)
            .args([
                "-nostdin",
                "-y",
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=161x91:rate=15",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440",
                "-t",
                "1.5",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv444p",
                "-c:a",
                "aac",
            ])
            .arg(&out)
            .status()
            .ok()?;
        status.success().then(|| std::fs::read(&out).ok()).flatten()
    }

    #[tokio::test]
    async fn upload_without_ffmpeg() {
        let (state, dir) = test_state(false).await;
        assert!(state.ffmpeg.is_none());
        db::broadcasts::create(&state.db, NewBroadcast::new("show", "Show"))
            .await
            .unwrap();
        let uri = "/api/v1/broadcasts/show/waiting-video";

        // Non-MP4 container without ffmpeg -> 415.
        let (st, body) = send(
            &state,
            multipart_request("POST", uri, "a.mov", "video/quicktime", b"xx"),
        )
        .await;
        assert_eq!(st, StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert!(
            body["error"]
                .as_str()
                .unwrap()
                .contains("ffmpeg not available")
        );

        // ".mp4" that is not an MP4 -> 415.
        let (st, _) = send(
            &state,
            multipart_request("POST", uri, "a.mp4", "video/mp4", b"not a video"),
        )
        .await;
        assert_eq!(st, StatusCode::UNSUPPORTED_MEDIA_TYPE);

        // Unknown broadcast -> 404.
        let (st, _) = send(
            &state,
            multipart_request(
                "POST",
                "/api/v1/broadcasts/nope/waiting-video",
                "a.mp4",
                "video/mp4",
                b"x",
            ),
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND);

        // MP4 stored as-is.
        let fake_mp4 = b"\0\0\0\x18ftypisom\0\0\x02\0isomiso2rest-of-file";
        let (st, body) = send(
            &state,
            multipart_request("POST", uri, "w.mp4", "video/mp4", fake_mp4),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{body}");
        let rel = body["waiting_video"].as_str().unwrap().to_string();
        assert!(rel.starts_with("show/") && rel.ends_with(".mp4"));
        assert_eq!(
            std::fs::read(dir.join("waiting").join(&rel)).unwrap(),
            fake_mp4
        );
        assert!(body["waiting_video_url"].as_str().unwrap().ends_with(&rel));

        // Replacing deletes the previous file.
        let (st, body) = send(
            &state,
            multipart_request("POST", uri, "w2.mp4", "video/mp4", fake_mp4),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        let rel2 = body["waiting_video"].as_str().unwrap().to_string();
        assert_ne!(rel, rel2);
        assert!(!dir.join("waiting").join(&rel).exists());

        // Delete clears the field and the file.
        let (st, body) = send(&state, delete_request(uri)).await;
        assert_eq!(st, StatusCode::OK);
        assert!(body["waiting_video"].is_null());
        assert!(body["waiting_video_url"].is_null());
        assert!(!dir.join("waiting").join(&rel2).exists());
        // No temp leftovers.
        let leftovers = std::fs::read_dir(dir.join(TMP_SUBDIR)).unwrap().count();
        assert_eq!(leftovers, 0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn upload_with_ffmpeg_normalizes_and_makes_poster() {
        let (state, dir) = test_state(true).await;
        if state.ffmpeg.is_none() {
            eprintln!("ffmpeg not found: skipping");
            return;
        }
        let Some(src) = tiny_mp4(&dir) else {
            eprintln!("could not generate a test video: skipping");
            return;
        };
        db::broadcasts::create(&state.db, NewBroadcast::new("show", "Show"))
            .await
            .unwrap();
        let uri = "/api/v1/broadcasts/show/waiting-video";
        let (st, body) = send(
            &state,
            multipart_request("POST", uri, "clip.mov", "video/quicktime", &src),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{body}");
        let rel = body["waiting_video"].as_str().unwrap().to_string();
        let video = dir.join("waiting").join(&rel);
        let poster = dir.join("waiting").join(poster_path_for(&rel));
        let head = std::fs::read(&video).unwrap();
        assert_eq!(&head[4..8], b"ftyp");
        assert!(std::fs::metadata(&poster).unwrap().len() > 0);
        // Public API exposes the poster.
        let req = Request::builder()
            .uri("/api/v1/public/broadcasts/show")
            .body(Body::empty())
            .unwrap();
        let (st, public) = send(&state, req).await;
        assert_eq!(st, StatusCode::OK);
        assert!(public["poster_url"].as_str().unwrap().ends_with(".jpg"));

        // Garbage with a video extension -> 422 and nothing left behind.
        let (st, body) = send(
            &state,
            multipart_request("POST", uri, "bad.mkv", "video/x-matroska", b"garbage"),
        )
        .await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        let files = std::fs::read_dir(dir.join("waiting").join("show"))
            .unwrap()
            .count();
        assert_eq!(files, 2, "only the current video and its poster");

        let (st, _) = send(&state, delete_request(uri)).await;
        assert_eq!(st, StatusCode::OK);
        assert!(!video.exists() && !poster.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn upload_limits_and_missing_field() {
        let (state, dir) = test_state(false).await;
        db::broadcasts::create(&state.db, NewBroadcast::new("show", "Show"))
            .await
            .unwrap();
        let uri = "/api/v1/broadcasts/show/waiting-video";
        // 9 MB > max_upload_mb (8) -> 413.
        let big = vec![0u8; 9 * 1024 * 1024];
        let (st, _) = send(
            &state,
            multipart_request("POST", uri, "big.mp4", "video/mp4", &big),
        )
        .await;
        assert_eq!(st, StatusCode::PAYLOAD_TOO_LARGE);
        // Not multipart -> 400.
        let req = Request::builder()
            .method("POST")
            .uri(uri)
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{}"))
            .unwrap();
        let (st, _) = send(&state, req).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        // Without auth -> 401.
        let mut req = multipart_request("POST", uri, "a.mp4", "video/mp4", b"x");
        req.headers_mut().remove(header::AUTHORIZATION);
        let (st, _) = send(&state, req).await;
        assert_eq!(st, StatusCode::UNAUTHORIZED);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
