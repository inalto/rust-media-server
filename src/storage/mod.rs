//! Bucket storage of the users (phase 8A, docs/ACCOUNTS.md §1, §3, §5).
//!
//! Files live under `<storage root>/<bucket id>/<path>` (`storage.root`, default
//! `data/buckets`); `bucket_objects` indexes them (files and directories). Every write
//! goes through this module, which keeps the index, the files and the counters in step:
//!
//! - **upload** ([`put_file`]): the body is streamed to `data/upload-tmp/` (never held
//!   in memory) while its SHA-256 is computed, bounded by `storage.max_object_mb` (413)
//!   and by the owner's quota (507, overwritten bytes are given back); the optional
//!   `Checksum` is verified (400); then, under a per-object lock, the parents are created
//!   and the temp file is renamed into place; the row is upserted (an overwrite keeps
//!   the object id) and the encoding policy applied ([`encoding_after_write`]);
//! - **delete** of a file or of a directory tree removes the encodings of the files
//!   (job cancelled, `data/vod/<id>/` removed) with them;
//! - after every change the bucket counters are recomputed ([`recount`]): `bytes_used` =
//!   files + encoding outputs, `objects_count` = files. [`repair_user`] re-measures the
//!   files and outputs on disk (admin `POST /users/{id}/recount`).
//!
//! Encodings reference the original in place (`vod_items.source_path` = object path,
//! `bucket_id`, `object_id`); a private one (`visibility = private`) is played with a
//! signed URL ([`sign`]).

pub mod api;
pub mod paths;
pub mod sign;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex, Weak};

use axum::body::Body;
use chrono::{DateTime, Utc};
use futures::StreamExt;
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::api::BaseUrl;
use crate::api::waiting::{TMP_SUBDIR, TempFile};
use crate::config::Config;
use crate::db::buckets::{self, Bucket};
use crate::db::objects::{self, BucketObject};
use crate::db::users::{self, User};
use crate::db::vod::{
    self as vod_repo, Rendition, VISIBILITIES, VISIBILITY_PRIVATE, VISIBILITY_PUBLIC, VodItem,
};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use paths::ObjectPath;
use sign::SignedToken;

pub const MIB: u64 = 1024 * 1024;

/// `<storage root>/<bucket id>`.
pub fn bucket_dir(config: &Config, bucket_id: &str) -> PathBuf {
    config.storage_root().join(bucket_id)
}

/// The file of normalized `path` (`/a/b.mp4`) in a bucket.
pub fn object_file(config: &Config, bucket_id: &str, path: &str) -> PathBuf {
    let mut p = bucket_dir(config, bucket_id);
    for seg in path.split('/').filter(|s| !s.is_empty()) {
        p.push(seg);
    }
    p
}

/// `<base>/storage/<bucket name><path>` (path percent-encoded; directories end with `/`).
pub fn storage_url(base: &BaseUrl, bucket: &Bucket, path: &str, is_dir: bool) -> String {
    let mut url = format!(
        "{}/storage/{}{}",
        base.http,
        bucket.name,
        paths::encode_path(path)
    );
    if is_dir && !url.ends_with('/') {
        url.push('/');
    }
    url
}

// ---------------------------------------------------------------------------
// Per-object locks
// ---------------------------------------------------------------------------

type LockMap = Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>;
static LOCKS: LazyLock<LockMap> = LazyLock::new(Default::default);

/// Serialize the final step of writes to one object on this node (rename + index +
/// encoding), so concurrent uploads of the same path cannot interleave.
async fn lock_object(bucket_id: &str, path: &str) -> tokio::sync::OwnedMutexGuard<()> {
    let key = format!("{bucket_id}\n{path}");
    let lock = {
        let mut map = LOCKS.lock().unwrap_or_else(|e| e.into_inner());
        if map.len() > 1024 {
            map.retain(|_, w| w.strong_count() > 0);
        }
        match map.get(&key).and_then(Weak::upgrade) {
            Some(l) => l,
            None => {
                let l = Arc::new(tokio::sync::Mutex::new(()));
                map.insert(key, Arc::downgrade(&l));
                l
            }
        }
    };
    lock.lock_owned().await
}

// ---------------------------------------------------------------------------
// Views
// ---------------------------------------------------------------------------

/// The encoding of a file (`encoding` of an [`ObjectView`]).
#[derive(Debug, Clone, Serialize)]
pub struct EncodingView {
    pub vod_id: String,
    /// `queued` | `processing` | `ready` | `failed`
    pub status: String,
    pub progress: u32,
    pub ladder: String,
    /// `public` | `private`
    pub visibility: String,
    pub renditions: Vec<Rendition>,
    pub error: Option<String>,
    pub duration_secs: Option<f64>,
    /// Size of the output (renditions + poster), counted in the quota.
    pub output_bytes: u64,
    /// Unsigned playback URLs (a private encoding needs `?token=&expires=`, see
    /// `?action=sign`).
    pub hls_url: String,
    pub dash_url: String,
    pub poster_url: Option<String>,
    pub embed_url: String,
    pub watch_url: String,
    pub created_at: DateTime<Utc>,
    pub ready_at: Option<DateTime<Utc>>,
}

impl EncodingView {
    pub fn new(state: &AppState, base: &BaseUrl, item: &VodItem) -> Self {
        let urls = crate::api::vod::VodUrls::new(base, &item.id);
        Self {
            vod_id: item.id.clone(),
            status: item.status.clone(),
            progress: item.progress,
            ladder: item.ladder.clone(),
            visibility: item.visibility.clone(),
            renditions: item.renditions.clone(),
            error: item.error.clone(),
            duration_secs: item.source_duration_secs,
            output_bytes: item.output_bytes,
            poster_url: state
                .vod
                .poster_path(&item.id)
                .is_file()
                .then(|| urls.poster.clone()),
            hls_url: urls.hls,
            dash_url: urls.dash,
            embed_url: urls.embed,
            watch_url: urls.watch,
            created_at: item.created_at,
            ready_at: item.ready_at,
        }
    }
}

/// One entry of a listing / the response of an upload.
#[derive(Debug, Clone, Serialize)]
pub struct ObjectView {
    pub id: String,
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    pub content_type: Option<String>,
    pub sha256: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoding: Option<EncodingView>,
}

impl ObjectView {
    pub fn new(o: BucketObject, encoding: Option<EncodingView>) -> Self {
        Self {
            name: o.name().to_string(),
            id: o.id,
            path: o.path,
            is_dir: o.is_dir,
            size: o.size,
            content_type: o.content_type,
            sha256: o.sha256,
            created_at: o.created_at,
            updated_at: o.updated_at,
            encoding,
        }
    }
}

/// Signed playback URLs of an encoding (`?action=sign`, `?info=1`, account `sign`).
#[derive(Debug, Clone, Serialize)]
pub struct SignedUrls {
    pub hls: String,
    pub dash: String,
    pub poster: String,
    pub embed: String,
    pub watch: String,
    pub token: String,
    /// Unix seconds.
    pub expires: i64,
    pub expires_at: DateTime<Utc>,
}

/// Sign the playback URLs of `vod_id` with the bucket key for `secs` seconds.
pub fn signed_urls(base: &BaseUrl, vod_id: &str, bucket: &Bucket, secs: u64) -> SignedUrls {
    let t = SignedToken::issue(&bucket.signing_key, vod_id, secs);
    let q = t.query();
    let urls = crate::api::vod::VodUrls::new(base, vod_id);
    SignedUrls {
        hls: sign::append_query(&urls.hls, &q),
        dash: sign::append_query(&urls.dash, &q),
        poster: sign::append_query(&urls.poster, &q),
        embed: sign::append_query(&urls.embed, &q),
        watch: sign::append_query(&urls.watch, &q),
        expires_at: DateTime::from_timestamp(t.expires, 0).unwrap_or_default(),
        expires: t.expires,
        token: t.token,
    }
}

/// Views of `objs` with their encodings (one query for the encodings).
pub async fn object_views(
    state: &AppState,
    base: &BaseUrl,
    objs: Vec<BucketObject>,
) -> ApiResult<Vec<ObjectView>> {
    let ids: Vec<String> = objs
        .iter()
        .filter(|o| !o.is_dir)
        .map(|o| o.id.clone())
        .collect();
    let mut encodings: HashMap<String, VodItem> = HashMap::new();
    // Newest first: keep the first one per object.
    for item in vod_repo::list_by_objects(&state.db, &ids).await? {
        if let Some(oid) = item.object_id.clone() {
            encodings.entry(oid).or_insert(item);
        }
    }
    Ok(objs
        .into_iter()
        .map(|o| {
            let enc = encodings
                .get(&o.id)
                .map(|i| EncodingView::new(state, base, i));
            ObjectView::new(o, enc)
        })
        .collect())
}

// ---------------------------------------------------------------------------
// Validation helpers
// ---------------------------------------------------------------------------

/// A configured ladder (400 naming `field` otherwise).
pub fn validate_ladder(config: &Config, field: &str, ladder: &str) -> ApiResult<()> {
    if config.vod.ladder(ladder).is_some() {
        return Ok(());
    }
    Err(ApiError::invalid(
        field,
        format!(
            "unknown ladder \"{ladder}\" (configured: {})",
            config
                .vod
                .ladders
                .iter()
                .map(|l| l.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    ))
}

/// `public` | `private` (case-insensitive), 400 naming `field` otherwise.
pub fn parse_visibility(field: &str, v: &str) -> ApiResult<String> {
    let v = v.trim().to_ascii_lowercase();
    if VISIBILITIES.contains(&v.as_str()) {
        Ok(v)
    } else {
        Err(ApiError::invalid(
            field,
            "visibility must be public or private",
        ))
    }
}

/// Default visibility of the new encodings of a bucket.
pub fn bucket_visibility(bucket: &Bucket) -> &'static str {
    if bucket.public_playback {
        VISIBILITY_PUBLIC
    } else {
        VISIBILITY_PRIVATE
    }
}

/// Ladder of a new encoding: the bucket's, else the user's, else `vod.default_ladder`
/// (ladders no longer in the configuration are skipped).
pub fn default_ladder(config: &Config, bucket: &Bucket, owner: &User) -> String {
    [&bucket.default_ladder, &owner.default_ladder]
        .into_iter()
        .flatten()
        .find(|l| config.vod.ladder(l).is_some())
        .cloned()
        .unwrap_or_else(|| config.vod.default_ladder.clone())
}

/// A video file name (encoded automatically when the bucket has `auto_encode`).
pub fn is_video_name(name: &str) -> bool {
    crate::vod::vod_extension(Some(name), None).is_some()
}

/// Lifetime of a signed URL: `secs` (default 1 h), 1 s ..= 1 year.
pub fn expires_secs(field: &str, secs: Option<u64>) -> ApiResult<u64> {
    let secs = secs.unwrap_or(sign::DEFAULT_EXPIRES_SECS);
    if secs == 0 || secs > sign::MAX_EXPIRES_SECS {
        return Err(ApiError::invalid(
            field,
            format!(
                "expires_secs must be between 1 and {}",
                sign::MAX_EXPIRES_SECS
            ),
        ));
    }
    Ok(secs)
}

fn io_error(context: &str, e: std::io::Error) -> ApiError {
    ApiError::internal(format!("{context}: {e}"))
}

async fn owner_of(state: &AppState, bucket: &Bucket) -> ApiResult<User> {
    users::get(&state.db, &bucket.user_id)
        .await?
        .ok_or_else(|| ApiError::not_found("bucket not found"))
}

/// 409 when an ancestor of `path` is a file.
async fn check_parents(state: &AppState, bucket: &Bucket, path: &ObjectPath) -> ApiResult<()> {
    for parent in path.parents() {
        if let Some(o) = objects::get(&state.db, &bucket.id, &parent).await?
            && !o.is_dir
        {
            return Err(ApiError::conflict(format!(
                "{parent} is a file, not a directory"
            )));
        }
    }
    Ok(())
}

/// Recompute the bucket counters (logged on error).
pub async fn recount(state: &AppState, bucket_id: &str) -> Option<(u64, u64)> {
    match buckets::recount(&state.db, bucket_id).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(bucket = %bucket_id, error = %e, "cannot recount the bucket usage");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Writes
// ---------------------------------------------------------------------------

/// `X-Rms-Encode` of an upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncodeChoice {
    /// No header: the bucket's `auto_encode` and the file extension decide (an encoded
    /// file is always re-encoded when overwritten).
    Auto,
    /// `none`: no encoding (an existing one is removed).
    Off,
    /// A ladder: encode with it.
    Ladder(String),
}

/// Options of [`put_file`] (from the request headers).
#[derive(Debug, Clone)]
pub struct PutRequest {
    pub encode: EncodeChoice,
    /// `X-Rms-Visibility` (validated).
    pub visibility: Option<String>,
    /// `Checksum` (lowercase hex SHA-256, validated).
    pub checksum: Option<String>,
    /// `Content-Length`, when sent (early 413/507).
    pub content_length: Option<u64>,
}

/// Result of an upload.
#[derive(Debug, Clone)]
pub struct PutOutcome {
    pub object: BucketObject,
    pub encoding: Option<VodItem>,
    /// False when an existing file was overwritten.
    pub created: bool,
}

/// Stream `body` into the file at `path` of `bucket` (see the module docs).
pub async fn put_file(
    state: &AppState,
    bucket: &Bucket,
    path: &ObjectPath,
    body: Body,
    req: PutRequest,
) -> ApiResult<PutOutcome> {
    if let EncodeChoice::Ladder(l) = &req.encode {
        validate_ladder(&state.config, "X-Rms-Encode", l)?;
        if !state.vod.enabled() {
            return Err(ApiError::unavailable(
                "video on demand is disabled (vod.enabled = false)",
            ));
        }
    }
    let owner = owner_of(state, bucket).await?;
    check_parents(state, bucket, path).await?;
    let existing = objects::get(&state.db, &bucket.id, &path.path).await?;
    if existing.as_ref().is_some_and(|o| o.is_dir) {
        return Err(ApiError::conflict("a directory exists at this path"));
    }
    let replaced = existing.as_ref().map(|o| o.size).unwrap_or(0);
    let max_mb = state.config.storage.max_object_mb;
    let max_bytes = max_mb.saturating_mul(MIB);
    let room = if owner.quota_bytes > 0 {
        let used = users::usage(&state.db, &owner.id).await?.bytes_used;
        Some(
            owner
                .quota_bytes
                .saturating_sub(used)
                .saturating_add(replaced),
        )
    } else {
        None
    };
    let too_large = || {
        ApiError::PayloadTooLarge(format!(
            "the file is larger than the limit of {max_mb} MB (storage.max_object_mb)"
        ))
    };
    let over_quota = || {
        ApiError::InsufficientStorage(format!(
            "storage quota exceeded ({} bytes)",
            owner.quota_bytes
        ))
    };
    if let Some(len) = req.content_length {
        if len > max_bytes {
            return Err(too_large());
        }
        if room.is_some_and(|r| len > r) {
            return Err(over_quota());
        }
    }

    // Stream to a temp file, hashing on the way.
    let tmp_dir = state.config.data_dir().join(TMP_SUBDIR);
    tokio::fs::create_dir_all(&tmp_dir)
        .await
        .map_err(|e| io_error("creating the upload directory", e))?;
    let mut tmp =
        TempFile::new(tmp_dir.join(format!(".upload-{}.bin", uuid::Uuid::new_v4().simple())));
    let file = tokio::fs::File::create(&tmp.path)
        .await
        .map_err(|e| io_error("creating the temp file", e))?;
    let mut out = tokio::io::BufWriter::with_capacity(1 << 20, file);
    let mut hasher = Sha256::new();
    let mut written: u64 = 0;
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| ApiError::bad_request(format!("upload interrupted: {e}")))?;
        written += chunk.len() as u64;
        if written > max_bytes {
            return Err(too_large());
        }
        if room.is_some_and(|r| written > r) {
            return Err(over_quota());
        }
        hasher.update(&chunk);
        out.write_all(&chunk)
            .await
            .map_err(|e| io_error("writing the temp file", e))?;
    }
    out.flush()
        .await
        .map_err(|e| io_error("writing the temp file", e))?;
    drop(out);
    let sha256 = hex::encode(hasher.finalize());
    if let Some(want) = &req.checksum
        && *want != sha256
    {
        return Err(ApiError::invalid(
            "Checksum",
            format!("checksum mismatch: the SHA-256 of the body is {sha256}"),
        ));
    }

    // Move into place and index it, one writer per object.
    let _guard = lock_object(&bucket.id, &path.path).await;
    if buckets::get(&state.db, &bucket.id).await?.is_none() {
        return Err(ApiError::not_found("bucket not found"));
    }
    let dest = object_file(&state.config, &bucket.id, &path.path);
    if let Some(parent) = dest.parent()
        && let Err(e) = tokio::fs::create_dir_all(parent).await
    {
        return Err(match e.kind() {
            std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::NotADirectory => {
                ApiError::conflict("a parent of this path is a file")
            }
            _ => io_error("creating the directories", e),
        });
    }
    if dest.is_dir() {
        return Err(ApiError::conflict("a directory exists at this path"));
    }
    crate::vod::inbox::move_file(&tmp.path, &dest)
        .await
        .map_err(|e| io_error("storing the file", e))?;
    tmp.disarm();
    objects::ensure_dirs(&state.db, &bucket.id, &path.parents()).await?;
    let content_type = mime_guess::from_path(path.name())
        .first_raw()
        .unwrap_or("application/octet-stream")
        .to_string();
    let object = objects::upsert_file(
        &state.db,
        &bucket.id,
        &path.path,
        written,
        Some(content_type),
        Some(sha256),
    )
    .await?;
    tracing::info!(bucket = %bucket.name, path = %object.path, bytes = written, "storage file stored");
    let encoding = encoding_after_write(state, bucket, &owner, &object, &req).await?;
    recount(state, &bucket.id).await;
    Ok(PutOutcome {
        object,
        encoding,
        created: existing.is_none(),
    })
}

/// Encoding policy after a file was written (see [`EncodeChoice`]); visibility from
/// `X-Rms-Visibility`, else the existing encoding's, else the bucket's default.
async fn encoding_after_write(
    state: &AppState,
    bucket: &Bucket,
    owner: &User,
    object: &BucketObject,
    req: &PutRequest,
) -> ApiResult<Option<VodItem>> {
    let existing = vod_repo::find_by_object(&state.db, &object.id).await?;
    let want = state.vod.enabled()
        && match &req.encode {
            EncodeChoice::Off => false,
            EncodeChoice::Ladder(_) => true,
            EncodeChoice::Auto => {
                existing.is_some() || (bucket.auto_encode && is_video_name(object.name()))
            }
        };
    if !want {
        if let Some(item) = existing {
            // The encoding of the previous content would be stale.
            state.vod.delete_item(&item.id).await?;
        }
        return Ok(None);
    }
    let ladder = match &req.encode {
        EncodeChoice::Ladder(l) => l.clone(),
        _ => existing
            .as_ref()
            .map(|i| i.ladder.clone())
            .filter(|l| state.config.vod.ladder(l).is_some())
            .unwrap_or_else(|| default_ladder(&state.config, bucket, owner)),
    };
    let visibility = req
        .visibility
        .clone()
        .or_else(|| existing.as_ref().map(|i| i.visibility.clone()))
        .unwrap_or_else(|| bucket_visibility(bucket).to_string());
    upsert_encoding(state, bucket, owner, object, existing, &ladder, &visibility)
        .await
        .map(Some)
}

/// Re-queue `existing` (or create the encoding) with `ladder` and `visibility`.
async fn upsert_encoding(
    state: &AppState,
    bucket: &Bucket,
    owner: &User,
    object: &BucketObject,
    existing: Option<VodItem>,
    ladder: &str,
    visibility: &str,
) -> ApiResult<VodItem> {
    let Some(item) = existing else {
        return Ok(state
            .vod
            .create_storage_item(&owner.id, &bucket.id, object, ladder, visibility)
            .await?);
    };
    let mut item = state
        .vod
        .reencode(&item.id, ladder, Some(object.size))
        .await?
        .ok_or_else(|| ApiError::conflict("the encoding was deleted meanwhile"))?;
    if item.visibility != visibility
        && let Some(v) = vod_repo::set_visibility(&state.db, &item.id, visibility).await?
    {
        item = v;
        state.vod.changed(&item.id);
    }
    Ok(item)
}

/// `?action=encode`: (re)queue the encoding of a file.
pub async fn encode(
    state: &AppState,
    bucket: &Bucket,
    object: &BucketObject,
    ladder: Option<String>,
    visibility: Option<String>,
) -> ApiResult<VodItem> {
    if !state.vod.enabled() {
        return Err(ApiError::unavailable(
            "video on demand is disabled (vod.enabled = false)",
        ));
    }
    if let Some(l) = &ladder {
        validate_ladder(&state.config, "ladder", l)?;
    }
    let owner = owner_of(state, bucket).await?;
    let _guard = lock_object(&bucket.id, &object.path).await;
    let existing = vod_repo::find_by_object(&state.db, &object.id).await?;
    let ladder = ladder
        .or_else(|| existing.as_ref().map(|i| i.ladder.clone()))
        .filter(|l| state.config.vod.ladder(l).is_some())
        .unwrap_or_else(|| default_ladder(&state.config, bucket, &owner));
    let visibility = visibility
        .or_else(|| existing.as_ref().map(|i| i.visibility.clone()))
        .unwrap_or_else(|| bucket_visibility(bucket).to_string());
    let item = upsert_encoding(
        state,
        bucket,
        &owner,
        object,
        existing,
        &ladder,
        &visibility,
    )
    .await?;
    recount(state, &bucket.id).await;
    Ok(item)
}

/// `PUT /storage/{b}/{dir}/`: create a directory (and its parents); idempotent.
pub async fn create_dir(
    state: &AppState,
    bucket: &Bucket,
    path: &ObjectPath,
) -> ApiResult<BucketObject> {
    if path.is_root() {
        return Err(ApiError::bad_request("the bucket root always exists"));
    }
    check_parents(state, bucket, path).await?;
    let _guard = lock_object(&bucket.id, &path.path).await;
    if let Some(o) = objects::get(&state.db, &bucket.id, &path.path).await? {
        if !o.is_dir {
            return Err(ApiError::conflict("a file exists at this path"));
        }
        return Ok(o);
    }
    let dir = object_file(&state.config, &bucket.id, &path.path);
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        return Err(match e.kind() {
            std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::NotADirectory => {
                ApiError::conflict("a file exists at this path")
            }
            _ => io_error("creating the directory", e),
        });
    }
    let mut all = path.parents();
    all.push(path.path.clone());
    objects::ensure_dirs(&state.db, &bucket.id, &all).await?;
    objects::get(&state.db, &bucket.id, &path.path)
        .await?
        .ok_or_else(|| ApiError::internal("directory row missing after insert"))
}

/// Remove a file (ignoring one that is already gone).
async fn remove_file_quiet(file: &std::path::Path) {
    if let Err(e) = tokio::fs::remove_file(file).await
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(path = %file.display(), error = %e, "cannot remove a storage file");
    }
}

/// Delete the encodings of the given objects.
async fn delete_encodings(state: &AppState, object_ids: &[String]) -> ApiResult<usize> {
    let items = vod_repo::list_by_objects(&state.db, object_ids).await?;
    for item in &items {
        state.vod.delete_item(&item.id).await?;
    }
    Ok(items.len())
}

/// `DELETE /storage/{b}/{file}`: the file and its encoding.
pub async fn delete_file(state: &AppState, bucket: &Bucket, path: &ObjectPath) -> ApiResult<()> {
    let _guard = lock_object(&bucket.id, &path.path).await;
    let obj = objects::get(&state.db, &bucket.id, &path.path)
        .await?
        .ok_or_else(|| ApiError::not_found("file not found"))?;
    if obj.is_dir {
        return Err(ApiError::conflict(
            "this is a directory: add a trailing / to delete it (needs admin access)",
        ));
    }
    delete_encodings(state, std::slice::from_ref(&obj.id)).await?;
    objects::delete_file(&state.db, &bucket.id, &obj.path).await?;
    remove_file_quiet(&object_file(&state.config, &bucket.id, &obj.path)).await;
    tracing::info!(bucket = %bucket.name, path = %obj.path, "storage file deleted");
    recount(state, &bucket.id).await;
    Ok(())
}

/// `DELETE /storage/{b}/{dir}/`: the directory, everything below it and the encodings.
/// Returns the number of index rows removed.
pub async fn delete_dir(state: &AppState, bucket: &Bucket, path: &ObjectPath) -> ApiResult<u64> {
    if path.is_root() {
        return Err(ApiError::bad_request(
            "the bucket root cannot be deleted: delete the bucket instead",
        ));
    }
    let _guard = lock_object(&bucket.id, &path.path).await;
    match objects::get(&state.db, &bucket.id, &path.path).await? {
        Some(o) if o.is_dir => {}
        Some(_) => return Err(ApiError::conflict("this is a file, not a directory")),
        None => return Err(ApiError::not_found("directory not found")),
    }
    let files: Vec<String> = objects::list_tree(&state.db, &bucket.id, &path.path)
        .await?
        .into_iter()
        .filter(|o| !o.is_dir)
        .map(|o| o.id)
        .collect();
    delete_encodings(state, &files).await?;
    let n = objects::delete_tree(&state.db, &bucket.id, &path.path).await?;
    let dir = object_file(&state.config, &bucket.id, &path.path);
    if let Err(e) = tokio::fs::remove_dir_all(&dir).await
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(path = %dir.display(), error = %e, "cannot remove a storage directory");
    }
    tracing::info!(bucket = %bucket.name, path = %path.path, rows = n, "storage directory deleted");
    recount(state, &bucket.id).await;
    Ok(n)
}

/// Delete a bucket with its files, index and encodings.
pub async fn delete_bucket(state: &AppState, bucket: &Bucket) -> ApiResult<()> {
    for item in vod_repo::list_by_bucket(&state.db, &bucket.id).await? {
        state.vod.delete_item(&item.id).await?;
    }
    buckets::delete(&state.db, &bucket.id).await?;
    let dir = bucket_dir(&state.config, &bucket.id);
    if let Err(e) = tokio::fs::remove_dir_all(&dir).await
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(path = %dir.display(), error = %e, "cannot remove a bucket directory");
    }
    state.vod.clear_access_cache();
    tracing::info!(bucket = %bucket.name, id = %bucket.id, "bucket deleted");
    Ok(())
}

/// Delete a user with its buckets (files, encodings), keys and row.
pub async fn delete_user(state: &AppState, user: &User) -> ApiResult<()> {
    for b in buckets::list_for_user(&state.db, &user.id).await? {
        delete_bucket(state, &b).await?;
    }
    for item in vod_repo::list_for_user(&state.db, &user.id).await? {
        state.vod.delete_item(&item.id).await?;
    }
    users::delete(&state.db, &user.id).await?;
    tracing::info!(user = %user.username, id = %user.id, "user deleted");
    Ok(())
}

/// What [`repair_user`] found.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct RepairReport {
    pub buckets: u64,
    pub files_checked: u64,
    /// Index rows whose size differed from the file on disk (corrected).
    pub sizes_fixed: u64,
    /// Index rows without a file on disk (left in place, logged).
    pub missing_files: u64,
    /// Encodings whose output size was re-measured.
    pub encodings_measured: u64,
}

/// Re-measure a user's files and encoding outputs on disk and recompute the bucket
/// counters (`POST /api/v1/users/{id}/recount`).
pub async fn repair_user(state: &AppState, user: &User) -> ApiResult<RepairReport> {
    let mut report = RepairReport::default();
    let list = buckets::list_for_user(&state.db, &user.id).await?;
    for b in &list {
        report.buckets += 1;
        for o in objects::list_tree(&state.db, &b.id, "/").await? {
            if o.is_dir {
                continue;
            }
            report.files_checked += 1;
            match tokio::fs::metadata(object_file(&state.config, &b.id, &o.path)).await {
                Ok(m) if m.len() != o.size => {
                    objects::set_size(&state.db, &o.id, m.len()).await?;
                    report.sizes_fixed += 1;
                }
                Ok(_) => {}
                Err(_) => {
                    tracing::warn!(bucket = %b.name, path = %o.path, "indexed file missing on disk");
                    report.missing_files += 1;
                }
            }
        }
    }
    for item in vod_repo::list_for_user(&state.db, &user.id).await? {
        let dir = state.vod.item_dir(&item.id);
        let bytes = tokio::task::spawn_blocking(move || crate::vod::dir_size(&dir))
            .await
            .unwrap_or(0);
        vod_repo::set_output_bytes(&state.db, &item.id, bytes).await?;
        report.encodings_measured += 1;
    }
    for b in &list {
        recount(state, &b.id).await;
    }
    Ok(report)
}

#[cfg(test)]
mod tests;
