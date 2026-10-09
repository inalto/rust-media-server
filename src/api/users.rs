//! Admin user management (docs/ACCOUNTS.md §4), behind the admin token:
//!
//! - `GET /users` → `[UserView]`, `POST /users` `{username, display_name?, email?,
//!   status?, quota_bytes?, max_buckets?, default_ladder?}` → `201 UserView`
//! - `GET|PATCH|DELETE /users/{id}` (`{id}` = id or username; DELETE needs `?force=1`
//!   when the user owns buckets and then removes everything: buckets, files,
//!   encodings, keys)
//! - `GET|POST /users/{id}/keys`, `PATCH|DELETE /users/{id}/keys/{kid}`
//! - `GET|POST /users/{id}/buckets`
//! - `GET /users/{id}/usage` → [`UsageView`], `GET /users/{id}/encodings` → `[VodView]`
//! - `POST /users/{id}/recount` → re-measure files/outputs and recompute the counters
//!
//! Every `/account/...` route is also open to the admin with `?user=<id>`.

use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use super::account::{
    self, BucketView, CreateBucketBody, CreateKeyBody, CreatedKey, ForceQuery, KeyView,
    PatchKeyBody, find_user, is_forced, is_valid_slug, nullable,
};
use super::vod::VodView;
use super::{ApiJson, BaseUrl};
use crate::db::buckets;
use crate::db::users::{self, STATUSES, Usage, User};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use crate::storage::{self, RepairReport};

const DISPLAY_NAME_MAX: usize = 100;
const EMAIL_MAX: usize = 254;
const MAX_BUCKETS_LIMIT: u32 = 10_000;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/users", get(list).post(create))
        .route("/users/{id}", get(get_one).patch(patch).delete(delete))
        .route("/users/{id}/keys", get(list_keys).post(create_key))
        .route(
            "/users/{id}/keys/{kid}",
            axum::routing::patch(patch_key).delete(revoke_key),
        )
        .route("/users/{id}/buckets", get(list_buckets).post(create_bucket))
        .route("/users/{id}/usage", get(usage))
        .route("/users/{id}/encodings", get(encodings))
        .route("/users/{id}/recount", post(recount))
}

/// A user with its usage.
#[derive(Debug, Clone, Serialize)]
pub struct UserView {
    #[serde(flatten)]
    pub user: User,
    pub usage: Usage,
}

/// `GET /users/{id}/usage`.
#[derive(Debug, Clone, Serialize)]
pub struct UsageView {
    pub user_id: String,
    pub quota_bytes: u64,
    pub max_buckets: u32,
    pub usage: Usage,
    pub buckets: Vec<BucketUsage>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BucketUsage {
    pub id: String,
    pub name: String,
    pub bytes_used: u64,
    pub objects_count: u64,
}

/// `POST /users/{id}/recount`.
#[derive(Debug, Clone, Serialize)]
pub struct RecountView {
    pub report: RepairReport,
    pub usage: Usage,
}

#[derive(Debug, Deserialize)]
struct CreateBody {
    username: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    quota_bytes: Option<u64>,
    #[serde(default)]
    max_buckets: Option<u32>,
    #[serde(default)]
    default_ladder: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct PatchBody {
    username: Option<String>,
    display_name: Option<String>,
    #[serde(default, deserialize_with = "nullable")]
    email: Option<Option<String>>,
    status: Option<String>,
    quota_bytes: Option<u64>,
    max_buckets: Option<u32>,
    #[serde(default, deserialize_with = "nullable")]
    default_ladder: Option<Option<String>>,
}

fn username(v: &str) -> ApiResult<String> {
    let u = v.trim();
    if !is_valid_slug(u, account::USERNAME_MAX) {
        return Err(ApiError::invalid(
            "username",
            "the username must be 2-32 characters among a-z, 0-9, - and _",
        ));
    }
    Ok(u.to_string())
}

fn display_name(v: &str) -> ApiResult<String> {
    let d = v.trim();
    if d.chars().count() > DISPLAY_NAME_MAX {
        return Err(ApiError::invalid(
            "display_name",
            format!("display_name is too long (max {DISPLAY_NAME_MAX} characters)"),
        ));
    }
    Ok(d.to_string())
}

fn email(v: Option<&str>) -> ApiResult<Option<String>> {
    let Some(e) = v.map(str::trim).filter(|e| !e.is_empty()) else {
        return Ok(None);
    };
    let valid = e.len() <= EMAIL_MAX
        && e.split_once('@')
            .is_some_and(|(l, d)| !l.is_empty() && !d.is_empty() && !d.contains('@'))
        && !e.chars().any(|c| c.is_whitespace() || c.is_control());
    if !valid {
        return Err(ApiError::invalid("email", "invalid email address"));
    }
    Ok(Some(e.to_string()))
}

fn status(v: &str) -> ApiResult<String> {
    let s = v.trim().to_ascii_lowercase();
    if STATUSES.contains(&s.as_str()) {
        Ok(s)
    } else {
        Err(ApiError::invalid(
            "status",
            "status must be active or suspended",
        ))
    }
}

fn max_buckets(v: u32) -> ApiResult<u32> {
    if v > MAX_BUCKETS_LIMIT {
        return Err(ApiError::invalid(
            "max_buckets",
            format!("max_buckets must be at most {MAX_BUCKETS_LIMIT}"),
        ));
    }
    Ok(v)
}

fn ladder(state: &AppState, v: Option<String>) -> ApiResult<Option<String>> {
    match v.map(|l| l.trim().to_string()).filter(|l| !l.is_empty()) {
        None => Ok(None),
        Some(l) => {
            storage::validate_ladder(&state.config, "default_ladder", &l)?;
            Ok(Some(l))
        }
    }
}

async fn view(state: &AppState, user: User) -> ApiResult<UserView> {
    let usage = users::usage(&state.db, &user.id).await?;
    Ok(UserView { user, usage })
}

async fn list(State(state): State<AppState>) -> ApiResult<Json<Vec<UserView>>> {
    let mut usage = users::usage_all(&state.db).await?;
    Ok(Json(
        users::list(&state.db)
            .await?
            .into_iter()
            .map(|u| UserView {
                usage: usage.remove(&u.id).unwrap_or_default(),
                user: u,
            })
            .collect(),
    ))
}

async fn create(
    State(state): State<AppState>,
    ApiJson(body): ApiJson<CreateBody>,
) -> ApiResult<(StatusCode, Json<UserView>)> {
    let name = username(&body.username)?;
    let mut user = User::new(
        &name,
        &display_name(body.display_name.as_deref().unwrap_or(""))?,
    );
    user.email = email(body.email.as_deref())?;
    if let Some(s) = &body.status {
        user.status = status(s)?;
    }
    user.quota_bytes = body.quota_bytes.unwrap_or(0);
    if let Some(m) = body.max_buckets {
        user.max_buckets = max_buckets(m)?;
    }
    user.default_ladder = ladder(&state, body.default_ladder)?;
    if users::find_by_username(&state.db, &name).await?.is_some() {
        return Err(ApiError::conflict("this username is already taken"));
    }
    users::insert(&state.db, &user).await.map_err(|e| {
        if e.is_unique_violation() {
            ApiError::conflict("this username is already taken")
        } else {
            e.into()
        }
    })?;
    tracing::info!(user = %user.username, id = %user.id, "user created");
    Ok((StatusCode::CREATED, Json(view(&state, user).await?)))
}

async fn get_one(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<UserView>> {
    let user = find_user(&state, &id).await?;
    Ok(Json(view(&state, user).await?))
}

async fn patch(
    State(state): State<AppState>,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<PatchBody>,
) -> ApiResult<Json<UserView>> {
    let mut user = find_user(&state, &id).await?;
    if let Some(u) = &body.username {
        let u = username(u)?;
        if u != user.username && users::find_by_username(&state.db, &u).await?.is_some() {
            return Err(ApiError::conflict("this username is already taken"));
        }
        user.username = u;
    }
    if let Some(d) = &body.display_name {
        let d = display_name(d)?;
        user.display_name = if d.is_empty() {
            user.username.clone()
        } else {
            d
        };
    }
    if let Some(e) = &body.email {
        user.email = email(e.as_deref())?;
    }
    if let Some(s) = &body.status {
        user.status = status(s)?;
    }
    if let Some(q) = body.quota_bytes {
        user.quota_bytes = q;
    }
    if let Some(m) = body.max_buckets {
        user.max_buckets = max_buckets(m)?;
    }
    if let Some(l) = body.default_ladder {
        user.default_ladder = ladder(&state, l)?;
    }
    let stored = users::update(&state.db, &user)
        .await
        .map_err(|e| {
            if e.is_unique_violation() {
                ApiError::conflict("this username is already taken")
            } else {
                e.into()
            }
        })?
        .ok_or_else(|| ApiError::not_found("user not found"))?;
    Ok(Json(view(&state, stored).await?))
}

async fn delete(
    State(state): State<AppState>,
    Path(id): Path<String>,
    q: Result<Query<ForceQuery>, QueryRejection>,
) -> ApiResult<StatusCode> {
    let Query(q) = q?;
    let user = find_user(&state, &id).await?;
    if !is_forced(q.force.as_deref()) && buckets::count_for_user(&state.db, &user.id).await? > 0 {
        return Err(ApiError::conflict(
            "the user owns buckets: add ?force=1 to delete it with its buckets, files, encodings and keys",
        ));
    }
    storage::delete_user(&state, &user).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_keys(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Vec<KeyView>>> {
    let user = find_user(&state, &id).await?;
    Ok(Json(account::do_list_keys(&state, &user).await?))
}

async fn create_key(
    State(state): State<AppState>,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<CreateKeyBody>,
) -> ApiResult<(StatusCode, Json<CreatedKey>)> {
    let user = find_user(&state, &id).await?;
    Ok((
        StatusCode::CREATED,
        Json(account::do_create_key(&state, &user, body).await?),
    ))
}

async fn patch_key(
    State(state): State<AppState>,
    Path((id, kid)): Path<(String, String)>,
    ApiJson(body): ApiJson<PatchKeyBody>,
) -> ApiResult<Json<KeyView>> {
    let user = find_user(&state, &id).await?;
    Ok(Json(
        account::do_patch_key(&state, &user, &kid, body).await?,
    ))
}

async fn revoke_key(
    State(state): State<AppState>,
    Path((id, kid)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    let user = find_user(&state, &id).await?;
    account::do_revoke_key(&state, &user, &kid).await
}

async fn list_buckets(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<Vec<BucketView>>> {
    let user = find_user(&state, &id).await?;
    let base = BaseUrl::from_request(&state.config, &headers);
    Ok(Json(account::do_list_buckets(&state, &base, &user).await?))
}

async fn create_bucket(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    ApiJson(body): ApiJson<CreateBucketBody>,
) -> ApiResult<(StatusCode, Json<BucketView>)> {
    let user = find_user(&state, &id).await?;
    let base = BaseUrl::from_request(&state.config, &headers);
    Ok((
        StatusCode::CREATED,
        Json(account::do_create_bucket(&state, &base, &user, body).await?),
    ))
}

async fn usage(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<UsageView>> {
    let user = find_user(&state, &id).await?;
    let usage = users::usage(&state.db, &user.id).await?;
    let buckets = buckets::list_for_user(&state.db, &user.id)
        .await?
        .into_iter()
        .map(|b| BucketUsage {
            id: b.id,
            name: b.name,
            bytes_used: b.bytes_used,
            objects_count: b.objects_count,
        })
        .collect();
    Ok(Json(UsageView {
        user_id: user.id,
        quota_bytes: user.quota_bytes,
        max_buckets: user.max_buckets,
        usage,
        buckets,
    }))
}

async fn encodings(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<Vec<VodView>>> {
    let user = find_user(&state, &id).await?;
    let base = BaseUrl::from_request(&state.config, &headers);
    Ok(Json(
        account::do_list_encodings(&state, &base, &user).await?,
    ))
}

async fn recount(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<RecountView>> {
    let user = find_user(&state, &id).await?;
    let report = storage::repair_user(&state, &user).await?;
    let usage = users::usage(&state.db, &user.id).await?;
    tracing::info!(user = %user.username, ?report, "usage recounted");
    Ok(Json(RecountView { report, usage }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_validation() {
        assert_eq!(username(" alice ").unwrap(), "alice");
        assert!(
            matches!(username("A"), Err(ApiError::Invalid { field, .. }) if field == "username")
        );
        assert_eq!(email(Some(" a@b.c ")).unwrap().as_deref(), Some("a@b.c"));
        assert_eq!(email(Some("")).unwrap(), None);
        assert!(email(Some("nope")).is_err());
        assert!(email(Some("a@b@c")).is_err());
        assert!(email(Some("a b@c")).is_err());
        assert_eq!(status("Suspended").unwrap(), "suspended");
        assert!(status("deleted").is_err());
        assert!(max_buckets(10_001).is_err());
        assert!(display_name(&"x".repeat(101)).is_err());
        let p: PatchBody =
            serde_json::from_str(r#"{"email":null,"default_ladder":"mobile"}"#).unwrap();
        assert_eq!(p.email, Some(None));
        assert_eq!(p.default_ladder, Some(Some("mobile".into())));
    }
}
