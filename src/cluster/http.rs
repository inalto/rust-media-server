//! HTTP surface of the cluster layer: the public `GET /healthz` (merged into the main
//! router) and the internal listener on `cluster.internal_bind` (phase 7C-1 skeleton:
//! `GET /internal/ping` only; phase 7C-2 adds kick, drain, relay and file routes).
//!
//! The internal routes require `Authorization: Bearer <cluster.internal_secret>`
//! (constant-time comparison) and must never be exposed through the public proxy
//! (docs/CLUSTER.md §2.6, §3.1).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use super::ClusterNode;
use crate::api::auth::{extract_token, token_matches};
use crate::error::ApiError;
use crate::state::AppState;

/// Upper bound of the database probe of `/healthz`.
pub const HEALTH_DB_TIMEOUT: Duration = Duration::from_secs(2);

/// `GET /healthz` body.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Health {
    pub node_id: String,
    /// Cluster mode enabled.
    pub cluster: bool,
    /// The database answered `SELECT 1` within [`HEALTH_DB_TIMEOUT`].
    pub db: bool,
    pub live_streams: usize,
    pub version: &'static str,
    /// Draining or shutting down (the status is then 503).
    pub draining: bool,
}

/// `/healthz` (public, no auth), merged at `/` by `main.rs`.
pub fn health_router() -> Router<AppState> {
    Router::new().route("/healthz", get(healthz))
}

/// Liveness for load balancers and container health checks: 200 even when the database
/// is unreachable (`db: false`: the data plane keeps working and an outage must not take
/// every node out of the pool); 503 only while draining or shutting down.
async fn healthz(State(state): State<AppState>) -> Response {
    let draining = state.shutdown.is_cancelled() || state.cluster.is_draining();
    let body = Health {
        node_id: state.cluster.node_id().to_string(),
        cluster: state.cluster.enabled(),
        db: state.db.ping(HEALTH_DB_TIMEOUT).await,
        live_streams: state.hub.live_count(),
        version: env!("CARGO_PKG_VERSION"),
        draining,
    };
    let status = if draining {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::OK
    };
    let mut resp = (status, Json(body)).into_response();
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

/// `GET /internal/ping` body.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Pong {
    pub node_id: String,
    pub version: &'static str,
    pub draining: bool,
}

/// Router of the internal listener.
pub fn internal_router(node: Arc<ClusterNode>, secret: String) -> Router {
    let secret = Arc::new(secret);
    Router::new()
        .route("/internal/ping", get(ping))
        .route_layer(middleware::from_fn(move |req: Request, next: Next| {
            let secret = Arc::clone(&secret);
            async move { require_secret(&secret, req, next).await }
        }))
        .fallback(|| async { ApiError::not_found("not found").into_response() })
        .with_state(node)
}

async fn require_secret(secret: &str, req: Request, next: Next) -> Response {
    match extract_bearer(&req) {
        Some(t) if token_matches(secret, &t) => next.run(req).await,
        _ => ApiError::Unauthorized.into_response(),
    }
}

/// Only the `Authorization: Bearer` header (no cookie on the internal routes).
fn extract_bearer(req: &Request) -> Option<String> {
    req.headers().get(header::AUTHORIZATION)?;
    let mut headers = req.headers().clone();
    headers.remove(header::COOKIE);
    extract_token(&headers)
}

async fn ping(State(node): State<Arc<ClusterNode>>) -> Json<Pong> {
    Json(Pong {
        node_id: node.node_id().to_string(),
        version: env!("CARGO_PKG_VERSION"),
        draining: node.is_draining(),
    })
}

/// Bind `cluster.internal_bind` and serve the internal routes until `shutdown`.
pub async fn serve_internal(
    node: Arc<ClusterNode>,
    bind: &str,
    secret: String,
    shutdown: CancellationToken,
) -> anyhow::Result<tokio::task::JoinHandle<()>> {
    let addr: SocketAddr = bind
        .parse()
        .with_context(|| format!("invalid cluster.internal_bind {bind:?}"))?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("cannot bind the internal listener on {addr}"))?;
    tracing::info!(addr = %addr, "internal cluster listener ready (/internal/*)");
    let app = internal_router(node, secret);
    Ok(tokio::spawn(async move {
        let server = axum::serve(listener, app.into_make_service())
            .with_graceful_shutdown(async move { shutdown.cancelled().await });
        if let Err(e) = server.await {
            tracing::warn!(error = %e, "internal cluster listener failed");
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::db::Db;
    use axum::body::Body;
    use tower::ServiceExt;

    #[tokio::test]
    async fn healthz_is_liveness() {
        let (state, dir) = crate::api::waiting::tests::test_state(false).await;
        let app = health_router().with_state(state.clone());
        let get = |app: Router| async move {
            let resp = app
                .oneshot(
                    Request::builder()
                        .uri("/healthz")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = resp.status();
            assert_eq!(resp.headers()[header::CACHE_CONTROL], "no-store");
            let body = axum::body::to_bytes(resp.into_body(), 1 << 16)
                .await
                .unwrap();
            (
                status,
                serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            )
        };
        let (status, v) = get(app.clone()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(v["node_id"], state.cluster.node_id());
        assert_eq!(v["cluster"], false);
        assert_eq!(v["db"], true);
        assert_eq!(v["live_streams"], 0);
        assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(v["draining"], false);
        // Draining (phase 7C-2 drain) and shutdown answer 503.
        state.cluster.set_draining(true);
        assert_eq!(get(app.clone()).await.0, StatusCode::SERVICE_UNAVAILABLE);
        state.cluster.set_draining(false);
        state.shutdown.cancel();
        let (status, v) = get(app).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(v["draining"], true);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn internal_ping_requires_the_secret() {
        let mut config = Config::default();
        config.cluster.node_id = "n1".into();
        let node = ClusterNode::new(Arc::new(config), Db::open_in_memory().unwrap());
        let secret = "x".repeat(32);
        let app = internal_router(node, secret.clone());
        let call = |auth: Option<String>, path: &'static str| {
            let app = app.clone();
            async move {
                let mut req = Request::builder().uri(path);
                if let Some(a) = auth {
                    req = req.header(header::AUTHORIZATION, a);
                }
                app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap()
            }
        };
        assert_eq!(
            call(None, "/internal/ping").await.status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            call(Some("Bearer nope".into()), "/internal/ping")
                .await
                .status(),
            StatusCode::UNAUTHORIZED
        );
        let ok = call(Some(format!("Bearer {secret}")), "/internal/ping").await;
        assert_eq!(ok.status(), StatusCode::OK);
        let body = axum::body::to_bytes(ok.into_body(), 1 << 16).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["node_id"], "n1");
        assert_eq!(v["draining"], false);
        assert_eq!(
            call(Some(format!("Bearer {secret}")), "/internal/nope")
                .await
                .status(),
            StatusCode::NOT_FOUND
        );
    }
}
