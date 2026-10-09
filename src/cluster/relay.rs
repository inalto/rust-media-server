//! Relay origin → edge (docs/CLUSTER.md §3.4).
//!
//! A request for a stream this node does not have, while `live_streams` says another
//! node owns it ([`ClusterNode::live_stream`]: FLV, WS-FLV, HLS/DASH, WHEP), starts
//! **one** relay per name (single-flight): `GET {owner}/internal/relay/{name}.flv`
//! (the owner's FLV bytes: header, GOP snapshot, live tags), parsed with
//! [`FlvStreamParser`] and pushed into a local [`LiveStream`] with
//! `origin = Remote(owner)`. Packager, WHEP and FLV egress consume it like any stream;
//! it has no `stream_sessions` row, no recording and is never relayed again (the owner
//! only serves its own publishers).
//!
//! The relay ends: after `cluster.relay_idle_secs` without local viewers (tracker
//! counts, all protocols), when cancelled (stream end / owner change notice, reconcile
//! against the refreshed `live_streams` view, a local publisher claiming the name,
//! shutdown) or when the owner closes the stream. After an upstream error or EOF it
//! retries with backoff (250 ms → 5 s) while the owner row is alive and someone is
//! watching; each attempt is a new local stream (viewers reconnect, like after a
//! publisher reconnect on a single node).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use futures::StreamExt;
use serde::Serialize;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::ClusterNode;
use super::directory::RemoteOwner;
use crate::db;
use crate::hub::flv_parser::FlvStreamParser;
use crate::hub::{HubError, LiveStream, MediaPacket, PacketKind, PublishInfo, StreamOrigin};

/// How long a request waits for a starting relay to deliver its first decodable frame.
pub const RELAY_START_WAIT: Duration = Duration::from_secs(6);
/// A connected relay is reported ready after this long even without a keyframe.
const READY_FALLBACK: Duration = Duration::from_secs(3);
const BACKOFF_MIN: Duration = Duration::from_millis(250);
const BACKOFF_MAX: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RelayState {
    Connecting,
    Ready,
    /// The last attempt failed; retrying while the owner row is alive.
    Failed,
    Stopped,
}

/// One relay (single flight per name).
pub struct RelayHandle {
    pub name: String,
    pub node_id: String,
    pub started_at: DateTime<Utc>,
    cancel: CancellationToken,
    state: watch::Sender<RelayState>,
    stream: Mutex<Weak<LiveStream>>,
    attempts: AtomicU32,
}

impl RelayHandle {
    fn new(name: &str, node_id: &str, cancel: CancellationToken) -> Arc<Self> {
        Arc::new(Self {
            name: name.to_string(),
            node_id: node_id.to_string(),
            started_at: Utc::now(),
            cancel,
            state: watch::channel(RelayState::Connecting).0,
            stream: Mutex::new(Weak::new()),
            attempts: AtomicU32::new(0),
        })
    }

    pub fn state(&self) -> RelayState {
        *self.state.borrow()
    }

    fn set_state(&self, s: RelayState) {
        self.state.send_replace(s);
    }

    pub fn is_finished(&self) -> bool {
        self.state() == RelayState::Stopped || self.cancel.is_cancelled()
    }

    pub fn stop(&self) {
        self.cancel.cancel();
    }

    fn current_stream(&self) -> Option<Arc<LiveStream>> {
        self.stream
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .upgrade()
    }
}

/// `GET /system` → `cluster.relays[]`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct RelayInfo {
    pub name: String,
    /// Owner node the stream is relayed from.
    pub origin_node: String,
    pub started_at: DateTime<Utc>,
    pub state: RelayState,
    /// Connection attempts so far.
    pub attempts: u32,
    pub bytes_in: u64,
    /// Local viewers (all protocols).
    pub viewers: u32,
}

/// Outcome of one upstream connection.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    /// Stopped (cancelled, shutdown) or idle: do not retry.
    Done,
    /// The owner closed the stream (EOF) or answered 404: retry while the row lives.
    Ended,
    /// A publisher of this node owns the name now.
    LocalPublisher,
}

/// Running relays by name.
#[derive(Default)]
pub struct Relays {
    map: Mutex<HashMap<String, Arc<RelayHandle>>>,
}

impl Relays {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<RelayHandle>>> {
        self.map.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn get(&self, name: &str) -> Option<Arc<RelayHandle>> {
        self.lock().get(name).cloned()
    }

    pub fn all(&self) -> Vec<Arc<RelayHandle>> {
        let mut v: Vec<_> = self.lock().values().cloned().collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    fn remove_if(&self, handle: &Arc<RelayHandle>) {
        let mut map = self.lock();
        if map
            .get(&handle.name)
            .is_some_and(|h| Arc::ptr_eq(h, handle))
        {
            map.remove(&handle.name);
        }
    }

    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl ClusterNode {
    /// The local stream of `name`, starting a relay from its owner when another node
    /// publishes it (cluster mode). `None` when it is live nowhere (or the relay could
    /// not start within [`RELAY_START_WAIT`]).
    pub async fn live_stream(self: &Arc<Self>, name: &str) -> Option<Arc<LiveStream>> {
        if let Some(s) = self.hub.get(name).filter(|s| !s.is_ended()) {
            return Some(s);
        }
        if !self.enabled() || self.is_draining() || self.shutdown.is_cancelled() {
            return None;
        }
        let owner = self.directory.remote_owner(name)?;
        self.ensure_relay(name, owner).await
    }

    /// Single-flight relay start; waits for it to be ready.
    pub async fn ensure_relay(
        self: &Arc<Self>,
        name: &str,
        owner: RemoteOwner,
    ) -> Option<Arc<LiveStream>> {
        let handle = {
            let mut map = self.relays.lock();
            match map.get(name) {
                Some(h) if !h.is_finished() && h.node_id == owner.node_id => Arc::clone(h),
                existing => {
                    if let Some(old) = existing {
                        old.stop();
                    }
                    let h = RelayHandle::new(name, &owner.node_id, self.shutdown.child_token());
                    map.insert(name.to_string(), Arc::clone(&h));
                    let me = Arc::clone(self);
                    let task = Arc::clone(&h);
                    tokio::spawn(async move { me.run_relay(task).await });
                    tracing::info!(task = "relay", broadcast = %name, origin = %owner.node_id, "relay requested");
                    h
                }
            }
        };
        let mut rx = handle.state.subscribe();
        let ready = tokio::time::timeout(
            RELAY_START_WAIT,
            rx.wait_for(|s| *s != RelayState::Connecting),
        )
        .await;
        match ready {
            Ok(Ok(s)) if *s == RelayState::Ready => handle
                .current_stream()
                .filter(|s| !s.is_ended())
                .or_else(|| self.hub.get(name).filter(|s| !s.is_ended())),
            _ => None,
        }
    }

    /// Stop the relay of `name` (only if it comes from `node`, when given).
    pub fn stop_relay(&self, name: &str, node: Option<&str>) -> bool {
        match self.relays.get(name) {
            Some(h) if node.is_none_or(|n| h.node_id == n) => {
                h.stop();
                true
            }
            _ => false,
        }
    }

    /// Stop the relay of `name` and wait (bounded) until its local stream is gone.
    pub async fn stop_relay_and_wait(&self, name: &str, timeout: Duration) {
        let Some(h) = self.relays.get(name) else {
            return;
        };
        h.stop();
        let mut rx = h.state.subscribe();
        let _ = tokio::time::timeout(timeout, rx.wait_for(|s| *s == RelayState::Stopped)).await;
    }

    /// Stop the relays whose owner is no longer the one in the `live_streams` view.
    pub fn reconcile_relays(&self) {
        for h in self.relays.all() {
            let valid = self
                .directory
                .remote_owner(&h.name)
                .is_some_and(|o| o.node_id == h.node_id);
            if !valid && !h.is_finished() {
                tracing::info!(task = "relay", broadcast = %h.name, origin = %h.node_id, "owner gone or changed; stopping the relay");
                h.stop();
            }
        }
    }

    /// `GET /system` → `cluster.relays`.
    pub fn relays_info(&self) -> Vec<RelayInfo> {
        self.relays
            .all()
            .into_iter()
            .map(|h| RelayInfo {
                name: h.name.clone(),
                origin_node: h.node_id.clone(),
                started_at: h.started_at,
                state: h.state(),
                attempts: h.attempts.load(Ordering::Relaxed),
                bytes_in: h.current_stream().map(|s| s.bytes_in()).unwrap_or(0),
                viewers: self.tracker.counts(&h.name).total(),
            })
            .collect()
    }

    async fn run_relay(self: Arc<Self>, h: Arc<RelayHandle>) {
        let name = h.name.clone();
        let idle_after = Duration::from_secs(self.config.cluster.relay_idle_secs.max(1));
        let mut backoff = BACKOFF_MIN;
        // Last time someone wanted the stream (start, or local viewers seen).
        let mut wanted = Instant::now();
        loop {
            if h.cancel.is_cancelled() {
                break;
            }
            let Some(owner) = self
                .directory
                .remote_owner(&name)
                .filter(|o| o.node_id == h.node_id)
            else {
                tracing::debug!(task = "relay", broadcast = %name, "owner row gone; relay not (re)started");
                break;
            };
            let base = match owner.advertise_url {
                Some(u) => Some(u),
                None => {
                    self.refresh_directory().await;
                    self.directory.node(&h.node_id).map(|n| n.advertise_url)
                }
            };
            let outcome = match base {
                Some(base) => {
                    h.attempts.fetch_add(1, Ordering::Relaxed);
                    self.relay_once(&h, &base, &mut wanted).await
                }
                None => Err(format!("node {} has no advertise URL", h.node_id)),
            };
            match outcome {
                Ok(Outcome::Done) | Ok(Outcome::LocalPublisher) => break,
                Ok(Outcome::Ended) => {
                    tracing::debug!(task = "relay", broadcast = %name, "upstream ended")
                }
                Err(e) => {
                    tracing::warn!(task = "relay", broadcast = %name, origin = %h.node_id, error = %e, retry_ms = backoff.as_millis() as u64, "relay connection failed")
                }
            }
            // Waiters get an answer now; the retry continues in the background, but only
            // while someone may still want the stream.
            h.set_state(RelayState::Failed);
            if self.tracker.counts(&name).total() > 0 {
                wanted = Instant::now();
            } else if wanted.elapsed() >= idle_after {
                break;
            }
            tokio::select! {
                _ = h.cancel.cancelled() => break,
                _ = tokio::time::sleep(backoff) => {}
            }
            backoff = (backoff * 2).min(BACKOFF_MAX);
        }
        h.set_state(RelayState::Stopped);
        self.relays.remove_if(&h);
        tracing::info!(task = "relay", broadcast = %name, origin = %h.node_id, "relay stopped");
    }

    /// One upstream connection: publish the local stream, pump packets until EOF,
    /// cancellation, idleness or an error; the local stream ends with it.
    async fn relay_once(
        &self,
        h: &Arc<RelayHandle>,
        base: &str,
        wanted: &mut Instant,
    ) -> Result<Outcome, String> {
        let name = &h.name;
        let resp = self
            .client
            .relay(base, name)
            .await
            .map_err(|e| e.to_string())?;
        match resp.status() {
            s if s.is_success() => {}
            reqwest::StatusCode::NOT_FOUND => return Ok(Outcome::Ended),
            s => return Err(format!("owner answered {s}")),
        }
        let broadcast = db::broadcasts::find_by_name(&self.db, name)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "broadcast not found".to_string())?;
        let mut info = PublishInfo::new(broadcast.id);
        info.origin = StreamOrigin::Remote(h.node_id.clone());
        let stream = loop {
            match self.hub.publish_start(name, info.clone()) {
                Ok(s) => break s,
                Err(HubError::AlreadyLive(_)) => match self.hub.get(name) {
                    // A previous relay of this name (older owner): replace it.
                    Some(old) if old.is_relay() => {
                        self.hub.publish_end_stream(&old);
                    }
                    Some(_) => return Ok(Outcome::LocalPublisher),
                    None => {}
                },
            }
        };
        *h.stream.lock().unwrap_or_else(|e| e.into_inner()) = Arc::downgrade(&stream);
        tracing::info!(task = "relay", broadcast = %name, origin = %h.node_id, "relay connected");
        let result = self.pump(h, &stream, resp, wanted).await;
        self.hub.publish_end_stream(&stream);
        result
    }

    async fn pump(
        &self,
        h: &Arc<RelayHandle>,
        stream: &Arc<LiveStream>,
        resp: reqwest::Response,
        wanted: &mut Instant,
    ) -> Result<Outcome, String> {
        let idle_after = Duration::from_secs(self.config.cluster.relay_idle_secs.max(1));
        let mut body = resp.bytes_stream();
        let mut parser = FlvStreamParser::new();
        let connected = Instant::now();
        let mut ready = false;
        let mut video_header = false;
        let mut idle_since: Option<Instant> = None;
        let mut tick = tokio::time::interval(Duration::from_millis(500));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                _ = h.cancel.cancelled() => return Ok(Outcome::Done),
                _ = tick.tick() => {
                    if stream.is_ended() {
                        // Ended by someone else (a local publisher took the name).
                        return Ok(Outcome::LocalPublisher);
                    }
                    if !ready && connected.elapsed() >= READY_FALLBACK {
                        ready = true;
                        h.set_state(RelayState::Ready);
                    }
                    if self.tracker.counts(&h.name).total() == 0 {
                        // Idle since the last viewer left (or since the request that
                        // started the relay, whose viewer may never have connected).
                        let since = *idle_since.get_or_insert(*wanted);
                        if since.elapsed() >= idle_after {
                            tracing::info!(task = "relay", broadcast = %h.name, idle_secs = idle_after.as_secs(), "no local viewers; stopping the relay");
                            return Ok(Outcome::Done);
                        }
                    } else {
                        idle_since = None;
                        *wanted = Instant::now();
                    }
                }
                chunk = body.next() => match chunk {
                    Some(Ok(bytes)) => {
                        parser.push(&bytes);
                        loop {
                            match parser.next_packet() {
                                Ok(Some(p)) => {
                                    if p.kind == PacketKind::Video && p.is_sequence_header {
                                        video_header = true;
                                    }
                                    let decodable = !p.is_sequence_header
                                        && match p.kind {
                                            PacketKind::Video => p.is_keyframe,
                                            PacketKind::Audio => !video_header,
                                            PacketKind::Script => false,
                                        };
                                    push_relayed(stream, p);
                                    if decodable && !ready {
                                        ready = true;
                                        h.set_state(RelayState::Ready);
                                    }
                                }
                                Ok(None) => break,
                                Err(e) => return Err(format!("invalid FLV from the owner: {e}")),
                            }
                        }
                    }
                    Some(Err(e)) => return Err(e.to_string()),
                    None => return Ok(Outcome::Ended),
                },
            }
        }
    }
}

/// Push one relayed packet; `onMetaData` also updates the stream info (codec, fps,
/// declared bitrates) as the RTMP ingest does.
fn push_relayed(stream: &LiveStream, p: MediaPacket) {
    if p.kind == PacketKind::Script
        && let Ok(values) = rtmp_rs::amf::amf0::decode_all(&p.data)
        && values.first().and_then(|v| v.as_str()) == Some("onMetaData")
        && let Some(map) = values.get(1).and_then(|v| v.as_object())
    {
        stream.apply_metadata(map);
    }
    stream.push(p);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::db::Db;
    use crate::db::live_streams::LiveStreamRow;
    use crate::db::nodes::NodeInfo;
    use crate::hub::Hub;
    use crate::hub::packet::{flv_file_header, flv_tag_bytes};
    use crate::metrics::{Protocol, ViewerTracker};
    use axum::Router;
    use axum::extract::{Path, State};
    use axum::http::StatusCode;
    use axum::response::{IntoResponse, Response};
    use bytes::Bytes;
    use std::sync::atomic::AtomicU32;

    #[derive(Clone)]
    struct Origin {
        connections: Arc<AtomicU32>,
        live: bool,
    }

    /// Fake owner node: `GET /internal/relay/{name}.flv` streams an FLV header, the
    /// sequence header, a keyframe, then an inter frame every 40 ms.
    async fn relay_route(State(o): State<Origin>, Path(_file): Path<String>) -> Response {
        o.connections.fetch_add(1, Ordering::SeqCst);
        if !o.live {
            return StatusCode::NOT_FOUND.into_response();
        }
        let body = async_stream::stream! {
            let mut first = flv_file_header(true, false).to_vec();
            first.extend_from_slice(&flv_tag_bytes(&MediaPacket::video(
                0,
                Bytes::from_static(&[0x17, 0x00, 0, 0, 0, 1, 0x64, 0, 0x1f]),
            )));
            first.extend_from_slice(&flv_tag_bytes(&MediaPacket::video(
                0,
                Bytes::from_static(&[0x17, 0x01, 0, 0, 0, 9]),
            )));
            yield Ok::<Bytes, std::io::Error>(Bytes::from(first));
            let mut ts = 0u32;
            loop {
                tokio::time::sleep(Duration::from_millis(40)).await;
                ts += 40;
                yield Ok(flv_tag_bytes(&MediaPacket::video(
                    ts,
                    Bytes::from_static(&[0x27, 0x01, 0, 0, 0, 1]),
                )));
            }
        };
        Response::new(axum::body::Body::from_stream(body))
    }

    async fn fake_origin(live: bool) -> (String, Arc<AtomicU32>) {
        let connections = Arc::new(AtomicU32::new(0));
        let app = Router::new()
            .route("/internal/relay/{file}", axum::routing::get(relay_route))
            .with_state(Origin {
                connections: Arc::clone(&connections),
                live,
            });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}"), connections)
    }

    async fn edge(origin_url: &str) -> (Arc<ClusterNode>, Arc<Hub>, Arc<ViewerTracker>) {
        let mut config = Config::default();
        config.cluster.enabled = true;
        config.cluster.node_id = "edge".into();
        config.cluster.internal_secret = "s".repeat(32);
        config.cluster.relay_idle_secs = 1;
        let config = Arc::new(config);
        let db = Db::open_in_memory().unwrap();
        db::broadcasts::create(&db, db::broadcasts::NewBroadcast::new("show", "Show"))
            .await
            .unwrap();
        let hub = Hub::new();
        let tracker = ViewerTracker::new(&config, None, hub.events());
        let node = ClusterNode::new(
            config,
            db,
            Arc::clone(&hub),
            Arc::clone(&tracker),
            CancellationToken::new(),
        );
        node.directory.replace(
            vec![LiveStreamRow {
                name: "show".into(),
                node_id: "origin".into(),
                session_id: "s1".into(),
                started_at: Utc::now(),
                heartbeat: Utc::now(),
            }],
            vec![NodeInfo {
                node_id: "origin".into(),
                advertise_url: origin_url.to_string(),
                whep_url: None,
                version: "t".into(),
                started_at: Utc::now(),
                heartbeat: Utc::now(),
                live_streams: 1,
                viewers: 0,
                draining: false,
                alive: true,
            }],
        );
        (node, hub, tracker)
    }

    async fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !f() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn single_flight_then_idle_stop() {
        let (url, connections) = fake_origin(true).await;
        let (node, hub, _tracker) = edge(&url).await;
        let mut waiters = Vec::new();
        for _ in 0..8 {
            let n = Arc::clone(&node);
            waiters.push(tokio::spawn(async move { n.live_stream("show").await }));
        }
        let mut streams = Vec::new();
        for w in waiters {
            streams.push(w.await.unwrap().expect("relayed stream"));
        }
        assert!(streams.iter().all(|s| Arc::ptr_eq(s, &streams[0])));
        assert_eq!(
            connections.load(Ordering::SeqCst),
            1,
            "one upstream connection"
        );
        let s = &streams[0];
        assert_eq!(s.origin, StreamOrigin::Remote("origin".into()));
        assert!(s.stream_session_id.is_none());
        assert_eq!(s.info().video_codec.as_deref(), Some("h264"));
        let info = node.relays_info();
        assert_eq!(info.len(), 1);
        assert_eq!(
            (info[0].origin_node.as_str(), info[0].state),
            ("origin", RelayState::Ready)
        );
        assert_eq!(hub.local_count(), 0, "a relay is not a local stream");
        drop(streams);
        // No local viewer: stopped after relay_idle_secs (1 s).
        wait_until("idle stop", || {
            hub.get("show").is_none() && node.relays.is_empty()
        })
        .await;
        assert_eq!(
            connections.load(Ordering::SeqCst),
            1,
            "no reconnect when idle"
        );
    }

    #[tokio::test]
    async fn owner_gone_stops_the_relay_and_viewers_keep_it_alive() {
        let (url, _) = fake_origin(true).await;
        let (node, hub, tracker) = edge(&url).await;
        let s = node.live_stream("show").await.expect("relayed stream");
        let _viewer = tracker.open("show", Protocol::Flv, "10.0.0.1".parse().unwrap(), "ua");
        tokio::time::sleep(Duration::from_millis(1600)).await;
        assert!(
            !s.is_ended(),
            "a viewer keeps the relay alive past relay_idle_secs"
        );
        // The owner row disappears (end notice / reaper): reconcile stops the relay.
        node.directory.remove_stream("show", Some("origin"));
        node.reconcile_relays();
        wait_until("relay end", || s.is_ended() && node.relays.is_empty()).await;
        assert!(hub.get("show").is_none());
        // Nothing live anywhere: no relay starts.
        assert!(node.live_stream("show").await.is_none());
    }

    #[tokio::test]
    async fn owner_without_the_stream_fails_fast() {
        let (url, connections) = fake_origin(false).await;
        let (node, hub, _) = edge(&url).await;
        let t0 = Instant::now();
        assert!(node.live_stream("show").await.is_none());
        assert!(t0.elapsed() < Duration::from_secs(2), "{:?}", t0.elapsed());
        assert!(connections.load(Ordering::SeqCst) >= 1);
        assert!(hub.get("show").is_none());
        // Retries stop once nobody wants the stream.
        wait_until("retries stop", || node.relays.is_empty()).await;
    }

    #[tokio::test]
    async fn a_local_publish_replaces_the_relay() {
        let (url, _) = fake_origin(true).await;
        let (node, hub, tracker) = edge(&url).await;
        let relayed = node.live_stream("show").await.expect("relayed stream");
        let _viewer = tracker.open("show", Protocol::Flv, "10.0.0.1".parse().unwrap(), "ua");
        // The origin's row expired: this node takes the stream over (SQLite: no FK).
        node.directory.remove_stream("show", None);
        crate::db::testkit::execute(
            &node.db,
            "UPDATE live_streams SET heartbeat = '2000-01-01T00:00:00.000Z'",
        )
        .await;
        let claim = node.claim_publish("show").await.unwrap();
        assert!(claim.is_some());
        assert!(relayed.is_ended(), "the relay is stopped before publishing");
        assert!(hub.get("show").is_none());
        assert!(
            node.directory.remote_owner("show").is_none(),
            "own stream now"
        );
    }
}
