//! `rms` binary: CLI parsing and startup wiring only (see ARCHITECTURE.md §3).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use axum::Router;
use axum::response::IntoResponse;
use clap::Parser;
use tokio_util::sync::CancellationToken;
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

use rust_media_server::api::auth::{TokenSource, bootstrap_admin_token};
use rust_media_server::config::{CliOverrides, Config};
use rust_media_server::db::{self, Db};
use rust_media_server::error::ApiError;
use rust_media_server::hub::Hub;
use rust_media_server::ingest::rtmp::RtmpServer;
use rust_media_server::metrics::{self, ViewerTracker};
use rust_media_server::packager::{self, PackagerManager};
use rust_media_server::recorder::RecorderManager;
use rust_media_server::state::{AppState, AppStateParts};
use rust_media_server::webrtc::WebRtcManager;
use rust_media_server::{api, cluster, egress, vod, web};

/// rust-media-server: RTMP ingest, HTTP-FLV / WS-FLV / LL-HLS / LL-DASH egress.
#[derive(Debug, Parser)]
#[command(name = "rms", version, about)]
struct Cli {
    /// Configuration file (default: ./config.toml if present).
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,
    /// Data directory (database, recordings, waiting videos, admin token).
    #[arg(long, value_name = "DIR")]
    data_dir: Option<PathBuf>,
    /// HTTP listen address, e.g. 0.0.0.0:8080.
    #[arg(long, value_name = "ADDR")]
    http_bind: Option<String>,
    /// RTMP listen address, e.g. 0.0.0.0:1935.
    #[arg(long, value_name = "ADDR")]
    rtmp_bind: Option<String>,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let overrides = CliOverrides {
        data_dir: cli.data_dir.clone(),
        http_bind: cli.http_bind.clone(),
        rtmp_bind: cli.rtmp_bind.clone(),
    };
    let (config, warnings) = match Config::load(cli.config.as_deref(), &overrides) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("rms: configuration error: {e:#}");
            return ExitCode::FAILURE;
        }
    };
    init_tracing(&config.log.level);
    for w in warnings {
        tracing::warn!("{w}");
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!(error = %e, "cannot start tokio runtime");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run(config)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("fatal: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// `RUST_LOG` wins, otherwise `log.level`. Either way rtmp-rs is capped at WARN (its
/// registry at ERROR) unless the directives mention `rtmp_rs` explicitly: rtmp-rs logs
/// full stream keys (secrets) at INFO ("Publisher registered", "Publishing started")
/// and in one registry WARN. The shipped systemd unit sets `RUST_LOG=info`, which
/// previously bypassed the cap and wrote every stream key to the journal.
fn init_tracing(level: &str) {
    let env = std::env::var("RUST_LOG")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .and_then(|v| {
            let d = with_rtc_cap(&with_rtmp_rs_cap(&v));
            EnvFilter::try_new(&d).ok()
        });
    let filter = env
        .or_else(|| EnvFilter::try_new(with_rtc_cap(&with_rtmp_rs_cap(level))).ok())
        .unwrap_or_else(|| EnvFilter::new(with_rtc_cap(&with_rtmp_rs_cap("info"))));
    let ansi = std::io::IsTerminal::is_terminal(&std::io::stdout());
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_ansi(ansi)
        .init();
}

/// Append the rtmp-rs cap (see [`init_tracing`]) unless `rtmp_rs` is configured.
fn with_rtmp_rs_cap(directives: &str) -> String {
    const CAP: &str = "rtmp_rs=warn,rtmp_rs::registry=error";
    let d = directives.trim().trim_end_matches(',');
    if d.contains("rtmp_rs") {
        d.to_string()
    } else if d.is_empty() {
        format!("info,{CAP}")
    } else {
        format!("{d},{CAP}")
    }
}

/// The WebRTC stack (`rtc*` crates) logs every state change at INFO and a harmless DTLS
/// `server_name` WARN per session; cap it unless a directive targets an `rtc*` crate.
fn with_rtc_cap(directives: &str) -> String {
    const CAP: &str =
        "rtc=error,rtc_dtls=error,rtc_ice=warn,rtc_srtp=warn,rtc_sctp=warn,rtc_interceptor=warn";
    let d = directives.trim().trim_end_matches(',');
    let targets_rtc = d
        .split(',')
        .filter_map(|directive| directive.split('=').next())
        .any(|target| {
            target.trim() == "rtc"
                || target.trim().starts_with("rtc_")
                || target.trim().starts_with("rtc::")
        });
    if targets_rtc {
        d.to_string()
    } else if d.is_empty() {
        format!("info,{CAP}")
    } else {
        format!("{d},{CAP}")
    }
}

async fn run(config: Config) -> anyhow::Result<()> {
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        "starting rust-media-server"
    );

    // Data directory layout.
    for dir in [
        config.data_dir().to_path_buf(),
        config.waiting_dir(),
        config.recordings_dir(),
        config.vod_dir(),
    ] {
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    }

    // Database: PostgreSQL in cluster mode, SQLite otherwise (+ crash recovery of the
    // stream sessions this node left open).
    let db = if config.cluster.enabled {
        let url = &config.cluster.database_url;
        Db::connect_postgres(url)
            .await
            .with_context(|| {
                format!(
                    "connecting to PostgreSQL {} (cluster.database_url)",
                    rust_media_server::config::redact_url_password(url)
                )
            })?
            .with_node_id(&config.cluster.node_id)
    } else {
        let db_path = config.db_path();
        Db::open(&db_path).with_context(|| format!("opening database {}", db_path.display()))?
    };
    tracing::info!(
        backend = db.backend().as_str(),
        node_id = %config.cluster.node_id,
        cluster = config.cluster.enabled,
        "database ready"
    );
    let dangling = db::stream_sessions::close_dangling(&db).await?;
    if dangling > 0 {
        tracing::warn!(
            count = dangling,
            "closed stream sessions left open by a previous run"
        );
    }

    // Admin token.
    let (admin_token, source) = bootstrap_admin_token(&config)?;
    match source {
        TokenSource::Config => tracing::info!("admin token taken from configuration"),
        TokenSource::File => {
            tracing::info!(file = %config.admin_token_path().display(), "admin token loaded")
        }
        TokenSource::Generated => tracing::warn!(
            file = %config.admin_token_path().display(),
            "generated a new admin token: {admin_token}"
        ),
    }

    match config.ffmpeg_path() {
        Some(p) => tracing::info!(path = %p.display(), "ffmpeg found"),
        None => {
            tracing::warn!("ffmpeg not found: MP4 remux and waiting video normalization disabled")
        }
    }

    let config = Arc::new(config);
    let shutdown = CancellationToken::new();

    // Core components.
    let hub = Hub::new();
    let tracker = ViewerTracker::new(&config, Some(db.clone()), hub.events());
    tracker.spawn();
    metrics::spawn_sampler(&config, db.clone(), hub.clone(), tracker.clone());
    let packager = PackagerManager::new(config.clone(), hub.clone(), tracker.clone());
    packager.spawn();
    let recorder = RecorderManager::new(config.clone(), db.clone(), hub.clone());
    recorder.spawn();
    let webrtc = WebRtcManager::new(config.clone(), hub.clone(), tracker.clone());
    webrtc.spawn(shutdown.clone());

    let state = AppState::new(AppStateParts {
        config: config.clone(),
        db: db.clone(),
        hub: hub.clone(),
        tracker: tracker.clone(),
        packager,
        recorder,
        webrtc,
        admin_token,
        shutdown: shutdown.clone(),
    });
    // Video on demand: re-queue interrupted jobs, job runner, inbox scanner.
    state.vod.spawn(shutdown.clone());

    // Cluster: node registry (heartbeat, stream ownership, reapers), directory refresh,
    // NOTIFY bus and the internal listener.
    let registry = state
        .cluster
        .spawn_registry(state.webrtc.clone(), shutdown.clone());
    let refresh = state.cluster.spawn_refresh();
    let bus_tasks = cluster::bus::spawn(&state);
    let internal = if config.cluster.enabled {
        Some(
            cluster::http::serve_internal(
                state.clone(),
                &config.cluster.internal_bind,
                config.cluster.internal_secret.clone(),
                shutdown.clone(),
            )
            .await?,
        )
    } else {
        None
    };

    // RTMP ingest.
    let rtmp = RtmpServer::bind(
        config.clone(),
        db.clone(),
        hub.clone(),
        tracker.clone(),
        state.cluster.clone(),
    )
    .await?;
    let rtmp_task = tokio::spawn(rtmp.run(shutdown.child_token()));

    // HTTP.
    let mut app = Router::new()
        .merge(api::router(&state))
        .merge(egress::router())
        .merge(packager::router())
        .merge(WebRtcManager::router())
        .merge(vod::router())
        .merge(cluster::http::health_router())
        .merge(web::router(&state))
        .fallback(|| async { ApiError::not_found("not found").into_response() });
    if config.cluster.enabled {
        app = cluster::http::with_node_header(app, state.cluster.node_id());
    }
    let app = app
        .layer(TraceLayer::new_for_http())
        .with_state(state.clone());
    let http_addr = config.http_bind_addr()?;
    let listener = tokio::net::TcpListener::bind(http_addr)
        .await
        .with_context(|| format!("cannot bind HTTP listener on {http_addr}"))?;
    tracing::info!(addr = %http_addr, "HTTP server listening");

    let signal_token = shutdown.clone();
    tokio::spawn(async move {
        wait_for_signal().await;
        tracing::info!("shutdown requested");
        signal_token.cancel();
    });

    let server_shutdown = shutdown.clone();
    let server = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move { server_shutdown.cancelled().await });
    let forced = shutdown.clone();
    tokio::select! {
        r = server => r.context("HTTP server error")?,
        _ = async move {
            forced.cancelled().await;
            tokio::time::sleep(Duration::from_secs(10)).await;
        } => tracing::warn!("HTTP connections did not close in time; forcing shutdown"),
    }

    shutdown.cancel();
    if tokio::time::timeout(Duration::from_secs(8), rtmp_task)
        .await
        .is_err()
    {
        tracing::warn!("RTMP ingest did not stop in time");
    }
    state.recorder.shutdown().await;
    state.vod.shutdown(Duration::from_secs(5)).await;
    for task in [registry, refresh, internal]
        .into_iter()
        .flatten()
        .chain(bus_tasks)
    {
        let _ = tokio::time::timeout(Duration::from_secs(3), task).await;
    }
    state.cluster.deregister().await;
    db.close().await;
    tracing::info!("bye");
    Ok(())
}

async fn wait_for_signal() {
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::error!(error = %e, "cannot listen for ctrl-c");
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(e) => {
                tracing::error!(error = %e, "cannot listen for SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

#[cfg(test)]
mod tests {
    use super::{with_rtc_cap, with_rtmp_rs_cap};

    #[test]
    fn rtc_is_capped_unless_configured() {
        assert!(with_rtc_cap("info").starts_with("info,rtc=error,"));
        // rust_media_server::webrtc contains "rtc" but is not an rtc crate.
        assert!(with_rtc_cap("info,rust_media_server::webrtc=debug").contains(",rtc=error"));
        assert_eq!(with_rtc_cap("info,rtc_ice=debug"), "info,rtc_ice=debug");
        assert_eq!(with_rtc_cap("rtc=info"), "rtc=info");
        for d in ["info", "debug,rtmp_rs=warn", ""] {
            assert!(tracing_subscriber::EnvFilter::try_new(with_rtc_cap(d)).is_ok());
        }
    }

    #[test]
    fn rtmp_rs_is_capped_unless_configured() {
        let cap = "rtmp_rs=warn,rtmp_rs::registry=error";
        assert_eq!(with_rtmp_rs_cap("info"), format!("info,{cap}"));
        assert_eq!(with_rtmp_rs_cap(" debug "), format!("debug,{cap}"));
        assert_eq!(
            with_rtmp_rs_cap("info,rust_media_server=debug"),
            format!("info,rust_media_server=debug,{cap}")
        );
        assert_eq!(with_rtmp_rs_cap(""), format!("info,{cap}"));
        assert_eq!(with_rtmp_rs_cap("info,rtmp_rs=debug"), "info,rtmp_rs=debug");
        for d in ["info", "trace,hyper=info", ""] {
            assert!(tracing_subscriber::EnvFilter::try_new(with_rtmp_rs_cap(d)).is_ok());
        }
    }
}
