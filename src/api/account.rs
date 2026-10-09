//! Account API (docs/ACCOUNTS.md §4): a user manages its keys, buckets and encodings
//! with a key holding the `account` scope; the admin token may call every route with
//! `?user=<id or username>`.
//!
//! - `GET /account` → [`AccountView`]
//! - `GET|POST /account/keys`, `PATCH|DELETE /account/keys/{kid}` (DELETE revokes)
//! - `GET|POST /account/buckets`, `GET|PATCH|DELETE /account/buckets/{id}`
//!   (`{id}` = bucket id or name; DELETE needs `?force=1` when the bucket is not empty),
//!   `POST /account/buckets/{id}/rotate-signing-key`, `POST /account/buckets/{id}/sign`
//! - `GET /account/encodings`, `GET|PATCH /account/encodings/{id}` (title, description,
//!   visibility; PATCH is an addition to the contract)
//! - `GET /account/events`: SSE `vod` / `vod_deleted` of the user's encodings + `heartbeat`
//!
//! The validation and the operations are shared with the admin routes of
//! [`super::users`] (`/users/{id}/keys`, `/users/{id}/buckets`, ...). Invalid input is a
//! `400 {"error", "field"}`.

use std::collections::HashSet;
use std::convert::Infallible;

use axum::extract::rejection::QueryRejection;
use axum::extract::{Extension, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::Event;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use tokio::sync::broadcast::error::RecvError;

use super::events::{HEARTBEAT, heartbeat_event, sse_response};
use super::principal::Principal;
use super::vod::{VodView, vod_view};
use super::{ApiJson, BaseUrl};
use crate::db::api_keys::{self, Access, ApiKey, Permission, SCOPE_ACCOUNT, SCOPE_STORAGE, SCOPES};
use crate::db::buckets::{self, Bucket};
use crate::db::objects;
use crate::db::users::{self, Usage, User};
use crate::db::vod::{self as vod_repo, VodItem};
use crate::error::{ApiError, ApiResult};
use crate::hub::HubEvent;
use crate::state::AppState;
use crate::storage::{self, SignedUrls, paths};

/// Longest bucket name.
pub const BUCKET_NAME_MAX: usize = 63;
/// Longest username.
pub const USERNAME_MAX: usize = 32;
const NAME_MAX: usize = 100;
const DESCRIPTION_MAX: usize = 1000;
const MAX_PERMISSIONS: usize = 100;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/account", get(account))
        .route("/account/keys", get(list_keys).post(create_key))
        .route(
            "/account/keys/{kid}",
            axum::routing::patch(patch_key).delete(revoke_key),
        )
        .route("/account/buckets", get(list_buckets).post(create_bucket))
        .route(
            "/account/buckets/{id}",
            get(get_bucket).patch(patch_bucket).delete(delete_bucket),
        )
        .route("/account/buckets/{id}/rotate-signing-key", post(rotate_key))
        .route("/account/buckets/{id}/sign", post(sign))
        .route("/account/encodings", get(list_encodings))
        .route(
            "/account/encodings/{id}",
            get(get_encoding).patch(patch_encoding),
        )
        .route("/account/events", get(events))
}

// ---------------------------------------------------------------------------
// Helpers shared with the admin routes
// ---------------------------------------------------------------------------

/// Slug `[a-z0-9_-]{2,max}`.
pub fn is_valid_slug(s: &str, max: usize) -> bool {
    (2..=max).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

/// `Option<Option<T>>` for PATCH fields: absent = `None`, `null` = `Some(None)`.
pub fn nullable<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

/// A user by id or username (404 otherwise).
pub async fn find_user(state: &AppState, id_or_name: &str) -> ApiResult<User> {
    let s = id_or_name.trim();
    let found = match crate::vod::parse_id(s) {
        Some(id) => users::get(&state.db, &id).await?,
        None => users::find_by_username(&state.db, s).await?,
    };
    found.ok_or_else(|| ApiError::not_found("user not found"))
}

/// `?user=` of the account routes (admin only).
#[derive(Debug, Default, Deserialize)]
pub struct AsUser {
    pub user: Option<String>,
}

/// The user an account route acts on: the key's owner (`account` scope required), or
/// for the admin token the user named by `?user=`.
pub async fn account_user(
    state: &AppState,
    principal: &Principal,
    as_user: Option<&str>,
) -> ApiResult<User> {
    match principal {
        Principal::Admin => {
            let name = as_user
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    ApiError::invalid(
                        "user",
                        "the admin token must name the user: add ?user=<id or username>",
                    )
                })?;
            find_user(state, name).await
        }
        Principal::User(p) => {
            if !p.has_scope(SCOPE_ACCOUNT) {
                return Err(ApiError::forbidden("this key lacks the account scope"));
            }
            if let Some(other) = as_user.map(str::trim).filter(|s| !s.is_empty())
                && other != p.user_id
                && other != p.username
            {
                return Err(ApiError::forbidden("a key can only manage its own account"));
            }
            users::get(&state.db, &p.user_id)
                .await?
                .ok_or(ApiError::Unauthorized)
        }
    }
}

async fn ctx(
    state: &AppState,
    principal: &Principal,
    q: Result<Query<AsUser>, QueryRejection>,
) -> ApiResult<User> {
    let Query(q) = q?;
    account_user(state, principal, q.user.as_deref()).await
}

/// A ladder field: `""`/`null` = none, else a configured ladder.
fn ladder_field(state: &AppState, field: &str, v: Option<String>) -> ApiResult<Option<String>> {
    match v.map(|l| l.trim().to_string()).filter(|l| !l.is_empty()) {
        None => Ok(None),
        Some(l) => {
            storage::validate_ladder(&state.config, field, &l)?;
            Ok(Some(l))
        }
    }
}

fn text_field(field: &str, v: &str, max: usize, required: bool) -> ApiResult<String> {
    let t = v.trim();
    if required && t.is_empty() {
        return Err(ApiError::invalid(
            field,
            format!("{field} must not be empty"),
        ));
    }
    if t.chars().count() > max {
        return Err(ApiError::invalid(
            field,
            format!("{field} is too long (max {max} characters)"),
        ));
    }
    Ok(t.to_string())
}

// ---------------------------------------------------------------------------
// Views
// ---------------------------------------------------------------------------

/// A key as listed (never the secret).
#[derive(Debug, Clone, Serialize)]
pub struct KeyView {
    #[serde(flatten)]
    pub key: ApiKey,
    /// `active` | `expired` | `revoked`
    pub state: &'static str,
}

impl From<ApiKey> for KeyView {
    fn from(key: ApiKey) -> Self {
        let state = if key.is_revoked() {
            "revoked"
        } else if key.is_expired(Utc::now()) {
            "expired"
        } else {
            "active"
        };
        Self { key, state }
    }
}

/// `201` of a key creation: the secret, shown this once.
#[derive(Debug, Clone, Serialize)]
pub struct CreatedKey {
    /// `rms_…`: store it now, it cannot be read again.
    pub key: String,
    #[serde(flatten)]
    pub view: KeyView,
}

/// A bucket with its storage URL (the signing key included: only its owner and the
/// admin see buckets).
#[derive(Debug, Clone, Serialize)]
pub struct BucketView {
    #[serde(flatten)]
    pub bucket: Bucket,
    /// `<base>/storage/<name>/`
    pub storage_url: String,
}

impl BucketView {
    pub fn new(base: &BaseUrl, bucket: Bucket) -> Self {
        Self {
            storage_url: storage::storage_url(base, &bucket, "/", true),
            bucket,
        }
    }
}

/// Limits of an account.
#[derive(Debug, Clone, Serialize)]
pub struct Limits {
    pub quota_bytes: u64,
    pub max_buckets: u32,
    pub max_object_mb: u64,
    pub ladders: Vec<String>,
    pub default_ladder: String,
    pub vod_available: bool,
}

/// The key the request was made with (user principals).
#[derive(Debug, Clone, Serialize)]
pub struct CurrentKey {
    pub id: String,
    pub scopes: Vec<String>,
}

/// `GET /account`.
#[derive(Debug, Clone, Serialize)]
pub struct AccountView {
    pub user: User,
    pub usage: AccountUsage,
    pub limits: Limits,
    pub key: Option<CurrentKey>,
}

/// Usage with the quota next to it.
#[derive(Debug, Clone, Serialize)]
pub struct AccountUsage {
    #[serde(flatten)]
    pub usage: Usage,
    pub quota_bytes: u64,
}

pub fn limits(state: &AppState, user: &User) -> Limits {
    Limits {
        quota_bytes: user.quota_bytes,
        max_buckets: user.max_buckets,
        max_object_mb: state.config.storage.max_object_mb,
        ladders: state
            .config
            .vod
            .ladders
            .iter()
            .map(|l| l.name.clone())
            .collect(),
        default_ladder: state.config.vod.default_ladder.clone(),
        vod_available: state.vod.available(),
    }
}

async fn account(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    q: Result<Query<AsUser>, QueryRejection>,
) -> ApiResult<Json<AccountView>> {
    let user = ctx(&state, &principal, q).await?;
    let usage = users::usage(&state.db, &user.id).await?;
    Ok(Json(AccountView {
        usage: AccountUsage {
            usage,
            quota_bytes: user.quota_bytes,
        },
        limits: limits(&state, &user),
        key: principal.user().map(|p| CurrentKey {
            id: p.key_id.clone(),
            scopes: p.scopes.clone(),
        }),
        user,
    }))
}

// ---------------------------------------------------------------------------
// Keys
// ---------------------------------------------------------------------------

/// A permission entry as sent by clients (validated into [`Permission`]).
#[derive(Debug, Clone, Deserialize)]
pub struct PermissionInput {
    pub bucket: String,
    #[serde(default)]
    pub prefix: Option<String>,
    pub access: String,
}

#[derive(Debug, Deserialize)]
pub struct CreateKeyBody {
    pub name: String,
    pub scopes: Vec<String>,
    #[serde(default)]
    pub permissions: Vec<PermissionInput>,
    #[serde(default)]
    pub expires_at: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct PatchKeyBody {
    pub name: Option<String>,
    pub scopes: Option<Vec<String>>,
    pub permissions: Option<Vec<PermissionInput>>,
    #[serde(default, deserialize_with = "nullable")]
    pub expires_at: Option<Option<String>>,
}

fn parse_scopes(scopes: &[String]) -> ApiResult<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for s in scopes {
        let s = s.trim().to_ascii_lowercase();
        if !SCOPES.contains(&s.as_str()) {
            return Err(ApiError::invalid(
                "scopes",
                format!("unknown scope \"{s}\" (account, storage)"),
            ));
        }
        if !out.contains(&s) {
            out.push(s);
        }
    }
    if out.is_empty() {
        return Err(ApiError::invalid(
            "scopes",
            "at least one scope is required (account, storage)",
        ));
    }
    Ok(out)
}

/// Validate permission entries: the bucket is `*` or one of the user's buckets (id or
/// name, stored as the id), the prefix a normalized path prefix (default `/`).
async fn parse_permissions(
    state: &AppState,
    user: &User,
    input: &[PermissionInput],
) -> ApiResult<Vec<Permission>> {
    if input.len() > MAX_PERMISSIONS {
        return Err(ApiError::invalid(
            "permissions",
            format!("at most {MAX_PERMISSIONS} permission entries"),
        ));
    }
    let own = buckets::list_for_user(&state.db, &user.id).await?;
    let mut out = Vec::with_capacity(input.len());
    for p in input {
        let b = p.bucket.trim();
        let bucket = if b == "*" {
            "*".to_string()
        } else {
            own.iter()
                .find(|x| x.id == b || x.name == b)
                .map(|x| x.id.clone())
                .ok_or_else(|| {
                    ApiError::invalid(
                        "permissions",
                        format!("bucket \"{b}\" is not one of this user's buckets"),
                    )
                })?
        };
        let prefix = paths::normalize_prefix(p.prefix.as_deref().unwrap_or("/"))
            .map_err(|e| ApiError::invalid("permissions", format!("prefix: {e}")))?;
        let access = match p.access.trim().to_ascii_lowercase().as_str() {
            "read" => Access::Read,
            "write" => Access::Write,
            "admin" => Access::Admin,
            other => {
                return Err(ApiError::invalid(
                    "permissions",
                    format!("unknown access \"{other}\" (read, write, admin)"),
                ));
            }
        };
        out.push(Permission {
            bucket,
            prefix,
            access,
        });
    }
    Ok(out)
}

fn parse_expiry(v: Option<&str>) -> ApiResult<Option<DateTime<Utc>>> {
    let Some(s) = v.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let t = DateTime::parse_from_rfc3339(s)
        .map_err(|_| ApiError::invalid("expires_at", "expires_at must be an RFC 3339 timestamp"))?
        .with_timezone(&Utc);
    if t <= Utc::now() {
        return Err(ApiError::invalid(
            "expires_at",
            "expires_at must be in the future",
        ));
    }
    Ok(Some(t))
}

fn check_storage_permissions(scopes: &[String], perms: &[Permission]) -> ApiResult<()> {
    let storage_only =
        scopes.iter().any(|s| s == SCOPE_STORAGE) && !scopes.iter().any(|s| s == SCOPE_ACCOUNT);
    if storage_only && perms.is_empty() {
        return Err(ApiError::invalid(
            "permissions",
            "a storage key needs at least one permission entry",
        ));
    }
    Ok(())
}

pub async fn do_list_keys(state: &AppState, user: &User) -> ApiResult<Vec<KeyView>> {
    Ok(api_keys::list_for_user(&state.db, &user.id)
        .await?
        .into_iter()
        .map(KeyView::from)
        .collect())
}

pub async fn do_create_key(
    state: &AppState,
    user: &User,
    body: CreateKeyBody,
) -> ApiResult<CreatedKey> {
    let name = text_field("name", &body.name, NAME_MAX, true)?;
    let scopes = parse_scopes(&body.scopes)?;
    let permissions = parse_permissions(state, user, &body.permissions).await?;
    check_storage_permissions(&scopes, &permissions)?;
    let expires_at = parse_expiry(body.expires_at.as_deref())?;
    let (key, plain) =
        api_keys::create(&state.db, &user.id, &name, scopes, permissions, expires_at).await?;
    tracing::info!(user = %user.username, key = %key.key_prefix, scopes = ?key.scopes, "API key created");
    Ok(CreatedKey {
        key: plain,
        view: key.into(),
    })
}

pub async fn do_patch_key(
    state: &AppState,
    user: &User,
    kid: &str,
    body: PatchKeyBody,
) -> ApiResult<KeyView> {
    let mut key = api_keys::get(&state.db, &user.id, kid.trim())
        .await?
        .ok_or_else(|| ApiError::not_found("key not found"))?;
    if key.is_revoked() {
        return Err(ApiError::conflict("the key is revoked"));
    }
    if let Some(n) = &body.name {
        key.name = text_field("name", n, NAME_MAX, true)?;
    }
    if let Some(s) = &body.scopes {
        key.scopes = parse_scopes(s)?;
    }
    if let Some(p) = &body.permissions {
        key.permissions = parse_permissions(state, user, p).await?;
    }
    check_storage_permissions(&key.scopes, &key.permissions)?;
    if let Some(e) = &body.expires_at {
        key.expires_at = parse_expiry(e.as_deref())?;
    }
    let stored = api_keys::update(&state.db, &key)
        .await?
        .ok_or_else(|| ApiError::not_found("key not found"))?;
    Ok(stored.into())
}

pub async fn do_revoke_key(state: &AppState, user: &User, kid: &str) -> ApiResult<StatusCode> {
    let key = api_keys::revoke(&state.db, &user.id, kid.trim())
        .await?
        .ok_or_else(|| ApiError::not_found("key not found"))?;
    tracing::info!(user = %user.username, key = %key.key_prefix, "API key revoked");
    Ok(StatusCode::NO_CONTENT)
}

async fn list_keys(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    q: Result<Query<AsUser>, QueryRejection>,
) -> ApiResult<Json<Vec<KeyView>>> {
    let user = ctx(&state, &principal, q).await?;
    Ok(Json(do_list_keys(&state, &user).await?))
}

async fn create_key(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    q: Result<Query<AsUser>, QueryRejection>,
    ApiJson(body): ApiJson<CreateKeyBody>,
) -> ApiResult<(StatusCode, Json<CreatedKey>)> {
    let user = ctx(&state, &principal, q).await?;
    Ok((
        StatusCode::CREATED,
        Json(do_create_key(&state, &user, body).await?),
    ))
}

async fn patch_key(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(kid): Path<String>,
    q: Result<Query<AsUser>, QueryRejection>,
    ApiJson(body): ApiJson<PatchKeyBody>,
) -> ApiResult<Json<KeyView>> {
    let user = ctx(&state, &principal, q).await?;
    Ok(Json(do_patch_key(&state, &user, &kid, body).await?))
}

async fn revoke_key(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(kid): Path<String>,
    q: Result<Query<AsUser>, QueryRejection>,
) -> ApiResult<StatusCode> {
    let user = ctx(&state, &principal, q).await?;
    do_revoke_key(&state, &user, &kid).await
}

// ---------------------------------------------------------------------------
// Buckets
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CreateBucketBody {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub default_ladder: Option<String>,
    #[serde(default)]
    pub auto_encode: Option<bool>,
    #[serde(default)]
    pub public_playback: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
pub struct PatchBucketBody {
    pub name: Option<String>,
    pub description: Option<String>,
    #[serde(default, deserialize_with = "nullable")]
    pub default_ladder: Option<Option<String>>,
    pub auto_encode: Option<bool>,
    pub public_playback: Option<bool>,
}

fn bucket_name(v: &str) -> ApiResult<String> {
    let n = v.trim();
    if !is_valid_slug(n, BUCKET_NAME_MAX) {
        return Err(ApiError::invalid(
            "name",
            format!(
                "the bucket name must be 2-{BUCKET_NAME_MAX} characters among a-z, 0-9, - and _"
            ),
        ));
    }
    Ok(n.to_string())
}

/// A bucket of `user` by id or name (404 otherwise).
pub async fn user_bucket(state: &AppState, user: &User, id_or_name: &str) -> ApiResult<Bucket> {
    let s = id_or_name.trim();
    let found = match crate::vod::parse_id(s) {
        Some(id) => buckets::get(&state.db, &id).await?,
        None => buckets::find_by_name(&state.db, s).await?,
    };
    found
        .filter(|b| b.user_id == user.id)
        .ok_or_else(|| ApiError::not_found("bucket not found"))
}

fn name_taken(e: crate::db::DbError) -> ApiError {
    if e.is_unique_violation() {
        ApiError::conflict("this bucket name is already taken")
    } else {
        e.into()
    }
}

pub async fn do_list_buckets(
    state: &AppState,
    base: &BaseUrl,
    user: &User,
) -> ApiResult<Vec<BucketView>> {
    Ok(buckets::list_for_user(&state.db, &user.id)
        .await?
        .into_iter()
        .map(|b| BucketView::new(base, b))
        .collect())
}

pub async fn do_create_bucket(
    state: &AppState,
    base: &BaseUrl,
    user: &User,
    body: CreateBucketBody,
) -> ApiResult<BucketView> {
    let name = bucket_name(&body.name)?;
    let description = text_field(
        "description",
        body.description.as_deref().unwrap_or(""),
        DESCRIPTION_MAX,
        false,
    )?;
    let default_ladder = ladder_field(state, "default_ladder", body.default_ladder)?;
    let count = buckets::count_for_user(&state.db, &user.id).await?;
    if count >= u64::from(user.max_buckets) {
        return Err(ApiError::conflict(format!(
            "bucket limit reached (max_buckets = {})",
            user.max_buckets
        )));
    }
    if buckets::find_by_name(&state.db, &name).await?.is_some() {
        return Err(ApiError::conflict("this bucket name is already taken"));
    }
    let mut b = Bucket::new(&user.id, &name);
    b.description = description;
    b.default_ladder = default_ladder;
    b.auto_encode = body.auto_encode.unwrap_or(true);
    b.public_playback = body.public_playback.unwrap_or(true);
    buckets::insert(&state.db, &b).await.map_err(name_taken)?;
    tokio::fs::create_dir_all(storage::bucket_dir(&state.config, &b.id))
        .await
        .map_err(|e| ApiError::internal(format!("creating the bucket directory: {e}")))?;
    tracing::info!(user = %user.username, bucket = %b.name, id = %b.id, "bucket created");
    Ok(BucketView::new(base, b))
}

async fn list_buckets(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    headers: HeaderMap,
    q: Result<Query<AsUser>, QueryRejection>,
) -> ApiResult<Json<Vec<BucketView>>> {
    let user = ctx(&state, &principal, q).await?;
    let base = BaseUrl::from_request(&state.config, &headers);
    Ok(Json(do_list_buckets(&state, &base, &user).await?))
}

async fn create_bucket(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    headers: HeaderMap,
    q: Result<Query<AsUser>, QueryRejection>,
    ApiJson(body): ApiJson<CreateBucketBody>,
) -> ApiResult<(StatusCode, Json<BucketView>)> {
    let user = ctx(&state, &principal, q).await?;
    let base = BaseUrl::from_request(&state.config, &headers);
    Ok((
        StatusCode::CREATED,
        Json(do_create_bucket(&state, &base, &user, body).await?),
    ))
}

async fn get_bucket(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
    headers: HeaderMap,
    q: Result<Query<AsUser>, QueryRejection>,
) -> ApiResult<Json<BucketView>> {
    let user = ctx(&state, &principal, q).await?;
    let b = user_bucket(&state, &user, &id).await?;
    Ok(Json(BucketView::new(
        &BaseUrl::from_request(&state.config, &headers),
        b,
    )))
}

async fn patch_bucket(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
    headers: HeaderMap,
    q: Result<Query<AsUser>, QueryRejection>,
    ApiJson(body): ApiJson<PatchBucketBody>,
) -> ApiResult<Json<BucketView>> {
    let user = ctx(&state, &principal, q).await?;
    let mut b = user_bucket(&state, &user, &id).await?;
    if let Some(n) = &body.name {
        let n = bucket_name(n)?;
        if n != b.name && buckets::find_by_name(&state.db, &n).await?.is_some() {
            return Err(ApiError::conflict("this bucket name is already taken"));
        }
        b.name = n;
    }
    if let Some(d) = &body.description {
        b.description = text_field("description", d, DESCRIPTION_MAX, false)?;
    }
    if let Some(l) = body.default_ladder {
        b.default_ladder = ladder_field(&state, "default_ladder", l)?;
    }
    if let Some(v) = body.auto_encode {
        b.auto_encode = v;
    }
    if let Some(v) = body.public_playback {
        b.public_playback = v;
    }
    let stored = buckets::update(&state.db, &b)
        .await
        .map_err(name_taken)?
        .ok_or_else(|| ApiError::not_found("bucket not found"))?;
    Ok(Json(BucketView::new(
        &BaseUrl::from_request(&state.config, &headers),
        stored,
    )))
}

#[derive(Debug, Default, Deserialize)]
pub struct ForceQuery {
    pub user: Option<String>,
    pub force: Option<String>,
}

pub fn is_forced(v: Option<&str>) -> bool {
    matches!(
        v.map(|s| s.trim().to_ascii_lowercase()).as_deref(),
        Some("1" | "true" | "yes")
    )
}

pub async fn do_delete_bucket(
    state: &AppState,
    bucket: &Bucket,
    force: bool,
) -> ApiResult<StatusCode> {
    if !force && objects::count(&state.db, &bucket.id).await? > 0 {
        return Err(ApiError::conflict(
            "the bucket is not empty: add ?force=1 to delete it with its files and encodings",
        ));
    }
    storage::delete_bucket(state, bucket).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_bucket(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
    q: Result<Query<ForceQuery>, QueryRejection>,
) -> ApiResult<StatusCode> {
    let Query(q) = q?;
    let user = account_user(&state, &principal, q.user.as_deref()).await?;
    let b = user_bucket(&state, &user, &id).await?;
    do_delete_bucket(&state, &b, is_forced(q.force.as_deref())).await
}

async fn rotate_key(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
    headers: HeaderMap,
    q: Result<Query<AsUser>, QueryRejection>,
) -> ApiResult<Json<BucketView>> {
    let user = ctx(&state, &principal, q).await?;
    let b = user_bucket(&state, &user, &id).await?;
    let rotated = buckets::rotate_signing_key(&state.db, &b.id)
        .await?
        .ok_or_else(|| ApiError::not_found("bucket not found"))?;
    state.vod.clear_access_cache();
    tracing::info!(bucket = %b.name, "bucket signing key rotated");
    Ok(Json(BucketView::new(
        &BaseUrl::from_request(&state.config, &headers),
        rotated,
    )))
}

#[derive(Debug, Deserialize)]
pub struct SignBody {
    pub path: String,
    #[serde(default)]
    pub expires_secs: Option<u64>,
}

async fn sign(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
    headers: HeaderMap,
    q: Result<Query<AsUser>, QueryRejection>,
    ApiJson(body): ApiJson<SignBody>,
) -> ApiResult<Json<SignedUrls>> {
    let user = ctx(&state, &principal, q).await?;
    let b = user_bucket(&state, &user, &id).await?;
    let path = paths::parse_plain(body.path.trim())
        .map_err(|e| ApiError::invalid("path", e.to_string()))?;
    let secs = storage::expires_secs("expires_secs", body.expires_secs)?;
    let object = objects::get(&state.db, &b.id, &path.path)
        .await?
        .filter(|o| !o.is_dir)
        .ok_or_else(|| ApiError::not_found("file not found"))?;
    let item = vod_repo::find_by_object(&state.db, &object.id)
        .await?
        .ok_or_else(|| ApiError::conflict("this file has no encoding"))?;
    let base = BaseUrl::from_request(&state.config, &headers);
    Ok(Json(storage::signed_urls(&base, &item.id, &b, secs)))
}

// ---------------------------------------------------------------------------
// Encodings
// ---------------------------------------------------------------------------

pub async fn do_list_encodings(
    state: &AppState,
    base: &BaseUrl,
    user: &User,
) -> ApiResult<Vec<VodView>> {
    Ok(vod_repo::list_for_user(&state.db, &user.id)
        .await?
        .into_iter()
        .map(|i| vod_view(state, base, i))
        .collect())
}

async fn list_encodings(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    headers: HeaderMap,
    q: Result<Query<AsUser>, QueryRejection>,
) -> ApiResult<Json<Vec<VodView>>> {
    let user = ctx(&state, &principal, q).await?;
    let base = BaseUrl::from_request(&state.config, &headers);
    Ok(Json(do_list_encodings(&state, &base, &user).await?))
}

async fn user_encoding(state: &AppState, user: &User, id: &str) -> ApiResult<VodItem> {
    let id = crate::vod::parse_id(id).ok_or_else(|| ApiError::not_found("video not found"))?;
    vod_repo::get(&state.db, &id)
        .await?
        .filter(|i| i.user_id.as_deref() == Some(user.id.as_str()))
        .ok_or_else(|| ApiError::not_found("video not found"))
}

async fn get_encoding(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
    headers: HeaderMap,
    q: Result<Query<AsUser>, QueryRejection>,
) -> ApiResult<Json<VodView>> {
    let user = ctx(&state, &principal, q).await?;
    let item = user_encoding(&state, &user, &id).await?;
    Ok(Json(vod_view(
        &state,
        &BaseUrl::from_request(&state.config, &headers),
        item,
    )))
}

#[derive(Debug, Default, Deserialize)]
struct PatchEncodingBody {
    title: Option<String>,
    description: Option<String>,
    visibility: Option<String>,
}

async fn patch_encoding(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
    headers: HeaderMap,
    q: Result<Query<AsUser>, QueryRejection>,
    ApiJson(body): ApiJson<PatchEncodingBody>,
) -> ApiResult<Json<VodView>> {
    let user = ctx(&state, &principal, q).await?;
    let mut item = user_encoding(&state, &user, &id).await?;
    let title = body
        .title
        .as_deref()
        .map(|t| text_field("title", t, 200, true))
        .transpose()?;
    let description = body
        .description
        .as_deref()
        .map(|d| text_field("description", d, 5000, false))
        .transpose()?;
    let visibility = body
        .visibility
        .as_deref()
        .map(|v| storage::parse_visibility("visibility", v))
        .transpose()?;
    if title.is_some() || description.is_some() {
        item = vod_repo::update_meta(&state.db, &item.id, title, description)
            .await?
            .ok_or_else(|| ApiError::not_found("video not found"))?;
    }
    if let Some(v) = visibility {
        item = vod_repo::set_visibility(&state.db, &item.id, &v)
            .await?
            .ok_or_else(|| ApiError::not_found("video not found"))?;
    }
    state.vod.changed(&item.id);
    Ok(Json(vod_view(
        &state,
        &BaseUrl::from_request(&state.config, &headers),
        item,
    )))
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// `GET /account/events`: `vod` (a [`VodView`]) when one of the user's encodings is
/// created or changes, `vod_deleted` `{id}` when it is deleted, `heartbeat` every 15 s.
async fn events(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    headers: HeaderMap,
    q: Result<Query<AsUser>, QueryRejection>,
) -> ApiResult<Response> {
    let user = ctx(&state, &principal, q).await?;
    let mut known: HashSet<String> = vod_repo::list_for_user(&state.db, &user.id)
        .await?
        .into_iter()
        .map(|i| i.id)
        .collect();
    let mut rx = state.hub.subscribe_events();
    let shutdown = state.shutdown.clone();
    let base = BaseUrl::from_request(&state.config, &headers);
    let stream = async_stream::stream! {
        yield Ok::<Event, Infallible>(heartbeat_event());
        let mut tick = tokio::time::interval(HEARTBEAT);
        tick.tick().await;
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = tick.tick() => yield Ok(heartbeat_event()),
                r = rx.recv() => match r {
                    Ok(HubEvent::VodChanged { id }) => {
                        match vod_repo::get(&state.db, &id).await {
                            Ok(Some(item)) if item.user_id.as_deref() == Some(user.id.as_str()) => {
                                known.insert(id);
                                if let Ok(data) = serde_json::to_string(&vod_view(&state, &base, item)) {
                                    yield Ok(Event::default().event("vod").data(data));
                                }
                            }
                            Ok(None) if known.remove(&id) => {
                                yield Ok(Event::default()
                                    .event("vod_deleted")
                                    .data(serde_json::json!({ "id": id }).to_string()));
                            }
                            Ok(_) => {}
                            Err(e) => tracing::warn!(error = %e, "account SSE: failed to load video"),
                        }
                    }
                    Ok(_) => {}
                    Err(RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "account SSE client lagging on hub events");
                    }
                    Err(RecvError::Closed) => break,
                },
            }
        }
    };
    Ok(sse_response(stream).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_and_scopes() {
        assert!(is_valid_slug("ab", 32));
        assert!(is_valid_slug("my-bucket_01", 63));
        assert!(!is_valid_slug("a", 32));
        assert!(!is_valid_slug("Abc", 32));
        assert!(!is_valid_slug("a b", 32));
        assert!(!is_valid_slug(&"a".repeat(33), 32));
        assert_eq!(
            parse_scopes(&["Storage".into(), "account".into(), "storage".into()]).unwrap(),
            ["storage", "account"]
        );
        assert!(parse_scopes(&[]).is_err());
        assert!(
            matches!(parse_scopes(&["root".into()]), Err(ApiError::Invalid { field, .. }) if field == "scopes")
        );
        assert!(parse_expiry(None).unwrap().is_none());
        assert!(parse_expiry(Some("2000-01-01T00:00:00Z")).is_err());
        assert!(parse_expiry(Some("nope")).is_err());
        assert!(
            parse_expiry(Some("2999-01-01T00:00:00Z"))
                .unwrap()
                .is_some()
        );
        assert!(
            is_forced(Some("1"))
                && is_forced(Some("TRUE"))
                && !is_forced(None)
                && !is_forced(Some("0"))
        );
        let p: PatchKeyBody = serde_json::from_str(r#"{"expires_at":null}"#).unwrap();
        assert_eq!(p.expires_at, Some(None));
        let p: PatchKeyBody = serde_json::from_str(r#"{}"#).unwrap();
        assert_eq!(p.expires_at, None);
    }
}
