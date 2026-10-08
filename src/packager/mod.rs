//! PackagerManager: one CMAF packager task per live stream, feeding LL-HLS and LL-DASH.
//!
//! PHASE 1 STUB: the public signatures used by `main.rs` are final. The
//! `feat/packager` branch adds `cmaf.rs`, `store.rs`, `hls_playlist.rs`,
//! `dash_mpd.rs`, `egress/hls.rs` and `egress/dash.rs`, and mounts the
//! `/hls/...` and `/dash/...` routes in [`router`].

pub mod cmaf;
pub mod dash_mpd;
pub mod hls_playlist;
pub mod segmenter;
pub mod store;

use std::sync::Arc;

use axum::Router;
use tokio::sync::broadcast::error::RecvError;

use crate::config::Config;
use crate::hub::{Hub, HubEvent};
use crate::metrics::ViewerTracker;
use crate::state::AppState;

pub struct PackagerManager {
    #[allow(dead_code)]
    config: Arc<Config>,
    hub: Arc<Hub>,
    #[allow(dead_code)]
    tracker: Arc<ViewerTracker>,
}

impl PackagerManager {
    pub fn new(config: Arc<Config>, hub: Arc<Hub>, tracker: Arc<ViewerTracker>) -> Arc<Self> {
        Arc::new(Self {
            config,
            hub,
            tracker,
        })
    }

    /// Spawn the manager task: watches hub events and (in phase 2) starts/stops one
    /// packager per live stream. Currently only logs stream start/end.
    pub fn spawn(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let mut events = self.hub.subscribe_events();
        tokio::spawn(async move {
            tracing::debug!(task = "packager-manager", "started (phase 1 stub)");
            loop {
                match events.recv().await {
                    Ok(HubEvent::StreamStarted { name }) => {
                        tracing::debug!(task = "packager-manager", broadcast = %name, "stream started (packaging not implemented yet)");
                    }
                    Ok(HubEvent::StreamEnded { name }) => {
                        tracing::debug!(task = "packager-manager", broadcast = %name, "stream ended");
                    }
                    Ok(_) => {}
                    Err(RecvError::Lagged(n)) => {
                        tracing::warn!(
                            task = "packager-manager",
                            skipped = n,
                            "lagged on hub events"
                        );
                    }
                    Err(RecvError::Closed) => break,
                }
            }
        })
    }
}

/// HLS/DASH routes (`/hls/{name}/...`, `/dash/{name}/...`, `/dash/time`).
/// Empty in phase 1; merged at `/` by `main.rs`.
pub fn router() -> Router<AppState> {
    Router::new()
}
