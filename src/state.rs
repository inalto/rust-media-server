//! `AppState`: shared, cheaply clonable handle given to axum handlers and tasks.

use std::ops::Deref;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use tokio::sync::{OnceCell, broadcast};
use tokio_util::sync::CancellationToken;

use crate::cluster::ClusterNode;
use crate::config::Config;
use crate::db::Db;
use crate::hub::{Hub, HubEvent};
use crate::metrics::ViewerTracker;
use crate::packager::PackagerManager;
use crate::recorder::RecorderManager;
use crate::vod::VodManager;
use crate::webrtc::WebRtcManager;

#[derive(Clone)]
pub struct AppState(Arc<Inner>);

pub struct Inner {
    pub config: Arc<Config>,
    pub db: Db,
    pub hub: Arc<Hub>,
    pub tracker: Arc<ViewerTracker>,
    pub packager: Arc<PackagerManager>,
    pub recorder: Arc<RecorderManager>,
    /// WebRTC (WHEP) egress.
    pub webrtc: Arc<WebRtcManager>,
    /// Video on demand (built here from the config, the DB and the resolved ffmpeg;
    /// `main.rs` spawns its tasks).
    pub vod: Arc<VodManager>,
    /// Cluster identity, node registry and drain flag (`enabled = false` on a single
    /// node; `main.rs` spawns the registry in cluster mode).
    pub cluster: Arc<ClusterNode>,
    /// Monotonic process start (uptime).
    pub started_at: Instant,
    pub started_at_utc: DateTime<Utc>,
    /// Sender side of the hub event bus (same channel as `hub.subscribe_events()`).
    pub events: broadcast::Sender<HubEvent>,
    /// Effective admin token (config, env or bootstrapped file).
    pub admin_token: String,
    /// Resolved ffmpeg executable, `None` when unavailable.
    pub ffmpeg: Option<PathBuf>,
    /// Cancelled on shutdown: long-lived responses (FLV, SSE) end when it fires.
    pub shutdown: CancellationToken,
    ffmpeg_version: OnceCell<Option<String>>,
}

/// Everything needed to build an [`AppState`].
pub struct AppStateParts {
    pub config: Arc<Config>,
    pub db: Db,
    pub hub: Arc<Hub>,
    pub tracker: Arc<ViewerTracker>,
    pub packager: Arc<PackagerManager>,
    pub recorder: Arc<RecorderManager>,
    pub webrtc: Arc<WebRtcManager>,
    pub admin_token: String,
    pub shutdown: CancellationToken,
}

impl AppState {
    pub fn new(p: AppStateParts) -> Self {
        let ffmpeg = p.config.ffmpeg_path();
        let events = p.hub.events();
        let vod = VodManager::new(
            p.config.clone(),
            p.db.clone(),
            events.clone(),
            ffmpeg.clone(),
        );
        let cluster = ClusterNode::new(p.config.clone(), p.db.clone());
        AppState(Arc::new(Inner {
            config: p.config,
            db: p.db,
            hub: p.hub,
            tracker: p.tracker,
            packager: p.packager,
            recorder: p.recorder,
            webrtc: p.webrtc,
            vod,
            cluster,
            started_at: Instant::now(),
            started_at_utc: Utc::now(),
            events,
            admin_token: p.admin_token,
            ffmpeg,
            shutdown: p.shutdown,
            ffmpeg_version: OnceCell::new(),
        }))
    }

    pub fn uptime(&self) -> Duration {
        self.started_at.elapsed()
    }

    /// First line of `ffmpeg -version` (cached; `None` if unavailable).
    pub async fn ffmpeg_version(&self) -> Option<String> {
        let path = self.ffmpeg.clone()?;
        self.ffmpeg_version
            .get_or_init(|| async move {
                let out = tokio::time::timeout(
                    Duration::from_secs(5),
                    tokio::process::Command::new(&path)
                        .arg("-hide_banner")
                        .arg("-version")
                        .kill_on_drop(true)
                        .output(),
                )
                .await
                .ok()?
                .ok()?;
                let text = String::from_utf8_lossy(&out.stdout);
                text.lines().next().map(|l| l.trim().to_string())
            })
            .await
            .clone()
    }
}

impl Deref for AppState {
    type Target = Inner;
    fn deref(&self) -> &Inner {
        &self.0
    }
}
