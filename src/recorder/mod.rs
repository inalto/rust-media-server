//! RecorderManager: FLV writer per live stream (auto-record flag or manual start/stop),
//! remuxed to MP4 with ffmpeg when available.
//!
//! PHASE 1 STUB: signatures are final (used by `main.rs` and `api/recordings.rs`);
//! the `feat/metrics-recording` branch implements them.

use std::sync::Arc;

use tokio::sync::broadcast::error::RecvError;

use crate::config::Config;
use crate::db::Db;
use crate::hub::Hub;

/// The in-progress recording of a broadcast (a `recordings` row, status "recording").
pub type RecordingInfo = crate::db::Recording;

#[derive(Debug, thiserror::Error)]
pub enum RecorderError {
    #[error("not implemented yet")]
    NotImplemented,
    #[error("broadcast is not live")]
    NotLive,
    #[error("broadcast is already being recorded")]
    AlreadyRecording,
    #[error("broadcast is not being recorded")]
    NotRecording,
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub struct RecorderManager {
    #[allow(dead_code)]
    config: Arc<Config>,
    #[allow(dead_code)]
    db: Db,
    hub: Arc<Hub>,
}

impl RecorderManager {
    pub fn new(config: Arc<Config>, db: Db, hub: Arc<Hub>) -> Arc<Self> {
        Arc::new(Self { config, db, hub })
    }

    /// Spawn the manager task (phase 2: auto-record on `StreamStarted` when the
    /// broadcast has `record = true`, finalize on `StreamEnded`).
    pub fn spawn(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let mut events = self.hub.subscribe_events();
        tokio::spawn(async move {
            tracing::debug!(task = "recorder-manager", "started (phase 1 stub)");
            loop {
                match events.recv().await {
                    Ok(_) => {}
                    Err(RecvError::Lagged(n)) => {
                        tracing::warn!(
                            task = "recorder-manager",
                            skipped = n,
                            "lagged on hub events"
                        );
                    }
                    Err(RecvError::Closed) => break,
                }
            }
        })
    }

    /// Start a manual recording of the live broadcast `name`.
    pub async fn start_manual(&self, _name: &str) -> Result<RecordingInfo, RecorderError> {
        Err(RecorderError::NotImplemented)
    }

    /// Stop the current recording of `name`.
    pub async fn stop_manual(&self, _name: &str) -> Result<(), RecorderError> {
        Err(RecorderError::NotImplemented)
    }

    /// The in-progress recording of `name`, if any.
    pub fn current(&self, _name: &str) -> Option<RecordingInfo> {
        None
    }
}
