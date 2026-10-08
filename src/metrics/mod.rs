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
//! the `db::metrics` hooks (stubs in phase 1). The DB sampler task is phase 2.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::http::HeaderMap;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::broadcast;

use crate::config::Config;
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

impl ViewerTracker {
    pub fn new(config: &Config, db: Option<Db>, events: broadcast::Sender<HubEvent>) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner::default()),
            events,
            db,
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
        let id = Self::request_session_id(name, protocol, ip, user_agent);
        {
            let mut inner = self.lock();
            if let Some(e) = inner.sessions.get_mut(&id) {
                e.last_seen = Instant::now();
                return;
            }
        }
        self.insert(id, name, protocol, ip, user_agent);
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

    fn insert(&self, id: String, name: &str, protocol: Protocol, ip: IpAddr, user_agent: &str) {
        let ua: String = user_agent.chars().take(MAX_UA_LEN).collect();
        let info = SessionInfo {
            id: id.clone(),
            protocol,
            ip: ip.to_string(),
            user_agent: ua,
            started_at: Utc::now(),
        };
        let counts = {
            let mut inner = self.lock();
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
            counts
        };
        tracing::debug!(broadcast = %name, %protocol, ip = %info.ip, "viewer session opened");
        self.on_session_open(name, &info);
        self.emit(name, counts);
    }

    fn close(&self, id: &str) {
        let removed = {
            let mut inner = self.lock();
            inner.sessions.remove(id).map(|e| {
                let counts = Self::decrement(&mut inner, &e.name, e.info.protocol);
                (e, counts)
            })
        };
        if let Some((entry, counts)) = removed {
            tracing::debug!(broadcast = %entry.name, protocol = %entry.info.protocol, "viewer session closed");
            self.on_session_close(&entry.info.id);
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
        let expired: Vec<(String, String, ViewerCounts)> = {
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
                    out.push((id, e.name, c));
                }
            }
            out
        };
        // Emit only the final count per broadcast.
        let mut last: HashMap<String, ViewerCounts> = HashMap::new();
        for (id, name, counts) in expired {
            self.on_session_close(&id);
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

    /// Hook: persist a `viewer_sessions` row (db::metrics stub in phase 1).
    fn on_session_open(&self, name: &str, info: &SessionInfo) {
        let Some(db) = self.db.clone() else { return };
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let record = db::metrics::ViewerSessionRecord {
            id: info.id.clone(),
            broadcast_name: name.to_string(),
            protocol: info.protocol,
            ip: Some(info.ip.clone()),
            user_agent: Some(info.user_agent.clone()),
            started_at: info.started_at,
        };
        rt.spawn(async move {
            if let Err(e) = db::metrics::insert_viewer_session(&db, record).await {
                tracing::warn!(error = %e, "failed to record viewer session");
            }
        });
    }

    /// Hook: close the `viewer_sessions` row (db::metrics stub in phase 1).
    fn on_session_close(&self, id: &str) {
        let Some(db) = self.db.clone() else { return };
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let id = id.to_string();
        rt.spawn(async move {
            if let Err(e) = db::metrics::close_viewer_session(&db, &id, Utc::now()).await {
                tracing::warn!(error = %e, "failed to close viewer session");
            }
        });
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
