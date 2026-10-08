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
use rust_media_server::{api, egress, web};

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
            let d = with_rtmp_rs_cap(&v);
            EnvFilter::try_new(&d).ok()
        });
    let filter = env
        .or_else(|| EnvFilter::try_new(with_rtmp_rs_cap(level)).ok())
        .unwrap_or_else(|| EnvFilter::new(with_rtmp_rs_cap("info")));
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
    ] {
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    }

    // Database (+ crash recovery of open stream sessions).
    let db_path = config.db_path();
    let db =
        Db::open(&db_path).with_context(|| format!("opening database {}", db_path.display()))?;
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

    let state = AppState::new(AppStateParts {
        config: config.clone(),
        db: db.clone(),
        hub: hub.clone(),
        tracker: tracker.clone(),
        packager,
        recorder,
        admin_token,
        shutdown: shutdown.clone(),
    });

    // RTMP ingest.
    let rtmp = RtmpServer::bind(config.clone(), db.clone(), hub.clone(), tracker.clone()).await?;
    let rtmp_task = tokio::spawn(rtmp.run(shutdown.child_token()));

    // HTTP.
    let app = Router::new()
        .merge(api::router(&state))
        .merge(egress::router())
        .merge(packager::router())
        .merge(web::router(&state))
        .fallback(|| async { ApiError::not_found("not found").into_response() })
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
    use super::with_rtmp_rs_cap;

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
