//! Cluster mode, phase 7C-1 (docs/CLUSTER.md §3): node identity, the node registry
//! (heartbeat + reaper on the `nodes` table), `GET /healthz` and the skeleton of the
//! internal listener (`/internal/ping`).
//!
//! [`ClusterNode`] (`AppState.cluster`) exists in both modes: on a single node it only
//! reports `enabled = false`, the node id (host name by default) and the `sqlite`
//! backend. In cluster mode `main.rs` spawns [`ClusterNode::spawn_registry`] and the
//! internal listener ([`http::serve_internal`]), and calls [`ClusterNode::deregister`]
//! at the end of a graceful shutdown.
//!
//! Registry protocol: register (upsert) at startup, then every `cluster.heartbeat_secs`
//! refresh `heartbeat`, `live_streams` (hub), `viewers` (tracker, live broadcasts only)
//! and `draining`; a missing row (reaped after a long pause) is registered again. Every
//! node also reaps rows older than `cluster.node_ttl_secs` (database clock). On shutdown
//! the node sends one last heartbeat with `draining = true` and deletes its row.

pub mod http;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::db::nodes::{self, NodeInfo, NodeLoad, NodeRegistration};
use crate::db::{BackendKind, Db};
use crate::hub::Hub;
use crate::metrics::ViewerTracker;
use crate::webrtc::WebRtcManager;

/// `GET /api/v1/system` → `cluster`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ClusterInfo {
    pub enabled: bool,
    pub node_id: String,
    /// `"sqlite"` | `"postgres"`.
    pub backend: &'static str,
    /// Registered nodes (cluster mode; empty on a single node), by `node_id`.
    pub nodes: Vec<NodeInfo>,
}

pub struct ClusterNode {
    config: Arc<Config>,
    db: Db,
    node_id: String,
    started_at: DateTime<Utc>,
    draining: AtomicBool,
    /// Last successful registration/heartbeat (auto-fencing needs it in phase 7C-2).
    last_heartbeat: Mutex<Option<Instant>>,
}

impl ClusterNode {
    pub fn new(config: Arc<Config>, db: Db) -> Arc<Self> {
        let node_id = if config.cluster.node_id.is_empty() {
            crate::config::hostname()
        } else {
            config.cluster.node_id.clone()
        };
        Arc::new(Self {
            config,
            db,
            node_id,
            started_at: Utc::now(),
            draining: AtomicBool::new(false),
            last_heartbeat: Mutex::new(None),
        })
    }

    /// `cluster.enabled`.
    pub fn enabled(&self) -> bool {
        self.config.cluster.enabled
    }

    /// This node's id (`cluster.node_id`, the host name by default).
    pub fn node_id(&self) -> &str {
        &self.node_id
    }

    pub fn backend(&self) -> BackendKind {
        self.db.backend()
    }

    /// A node is dead after this long without heartbeat (`cluster.node_ttl_secs`).
    pub fn node_ttl(&self) -> Duration {
        Duration::from_secs(self.config.cluster.node_ttl_secs.max(1))
    }

    pub fn heartbeat_interval(&self) -> Duration {
        Duration::from_secs(self.config.cluster.heartbeat_secs.max(1))
    }

    /// Draining: `/healthz` answers 503 and the next heartbeat says so. Set on shutdown
    /// (and by `POST /internal/drain` in phase 7C-2).
    pub fn is_draining(&self) -> bool {
        self.draining.load(Ordering::Relaxed)
    }

    pub fn set_draining(&self, on: bool) {
        self.draining.store(on, Ordering::Relaxed);
    }

    /// When the registry last wrote this node's row successfully (`None` before the
    /// first registration or on a single node).
    pub fn last_heartbeat(&self) -> Option<Instant> {
        *self
            .last_heartbeat
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn heartbeat_ok(&self) {
        *self
            .last_heartbeat
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
    }

    /// Row written at registration.
    pub fn registration(&self, webrtc_available: bool) -> NodeRegistration {
        let public = &self.config.server.public_url;
        NodeRegistration {
            node_id: self.node_id.clone(),
            advertise_url: self.config.cluster.advertise_url.clone(),
            whep_url: (webrtc_available && !public.is_empty()).then(|| format!("{public}/whep")),
            version: env!("CARGO_PKG_VERSION").to_string(),
            started_at: self.started_at,
        }
    }

    /// Counters of the next heartbeat.
    pub fn load(&self, hub: &Hub, tracker: &ViewerTracker) -> NodeLoad {
        let viewers: u32 = tracker
            .all_counts()
            .iter()
            .filter(|(name, _)| !name.starts_with("vod:"))
            .map(|(_, c)| c.total())
            .sum();
        NodeLoad {
            live_streams: hub.live_count().min(u32::MAX as usize) as u32,
            viewers,
            draining: self.is_draining(),
        }
    }

    /// `GET /system` → `cluster` (the node list is read from the database in cluster mode).
    pub async fn info(&self) -> ClusterInfo {
        let nodes = if self.enabled() {
            nodes::list(&self.db, self.node_ttl())
                .await
                .unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "cannot list the cluster nodes");
                    Vec::new()
                })
        } else {
            Vec::new()
        };
        ClusterInfo {
            enabled: self.enabled(),
            node_id: self.node_id.clone(),
            backend: self.backend().as_str(),
            nodes,
        }
    }

    /// Start the registry task (cluster mode only; `None` on a single node).
    pub fn spawn_registry(
        self: &Arc<Self>,
        hub: Arc<Hub>,
        tracker: Arc<ViewerTracker>,
        webrtc: Arc<WebRtcManager>,
        shutdown: CancellationToken,
    ) -> Option<tokio::task::JoinHandle<()>> {
        if !self.enabled() {
            return None;
        }
        let me = Arc::clone(self);
        Some(tokio::spawn(async move {
            me.registry_loop(&hub, &tracker, &webrtc, shutdown).await
        }))
    }

    async fn registry_loop(
        &self,
        hub: &Hub,
        tracker: &ViewerTracker,
        webrtc: &WebRtcManager,
        shutdown: CancellationToken,
    ) {
        let ttl = self.node_ttl();
        let mut tick = tokio::time::interval(self.heartbeat_interval());
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut registered = false;
        let mut failing = false;
        tracing::info!(
            task = "cluster-registry",
            node_id = %self.node_id,
            heartbeat_secs = self.heartbeat_interval().as_secs(),
            node_ttl_secs = ttl.as_secs(),
            "started"
        );
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = tick.tick() => {}
            }
            let result = if registered {
                match nodes::heartbeat(&self.db, &self.node_id, self.load(hub, tracker)).await {
                    Ok(true) => Ok(()),
                    Ok(false) => {
                        tracing::warn!(
                            task = "cluster-registry",
                            "this node's row is gone (reaped after a pause?); registering again"
                        );
                        registered = false;
                        nodes::register(&self.db, &self.registration(webrtc.available()))
                            .await
                            .map(|()| registered = true)
                    }
                    Err(e) => Err(e),
                }
            } else {
                nodes::register(&self.db, &self.registration(webrtc.available()))
                    .await
                    .map(|()| {
                        registered = true;
                        tracing::info!(
                            task = "cluster-registry",
                            node_id = %self.node_id,
                            advertise_url = %self.config.cluster.advertise_url,
                            "node registered"
                        );
                    })
            };
            match result {
                Ok(()) => {
                    self.heartbeat_ok();
                    if failing {
                        tracing::info!(task = "cluster-registry", "heartbeat restored");
                        failing = false;
                    }
                }
                Err(e) => {
                    if !failing {
                        tracing::warn!(task = "cluster-registry", error = %e, "heartbeat failed");
                        failing = true;
                    }
                    continue;
                }
            }
            match nodes::reap(&self.db, ttl).await {
                Ok(gone) => {
                    for id in gone {
                        tracing::warn!(
                            task = "cluster-registry",
                            node = %id,
                            ttl_secs = ttl.as_secs(),
                            "removed a dead node (no heartbeat within the TTL)"
                        );
                    }
                }
                Err(e) => tracing::debug!(task = "cluster-registry", error = %e, "reaper failed"),
            }
        }
        self.set_draining(true);
        if registered {
            let _ = tokio::time::timeout(
                Duration::from_secs(2),
                nodes::heartbeat(&self.db, &self.node_id, self.load(hub, tracker)),
            )
            .await;
        }
        tracing::info!(task = "cluster-registry", "stopped (draining)");
    }

    /// Delete this node's row (end of a graceful shutdown; cluster mode only).
    pub async fn deregister(&self) {
        if !self.enabled() {
            return;
        }
        match tokio::time::timeout(
            Duration::from_secs(3),
            nodes::remove(&self.db, &self.node_id),
        )
        .await
        {
            Ok(Ok(_)) => tracing::info!(node_id = %self.node_id, "node deregistered"),
            Ok(Err(e)) => tracing::warn!(error = %e, "cannot deregister the node"),
            Err(_) => tracing::warn!("timed out deregistering the node"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::Protocol;

    #[tokio::test]
    async fn single_node_info_and_load() {
        let config = Arc::new(Config::default());
        let db = Db::open_in_memory().unwrap();
        let node = ClusterNode::new(config.clone(), db);
        assert!(!node.enabled());
        assert_eq!(node.node_id(), crate::config::hostname());
        let info = node.info().await;
        assert_eq!(
            (info.enabled, info.backend, info.nodes.len()),
            (false, "sqlite", 0)
        );
        let v = serde_json::to_value(&info).unwrap();
        assert_eq!(v["enabled"], false);
        assert!(v["nodes"].as_array().unwrap().is_empty());

        let hub = Hub::new();
        let tracker = ViewerTracker::new(&config, None, hub.events());
        let ip = "10.0.0.1".parse().unwrap();
        let _a = tracker.open("show", Protocol::Flv, ip, "ua");
        let _b = tracker.open("show", Protocol::WsFlv, ip, "ua");
        let _v = tracker.open("vod:x", Protocol::Hls, ip, "ua");
        let load = node.load(&hub, &tracker);
        assert_eq!(
            (load.live_streams, load.viewers, load.draining),
            (0, 2, false)
        );
        node.set_draining(true);
        assert!(node.load(&hub, &tracker).draining);
        assert!(node.registration(true).whep_url.is_none(), "no public_url");
        assert!(node.last_heartbeat().is_none());
    }

    #[test]
    fn registration_whep_url() {
        let mut config = Config::default();
        config.server.public_url = "https://stream.example.com".into();
        config.cluster.node_id = "n1".into();
        config.cluster.advertise_url = "http://rms1:8081".into();
        let node = ClusterNode::new(Arc::new(config), Db::open_in_memory().unwrap());
        let r = node.registration(true);
        assert_eq!(r.node_id, "n1");
        assert_eq!(r.advertise_url, "http://rms1:8081");
        assert_eq!(
            r.whep_url.as_deref(),
            Some("https://stream.example.com/whep")
        );
        assert!(node.registration(false).whep_url.is_none());
    }
}
