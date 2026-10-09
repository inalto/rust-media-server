//! `GET /api/v1/events`: admin Server-Sent Events (ARCHITECTURE.md §8.1).
//!
//! Event names:
//! - `broadcast`: full `BroadcastView` whenever a broadcast changes (settings, pause,
//!   key, stream start/end, metadata);
//! - `broadcast_deleted`: `{name}` when a broadcast is deleted (extension of §8.1);
//! - `viewers`: `{name, counts}` when viewer counts change;
//! - `stream`: `{name, status}` when a stream starts or ends (in cluster mode also when
//!   a stream of another node starts, ends or changes owner; `status` is effective);
//! - `settings`: `{ui_language, ui_theme, default_waiting_text}` after `PATCH /settings`
//!   (the dashboard switches language and default theme without a reload);
//! - `vod`: `VodView` when a video on demand item is created or changes (status,
//!   progress, title, plays); `vod_deleted`: `{id}`;
//! - `heartbeat`: `{ts}` every 15 s.

use std::convert::Infallible;
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures::Stream;
use serde_json::json;
use tokio::sync::broadcast::error::RecvError;

use super::{BaseUrl, BroadcastStatus, ViewContext};
use crate::db;
use crate::hub::HubEvent;
use crate::state::AppState;

pub const HEARTBEAT: Duration = Duration::from_secs(15);

pub fn routes() -> Router<AppState> {
    Router::new().route("/events", get(events))
}

/// SSE response with headers that disable proxy buffering.
pub fn sse_response<S>(stream: S) -> Response
where
    S: Stream<Item = Result<Event, Infallible>> + Send + 'static,
{
    let mut resp = Sse::new(stream).into_response();
    let h = resp.headers_mut();
    h.insert("x-accel-buffering", HeaderValue::from_static("no"));
    h.insert(
        axum::http::header::CACHE_CONTROL,
        HeaderValue::from_static("no-cache"),
    );
    resp
}

pub fn heartbeat_event() -> Event {
    Event::default()
        .event("heartbeat")
        .data(json!({ "ts": db::now_ts() }).to_string())
}

fn json_event(name: &str, value: &impl serde::Serialize) -> Option<Event> {
    serde_json::to_string(value)
        .ok()
        .map(|d| Event::default().event(name).data(d))
}

/// Translate one hub event into SSE events.
async fn translate(state: &AppState, headers: &HeaderMap, ev: HubEvent) -> Vec<Event> {
    let mut out = Vec::new();
    match &ev {
        HubEvent::ViewerCountChanged { name, counts } => {
            // Cluster mode: plus the other nodes' viewers (docs/CLUSTER.md §3.6).
            let counts = state.cluster.with_remote_viewers(name, *counts);
            out.extend(json_event(
                "viewers",
                &json!({ "name": name, "counts": counts }),
            ));
            return out;
        }
        HubEvent::SettingsChanged => {
            match ViewContext::load(state, headers).await {
                Ok(ctx) => out.extend(json_event(
                    "settings",
                    &json!({
                        "ui_language": ctx.language,
                        "ui_theme": ctx.theme,
                        "default_waiting_text": ctx.effective_default_waiting_text(),
                    }),
                )),
                Err(e) => tracing::warn!(error = %e, "SSE: failed to load settings"),
            }
            return out;
        }
        HubEvent::VodChanged { id } => {
            match db::vod::get(&state.db, id).await {
                Ok(Some(item)) => {
                    let base = BaseUrl::from_request(&state.config, headers);
                    out.extend(json_event("vod", &super::vod::vod_view(state, &base, item)));
                }
                Ok(None) => out.extend(json_event("vod_deleted", &json!({ "id": id }))),
                Err(e) => tracing::warn!(error = %e, "SSE: failed to load video"),
            }
            return out;
        }
        HubEvent::StreamStarted { .. }
        | HubEvent::StreamEnded { .. }
        | HubEvent::RemoteStreamChanged { .. }
        | HubEvent::Metadata { .. }
        | HubEvent::BroadcastUpdated { .. } => {}
    }
    let name = ev.name();
    let broadcast = match db::broadcasts::find_by_name(&state.db, name).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "SSE: failed to load broadcast");
            return out;
        }
    };
    let Some(b) = broadcast else {
        if matches!(ev, HubEvent::BroadcastUpdated { .. }) {
            out.extend(json_event("broadcast_deleted", &json!({ "name": name })));
        }
        return out;
    };
    if matches!(
        ev,
        HubEvent::StreamStarted { .. }
            | HubEvent::StreamEnded { .. }
            | HubEvent::RemoteStreamChanged { .. }
    ) {
        let status = BroadcastStatus::of(&b, state);
        out.extend(json_event(
            "stream",
            &json!({ "name": name, "status": status }),
        ));
    }
    match ViewContext::load(state, headers).await {
        Ok(ctx) => out.extend(json_event("broadcast", &ctx.view(state, b).await)),
        Err(e) => tracing::warn!(error = %e, "SSE: failed to build view"),
    }
    out
}

async fn events(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let mut rx = state.hub.subscribe_events();
    let shutdown = state.shutdown.clone();
    let stream = async_stream::stream! {
        yield Ok::<Event, Infallible>(heartbeat_event());
        let mut tick = tokio::time::interval(HEARTBEAT);
        tick.tick().await;
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = tick.tick() => yield Ok(heartbeat_event()),
                r = rx.recv() => match r {
                    Ok(ev) => {
                        for e in translate(&state, &headers, ev).await {
                            yield Ok(e);
                        }
                    }
                    Err(RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "admin SSE client lagging on hub events");
                    }
                    Err(RecvError::Closed) => break,
                },
            }
        }
    };
    sse_response(stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::waiting::tests::test_state;

    #[tokio::test]
    async fn settings_event() {
        let (state, dir) = test_state(false).await;
        db::settings::set(&state.db, db::settings::UI_LANGUAGE, "en")
            .await
            .unwrap();
        db::settings::set(&state.db, db::settings::UI_THEME, "light")
            .await
            .unwrap();
        let out = translate(&state, &HeaderMap::new(), HubEvent::SettingsChanged).await;
        assert_eq!(out.len(), 1);
        let text = format!("{:?}", out[0]);
        assert!(text.contains("settings"), "{text}");
        assert!(text.contains(r#"\"ui_language\":\"en\""#), "{text}");
        assert!(text.contains(r#"\"ui_theme\":\"light\""#), "{text}");
        assert!(
            text.contains("The live stream will start shortly"),
            "{text}"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
