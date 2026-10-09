//! Cluster mode (docs/CLUSTER.md §3): node identity and registry (phase 7C-1), stream
//! ownership, the NOTIFY event bus, relays between nodes and the cluster-wide view used
//! by the API (phase 7C-2).
//!
//! [`ClusterNode`] (`AppState.cluster`) exists in both modes: on a single node it only
//! reports `enabled = false`, the node id (host name by default) and the `sqlite`
//! backend, and every cluster hook is a no-op. In cluster mode `main.rs` spawns
//! [`ClusterNode::spawn_registry`], [`ClusterNode::spawn_refresh`], the bus
//! ([`bus::spawn`]) and the internal listener ([`http::serve_internal`]), and calls
//! [`ClusterNode::deregister`] at the end of a graceful shutdown.
//!
//! - **Registry** (every `cluster.heartbeat_secs`): node row (heartbeat, counters,
//!   draining), then this node's `live_streams` rows (lost rows are claimed again or
//!   their publishers kicked, leaked rows released), then the reapers of dead streams
//!   (NOTIFY `reaped`) and dead nodes.
//! - **Ownership** ([`ClusterNode::claim_publish`] before `Hub::publish_start`,
//!   [`ClusterNode::release_publish`] when the publisher leaves): CLUSTER.md §3.3, with
//!   an internal kick of the live owner and short retries before rejecting.
//! - **Directory** ([`directory::Directory`]): the fresh `live_streams` rows, the alive
//!   nodes, the other nodes' viewer counts and the owners' status answers, refreshed
//!   every 2 s and by the bus; the effective status and the relays read it.
//! - **Relays** ([`relay`]), **bus** ([`bus`]), **internal HTTP** ([`http`], [`client`]).

pub mod bus;
pub mod client;
pub mod directory;
pub mod http;
pub mod relay;

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::db::live_streams::{self, Claim, LiveStreamRow};
use crate::db::nodes::{self, NodeInfo, NodeLoad, NodeRegistration};
use crate::db::recordings::Recording;
use crate::db::{BackendKind, Db, DbError};
use crate::hub::{Hub, HubEvent, LiveStream};
use crate::metrics::{ViewerCounts, ViewerTracker};
use crate::packager::PackagerStatus;
use crate::state::AppState;
use crate::webrtc::WebRtcManager;
use bus::{Channel, Notice, OP_END, OP_REAPED, OP_START, Received};
use client::InternalClient;
use directory::Directory;
use relay::{RelayInfo, Relays};

/// Claim attempts (one every [`CLAIM_RETRY`]) after kicking the live owner: about 3 s
/// for the owner to drop its publisher and release the row.
const CLAIM_ATTEMPTS: u32 = 30;
const CLAIM_RETRY: Duration = Duration::from_millis(100);
/// A `live_streams` row of this node without a local stream is released once it is
/// this old (a publish between claim and `Hub::publish_start` is younger).
const LEAKED_ROW_AGE: chrono::TimeDelta = chrono::TimeDelta::seconds(10);

/// `GET /api/v1/system` → `cluster`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ClusterInfo {
    pub enabled: bool,
    pub node_id: String,
    /// `"sqlite"` | `"postgres"`.
    pub backend: &'static str,
    /// Registered nodes (cluster mode; empty on a single node), by `node_id`.
    pub nodes: Vec<NodeInfo>,
    /// Stream owners (`live_streams` rows with a fresh heartbeat), by name.
    pub live_streams: Vec<LiveStreamRow>,
    /// Relays running on this node (streams owned by other nodes).
    pub relays: Vec<RelayInfo>,
}

/// `GET /internal/status/{name}`: the live details of a stream published on the
/// answering node (fills `BroadcastView.live` on the other nodes).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RemoteStreamStatus {
    pub name: String,
    pub node_id: String,
    pub started_at: DateTime<Utc>,
    pub publisher_ip: Option<String>,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<f64>,
    pub bytes_in: u64,
    pub recording: Option<Recording>,
    pub packager: Option<PackagerStatus>,
}

/// Why a publish cannot take ownership of a stream.
#[derive(Debug, thiserror::Error)]
pub enum ClaimError {
    #[error("this node is draining")]
    Draining,
    #[error("broadcast is live on node {0}")]
    LiveElsewhere(String),
    #[error("cluster database: {0}")]
    Db(#[from] DbError),
}

pub struct ClusterNode {
    config: Arc<Config>,
    db: Db,
    node_id: String,
    started_at: DateTime<Utc>,
    draining: AtomicBool,
    /// Last successful registration/heartbeat.
    last_heartbeat: Mutex<Option<Instant>>,
    hub: Arc<Hub>,
    tracker: Arc<ViewerTracker>,
    shutdown: CancellationToken,
    /// Cluster-wide view (streams, nodes, remote viewers, owners' status).
    pub directory: Directory,
    relays: Relays,
    client: InternalClient,
}

impl ClusterNode {
    pub fn new(
        config: Arc<Config>,
        db: Db,
        hub: Arc<Hub>,
        tracker: Arc<ViewerTracker>,
        shutdown: CancellationToken,
    ) -> Arc<Self> {
        let node_id = if config.cluster.node_id.is_empty() {
            crate::config::hostname()
        } else {
            config.cluster.node_id.clone()
        };
        Arc::new(Self {
            directory: Directory::new(&node_id),
            client: InternalClient::new(&config.cluster.internal_secret),
            relays: Relays::default(),
            config,
            db,
            node_id,
            started_at: Utc::now(),
            draining: AtomicBool::new(false),
            last_heartbeat: Mutex::new(None),
            hub,
            tracker,
            shutdown,
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

    /// Draining (`POST /internal/drain` or shutdown): `/healthz` answers 503, the next
    /// heartbeat says so, no new claims or relays; current publishers keep running.
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

    /// Public WHEP base this node would announce (`cluster.whep_url`, else
    /// `<server.public_url>/whep`).
    pub fn whep_base(&self) -> Option<String> {
        let own = self.config.cluster.whep_url.trim_end_matches('/');
        if !own.is_empty() {
            return Some(own.to_string());
        }
        let public = self.config.server.public_url.trim_end_matches('/');
        (!public.is_empty()).then(|| format!("{public}/whep"))
    }

    /// Row written at registration.
    pub fn registration(&self, webrtc_available: bool) -> NodeRegistration {
        NodeRegistration {
            node_id: self.node_id.clone(),
            advertise_url: self.config.cluster.advertise_url.clone(),
            whep_url: if webrtc_available {
                self.whep_base()
            } else {
                None
            },
            version: env!("CARGO_PKG_VERSION").to_string(),
            started_at: self.started_at,
        }
    }

    /// Counters of the next heartbeat (relays are not this node's live streams; their
    /// viewers are this node's viewers).
    pub fn load(&self, hub: &Hub, tracker: &ViewerTracker) -> NodeLoad {
        let viewers: u32 = tracker
            .all_counts()
            .iter()
            .filter(|(name, _)| !name.starts_with("vod:"))
            .map(|(_, c)| c.total())
            .sum();
        NodeLoad {
            live_streams: hub.local_count().min(u32::MAX as usize) as u32,
            viewers,
            draining: self.is_draining(),
        }
    }

    /// `GET /system` → `cluster` (nodes and stream owners read from the database in
    /// cluster mode).
    pub async fn info(&self) -> ClusterInfo {
        let (nodes, live_streams) = if self.enabled() {
            let nodes = nodes::list(&self.db, self.node_ttl())
                .await
                .unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "cannot list the cluster nodes");
                    Vec::new()
                });
            let streams = live_streams::list_alive(&self.db, self.node_ttl())
                .await
                .unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "cannot list the cluster streams");
                    Vec::new()
                });
            (nodes, streams)
        } else {
            (Vec::new(), Vec::new())
        };
        ClusterInfo {
            enabled: self.enabled(),
            node_id: self.node_id.clone(),
            backend: self.backend().as_str(),
            nodes,
            live_streams,
            relays: self.relays_info(),
        }
    }

    // -----------------------------------------------------------------------------
    // Cluster-wide view used by the API
    // -----------------------------------------------------------------------------

    /// Effective liveness (docs/CLUSTER.md §3.3): the local hub has the stream or
    /// another node owns it (fresh `live_streams` row).
    pub fn is_live(&self, name: &str) -> bool {
        self.hub.is_live(name) || (self.enabled() && self.directory.remote_owner(name).is_some())
    }

    /// Viewers of `name` on every node: the local tracker plus the other nodes' latest
    /// samples (docs/CLUSTER.md §3.6). The local count only on a single node.
    pub fn viewer_counts(&self, name: &str) -> ViewerCounts {
        let mut c = self.tracker.counts(name);
        if self.enabled() {
            c.add(&self.directory.remote_viewers(name));
        }
        c
    }

    /// [`Self::viewer_counts`] with the local part given (SSE events carry it).
    pub fn with_remote_viewers(&self, name: &str, local: ViewerCounts) -> ViewerCounts {
        let mut c = local;
        if self.enabled() {
            c.add(&self.directory.remote_viewers(name));
        }
        c
    }

    /// The owner's live details of a stream published on another node (cached for
    /// [`directory::STATUS_TTL`]; `None` when not remote or the owner did not answer).
    pub async fn remote_status(&self, name: &str) -> Option<RemoteStreamStatus> {
        if !self.enabled() {
            return None;
        }
        let owner = self.directory.remote_owner(name)?;
        if let Some(cached) = self.directory.cached_status(name) {
            return cached;
        }
        let base = match owner.advertise_url {
            Some(u) => u,
            None => self.node_url(&owner.node_id).await?,
        };
        let status = match self.client.status(&base, name).await {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!(broadcast = %name, origin = %owner.node_id, error = %e, "cannot read the owner's stream status");
                self.directory.last_status(name)
            }
        };
        self.directory.set_status(name, status.clone());
        status
    }

    /// Internal URL of `node_id` (directory, else database).
    pub async fn node_url(&self, node_id: &str) -> Option<String> {
        if let Some(n) = self.directory.node(node_id) {
            return Some(n.advertise_url);
        }
        nodes::get(&self.db, node_id, self.node_ttl())
            .await
            .ok()
            .flatten()
            .filter(|n| n.alive)
            .map(|n| n.advertise_url)
    }

    /// Kick the publisher of `name` on its owner node (`POST /internal/kick`).
    /// `Ok(false)` when no node publishes it.
    pub async fn kick_remote(&self, name: &str) -> Result<bool, String> {
        if !self.enabled() {
            return Ok(false);
        }
        let Some(owner) = self.directory.remote_owner(name) else {
            return Ok(false);
        };
        let base = match owner.advertise_url {
            Some(u) => u,
            None => self
                .node_url(&owner.node_id)
                .await
                .ok_or_else(|| format!("node {} is not reachable", owner.node_id))?,
        };
        self.client
            .kick(&base, name)
            .await
            .map_err(|e| e.to_string())
    }

    /// WHEP redirect target (docs/CLUSTER.md §3.8) for a node without usable WebRTC:
    /// `<whep_url>/<name>` of the least loaded alive, non-draining node announcing a
    /// WHEP base different from this node's (same base = same hostname: no redirect,
    /// it would loop). `None` when disabled (`cluster.whep_redirect = false`).
    pub fn whep_redirect_target(&self, name: &str) -> Option<String> {
        if !self.enabled() || !self.config.cluster.whep_redirect {
            return None;
        }
        let own = self.whep_base();
        self.directory
            .nodes()
            .into_iter()
            .filter(|n| n.alive && !n.draining && n.node_id != self.node_id)
            .filter_map(|n| {
                let url = n.whep_url.clone()?;
                (Some(url.trim_end_matches('/')) != own.as_deref()).then_some((n.viewers, url))
            })
            .min_by_key(|(viewers, _)| *viewers)
            .map(|(_, url)| format!("{}/{name}", url.trim_end_matches('/')))
    }

    /// Internal client (internal routes of the other nodes).
    pub fn client(&self) -> &InternalClient {
        &self.client
    }

    // -----------------------------------------------------------------------------
    // Ownership (docs/CLUSTER.md §3.3)
    // -----------------------------------------------------------------------------

    /// Take ownership of `name` before `Hub::publish_start` (cluster mode; `Ok(None)` on
    /// a single node). When another node publishes it, its publisher is kicked
    /// (`POST /internal/kick`) and the claim retried for about 3 s; a local relay of
    /// `name` is stopped once the claim succeeds. Returns the claim id
    /// (`live_streams.session_id`).
    pub async fn claim_publish(&self, name: &str) -> Result<Option<String>, ClaimError> {
        if !self.enabled() {
            return Ok(None);
        }
        if self.is_draining() || self.shutdown.is_cancelled() {
            return Err(ClaimError::Draining);
        }
        let sid = uuid::Uuid::new_v4().simple().to_string();
        let ttl = self.node_ttl();
        let mut kicked = false;
        for attempt in 0..CLAIM_ATTEMPTS {
            match live_streams::claim(&self.db, name, &self.node_id, &sid, ttl).await? {
                Claim::Claimed => {
                    if attempt > 0 {
                        tracing::info!(broadcast = %name, attempts = attempt + 1, "stream ownership taken over");
                    }
                    self.directory.upsert_stream(name, &self.node_id);
                    self.stop_relay_and_wait(name, Duration::from_secs(3)).await;
                    return Ok(Some(sid));
                }
                Claim::OwnedBy(row) => {
                    if !kicked && !row.node_id.is_empty() {
                        kicked = true;
                        match self.node_url(&row.node_id).await {
                            Some(base) => match self.client.kick(&base, name).await {
                                Ok(true) => {
                                    tracing::info!(broadcast = %name, owner = %row.node_id, "kicked the publisher on the owner node")
                                }
                                Ok(false) => {
                                    tracing::info!(broadcast = %name, owner = %row.node_id, "owner node has no publisher; waiting for its row")
                                }
                                Err(e) => {
                                    tracing::warn!(broadcast = %name, owner = %row.node_id, error = %e, "cannot kick the publisher on the owner node")
                                }
                            },
                            None => {
                                tracing::warn!(broadcast = %name, owner = %row.node_id, "owner node unknown; waiting for its row to expire")
                            }
                        }
                    }
                    if attempt + 1 == CLAIM_ATTEMPTS {
                        return Err(ClaimError::LiveElsewhere(row.node_id));
                    }
                    tokio::time::sleep(CLAIM_RETRY).await;
                }
            }
        }
        Err(ClaimError::LiveElsewhere(String::new()))
    }

    /// `Hub::publish_start` succeeded for a claimed stream: tell the other nodes.
    pub fn announce_start(&self, name: &str) {
        if self.enabled() {
            self.hub.outbox().send(Notice::stream(name, OP_START));
        }
    }

    /// The publisher of a claimed stream left: release the row (only this claim's) and
    /// tell the other nodes.
    pub async fn release_publish(&self, name: &str, claim_id: Option<&str>) {
        if !self.enabled() {
            return;
        }
        let Some(sid) = claim_id else { return };
        match live_streams::release(&self.db, name, &self.node_id, Some(sid)).await {
            Ok(true) => {
                self.directory.remove_stream(name, Some(&self.node_id));
                self.hub.outbox().send(Notice::stream(name, OP_END));
            }
            Ok(false) => {
                tracing::debug!(broadcast = %name, "stream row already taken over or gone")
            }
            Err(e) => {
                // The row expires with the TTL (the heartbeat releases leaked rows too).
                tracing::warn!(broadcast = %name, error = %e, "cannot release the stream row")
            }
        }
    }

    /// Heartbeat of this node's `live_streams` rows. Rows lost while the node was cut
    /// off are claimed again (same claim id) unless another node owns the stream now,
    /// in which case the local publisher is kicked; rows without a local stream are
    /// released.
    async fn heartbeat_streams(&self) -> Result<(), DbError> {
        let refreshed = live_streams::heartbeat_all(&self.db, &self.node_id).await?;
        let local: Vec<Arc<LiveStream>> = self
            .hub
            .list()
            .into_iter()
            .filter(|s| s.origin.is_local() && !s.is_ended())
            .collect();
        if refreshed as usize == local.len() {
            return Ok(());
        }
        let owned = live_streams::owned_by(&self.db, &self.node_id).await?;
        let owned_names: HashSet<&str> = owned.iter().map(|r| r.name.as_str()).collect();
        for s in &local {
            if owned_names.contains(s.name.as_str()) {
                continue;
            }
            let sid = s
                .claim_id
                .clone()
                .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
            match live_streams::claim(&self.db, &s.name, &self.node_id, &sid, self.node_ttl())
                .await?
            {
                Claim::Claimed => {
                    tracing::warn!(broadcast = %s.name, "stream row was lost (database outage or pause); claimed again");
                    self.hub.outbox().send(Notice::stream(&s.name, OP_START));
                }
                Claim::OwnedBy(row) => {
                    tracing::warn!(broadcast = %s.name, owner = %row.node_id, "stream ownership lost to another node; kicking the local publisher");
                    s.kick();
                }
            }
        }
        let local_names: HashSet<&str> = local.iter().map(|s| s.name.as_str()).collect();
        let now = Utc::now();
        for row in owned {
            if local_names.contains(row.name.as_str()) || now - row.started_at < LEAKED_ROW_AGE {
                continue;
            }
            if live_streams::release(&self.db, &row.name, &self.node_id, Some(&row.session_id))
                .await?
            {
                tracing::warn!(broadcast = %row.name, "released a stream row without a local publisher");
                self.directory.remove_stream(&row.name, Some(&self.node_id));
                self.hub.outbox().send(Notice::stream(&row.name, OP_END));
            }
        }
        Ok(())
    }

    /// Reaper of dead owners' rows (every node): NOTIFY and apply locally.
    async fn reap_streams(&self) -> Result<(), DbError> {
        for row in live_streams::reap(&self.db, self.node_ttl()).await? {
            tracing::warn!(broadcast = %row.name, owner = %row.node_id, ttl_secs = self.node_ttl().as_secs(), "removed the stream of a dead owner (no heartbeat within the TTL)");
            let mut notice = Notice::stream(&row.name, OP_REAPED);
            notice.owner = Some(row.node_id.clone());
            self.hub.outbox().send(notice);
            self.stream_gone(&row.name, &row.node_id);
        }
        Ok(())
    }

    /// `name` is no longer live on `owner` (release, reap; `""` = whoever owned it):
    /// directory, relay, SSE.
    fn stream_gone(&self, name: &str, owner: &str) {
        let owner = (!owner.is_empty()).then_some(owner);
        self.directory.remove_stream(name, owner);
        self.stop_relay(name, owner);
        self.hub.emit(HubEvent::RemoteStreamChanged {
            name: name.to_string(),
        });
    }

    // -----------------------------------------------------------------------------
    // Directory refresh and bus
    // -----------------------------------------------------------------------------

    /// Reload the `live_streams` view and the alive nodes; emits `RemoteStreamChanged`
    /// for names whose remote owner changed and stops relays whose owner is gone.
    pub async fn refresh_directory(&self) {
        let ttl = self.node_ttl();
        let (rows, nodes) = match tokio::join!(
            live_streams::list_alive(&self.db, ttl),
            nodes::list(&self.db, ttl)
        ) {
            (Ok(r), Ok(n)) => (r, n),
            (Err(e), _) | (_, Err(e)) => {
                tracing::debug!(error = %e, "cannot refresh the cluster directory");
                return;
            }
        };
        let before: Vec<(String, String)> = self
            .directory
            .rows()
            .into_iter()
            .map(|r| (r.name, r.node_id))
            .collect();
        self.directory.replace(rows, nodes);
        let after: Vec<(String, String)> = self
            .directory
            .rows()
            .into_iter()
            .map(|r| (r.name, r.node_id))
            .collect();
        let changed: HashSet<&String> = before
            .iter()
            .filter(|b| !after.contains(b))
            .chain(after.iter().filter(|a| !before.contains(a)))
            .filter(|(_, node)| *node != self.node_id)
            .map(|(name, _)| name)
            .collect();
        for name in changed {
            self.hub
                .emit(HubEvent::RemoteStreamChanged { name: name.clone() });
        }
        self.reconcile_relays();
    }

    /// Reload the other nodes' viewer counts (latest samples); emits
    /// `ViewerCountChanged` (local counts, the SSE adds the remote part) on changes.
    async fn refresh_remote_viewers(&self) {
        let window = 2 * self.tracker.sample_interval_secs() as i64 + 2;
        let since = Utc::now().timestamp() - window;
        match crate::db::metrics::recent_counts_of_other_nodes(&self.db, &self.node_id, since).await
        {
            Ok(counts) => {
                for name in self.directory.set_remote_viewers(counts) {
                    self.hub.emit(HubEvent::ViewerCountChanged {
                        counts: self.tracker.counts(&name),
                        name,
                    });
                }
            }
            Err(e) => tracing::debug!(error = %e, "cannot read the other nodes' viewers"),
        }
    }

    /// Full resync after the bus (re)connected: notifications sent meanwhile are lost.
    pub async fn resync(&self, state: &AppState) {
        self.db.broadcast_cache().invalidate_all();
        self.refresh_directory().await;
        state.vod.wake();
        tracing::debug!(
            task = "cluster-bus",
            streams = self.directory.rows().len(),
            "resynced"
        );
    }

    /// Apply a notification of another node.
    pub async fn on_notification(&self, state: &AppState, rx: Received) {
        let name = rx.payload.name.clone();
        let node = rx.payload.node.clone();
        tracing::debug!(task = "cluster-bus", channel = rx.channel.as_str(), name = %name, op = %rx.payload.op, from = %node, "notification");
        match rx.channel {
            Channel::Broadcasts => {
                self.db.broadcast_cache().invalidate(&name);
                self.hub
                    .emit(HubEvent::BroadcastUpdated { name: name.clone() });
                match crate::db::broadcasts::find_by_name(&self.db, &name).await {
                    Ok(None) => {
                        // Deleted elsewhere.
                        self.hub.kick(&name);
                        self.stop_relay(&name, None);
                        state.webrtc.close_broadcast(&name);
                    }
                    Ok(Some(b)) if !b.enable_webrtc => {
                        state.webrtc.close_broadcast(&name);
                    }
                    Ok(Some(_)) => {}
                    Err(e) => tracing::debug!(error = %e, "cannot reload a changed broadcast"),
                }
            }
            Channel::Settings => {
                self.db.broadcast_cache().invalidate_all();
                self.hub.emit(HubEvent::SettingsChanged);
            }
            Channel::Vod => {
                self.hub.emit(HubEvent::VodChanged { id: name });
                state.vod.wake();
            }
            Channel::Streams => match rx.payload.op.as_str() {
                OP_START => {
                    self.directory.upsert_stream(&name, &node);
                    if let Some(s) = self.hub.get(&name) {
                        if s.origin.is_local() {
                            tracing::warn!(broadcast = %name, owner = %node, "another node claimed this stream; kicking the local publisher");
                            s.kick();
                        } else if s.origin.remote_node() != Some(node.as_str()) {
                            self.stop_relay(&name, None);
                        }
                    }
                    self.hub.emit(HubEvent::RemoteStreamChanged { name });
                }
                OP_END => self.stream_gone(&name, &node),
                OP_REAPED => {
                    let owner = rx.payload.owner.clone().unwrap_or_default();
                    self.stream_gone(&name, &owner);
                }
                other => tracing::debug!(op = %other, "unknown stream notice"),
            },
        }
    }

    /// Start the registry task (cluster mode only; `None` on a single node).
    pub fn spawn_registry(
        self: &Arc<Self>,
        webrtc: Arc<WebRtcManager>,
        shutdown: CancellationToken,
    ) -> Option<tokio::task::JoinHandle<()>> {
        if !self.enabled() {
            return None;
        }
        let me = Arc::clone(self);
        Some(tokio::spawn(async move {
            me.registry_loop(&webrtc, shutdown).await
        }))
    }

    /// Start the directory refresh task (every [`directory::REFRESH`]; cluster mode).
    pub fn spawn_refresh(self: &Arc<Self>) -> Option<tokio::task::JoinHandle<()>> {
        if !self.enabled() {
            return None;
        }
        let me = Arc::clone(self);
        Some(tokio::spawn(async move {
            let mut tick = tokio::time::interval(directory::REFRESH);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = me.shutdown.cancelled() => break,
                    _ = tick.tick() => {}
                }
                me.refresh_directory().await;
                me.refresh_remote_viewers().await;
            }
            for h in me.relays.all() {
                h.stop();
            }
        }))
    }

    async fn registry_loop(&self, webrtc: &WebRtcManager, shutdown: CancellationToken) {
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
            let load = self.load(&self.hub, &self.tracker);
            let result = if registered {
                match nodes::heartbeat(&self.db, &self.node_id, load).await {
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
            let result = match result {
                Ok(()) => self.heartbeat_streams().await,
                Err(e) => Err(e),
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
            if let Err(e) = self.reap_streams().await {
                tracing::debug!(task = "cluster-registry", error = %e, "stream reaper failed");
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
                nodes::heartbeat(&self.db, &self.node_id, self.load(&self.hub, &self.tracker)),
            )
            .await;
        }
        tracing::info!(task = "cluster-registry", "stopped (draining)");
    }

    /// Delete this node's row (end of a graceful shutdown; cluster mode only). Its
    /// `live_streams` rows go with it (cascade); the others learn it from NOTIFY.
    pub async fn deregister(&self) {
        if !self.enabled() {
            return;
        }
        let names: Vec<String> = live_streams::owned_by(&self.db, &self.node_id)
            .await
            .map(|rows| rows.into_iter().map(|r| r.name).collect())
            .unwrap_or_default();
        match tokio::time::timeout(
            Duration::from_secs(3),
            nodes::remove(&self.db, &self.node_id),
        )
        .await
        {
            Ok(Ok(_)) => {
                tracing::info!(node_id = %self.node_id, "node deregistered");
                // The bus sender may already be gone: notify directly.
                if let Some(pool) = self.db.pg() {
                    for name in names {
                        let payload = bus::Payload {
                            name,
                            op: OP_END.into(),
                            node: self.node_id.clone(),
                            owner: None,
                        };
                        if let Some(text) = payload.encode() {
                            let _ = tokio::time::timeout(
                                Duration::from_secs(1),
                                crate::db::pg::q("SELECT pg_notify($1, $2)")
                                    .bind(Channel::Streams.as_str())
                                    .bind(text)
                                    .execute(pool),
                            )
                            .await;
                        }
                    }
                }
            }
            Ok(Err(e)) => tracing::warn!(error = %e, "cannot deregister the node"),
            Err(_) => tracing::warn!("timed out deregistering the node"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::Protocol;

    fn node_with(config: Config) -> (Arc<ClusterNode>, Arc<Hub>, Arc<ViewerTracker>) {
        let config = Arc::new(config);
        let hub = Hub::new();
        let tracker = ViewerTracker::new(&config, None, hub.events());
        let node = ClusterNode::new(
            config,
            Db::open_in_memory().unwrap(),
            hub.clone(),
            tracker.clone(),
            CancellationToken::new(),
        );
        (node, hub, tracker)
    }

    #[tokio::test]
    async fn single_node_info_and_load() {
        let (node, hub, tracker) = node_with(Config::default());
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
        assert!(v["relays"].as_array().unwrap().is_empty());

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
        // Single node: no claims, no remote views.
        assert_eq!(node.claim_publish("show").await.unwrap(), None);
        assert!(!node.is_live("show"));
        assert_eq!(node.viewer_counts("show").total(), 2);
        assert!(node.whep_redirect_target("show").is_none());
    }

    #[test]
    fn registration_whep_url() {
        let mut config = Config::default();
        config.server.public_url = "https://stream.example.com".into();
        config.cluster.node_id = "n1".into();
        config.cluster.advertise_url = "http://rms1:8081".into();
        let (node, _, _) = node_with(config.clone());
        let r = node.registration(true);
        assert_eq!(r.node_id, "n1");
        assert_eq!(r.advertise_url, "http://rms1:8081");
        assert_eq!(
            r.whep_url.as_deref(),
            Some("https://stream.example.com/whep")
        );
        assert!(node.registration(false).whep_url.is_none());
        config.cluster.whep_url = "https://n1.stream.example.com/whep/".into();
        let (node, _, _) = node_with(config);
        assert_eq!(
            node.registration(true).whep_url.as_deref(),
            Some("https://n1.stream.example.com/whep")
        );
    }

    #[test]
    fn whep_redirect_picks_another_base() {
        let mut config = Config::default();
        config.cluster.enabled = true;
        config.cluster.node_id = "n2".into();
        config.server.public_url = "https://stream.example.com".into();
        let (node, _, _) = node_with(config);
        let n = |id: &str, url: Option<&str>, viewers: u32, draining: bool| NodeInfo {
            node_id: id.into(),
            advertise_url: format!("http://{id}:8081"),
            whep_url: url.map(str::to_string),
            version: "t".into(),
            started_at: Utc::now(),
            heartbeat: Utc::now(),
            live_streams: 0,
            viewers,
            draining,
            alive: true,
        };
        node.directory.replace(
            Vec::new(),
            vec![
                // Same base as this node (same hostname): never a target.
                n("n1", Some("https://stream.example.com/whep"), 0, false),
                n("n3", Some("https://n3.example.com/whep"), 9, false),
                n("n4", Some("https://n4.example.com/whep/"), 2, false),
                n("n5", Some("https://n5.example.com/whep"), 0, true),
                n("n6", None, 0, false),
            ],
        );
        assert_eq!(
            node.whep_redirect_target("show").as_deref(),
            Some("https://n4.example.com/whep/show")
        );
    }
}
