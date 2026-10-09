//! Broadcast management endpoints (ARCHITECTURE.md §8.1).

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;

use super::{
    ApiJson, BroadcastView, EmbedInfo, ObsSettings, PlaybackUrls, ViewContext, poster_path_for,
};
use crate::db::Broadcast;
use crate::db::broadcasts::{self as repo, BroadcastPatch, NewBroadcast, is_valid_name, slugify};
use crate::error::{ApiError, ApiResult};
use crate::hub::HubEvent;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/broadcasts", get(list).post(create))
        .route(
            "/broadcasts/{name}",
            get(get_one).patch(update).delete(delete_one),
        )
        .route("/broadcasts/{name}/regenerate-key", post(regenerate_key))
        .route("/broadcasts/{name}/pause", post(pause))
        .route("/broadcasts/{name}/resume", post(resume))
        .route("/broadcasts/{name}/disconnect", post(disconnect))
        .route("/broadcasts/{name}/embed", get(embed))
}

const TITLE_MAX: usize = 200;
const DESCRIPTION_MAX: usize = 5000;
const WAITING_TEXT_MAX: usize = 500;

#[derive(Debug, Deserialize)]
pub struct CreateBody {
    pub name: Option<String>,
    pub title: Option<String>,
    pub description: Option<String>,
    pub record: Option<bool>,
    pub enable_flv: Option<bool>,
    pub enable_hls: Option<bool>,
    pub enable_dash: Option<bool>,
    pub enable_webrtc: Option<bool>,
    pub waiting_text: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct PatchBody {
    /// Not changeable; accepted only if equal to the current name.
    pub name: Option<String>,
    pub title: Option<String>,
    pub description: Option<String>,
    pub record: Option<bool>,
    pub enable_flv: Option<bool>,
    pub enable_hls: Option<bool>,
    pub enable_dash: Option<bool>,
    pub enable_webrtc: Option<bool>,
    pub waiting_text: Option<String>,
}

fn check_len(field: &str, value: &str, max: usize) -> ApiResult<()> {
    if value.chars().count() > max {
        return Err(ApiError::bad_request(format!(
            "{field} is too long (max {max} characters)"
        )));
    }
    Ok(())
}

fn clean_title(t: &str) -> ApiResult<String> {
    let t = t.trim();
    if t.is_empty() {
        return Err(ApiError::bad_request("title must not be empty"));
    }
    check_len("title", t, TITLE_MAX)?;
    Ok(t.to_string())
}

/// Load a broadcast by name from a path parameter (404 if invalid or unknown).
pub(crate) async fn load(state: &AppState, name: &str) -> ApiResult<Broadcast> {
    if !is_valid_name(name) {
        return Err(ApiError::broadcast_not_found());
    }
    repo::find_by_name(&state.db, name)
        .await?
        .ok_or_else(ApiError::broadcast_not_found)
}

async fn view_of(
    state: &AppState,
    headers: &HeaderMap,
    b: Broadcast,
) -> ApiResult<Json<BroadcastView>> {
    let ctx = ViewContext::load(state, headers).await?;
    Ok(Json(ctx.view(state, b)))
}

fn changed(state: &AppState, name: &str) {
    state.hub.emit(HubEvent::BroadcastUpdated {
        name: name.to_string(),
    });
}

async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Vec<BroadcastView>>> {
    let ctx = ViewContext::load(&state, &headers).await?;
    let all = repo::list(&state.db).await?;
    Ok(Json(all.into_iter().map(|b| ctx.view(&state, b)).collect()))
}

/// Pick a free slug derived from the title (`title`, `title-2`, `title-3`, ...).
async fn unique_slug(state: &AppState, title: &str) -> ApiResult<String> {
    let mut base = slugify(title);
    if base.is_empty() {
        base = format!("live-{}", &uuid::Uuid::new_v4().simple().to_string()[..6]);
    }
    if !repo::name_exists(&state.db, &base).await? {
        return Ok(base);
    }
    for n in 2..1000 {
        let suffix = format!("-{n}");
        let mut candidate = base.clone();
        candidate.truncate(crate::db::broadcasts::NAME_MAX_LEN - suffix.len());
        candidate.push_str(&suffix);
        if !repo::name_exists(&state.db, &candidate).await? {
            return Ok(candidate);
        }
    }
    Err(ApiError::conflict(
        "could not find a free name; please choose one",
    ))
}

async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    ApiJson(body): ApiJson<CreateBody>,
) -> ApiResult<(StatusCode, Json<BroadcastView>)> {
    let title = clean_title(body.title.as_deref().unwrap_or(""))?;
    let name = match body
        .name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
    {
        Some(n) => {
            if !is_valid_name(n) {
                return Err(ApiError::bad_request(
                    "name must match [a-z0-9-_]{1,64} (lowercase letters, digits, '-' and '_')",
                ));
            }
            n.to_string()
        }
        None => unique_slug(&state, &title).await?,
    };
    let mut new = NewBroadcast::new(name.clone(), title);
    if let Some(d) = body.description {
        check_len("description", &d, DESCRIPTION_MAX)?;
        new.description = d.trim().to_string();
    }
    if let Some(t) = body.waiting_text {
        check_len("waiting_text", &t, WAITING_TEXT_MAX)?;
        new.waiting_text = t.trim().to_string();
    }
    new.record = body.record.unwrap_or(false);
    new.enable_flv = body.enable_flv.unwrap_or(true);
    new.enable_hls = body.enable_hls.unwrap_or(true);
    new.enable_dash = body.enable_dash.unwrap_or(true);
    new.enable_webrtc = body.enable_webrtc.unwrap_or(true);

    let created = repo::create(&state.db, new).await.map_err(|e| {
        if e.is_unique_violation() {
            ApiError::conflict(format!("a broadcast named {name:?} already exists"))
        } else {
            e.into()
        }
    })?;
    tracing::info!(broadcast = %created.name, "broadcast created");
    changed(&state, &created.name);
    let view = view_of(&state, &headers, created).await?;
    Ok((StatusCode::CREATED, view))
}

async fn get_one(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<BroadcastView>> {
    let b = load(&state, &name).await?;
    view_of(&state, &headers, b).await
}

async fn update(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    ApiJson(body): ApiJson<PatchBody>,
) -> ApiResult<Json<BroadcastView>> {
    let current = load(&state, &name).await?;
    if let Some(n) = &body.name
        && n.trim() != current.name
    {
        return Err(ApiError::bad_request("name cannot be changed"));
    }
    let mut patch = BroadcastPatch {
        record: body.record,
        enable_flv: body.enable_flv,
        enable_hls: body.enable_hls,
        enable_dash: body.enable_dash,
        enable_webrtc: body.enable_webrtc,
        ..Default::default()
    };
    if let Some(t) = body.title {
        patch.title = Some(clean_title(&t)?);
    }
    if let Some(d) = body.description {
        check_len("description", &d, DESCRIPTION_MAX)?;
        patch.description = Some(d.trim().to_string());
    }
    if let Some(t) = body.waiting_text {
        check_len("waiting_text", &t, WAITING_TEXT_MAX)?;
        patch.waiting_text = Some(t.trim().to_string());
    }
    let updated = repo::update(&state.db, &name, patch)
        .await?
        .ok_or_else(ApiError::broadcast_not_found)?;
    if !updated.enable_webrtc {
        state.webrtc.close_broadcast(&name);
    }
    changed(&state, &name);
    view_of(&state, &headers, updated).await
}

/// `DELETE /broadcasts/{name}`: kicks the publisher, deletes the waiting video (and its
/// poster), deletes the row. Recordings are kept.
async fn delete_one(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> ApiResult<StatusCode> {
    let b = load(&state, &name).await?;
    state.hub.kick(&b.name);
    let deleted = repo::delete(&state.db, &b.name)
        .await?
        .ok_or_else(ApiError::broadcast_not_found)?;
    state.webrtc.close_broadcast(&b.name);
    if let Some(rel) = deleted.waiting_video.as_deref() {
        remove_waiting_file(&state, rel).await;
        remove_waiting_file(&state, &poster_path_for(rel)).await;
    }
    tracing::info!(broadcast = %b.name, "broadcast deleted");
    changed(&state, &b.name);
    Ok(StatusCode::NO_CONTENT)
}

/// Delete a file under `data/waiting/` (relative path; `..` components rejected).
pub(crate) async fn remove_waiting_file(state: &AppState, rel: &str) {
    let rel_path = std::path::Path::new(rel);
    if rel_path
        .components()
        .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        tracing::warn!(path = %rel, "refusing to delete suspicious waiting video path");
        return;
    }
    let full = state.config.waiting_dir().join(rel_path);
    match tokio::fs::remove_file(&full).await {
        Ok(()) => tracing::info!(path = %full.display(), "waiting file deleted"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            tracing::warn!(path = %full.display(), error = %e, "failed to delete waiting file")
        }
    }
}

async fn regenerate_key(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<BroadcastView>> {
    load(&state, &name).await?;
    let b = repo::regenerate_key(&state.db, &name)
        .await?
        .ok_or_else(ApiError::broadcast_not_found)?;
    // The current publisher (if any) stays connected; use /disconnect to kick it.
    tracing::info!(broadcast = %name, "stream key regenerated");
    changed(&state, &name);
    view_of(&state, &headers, b).await
}

async fn set_paused(
    state: &AppState,
    name: &str,
    headers: &HeaderMap,
    paused: bool,
) -> ApiResult<Json<BroadcastView>> {
    load(state, name).await?;
    let b = repo::set_paused(&state.db, name, paused)
        .await?
        .ok_or_else(ApiError::broadcast_not_found)?;
    tracing::info!(broadcast = %name, paused, "pause state changed");
    changed(state, name);
    view_of(state, headers, b).await
}

async fn pause(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<BroadcastView>> {
    set_paused(&state, &name, &headers, true).await
}

async fn resume(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<BroadcastView>> {
    set_paused(&state, &name, &headers, false).await
}

/// `POST /broadcasts/{name}/disconnect` → 204 (publisher kicked), 409 if not live.
async fn disconnect(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> ApiResult<StatusCode> {
    let b = load(&state, &name).await?;
    if state.hub.kick(&b.name) {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::conflict("broadcast is not live"))
    }
}

async fn embed(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<EmbedInfo>> {
    let b = load(&state, &name).await?;
    let ctx = ViewContext::load(&state, &headers).await?;
    let urls = PlaybackUrls::new(&ctx.base, &b.name);
    let obs = ObsSettings::new(&ctx.base, &state.config, &b.stream_key);
    Ok(Json(EmbedInfo::new(urls, obs)))
}
