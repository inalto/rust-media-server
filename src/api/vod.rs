//! Video on demand API (ARCHITECTURE.md §19; field-level detail in docs/API.md §4.7, §5).
//!
//! Admin (`/api/v1`):
//! - `GET /vod[?status=queued|processing|ready|failed]` → `[VodView]`, newest first
//! - `POST /vod` multipart `file` (+ optional `title`, `description`, `ladder`) → `201 VodView`
//!   (streamed to `data/upload-tmp/`, ffprobed, moved to `data/vod/<id>/source.<ext>`, queued)
//! - `GET /vod/ladders` → [`VodLadders`] (from the configuration)
//! - `GET /vod/{id}`, `PATCH /vod/{id}` `{title?, description?}` → `VodView`
//! - `DELETE /vod/{id}[?force=1]` → 204 (409 while processing unless `force`, which
//!   cancels the job)
//! - `POST /vod/{id}/retranscode` `{ladder?}` → `VodView` (re-queued, old output removed)
//! - `GET /vod/{id}/embed` → [`VodEmbedInfo`]
//! - `GET /vod/{id}/source` → the original file (attachment, Range)
//!
//! Public (`/api/v1/public`, CORS `*`): `GET /public/vod` → ready **public** videos,
//! `GET /public/vod/{id}` → [`PublicVod`] (any status, so the player can say "processing").
//! A private video (phase 8A) needs `?token=&expires=` (signed URL, `storage::sign`):
//! 401 without a valid one, otherwise its playback URLs carry the token.

use axum::body::{Body, Bytes};
use axum::extract::multipart::{Field, MultipartRejection};
use axum::extract::rejection::QueryRejection;
use axum::extract::{DefaultBodyLimit, Multipart, Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tower::ServiceExt;
use tower_http::services::ServeFile;

use super::waiting::{
    TempFile, UploadError, multipart_error, multipart_rejection, save_field, upload_body_limit,
    upload_tmp_dir,
};
use super::{ApiJson, BaseUrl, cdn, escape_html_js};
use crate::config::VodLadder;
use crate::db::settings::{UiLanguage, UiTheme};
use crate::db::vod::{self as repo, ORIGIN_UPLOAD, Rendition, STATUSES, VodItem};
use crate::db::{self};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use crate::vod::{
    ImportRequest, VOD_EXTENSIONS, ffmpeg, parse_id, title_from_file_name, vod_extension,
};

/// Same limits as a broadcast.
const TITLE_MAX: usize = 200;
const DESCRIPTION_MAX: usize = 5000;
/// Text fields of the upload form are read into memory: keep them small.
const TEXT_FIELD_MAX: usize = 64 * 1024;

pub fn routes(state: &AppState) -> Router<AppState> {
    Router::new()
        .route(
            "/vod",
            get(list)
                .post(upload)
                .layer(DefaultBodyLimit::max(upload_body_limit(&state.config))),
        )
        .route("/vod/ladders", get(ladders))
        .route("/vod/{id}", get(get_one).patch(patch).delete(delete))
        .route("/vod/{id}/retranscode", post(retranscode))
        .route("/vod/{id}/embed", get(embed))
        .route("/vod/{id}/source", get(source))
}

pub fn public_routes() -> Router<AppState> {
    Router::new()
        .route("/public/vod", get(public_list))
        .route("/public/vod/{id}", get(public_info))
}

// ---------------------------------------------------------------------------
// Views
// ---------------------------------------------------------------------------

/// Absolute URLs of a video.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct VodUrls {
    pub hls: String,
    pub dash: String,
    pub poster: String,
    pub embed: String,
    pub watch: String,
}

impl VodUrls {
    pub fn new(base: &BaseUrl, id: &str) -> Self {
        Self {
            hls: format!("{}/vod/{id}/hls/master.m3u8", base.http),
            dash: format!("{}/vod/{id}/dash/manifest.mpd", base.http),
            poster: format!("{}/vod/{id}/poster.jpg", base.http),
            embed: format!("{}/embed/vod/{id}", base.http),
            watch: format!("{}/watch/{id}", base.http),
        }
    }
}

/// `VodItem` + URLs and live data.
#[derive(Debug, Clone, Serialize)]
pub struct VodView {
    #[serde(flatten)]
    pub item: VodItem,
    pub urls: VodUrls,
    /// Poster URL when the poster exists.
    pub poster_url: Option<String>,
    /// The original file is still stored (download, re-transcode).
    pub source_available: bool,
    /// Current HLS/DASH sessions of this video.
    pub viewers_now: u32,
    /// Path of the original in its bucket (encodings of bucket files, phase 8A).
    pub object_path: Option<String>,
}

pub fn vod_view(state: &AppState, base: &BaseUrl, item: VodItem) -> VodView {
    let urls = VodUrls::new(base, &item.id);
    let poster_url = state
        .vod
        .poster_path(&item.id)
        .is_file()
        .then(|| urls.poster.clone());
    let source_available = state.vod.source_file(&item).is_some_and(|p| p.is_file());
    let viewers_now = state
        .tracker
        .counts(&crate::vod::serve::tracker_name(&item.id))
        .total();
    VodView {
        urls,
        poster_url,
        source_available,
        viewers_now,
        object_path: item.bucket_id.as_ref().map(|_| item.source_path.clone()),
        item,
    }
}

/// `GET /vod/ladders`.
#[derive(Debug, Clone, Serialize)]
pub struct VodLadders {
    pub enabled: bool,
    pub available: bool,
    pub default_ladder: String,
    pub ladders: Vec<VodLadder>,
    pub inbox_dir: String,
    pub segment_duration: f64,
    pub encoder: String,
    pub keep_source: bool,
    pub max_upload_mb: u64,
    pub extensions: Vec<&'static str>,
}

#[derive(Debug, Clone, Serialize)]
pub struct VodSnippets {
    pub hlsjs: String,
    pub dashjs: String,
}

/// `GET /vod/{id}/embed`.
#[derive(Debug, Clone, Serialize)]
pub struct VodEmbedInfo {
    pub iframe: String,
    pub urls: VodUrls,
    pub snippets: VodSnippets,
}

impl VodEmbedInfo {
    pub fn new(urls: VodUrls) -> Self {
        let e = |s: &str| escape_html_js(s);
        let video = format!(
            r#"<video id="rms-vod" controls playsinline preload="metadata" poster="{}" style="width:100%;max-width:960px;aspect-ratio:16/9;background:#000"></video>"#,
            e(&urls.poster)
        );
        let iframe = format!(
            r#"<iframe src="{}" width="640" height="360" style="border:0;max-width:100%;aspect-ratio:16/9;height:auto" allow="autoplay; fullscreen; picture-in-picture" allowfullscreen></iframe>"#,
            e(&urls.embed)
        );
        let hlsjs = format!(
            r#"{video}
<script src="{lib}"></script>
<script>
  (function () {{
    var video = document.getElementById('rms-vod');
    var src = '{src}';
    if (window.Hls && Hls.isSupported()) {{
      // Adaptive bitrate: automatic start level, capped to the player size.
      var hls = new Hls({{ startLevel: -1, capLevelToPlayerSize: true }});
      hls.loadSource(src);
      hls.attachMedia(video);
    }} else if (video.canPlayType('application/vnd.apple.mpegurl')) {{
      video.src = src; // Safari / iOS: native HLS
    }}
  }})();
</script>"#,
            lib = cdn::HLS_JS,
            src = e(&urls.hls)
        );
        let dashjs = format!(
            r#"{video}
<script src="{lib}"></script>
<script>
  (function () {{
    var player = dashjs.MediaPlayer().create();
    player.initialize(document.getElementById('rms-vod'), '{src}', false);
  }})();
</script>"#,
            lib = cdn::DASH_JS,
            src = e(&urls.dash)
        );
        Self {
            iframe,
            urls,
            snippets: VodSnippets { hlsjs, dashjs },
        }
    }
}

/// `GET /api/v1/public/vod/{id}`.
#[derive(Debug, Clone, Serialize)]
pub struct PublicVod {
    pub id: String,
    pub title: String,
    pub description: String,
    /// `queued` | `processing` | `ready` | `failed` (no error details).
    pub status: String,
    pub progress: u32,
    pub duration_secs: Option<f64>,
    pub poster_url: Option<String>,
    /// Playback URLs; `null` until the video is ready.
    pub sources: PublicVodSources,
    pub renditions: Vec<PublicRendition>,
    pub ui_language: UiLanguage,
    pub ui_theme: UiTheme,
}

#[derive(Debug, Clone, Serialize)]
pub struct PublicVodSources {
    pub hls: Option<String>,
    pub dash: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PublicRendition {
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub bandwidth: u64,
}

impl From<&Rendition> for PublicRendition {
    fn from(r: &Rendition) -> Self {
        Self {
            name: r.name.clone(),
            width: r.width,
            height: r.height,
            bandwidth: r.bandwidth,
        }
    }
}

/// `GET /api/v1/public/vod` item.
#[derive(Debug, Clone, Serialize)]
pub struct PublicVodSummary {
    pub id: String,
    pub title: String,
    pub description: String,
    pub duration_secs: Option<f64>,
    pub poster_url: Option<String>,
    pub watch_url: String,
    pub ready_at: Option<chrono::DateTime<chrono::Utc>>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn load(state: &AppState, id: &str) -> ApiResult<VodItem> {
    let id = parse_id(id).ok_or_else(|| ApiError::not_found("video not found"))?;
    repo::get(&state.db, &id)
        .await?
        .ok_or_else(|| ApiError::not_found("video not found"))
}

fn require_processing(state: &AppState) -> ApiResult<()> {
    if !state.vod.enabled() {
        return Err(ApiError::unavailable(
            "video on demand is disabled (vod.enabled = false)",
        ));
    }
    if !state.vod.available() {
        return Err(ApiError::unavailable(
            "ffmpeg not available: video on demand needs ffmpeg and ffprobe",
        ));
    }
    Ok(())
}

fn clean_title(t: &str) -> ApiResult<String> {
    let t = t.trim();
    if t.is_empty() {
        return Err(ApiError::bad_request("title must not be empty"));
    }
    if t.chars().count() > TITLE_MAX {
        return Err(ApiError::bad_request(format!(
            "title is too long (max {TITLE_MAX} characters)"
        )));
    }
    Ok(t.to_string())
}

fn clean_description(d: &str) -> ApiResult<String> {
    let d = d.trim();
    if d.chars().count() > DESCRIPTION_MAX {
        return Err(ApiError::bad_request(format!(
            "description is too long (max {DESCRIPTION_MAX} characters)"
        )));
    }
    Ok(d.to_string())
}

fn check_ladder(state: &AppState, ladder: &str) -> ApiResult<()> {
    if state.config.vod.ladder(ladder).is_some() {
        Ok(())
    } else {
        Err(ApiError::bad_request(format!(
            "unknown ladder \"{ladder}\" (configured: {})",
            state
                .config
                .vod
                .ladders
                .iter()
                .map(|l| l.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )))
    }
}

fn truthy(v: Option<&str>) -> bool {
    matches!(
        v.map(|s| s.trim().to_ascii_lowercase()).as_deref(),
        Some("1" | "true" | "yes")
    )
}

// ---------------------------------------------------------------------------
// Admin handlers
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
struct ListQuery {
    status: Option<String>,
}

async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
    q: Result<Query<ListQuery>, QueryRejection>,
) -> ApiResult<Json<Vec<VodView>>> {
    let Query(q) = q?;
    let status = q.status.as_deref().map(str::trim).filter(|s| !s.is_empty());
    if let Some(s) = status
        && !STATUSES.contains(&s)
    {
        return Err(ApiError::bad_request(format!(
            "status must be one of {}",
            STATUSES.join(", ")
        )));
    }
    let base = BaseUrl::from_request(&state.config, &headers);
    let items = repo::list(&state.db, status).await?;
    Ok(Json(
        items
            .into_iter()
            .map(|i| vod_view(&state, &base, i))
            .collect(),
    ))
}

async fn get_one(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<VodView>> {
    let item = load(&state, &id).await?;
    let base = BaseUrl::from_request(&state.config, &headers);
    Ok(Json(vod_view(&state, &base, item)))
}

async fn ladders(State(state): State<AppState>) -> Json<VodLadders> {
    let v = &state.config.vod;
    Json(VodLadders {
        enabled: state.vod.enabled(),
        available: state.vod.available(),
        default_ladder: v.default_ladder.clone(),
        ladders: v.ladders.clone(),
        inbox_dir: state.config.vod_inbox_dir().display().to_string(),
        segment_duration: v.segment_duration,
        encoder: v.encoder.clone(),
        keep_source: v.keep_source,
        max_upload_mb: state.config.server.max_upload_mb,
        extensions: VOD_EXTENSIONS.to_vec(),
    })
}

/// A small text field of the upload form.
async fn text_field(field: &mut Field<'_>) -> Result<String, UploadError> {
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = field.chunk().await.map_err(multipart_error)? {
        if buf.len() + chunk.len() > TEXT_FIELD_MAX {
            return Err(ApiError::bad_request("form field is too long").into());
        }
        buf.extend_from_slice(&chunk);
    }
    String::from_utf8(buf).map_err(|_| ApiError::bad_request("form field is not UTF-8").into())
}

struct Received {
    tmp: TempFile,
    ext: &'static str,
    original_name: String,
    title: Option<String>,
    description: Option<String>,
    ladder: Option<String>,
}

/// Read the multipart form: text fields in memory, `file` streamed to a temp file.
async fn receive(state: &AppState, mut multipart: Multipart) -> Result<Received, UploadError> {
    let tmp_dir = upload_tmp_dir(state).await?;
    let (mut title, mut description, mut ladder) = (None, None, None);
    let mut file: Option<(TempFile, &'static str, String)> = None;
    while let Some(mut field) = multipart.next_field().await.map_err(multipart_error)? {
        match field.name() {
            Some("title") => title = Some(text_field(&mut field).await?),
            Some("description") => description = Some(text_field(&mut field).await?),
            Some("ladder") => {
                let l = text_field(&mut field).await?.trim().to_string();
                if !l.is_empty() {
                    // Reject an unknown ladder before receiving a large file.
                    check_ladder(state, &l)?;
                    ladder = Some(l);
                }
            }
            Some("file") if file.is_none() => {
                let ext =
                    vod_extension(field.file_name(), field.content_type()).ok_or_else(|| {
                        UploadError::UnsupportedMediaType(format!(
                            "unsupported file type: upload a video ({})",
                            VOD_EXTENSIONS.join(", ")
                        ))
                    })?;
                let original = field.file_name().unwrap_or("").to_string();
                let (tmp, written) =
                    save_field(&mut field, &tmp_dir, ext, state.config.server.max_upload_mb)
                        .await?;
                tracing::info!(file = %original, bytes = written, "video on demand upload received");
                file = Some((tmp, ext, original));
            }
            _ => {}
        }
    }
    let Some((tmp, ext, original_name)) = file else {
        return Err(ApiError::bad_request("missing multipart field \"file\"").into());
    };
    Ok(Received {
        tmp,
        ext,
        original_name,
        title,
        description,
        ladder,
    })
}

/// `POST /vod` → 201 `VodView`.
async fn upload(
    State(state): State<AppState>,
    headers: HeaderMap,
    multipart: Result<Multipart, MultipartRejection>,
) -> Result<(StatusCode, Json<VodView>), UploadError> {
    require_processing(&state)?;
    let multipart = multipart.map_err(multipart_rejection)?;
    let mut r = receive(&state, multipart).await?;
    let title = match r.title.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        Some(t) => clean_title(t)?,
        None => title_from_file_name(&r.original_name),
    };
    let description = clean_description(r.description.as_deref().unwrap_or(""))?;
    let ladder = r
        .ladder
        .clone()
        .unwrap_or_else(|| state.config.vod.default_ladder.clone());
    check_ladder(&state, &ladder)?;
    let ffprobe = state
        .vod
        .ffprobe()
        .ok_or_else(|| ApiError::unavailable("ffprobe not available"))?;
    let probe = ffmpeg::probe(ffprobe, &r.tmp.path)
        .await
        .map_err(|e| UploadError::Unprocessable(format!("could not process the video: {e}")))?;
    if probe.video.is_none() {
        return Err(UploadError::Unprocessable(
            "could not process the video: the file has no video stream".into(),
        ));
    }
    let item = state
        .vod
        .import(
            &r.tmp.path,
            ImportRequest {
                original_name: r.original_name.clone(),
                ext: r.ext.to_string(),
                title,
                description,
                ladder,
                origin: ORIGIN_UPLOAD.to_string(),
                probe,
            },
        )
        .await
        .map_err(ApiError::internal)?;
    r.tmp.disarm();
    let base = BaseUrl::from_request(&state.config, &headers);
    Ok((StatusCode::CREATED, Json(vod_view(&state, &base, item))))
}

#[derive(Debug, Deserialize)]
struct PatchBody {
    title: Option<String>,
    description: Option<String>,
    /// `public` | `private` (phase 8A).
    visibility: Option<String>,
}

async fn patch(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    ApiJson(body): ApiJson<PatchBody>,
) -> ApiResult<Json<VodView>> {
    let item = load(&state, &id).await?;
    let title = body.title.as_deref().map(clean_title).transpose()?;
    let description = body
        .description
        .as_deref()
        .map(clean_description)
        .transpose()?;
    let visibility = body
        .visibility
        .as_deref()
        .map(|v| crate::storage::parse_visibility("visibility", v))
        .transpose()?;
    // Signed URLs use the signing key of the bucket: the admin library stays public.
    if visibility.as_deref() == Some(repo::VISIBILITY_PRIVATE) && item.bucket_id.is_none() {
        return Err(ApiError::invalid(
            "visibility",
            "only encodings of bucket files can be private",
        ));
    }
    let mut updated = repo::update_meta(&state.db, &item.id, title, description)
        .await?
        .ok_or_else(|| ApiError::not_found("video not found"))?;
    if let Some(v) = visibility {
        updated = repo::set_visibility(&state.db, &item.id, &v)
            .await?
            .ok_or_else(|| ApiError::not_found("video not found"))?;
    }
    state.vod.changed(&updated.id);
    let base = BaseUrl::from_request(&state.config, &headers);
    Ok(Json(vod_view(&state, &base, updated)))
}

#[derive(Debug, Default, Deserialize)]
struct DeleteQuery {
    force: Option<String>,
}

/// `DELETE /vod/{id}[?force=1]` → 204.
async fn delete(
    State(state): State<AppState>,
    Path(id): Path<String>,
    q: Result<Query<DeleteQuery>, QueryRejection>,
) -> ApiResult<StatusCode> {
    let Query(q) = q?;
    let force = truthy(q.force.as_deref());
    let item = load(&state, &id).await?;
    if item.status == repo::STATUS_PROCESSING && !force {
        return Err(ApiError::conflict(
            "the video is being transcoded: add ?force=1 to cancel the job and delete it",
        ));
    }
    // Row first: a cancelled job then finds nothing to re-queue.
    if repo::delete(&state.db, &item.id, force).await?.is_none() {
        return Err(ApiError::conflict("the video is being transcoded"));
    }
    if state.vod.cancel(&item.id)
        && !state
            .vod
            .wait_stopped(&item.id, std::time::Duration::from_secs(15))
            .await
    {
        tracing::warn!(id = %item.id, "the VOD job did not stop in time");
    }
    state.vod.remove_files(&item.id).await;
    tracing::info!(id = %item.id, title = %item.title, "video on demand deleted");
    state.vod.changed(&item.id);
    // An encoding of a bucket file: the original stays, the counters drop its output.
    if let Some(bucket) = &item.bucket_id {
        crate::storage::recount(&state, bucket).await;
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Default, Deserialize)]
struct RetranscodeBody {
    ladder: Option<String>,
}

/// `POST /vod/{id}/retranscode` `{ladder?}` (body optional) → `VodView`.
async fn retranscode(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Json<VodView>> {
    require_processing(&state)?;
    let body: RetranscodeBody = if body.iter().all(u8::is_ascii_whitespace) {
        RetranscodeBody::default()
    } else {
        serde_json::from_slice(&body)
            .map_err(|e| ApiError::bad_request(format!("invalid JSON body: {e}")))?
    };
    let item = load(&state, &id).await?;
    let ladder = body
        .ladder
        .as_deref()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| item.ladder.clone());
    check_ladder(&state, &ladder)?;
    if item.status == repo::STATUS_PROCESSING {
        return Err(ApiError::conflict("the video is already being transcoded"));
    }
    if !state.vod.source_file(&item).is_some_and(|p| p.is_file()) {
        return Err(ApiError::conflict(
            "the source file is no longer available (vod.keep_source = false): upload it again",
        ));
    }
    state.vod.clear_output(&item.id).await;
    let updated = repo::requeue(&state.db, &item.id, &ladder)
        .await?
        .ok_or_else(|| ApiError::conflict("the video is already being transcoded"))?;
    tracing::info!(id = %updated.id, ladder = %ladder, "video on demand re-queued");
    state.vod.changed(&updated.id);
    state.vod.wake();
    let base = BaseUrl::from_request(&state.config, &headers);
    Ok(Json(vod_view(&state, &base, updated)))
}

async fn embed(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<VodEmbedInfo>> {
    let item = load(&state, &id).await?;
    let base = BaseUrl::from_request(&state.config, &headers);
    Ok(Json(VodEmbedInfo::new(VodUrls::new(&base, &item.id))))
}

/// Download name: the original name when it is plain ASCII, else `video-<id>.<ext>`.
fn download_name(item: &VodItem) -> String {
    let ext = item
        .source_path
        .rsplit_once('.')
        .map(|(_, e)| e)
        .unwrap_or("bin");
    let clean: String = item
        .source_name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ' '))
        .collect();
    let clean = clean.trim().trim_start_matches('.').to_string();
    if clean.is_empty() || !clean.contains('.') {
        format!("video-{}.{ext}", item.id)
    } else {
        clean
    }
}

/// `GET /vod/{id}/source`: the original file (attachment).
async fn source(
    State(state): State<AppState>,
    Path(id): Path<String>,
    req: Request,
) -> ApiResult<Response> {
    let item = load(&state, &id).await?;
    let path = state
        .vod
        .source_file(&item)
        .filter(|p| p.is_file())
        .ok_or_else(|| ApiError::not_found("source file not found"))?;
    let resp = match ServeFile::new(&path).oneshot(req).await {
        Ok(r) => r,
        Err(never) => match never {},
    };
    let mut resp = resp.map(Body::new);
    if resp.status().is_success() {
        let h = resp.headers_mut();
        if let Ok(v) = HeaderValue::from_str(&format!(
            "attachment; filename=\"{}\"",
            download_name(&item)
        )) {
            h.insert(header::CONTENT_DISPOSITION, v);
        }
        h.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("private, no-cache"),
        );
    }
    Ok(resp)
}

// ---------------------------------------------------------------------------
// Public handlers
// ---------------------------------------------------------------------------

/// `url?v=<ready_at ms>` for a ready item: the master playlist, MPD and poster are
/// cached for 60 s, so the player asks for the output of the current transcode (a
/// re-transcode keeps the same paths). Unchanged for items that are not ready.
fn revision_url(url: &str, item: &VodItem) -> String {
    match item.ready_at.filter(|_| item.is_ready()) {
        Some(t) => format!("{url}?v={}", t.timestamp_millis()),
        None => url.to_string(),
    }
}

/// The signed token of a request for a private item: valid `?token=&expires=` → its
/// query string; otherwise 401.
pub async fn require_signed(state: &AppState, item: &VodItem, uri: &Uri) -> ApiResult<String> {
    let key = state
        .vod
        .playback_access(&item.id)
        .await?
        .and_then(|a| a.signing_key);
    let now = chrono::Utc::now().timestamp();
    match (
        crate::storage::sign::SignedToken::from_query(uri.query()),
        key,
    ) {
        (Some(t), Some(key))
            if crate::storage::sign::verify(&key, &item.id, &t.token, t.expires, now) =>
        {
            Ok(t.query())
        }
        _ => Err(ApiError::TokenRequired(
            "this video is private: a valid signed link (token, expires) is required".into(),
        )),
    }
}

async fn public_info(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Json<PublicVod>> {
    let item = load(&state, &id).await?;
    let signed = if item.is_private() {
        require_signed(&state, &item, &uri).await?
    } else {
        String::new()
    };
    let (language, theme) = db::settings::ui_prefs(&state.db).await?;
    let base = BaseUrl::from_request(&state.config, &headers);
    let urls = VodUrls::new(&base, &item.id);
    let ready = item.is_ready();
    let url = |u: &str| crate::storage::sign::append_query(&revision_url(u, &item), &signed);
    Ok(Json(PublicVod {
        poster_url: state
            .vod
            .poster_path(&item.id)
            .is_file()
            .then(|| url(&urls.poster)),
        sources: PublicVodSources {
            hls: ready.then(|| url(&urls.hls)),
            dash: ready.then(|| url(&urls.dash)),
        },
        renditions: item.renditions.iter().map(PublicRendition::from).collect(),
        id: item.id,
        title: item.title,
        description: item.description,
        status: item.status,
        progress: item.progress,
        duration_secs: item.source_duration_secs,
        ui_language: language,
        ui_theme: theme,
    }))
}

async fn public_list(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Vec<PublicVodSummary>>> {
    let base = BaseUrl::from_request(&state.config, &headers);
    let mut items = repo::list(&state.db, Some(repo::STATUS_READY)).await?;
    // The catalogue lists public videos only.
    items.retain(|i| !i.is_private());
    // Most recently published first.
    items.sort_by(|a, b| {
        b.ready_at
            .cmp(&a.ready_at)
            .then_with(|| b.created_at.cmp(&a.created_at))
    });
    Ok(Json(
        items
            .into_iter()
            .map(|i| {
                let urls = VodUrls::new(&base, &i.id);
                PublicVodSummary {
                    poster_url: state
                        .vod
                        .poster_path(&i.id)
                        .is_file()
                        .then(|| revision_url(&urls.poster, &i)),
                    watch_url: urls.watch,
                    id: i.id,
                    title: i.title,
                    description: i.description,
                    duration_secs: i.source_duration_secs,
                    ready_at: i.ready_at,
                }
            })
            .collect(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::waiting::tests::{TOKEN, delete_request, multipart_request, send, test_state};
    use axum::http::Request;

    fn json_request(method: &str, uri: &str, body: &str) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    fn get_request(uri: &str) -> Request<Body> {
        Request::builder()
            .uri(uri)
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .body(Body::empty())
            .unwrap()
    }

    /// Insert an item with a source file on disk.
    async fn seed(state: &AppState, title: &str, status: &str, created: &str) -> VodItem {
        let id = uuid::Uuid::new_v4().hyphenated().to_string();
        let dir = state.vod.item_dir(&id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("source.mp4"), b"fake").unwrap();
        let mut it = VodItem::new(
            &id,
            title,
            &format!("{id}/source.mp4"),
            "standard",
            ORIGIN_UPLOAD,
        );
        it.status = status.into();
        it.created_at = db::parse_ts(created);
        it.ready_at = (status == repo::STATUS_READY).then(|| db::parse_ts(created));
        it.source_name = "Clip finale.mp4".into();
        repo::insert(&state.db, &it).await.unwrap();
        it
    }

    #[tokio::test]
    async fn list_get_patch_delete() {
        let (state, dir) = test_state(false).await;
        let a = seed(
            &state,
            "Primo",
            repo::STATUS_READY,
            "2026-10-09T10:00:00.000Z",
        )
        .await;
        let b = seed(
            &state,
            "Secondo",
            repo::STATUS_QUEUED,
            "2026-10-09T11:00:00.000Z",
        )
        .await;
        let c = seed(
            &state,
            "Terzo",
            repo::STATUS_PROCESSING,
            "2026-10-09T12:00:00.000Z",
        )
        .await;

        let (st, list) = send(&state, get_request("/api/v1/vod")).await;
        assert_eq!(st, StatusCode::OK);
        let ids: Vec<&str> = list
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, [c.id.as_str(), b.id.as_str(), a.id.as_str()]);
        let first = &list[0];
        assert_eq!(first["status"], "processing");
        assert!(
            first["urls"]["hls"]
                .as_str()
                .unwrap()
                .ends_with(&format!("/vod/{}/hls/master.m3u8", c.id))
        );
        assert!(
            first["urls"]["dash"]
                .as_str()
                .unwrap()
                .ends_with("/dash/manifest.mpd")
        );
        assert!(
            first["urls"]["embed"]
                .as_str()
                .unwrap()
                .ends_with(&format!("/embed/vod/{}", c.id))
        );
        assert!(
            first["urls"]["watch"]
                .as_str()
                .unwrap()
                .ends_with(&format!("/watch/{}", c.id))
        );
        assert_eq!(first["source_available"], true);
        assert_eq!(first["viewers_now"], 0);
        assert!(first["poster_url"].is_null());
        assert!(first.get("renditions").unwrap().is_array());

        let (st, ready) = send(&state, get_request("/api/v1/vod?status=ready")).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(ready.as_array().unwrap().len(), 1);
        let (st, _) = send(&state, get_request("/api/v1/vod?status=nope")).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);

        let (st, one) = send(
            &state,
            get_request(&format!("/api/v1/vod/{}", a.id.to_uppercase())),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "ids are case-insensitive");
        assert_eq!(one["title"], "Primo");
        let (st, _) = send(&state, get_request("/api/v1/vod/not-an-id")).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        let (st, _) = send(
            &state,
            get_request(&format!("/api/v1/vod/{}", uuid::Uuid::new_v4())),
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND);

        // PATCH
        let uri = format!("/api/v1/vod/{}", a.id);
        let (st, p) = send(
            &state,
            json_request(
                "PATCH",
                &uri,
                r#"{"title":"  Nuovo  ","description":"Descr"}"#,
            ),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(
            (p["title"].as_str(), p["description"].as_str()),
            (Some("Nuovo"), Some("Descr"))
        );
        let (st, _) = send(&state, json_request("PATCH", &uri, r#"{"title":"   "}"#)).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        let long = format!(r#"{{"title":"{}"}}"#, "x".repeat(201));
        let (st, _) = send(&state, json_request("PATCH", &uri, &long)).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);

        // Embed info.
        let (st, e) = send(&state, get_request(&format!("{uri}/embed"))).await;
        assert_eq!(st, StatusCode::OK);
        assert!(
            e["iframe"]
                .as_str()
                .unwrap()
                .contains(&format!("/embed/vod/{}", a.id))
        );
        assert!(
            e["snippets"]["hlsjs"]
                .as_str()
                .unwrap()
                .contains("capLevelToPlayerSize")
        );
        assert!(
            e["snippets"]["dashjs"]
                .as_str()
                .unwrap()
                .contains("manifest.mpd")
        );

        // Source download.
        let (st, _) = send(&state, get_request(&format!("{uri}/source"))).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(download_name(&a), "Clip finale.mp4");

        // Ladders.
        let (st, l) = send(&state, get_request("/api/v1/vod/ladders")).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(l["default_ladder"], "standard");
        assert_eq!(l["ladders"].as_array().unwrap().len(), 3);
        assert!(l["inbox_dir"].as_str().unwrap().ends_with("inbox"));

        // Public info: any status, sources only when ready; list: ready only.
        let public = |uri: &str| Request::builder().uri(uri).body(Body::empty()).unwrap();
        let (st, p) = send(&state, public(&format!("/api/v1/public/vod/{}", b.id))).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(p["status"], "queued");
        assert!(p["sources"]["hls"].is_null());
        assert!(p.get("error").is_none());
        let (st, p) = send(&state, public(&format!("/api/v1/public/vod/{}", a.id))).await;
        assert_eq!(st, StatusCode::OK);
        assert!(
            p["sources"]["hls"]
                .as_str()
                .unwrap()
                .contains("/hls/master.m3u8?v="),
            "{p}"
        );
        assert!(
            p["sources"]["dash"]
                .as_str()
                .unwrap()
                .contains("/dash/manifest.mpd?v=")
        );
        assert_eq!(p["ui_language"], "it");
        let (st, pl) = send(&state, public("/api/v1/public/vod")).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(pl.as_array().unwrap().len(), 1);
        assert_eq!(pl[0]["title"], "Nuovo");
        let (st, _) = send(
            &state,
            public(&format!("/api/v1/public/vod/{}", uuid::Uuid::new_v4())),
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND);

        // Without ffmpeg: no uploads, no re-transcode (503).
        let (st, _) = send(
            &state,
            multipart_request("POST", "/api/v1/vod", "a.mp4", "video/mp4", b"x"),
        )
        .await;
        assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE);
        let (st, _) = send(
            &state,
            json_request("POST", &format!("{uri}/retranscode"), ""),
        )
        .await;
        assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE);

        // DELETE: 409 while processing, forced works; files removed.
        let (st, _) = send(&state, delete_request(&format!("/api/v1/vod/{}", c.id))).await;
        assert_eq!(st, StatusCode::CONFLICT);
        assert!(state.vod.item_dir(&c.id).exists());
        let (st, _) = send(
            &state,
            delete_request(&format!("/api/v1/vod/{}?force=1", c.id)),
        )
        .await;
        assert_eq!(st, StatusCode::NO_CONTENT);
        assert!(!state.vod.item_dir(&c.id).exists());
        let (st, _) = send(&state, delete_request(&uri)).await;
        assert_eq!(st, StatusCode::NO_CONTENT);
        assert!(!state.vod.item_dir(&a.id).exists());
        let (st, _) = send(&state, delete_request(&uri)).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        let (st, left) = send(&state, get_request("/api/v1/vod")).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(left.as_array().unwrap().len(), 1);

        // Admin routes need the token.
        let (st, _) = send(&state, public("/api/v1/vod")).await;
        assert_eq!(st, StatusCode::UNAUTHORIZED);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn upload_validation_and_retranscode() {
        let (state, dir) = test_state(true).await;
        if !state.vod.available() {
            eprintln!("ffmpeg/ffprobe not found: skipping");
            return;
        }
        let Some(src) = crate::api::waiting::tests::tiny_mp4(&dir) else {
            eprintln!("could not generate a test video: skipping");
            return;
        };
        // Unsupported type, garbage, unknown ladder.
        let (st, _) = send(
            &state,
            multipart_request("POST", "/api/v1/vod", "a.txt", "text/plain", b"x"),
        )
        .await;
        assert_eq!(st, StatusCode::UNSUPPORTED_MEDIA_TYPE);
        let (st, body) = send(
            &state,
            multipart_request(
                "POST",
                "/api/v1/vod",
                "a.mkv",
                "video/x-matroska",
                b"garbage",
            ),
        )
        .await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        let boundary = "b0undary";
        let mut form = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"ladder\"\r\n\r\nnope\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.mov\"\r\nContent-Type: video/quicktime\r\n\r\n"
        )
        .into_bytes();
        form.extend_from_slice(&src);
        form.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        let req = Request::builder()
            .method("POST")
            .uri("/api/v1/vod")
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .header(
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(Body::from(form))
            .unwrap();
        let (st, body) = send(&state, req).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
        assert!(body["error"].as_str().unwrap().contains("unknown ladder"));

        // A valid upload is queued (the job runner is not running in tests).
        let (st, v) = send(
            &state,
            multipart_request(
                "POST",
                "/api/v1/vod",
                "My_clip.mov",
                "video/quicktime",
                &src,
            ),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "{v}");
        assert_eq!(v["title"], "My clip");
        assert_eq!(v["status"], "queued");
        assert_eq!(v["ladder"], "standard");
        assert_eq!(v["origin"], "upload");
        assert_eq!(v["source_name"], "My_clip.mov");
        assert!(
            v["source_width"].as_u64().unwrap() >= 160
                && v["source_height"].as_u64().unwrap() >= 90,
            "{v}"
        );
        assert!(v["source_duration_secs"].as_f64().unwrap() > 1.0, "{v}");
        let id = v["id"].as_str().unwrap().to_string();
        assert!(state.vod.item_dir(&id).join("source.mov").is_file());
        assert_eq!(
            std::fs::read_dir(dir.join("upload-tmp")).unwrap().count(),
            0,
            "no temp leftovers"
        );

        // Re-transcode a queued item with another ladder; unknown ladder → 400.
        let uri = format!("/api/v1/vod/{id}/retranscode");
        let (st, v) = send(&state, json_request("POST", &uri, r#"{"ladder":"mobile"}"#)).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(v["ladder"], "mobile");
        let (st, _) = send(&state, json_request("POST", &uri, r#"{"ladder":"nope"}"#)).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        // Processing → 409.
        assert!(repo::claim(&state.db, &id).await.unwrap());
        let (st, _) = send(&state, json_request("POST", &uri, "")).await;
        assert_eq!(st, StatusCode::CONFLICT);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
