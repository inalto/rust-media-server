//! Viewer tracking (ARCHITECTURE.md §4.4).
//!
//! [`ViewerTracker`] keeps live per-broadcast, per-protocol viewer counts:
//! - connection-oriented protocols (FLV, WS-FLV, WebRTC) use [`ViewerTracker::open`],
//!   which returns a [`ViewerSession`] guard that closes the session on drop;
//! - request-oriented protocols (HLS, DASH) call [`ViewerTracker::touch`] on every
//!   playlist/segment request; sessions are keyed by `sha256(ip|ua|name|protocol)`
//!   and expire after `metrics.session_timeout_secs` without touches (reaper task).
//!
//! Every change emits `HubEvent::ViewerCountChanged`. Opening/closing a session calls
//! the `db::metrics` hooks (persisted `viewer_sessions` rows). The DB sampler task
//! lives in [`sampler`] ([`spawn_sampler`]); the read side of the time series in
//! [`query`].

pub mod query;
pub mod sampler;

pub use sampler::spawn_sampler;

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::http::HeaderMap;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::{broadcast, mpsc};

use crate::config::Config;
use crate::db::metrics::SessionWrite;
use crate::db::{self, Db};
use crate::hub::HubEvent;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Protocol {
    #[serde(rename = "flv")]
    Flv,
    #[serde(rename = "ws_flv")]
    WsFlv,
    #[serde(rename = "hls")]
    Hls,
    #[serde(rename = "dash")]
    Dash,
    #[serde(rename = "webrtc")]
    WebRtc,
}

impl Protocol {
    pub const ALL: [Protocol; 5] = [
        Protocol::Flv,
        Protocol::WsFlv,
        Protocol::Hls,
        Protocol::Dash,
        Protocol::WebRtc,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Protocol::Flv => "flv",
            Protocol::WsFlv => "ws_flv",
            Protocol::Hls => "hls",
            Protocol::Dash => "dash",
            Protocol::WebRtc => "webrtc",
        }
    }

    /// Whether sessions are request-based (expire on inactivity) rather than guards.
    pub fn is_request_based(self) -> bool {
        matches!(self, Protocol::Hls | Protocol::Dash)
    }
}

impl std::fmt::Display for Protocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Concurrent viewers per protocol. Serialized with an extra `total` field.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub struct ViewerCounts {
    #[serde(default)]
    pub flv: u32,
    #[serde(default)]
    pub ws_flv: u32,
    #[serde(default)]
    pub hls: u32,
    #[serde(default)]
    pub dash: u32,
    #[serde(default)]
    pub webrtc: u32,
}

impl ViewerCounts {
    pub fn total(&self) -> u32 {
        self.flv + self.ws_flv + self.hls + self.dash + self.webrtc
    }

    pub fn get(&self, p: Protocol) -> u32 {
        match p {
            Protocol::Flv => self.flv,
            Protocol::WsFlv => self.ws_flv,
            Protocol::Hls => self.hls,
            Protocol::Dash => self.dash,
            Protocol::WebRtc => self.webrtc,
        }
    }

    fn slot(&mut self, p: Protocol) -> &mut u32 {
        match p {
            Protocol::Flv => &mut self.flv,
            Protocol::WsFlv => &mut self.ws_flv,
            Protocol::Hls => &mut self.hls,
            Protocol::Dash => &mut self.dash,
            Protocol::WebRtc => &mut self.webrtc,
        }
    }

    /// Element-wise sum.
    pub fn add(&mut self, other: &ViewerCounts) {
        for p in Protocol::ALL {
            *self.slot(p) += other.get(p);
        }
    }
}

impl Serialize for ViewerCounts {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut st = s.serialize_struct("ViewerCounts", 6)?;
        st.serialize_field("flv", &self.flv)?;
        st.serialize_field("ws_flv", &self.ws_flv)?;
        st.serialize_field("hls", &self.hls)?;
        st.serialize_field("dash", &self.dash)?;
        st.serialize_field("webrtc", &self.webrtc)?;
        st.serialize_field("total", &self.total())?;
        st.end()
    }
}

/// A currently connected viewer (`GET /broadcasts/{name}/sessions`).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SessionInfo {
    pub id: String,
    pub protocol: Protocol,
    pub ip: String,
    pub user_agent: String,
    pub started_at: DateTime<Utc>,
}

#[derive(Debug)]
struct SessionEntry {
    name: String,
    info: SessionInfo,
    last_seen: Instant,
}

#[derive(Default)]
struct Inner {
    sessions: HashMap<String, SessionEntry>,
    counts: HashMap<String, ViewerCounts>,
    /// Peak total concurrent viewers per broadcast since the last reset (stream start).
    peaks: HashMap<String, u32>,
}

pub struct ViewerTracker {
    inner: Mutex<Inner>,
    events: broadcast::Sender<HubEvent>,
    db: Option<Db>,
    /// Ordered queue of `viewer_sessions` writes, drained by one writer task (created
    /// lazily on the first write made inside a tokio runtime).
    db_queue: Mutex<Option<mpsc::Sender<SessionWrite>>>,
    /// Writes dropped because the queue was full (database far behind).
    db_dropped: AtomicU64,
    session_timeout: Duration,
}

/// Guard for a connection-oriented viewer session; dropping it closes the session.
pub struct ViewerSession {
    tracker: Arc<ViewerTracker>,
    id: String,
}

impl ViewerSession {
    pub fn id(&self) -> &str {
        &self.id
    }
}

impl Drop for ViewerSession {
    fn drop(&mut self) {
        self.tracker.close(&self.id);
    }
}

impl std::fmt::Debug for ViewerSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ViewerSession")
            .field("id", &self.id)
            .finish()
    }
}

const MAX_UA_LEN: usize = 512;
/// Capacity of the `viewer_sessions` write queue.
const DB_QUEUE_CAPACITY: usize = 8192;
/// Writes applied per database transaction by the writer task.
const DB_BATCH: usize = 256;

impl ViewerTracker {
    pub fn new(config: &Config, db: Option<Db>, events: broadcast::Sender<HubEvent>) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner::default()),
            events,
            db,
            db_queue: Mutex::new(None),
            db_dropped: AtomicU64::new(0),
            session_timeout: Duration::from_secs(config.metrics.session_timeout_secs.max(1)),
        })
    }

    /// Spawn the reaper task expiring idle HLS/DASH sessions (every 5 s).
    pub fn spawn(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let tracker = Arc::downgrade(self);
        tokio::spawn(async move {
            tracing::debug!(task = "viewer-reaper", "started");
            let mut tick = tokio::time::interval(Duration::from_secs(5));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let Some(t) = tracker.upgrade() else { break };
                t.reap(Instant::now());
            }
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Open a connection-oriented session (FLV, WS-FLV, WebRTC).
    pub fn open(
        self: &Arc<Self>,
        name: &str,
        protocol: Protocol,
        ip: IpAddr,
        user_agent: &str,
    ) -> ViewerSession {
        let id = uuid::Uuid::new_v4().simple().to_string();
        self.insert(id.clone(), name, protocol, ip, user_agent);
        ViewerSession {
            tracker: Arc::clone(self),
            id,
        }
    }

    /// Register activity of a request-oriented session (HLS, DASH).
    pub fn touch(&self, name: &str, protocol: Protocol, ip: IpAddr, user_agent: &str) {
        self.touch_is_new(name, protocol, ip, user_agent);
    }

    /// [`Self::touch`], returning `true` when the request opened a new session (used to
    /// count video on demand plays).
    pub fn touch_is_new(
        &self,
        name: &str,
        protocol: Protocol,
        ip: IpAddr,
        user_agent: &str,
    ) -> bool {
        let id = Self::request_session_id(name, protocol, ip, user_agent);
        self.insert(id, name, protocol, ip, user_agent)
    }

    /// `sha256(ip|ua|name|protocol)` as hex.
    pub fn request_session_id(
        name: &str,
        protocol: Protocol,
        ip: IpAddr,
        user_agent: &str,
    ) -> String {
        let mut h = Sha256::new();
        h.update(ip.to_string().as_bytes());
        h.update(b"|");
        h.update(user_agent.as_bytes());
        h.update(b"|");
        h.update(name.as_bytes());
        h.update(b"|");
        h.update(protocol.as_str().as_bytes());
        hex::encode(h.finalize())
    }

    /// Register session `id`, or only refresh its `last_seen` if it already exists.
    /// Lookup and insertion happen under one lock: concurrent first requests of the
    /// same HLS/DASH viewer must not count it twice (the reaper removes the entry
    /// once, so a double count would never go away). Returns `true` for a new session.
    fn insert(
        &self,
        id: String,
        name: &str,
        protocol: Protocol,
        ip: IpAddr,
        user_agent: &str,
    ) -> bool {
        let (info, counts) = {
            let mut inner = self.lock();
            if let Some(e) = inner.sessions.get_mut(&id) {
                e.last_seen = Instant::now();
                return false;
            }
            let info = SessionInfo {
                id: id.clone(),
                protocol,
                ip: ip.to_string(),
                user_agent: user_agent.chars().take(MAX_UA_LEN).collect(),
                started_at: Utc::now(),
            };
            inner.sessions.insert(
                id,
                SessionEntry {
                    name: name.to_string(),
                    info: info.clone(),
                    last_seen: Instant::now(),
                },
            );
            let c = inner.counts.entry(name.to_string()).or_default();
            *c.slot(protocol) += 1;
            let counts = *c;
            let peak = inner.peaks.entry(name.to_string()).or_default();
            *peak = (*peak).max(counts.total());
            // Queued under the tracker lock: the queue order is the order in which
            // sessions were opened/closed (see `queue_db_write`).
            self.on_session_open(name, &info);
            (info, counts)
        };
        tracing::debug!(broadcast = %name, %protocol, ip = %info.ip, "viewer session opened");
        self.emit(name, counts);
        true
    }

    fn close(&self, id: &str) {
        let removed = {
            let mut inner = self.lock();
            inner.sessions.remove(id).map(|e| {
                let counts = Self::decrement(&mut inner, &e.name, e.info.protocol);
                self.on_session_close(&e.info.id, Utc::now());
                (e, counts)
            })
        };
        if let Some((entry, counts)) = removed {
            tracing::debug!(broadcast = %entry.name, protocol = %entry.info.protocol, "viewer session closed");
            self.emit(&entry.name, counts);
        }
    }

    fn decrement(inner: &mut Inner, name: &str, protocol: Protocol) -> ViewerCounts {
        let counts = inner.counts.entry(name.to_string()).or_default();
        let slot = counts.slot(protocol);
        *slot = slot.saturating_sub(1);
        let c = *counts;
        if c.total() == 0 {
            inner.counts.remove(name);
        }
        c
    }

    /// Expire request-based sessions idle for longer than the session timeout.
    fn reap(&self, now: Instant) {
        let wall_now = Utc::now();
        let expired: Vec<(String, ViewerCounts)> = {
            let mut inner = self.lock();
            let ids: Vec<String> = inner
                .sessions
                .iter()
                .filter(|(_, e)| {
                    e.info.protocol.is_request_based()
                        && now.saturating_duration_since(e.last_seen) > self.session_timeout
                })
                .map(|(id, _)| id.clone())
                .collect();
            let mut out = Vec::with_capacity(ids.len());
            for id in ids {
                if let Some(e) = inner.sessions.remove(&id) {
                    let c = Self::decrement(&mut inner, &e.name, e.info.protocol);
                    // The viewer was last seen at `last_seen`, not when the reaper noticed.
                    let idle =
                        chrono::Duration::from_std(now.saturating_duration_since(e.last_seen))
                            .unwrap_or_default();
                    self.on_session_close(&id, wall_now - idle);
                    out.push((e.name, c));
                }
            }
            out
        };
        // Emit only the final count per broadcast.
        let mut last: HashMap<String, ViewerCounts> = HashMap::new();
        for (name, counts) in expired {
            last.insert(name, counts);
        }
        for (name, counts) in last {
            self.emit(&name, counts);
        }
    }

    fn emit(&self, name: &str, counts: ViewerCounts) {
        let _ = self.events.send(HubEvent::ViewerCountChanged {
            name: name.to_string(),
            counts,
        });
    }

    /// Hook: persist a `viewer_sessions` row.
    fn on_session_open(&self, name: &str, info: &SessionInfo) {
        if self.db.is_none() {
            return;
        }
        self.queue_db_write(SessionWrite::Open(db::metrics::ViewerSessionRecord {
            id: info.id.clone(),
            broadcast_name: name.to_string(),
            protocol: info.protocol,
            ip: Some(info.ip.clone()),
            user_agent: Some(info.user_agent.clone()),
            started_at: info.started_at,
        }));
    }

    /// Hook: close the `viewer_sessions` row.
    fn on_session_close(&self, id: &str, ended_at: DateTime<Utc>) {
        if self.db.is_none() {
            return;
        }
        self.queue_db_write(SessionWrite::Close {
            id: id.to_string(),
            ended_at,
        });
    }

    /// Queue a `viewer_sessions` write for the single writer task, which applies the
    /// writes in queue order. (Spawning one task per write let a quick close overtake
    /// its own insert, leaving the row open forever.) Non-blocking: when the database
    /// falls far behind, writes are dropped and counted rather than buffered without
    /// bound. Callers hold the tracker lock, so the queue order matches the order of
    /// the in-memory session changes.
    fn queue_db_write(&self, write: SessionWrite) {
        let Some(db) = &self.db else { return };
        let mut queue = self.db_queue.lock().unwrap_or_else(|e| e.into_inner());
        if queue.as_ref().is_none_or(mpsc::Sender::is_closed) {
            let Ok(rt) = tokio::runtime::Handle::try_current() else {
                return;
            };
            let (tx, rx) = mpsc::channel(DB_QUEUE_CAPACITY);
            rt.spawn(session_writer(db.clone(), rx));
            *queue = Some(tx);
        }
        let Some(tx) = queue.as_ref() else { return };
        if let Err(mpsc::error::TrySendError::Full(w)) = tx.try_send(write) {
            let n = self.db_dropped.fetch_add(1, Ordering::Relaxed);
            if n.is_multiple_of(1000) {
                tracing::warn!(dropped = n + 1, write = ?w, "viewer session write queue full; dropping writes");
            }
        }
    }

    pub fn counts(&self, name: &str) -> ViewerCounts {
        self.lock().counts.get(name).copied().unwrap_or_default()
    }

    /// Counts of every broadcast with at least one viewer.
    pub fn all_counts(&self) -> HashMap<String, ViewerCounts> {
        self.lock().counts.clone()
    }

    /// Currently connected sessions of a broadcast, oldest first.
    pub fn sessions(&self, name: &str) -> Vec<SessionInfo> {
        let mut v: Vec<SessionInfo> = self
            .lock()
            .sessions
            .values()
            .filter(|e| e.name == name)
            .map(|e| e.info.clone())
            .collect();
        v.sort_by_key(|a| a.started_at);
        v
    }

    /// Peak total concurrent viewers since the last [`Self::reset_peak`].
    pub fn peak(&self, name: &str) -> u32 {
        self.lock().peaks.get(name).copied().unwrap_or(0)
    }

    /// Reset the peak to the current total (called when a stream starts).
    pub fn reset_peak(&self, name: &str) {
        let mut inner = self.lock();
        let current = inner.counts.get(name).map(|c| c.total()).unwrap_or(0);
        inner.peaks.insert(name.to_string(), current);
    }

    /// Return the peak and reset it (called when a stream ends).
    pub fn take_peak(&self, name: &str) -> u32 {
        let mut inner = self.lock();
        let current = inner.counts.get(name).map(|c| c.total()).unwrap_or(0);
        let peak = inner.peaks.insert(name.to_string(), current).unwrap_or(0);
        peak.max(current)
    }
}

/// Writer task of the `viewer_sessions` queue: applies writes in order, in batches.
async fn session_writer(db: Db, mut rx: mpsc::Receiver<SessionWrite>) {
    tracing::debug!(task = "viewer-session-writer", "started");
    let mut batch = Vec::with_capacity(DB_BATCH);
    while rx.recv_many(&mut batch, DB_BATCH).await > 0 {
        let writes = std::mem::take(&mut batch);
        if let Err(e) = db::metrics::apply_session_writes(&db, writes).await {
            tracing::warn!(task = "viewer-session-writer", error = %e, "failed to persist viewer sessions");
        }
    }
    tracing::debug!(task = "viewer-session-writer", "stopped");
}

/// Peer IP of a request: first `X-Forwarded-For` element or `X-Real-IP` when
/// `trust_proxy` is set (and parseable), otherwise the socket address.
pub fn client_ip(headers: &HeaderMap, peer: SocketAddr, trust_proxy: bool) -> IpAddr {
    if trust_proxy {
        let forwarded = headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(',').next())
            .and_then(|v| v.trim().parse::<IpAddr>().ok());
        if let Some(ip) = forwarded {
            return ip;
        }
        if let Some(ip) = headers
            .get("x-real-ip")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<IpAddr>().ok())
        {
            return ip;
        }
    }
    peer.ip()
}

/// `User-Agent` header (lossy, possibly empty).
pub fn user_agent(headers: &HeaderMap) -> String {
    headers
        .get(axum::http::header::USER_AGENT)
        .map(|v| {
            String::from_utf8_lossy(v.as_bytes())
                .chars()
                .take(MAX_UA_LEN)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracker(timeout: u64) -> (Arc<ViewerTracker>, broadcast::Receiver<HubEvent>) {
        let mut cfg = Config::default();
        cfg.metrics.session_timeout_secs = timeout;
        let (tx, rx) = broadcast::channel(64);
        (ViewerTracker::new(&cfg, None, tx), rx)
    }

    #[test]
    fn counts_serialize_with_total() {
        let c = ViewerCounts {
            flv: 1,
            hls: 2,
            ..Default::default()
        };
        let v = serde_json::to_value(c).unwrap();
        assert_eq!(v["total"], 3);
        assert_eq!(v["ws_flv"], 0);
        let back: ViewerCounts = serde_json::from_value(v).unwrap();
        assert_eq!(back, c);
        assert_eq!(serde_json::to_value(Protocol::WsFlv).unwrap(), "ws_flv");
        assert_eq!(serde_json::to_value(Protocol::WebRtc).unwrap(), "webrtc");
    }

    #[tokio::test]
    async fn open_close_guard() {
        let (t, mut rx) = tracker(30);
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        let s1 = t.open("a", Protocol::Flv, ip, "ua");
        let s2 = t.open("a", Protocol::WsFlv, ip, "ua");
        assert_eq!(t.counts("a").total(), 2);
        assert_eq!(t.sessions("a").len(), 2);
        drop(s1);
        assert_eq!(t.counts("a").flv, 0);
        assert_eq!(t.counts("a").ws_flv, 1);
        assert_eq!(t.peak("a"), 2);
        drop(s2);
        assert_eq!(t.counts("a").total(), 0);
        assert_eq!(t.take_peak("a"), 2);
        assert_eq!(t.peak("a"), 0);
        let mut n = 0;
        while let Ok(HubEvent::ViewerCountChanged { .. }) = rx.try_recv() {
            n += 1;
        }
        assert_eq!(n, 4);
    }

    #[test]
    fn touch_sessions_expire() {
        let (t, _rx) = tracker(1);
        let ip: IpAddr = "10.0.0.2".parse().unwrap();
        t.touch("b", Protocol::Hls, ip, "safari");
        t.touch("b", Protocol::Hls, ip, "safari");
        t.touch("b", Protocol::Hls, ip, "chrome");
        t.touch("b", Protocol::Dash, ip, "chrome");
        assert_eq!(t.counts("b").hls, 2);
        assert_eq!(t.counts("b").dash, 1);
        t.reap(Instant::now());
        assert_eq!(t.counts("b").total(), 3);
        t.reap(Instant::now() + Duration::from_secs(5));
        assert_eq!(t.counts("b").total(), 0);
    }

    #[test]
    fn concurrent_first_touches_count_once() {
        // Players fire several requests at once (playlist + part + preload hint): the
        // first touches of the same session must not register it twice.
        let (t, _rx) = tracker(1);
        let threads = 16;
        let rounds = 200u32;
        let barrier = Arc::new(std::sync::Barrier::new(threads));
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                let (t, barrier) = (Arc::clone(&t), Arc::clone(&barrier));
                std::thread::spawn(move || {
                    for r in 0..rounds {
                        let ip = IpAddr::from([10, 1, (r >> 8) as u8, r as u8]);
                        barrier.wait();
                        t.touch("c", Protocol::Hls, ip, "hls.js");
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(t.counts("c").hls, rounds);
        assert_eq!(t.sessions("c").len(), rounds as usize);
        t.reap(Instant::now() + Duration::from_secs(5));
        assert_eq!(t.counts("c").total(), 0, "no session leaked in the counts");
    }

    async fn rows(db: &Db, sql: &'static str) -> i64 {
        db.call(move |c| Ok(c.query_row(sql, [], |r| r.get(0))?))
            .await
            .unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn session_rows_are_written_in_order() {
        let db = Db::open_in_memory().unwrap();
        db::broadcasts::create(&db, db::broadcasts::NewBroadcast::new("show", "Show"))
            .await
            .unwrap();
        let (tx, _rx) = broadcast::channel(16);
        let t = ViewerTracker::new(&Config::default(), Some(db.clone()), tx);
        let ip: IpAddr = "10.0.0.9".parse().unwrap();
        // Very short FLV sessions: the close must never overtake the insert.
        let n = 500;
        let tasks: Vec<_> = (0..n)
            .map(|_| {
                let t = Arc::clone(&t);
                tokio::spawn(async move { drop(t.open("show", Protocol::Flv, ip, "probe")) })
            })
            .collect();
        for task in tasks {
            task.await.unwrap();
        }
        // An HLS viewer that expires and comes back reuses its session id.
        t.touch("show", Protocol::Hls, ip, "hls.js");
        t.reap(Instant::now() + Duration::from_secs(60));
        t.touch("show", Protocol::Hls, ip, "hls.js");
        t.reap(Instant::now() + Duration::from_secs(60));
        for _ in 0..500 {
            if rows(
                &db,
                "SELECT COUNT(*) FROM viewer_sessions WHERE ended_at IS NOT NULL",
            )
            .await
                == n + 2
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            rows(&db, "SELECT COUNT(*) FROM viewer_sessions").await,
            n + 2
        );
        assert_eq!(
            rows(
                &db,
                "SELECT COUNT(*) FROM viewer_sessions WHERE ended_at IS NULL"
            )
            .await,
            0,
            "a session row was left open"
        );
        assert_eq!(
            rows(
                &db,
                "SELECT COUNT(*) FROM viewer_sessions WHERE protocol = 'flv' AND ended_at < started_at"
            )
            .await,
            0
        );
        // The returning HLS viewer: the first row was archived, the second one is
        // the current id; both closed.
        assert_eq!(
            rows(
                &db,
                "SELECT COUNT(*) FROM viewer_sessions WHERE protocol = 'hls'"
            )
            .await,
            2
        );
    }

    #[test]
    fn client_ip_proxy_handling() {
        let peer: SocketAddr = "127.0.0.1:5000".parse().unwrap();
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", "203.0.113.7, 10.0.0.1".parse().unwrap());
        assert_eq!(client_ip(&h, peer, false), peer.ip());
        assert_eq!(client_ip(&h, peer, true).to_string(), "203.0.113.7");
        let mut h2 = HeaderMap::new();
        h2.insert("x-real-ip", "198.51.100.1".parse().unwrap());
        assert_eq!(client_ip(&h2, peer, true).to_string(), "198.51.100.1");
        h2.insert("x-forwarded-for", "garbage".parse().unwrap());
        assert_eq!(client_ip(&h2, peer, true).to_string(), "198.51.100.1");
    }
}
