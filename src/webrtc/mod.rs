//! WebRTC egress via WHEP (`draft-ietf-wish-whep`), ARCHITECTURE.md §16.
//!
//! - [`WebRtcManager`] (in `AppState`): the shared UDP port ([`mux::UdpMux`]), the
//!   per-broadcast [`publisher::WhepPublisher`]s and the WHEP sessions.
//! - A publisher is started lazily by the first WHEP session of a live stream and
//!   stopped when the stream ends or after `webrtc.idle_stop_secs` without sessions.
//!   It packetizes the H.264 video once ([`h264`], [`publisher`]) and, when ffmpeg is
//!   available and the stream has audio, runs an AAC → Opus encoder ([`audio`]).
//! - Each viewer gets its own peer connection ([`session`]) built on `rtc` 0.21, the
//!   sans-I/O core of webrtc-rs 0.21: its async `webrtc` crate binds sockets per peer
//!   connection and its tracks bind to a single connection, while WHEP serving needs one
//!   UDP port for everyone and fan-out of one packetized stream.
//! - HTTP: `POST /whep/{name}`, `DELETE`/`PATCH`/`OPTIONS` (see [`whep`]), mounted by
//!   [`WebRtcManager::router`].

pub mod audio;
pub mod h264;
pub mod mux;
pub mod publisher;
pub mod session;
mod whep;

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::Router;
use axum::http::StatusCode;
use rand::RngExt;
use serde::Serialize;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::config::{Config, WebRtcConfig};
use crate::hub::{Hub, LiveStream, MediaPacket};
use crate::metrics::ViewerTracker;
use crate::state::AppState;
use mux::UdpMux;
use publisher::{PublisherState, SessionSlot, VideoEvent, VideoPacketizer, WhepPublisher};
use session::{NegotiateError, NegotiateOptions, SessionTask};
pub use whep::WhepError;

/// How long an offer waits for the publisher (H.264 sequence header) of a stream.
const READY_WAIT: Duration = Duration::from_secs(5);
/// Closed sessions are remembered this long so that a client's late `DELETE` (after
/// the server closed the session: stream ended, WebRTC disabled) still gets `200`.
const CLOSED_SESSION_TTL: Duration = Duration::from_secs(120);
const MAX_CLOSED_SESSIONS: usize = 4096;

/// `GET /system` → `webrtc`.
#[derive(Debug, Clone, Serialize)]
pub struct WebRtcStatus {
    /// `webrtc.enabled` in the configuration.
    pub enabled: bool,
    /// The UDP port is bound and WHEP offers are accepted.
    pub available: bool,
    pub udp_port: u16,
    /// Port advertised in the candidates (`webrtc.public_port`, or `udp_port` when 0).
    pub public_port: u16,
    pub public_ip: Vec<String>,
    /// Host candidates advertised to viewers (`ip:port`).
    pub candidates: Vec<String>,
    pub ice_lite: bool,
    /// Current WebRTC sessions (connecting or connected).
    pub sessions: usize,
    /// Broadcasts with a running WebRTC publisher.
    pub publishers: usize,
    /// Opus audio available (ffmpeg found); otherwise sessions are video-only.
    pub audio: bool,
    /// Why WebRTC is unavailable (e.g. the UDP port could not be bound).
    pub error: Option<String>,
}

struct Running {
    mux: Arc<UdpMux>,
    candidates: Vec<SocketAddr>,
    shutdown: CancellationToken,
}

struct SessionEntry {
    name: String,
    cancel: CancellationToken,
}

pub struct WebRtcManager {
    config: Arc<Config>,
    hub: Arc<Hub>,
    tracker: Arc<ViewerTracker>,
    ffmpeg: Option<PathBuf>,
    running: OnceLock<Running>,
    error: Mutex<Option<String>>,
    publishers: Mutex<HashMap<String, Arc<WhepPublisher>>>,
    sessions: Mutex<HashMap<String, SessionEntry>>,
    /// Recently closed sessions: id → (broadcast name, closed at).
    closed: Mutex<HashMap<String, (String, Instant)>>,
    next_id: AtomicU64,
    ffmpeg_warned: AtomicBool,
}

impl WebRtcManager {
    pub fn new(config: Arc<Config>, hub: Arc<Hub>, tracker: Arc<ViewerTracker>) -> Arc<Self> {
        let ffmpeg = config.ffmpeg_path();
        Arc::new(Self {
            config,
            hub,
            tracker,
            ffmpeg,
            running: OnceLock::new(),
            error: Mutex::new(None),
            publishers: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            closed: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            ffmpeg_warned: AtomicBool::new(false),
        })
    }

    fn cfg(&self) -> &WebRtcConfig {
        &self.config.webrtc
    }

    /// Bind the WebRTC UDP port and start its receive loop. A bind failure disables
    /// WebRTC (logged, reported by [`Self::status`]) without stopping the server.
    pub fn spawn(self: &Arc<Self>, shutdown: CancellationToken) {
        if !self.cfg().enabled {
            tracing::info!(task = "webrtc", "WebRTC (WHEP) disabled by configuration");
            return;
        }
        let port = self.cfg().udp_port;
        let socket = std::net::UdpSocket::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)))
            .and_then(|s| s.set_nonblocking(true).map(|_| s))
            .and_then(tokio::net::UdpSocket::from_std);
        let socket = match socket {
            Ok(s) => s,
            Err(e) => {
                let msg = format!("cannot bind WebRTC UDP port {port}: {e}");
                tracing::error!(task = "webrtc", "{msg}; WHEP disabled");
                *self.error.lock().unwrap_or_else(|e| e.into_inner()) = Some(msg);
                return;
            }
        };
        let candidates = advertised_addrs(self.cfg());
        let mux = UdpMux::new(socket);
        tokio::spawn(Arc::clone(&mux).run(shutdown.clone()));
        tracing::info!(
            task = "webrtc",
            udp_port = port,
            candidates = ?candidates,
            ice_lite = self.cfg().ice_lite,
            opus = self.ffmpeg.is_some(),
            "WebRTC (WHEP) ready"
        );
        let _ = self.running.set(Running {
            mux,
            candidates,
            shutdown,
        });
    }

    /// WHEP routes (`/whep/...`), merged at `/` by `main.rs`.
    pub fn router() -> Router<AppState> {
        whep::routes()
    }

    /// WHEP offers are accepted (enabled and UDP port bound).
    pub fn available(&self) -> bool {
        self.cfg().enabled && self.running.get().is_some()
    }

    pub fn session_count(&self) -> usize {
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }

    pub fn status(&self) -> WebRtcStatus {
        let running = self.running.get();
        WebRtcStatus {
            enabled: self.cfg().enabled,
            available: self.available(),
            udp_port: self.cfg().udp_port,
            public_port: self.cfg().advertised_port(),
            public_ip: self.cfg().public_ip.clone(),
            candidates: running
                .map(|r| r.candidates.iter().map(|a| a.to_string()).collect())
                .unwrap_or_default(),
            ice_lite: self.cfg().ice_lite,
            sessions: self.session_count(),
            publishers: self
                .publishers
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .len(),
            audio: self.ffmpeg.is_some(),
            error: self.error.lock().unwrap_or_else(|e| e.into_inner()).clone(),
        }
    }

    /// `Link` header values advertising the configured STUN server.
    pub fn link_headers(&self) -> Vec<String> {
        ice_server_links(&self.cfg().stun_server)
    }

    /// Close every session of `name` (WebRTC disabled for the broadcast, deleted...).
    /// Returns how many were closed.
    pub fn close_broadcast(&self, name: &str) -> usize {
        let sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let mut n = 0;
        for s in sessions.values().filter(|s| s.name == name) {
            s.cancel.cancel();
            n += 1;
        }
        n
    }

    /// `DELETE /whep/{name}/{id}`: close a session. False if unknown (sessions closed
    /// by the server in the last [`CLOSED_SESSION_TTL`] count as known).
    pub fn delete_session(&self, name: &str, id: &str) -> bool {
        {
            let sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(s) = sessions.get(id) {
                if s.name != name {
                    return false;
                }
                s.cancel.cancel();
                return true;
            }
        }
        let closed = self.closed.lock().unwrap_or_else(|e| e.into_inner());
        closed
            .get(id)
            .is_some_and(|(n, at)| n == name && at.elapsed() < CLOSED_SESSION_TTL)
    }

    /// Forget a finished session, keeping a short-lived tombstone for late DELETEs.
    fn session_finished(&self, id: &str) {
        let entry = self
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id);
        if let Some(entry) = entry {
            let mut closed = self.closed.lock().unwrap_or_else(|e| e.into_inner());
            closed.retain(|_, (_, at)| at.elapsed() < CLOSED_SESSION_TTL);
            if closed.len() < MAX_CLOSED_SESSIONS {
                closed.insert(id.to_string(), (entry.name, Instant::now()));
            }
        }
    }

    /// The publisher of `stream` (started if needed) and a session slot on it.
    fn publisher_for(self: &Arc<Self>, stream: &Arc<LiveStream>) -> SessionSlot {
        let mut map = self.publishers.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(p) = map.get(&stream.name)
            && p.is_for(stream)
            && !p.stop.is_cancelled()
        {
            return p.reserve();
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let p = WhepPublisher::new(id, stream, false);
        if let Some(old) = map.insert(stream.name.clone(), Arc::clone(&p)) {
            old.stop.cancel();
        }
        let slot = p.reserve();
        drop(map);
        tracing::debug!(task = "webrtc-publisher", broadcast = %stream.name, "starting");
        tokio::spawn(Arc::clone(self).run_publisher(p, Arc::clone(stream)));
        slot
    }

    /// Remove `p` from the map if it still has no session (idle stop).
    fn remove_if_idle(&self, p: &Arc<WhepPublisher>) -> bool {
        let mut map = self.publishers.lock().unwrap_or_else(|e| e.into_inner());
        if p.sessions() > 0 {
            return false;
        }
        if map.get(&p.name).is_some_and(|cur| cur.id == p.id) {
            map.remove(&p.name);
        }
        true
    }

    fn remove_publisher(&self, p: &Arc<WhepPublisher>) {
        let mut map = self.publishers.lock().unwrap_or_else(|e| e.into_inner());
        if map.get(&p.name).is_some_and(|cur| cur.id == p.id) {
            map.remove(&p.name);
        }
    }

    /// Publisher task: packetize the stream's video, start the Opus encoder on the
    /// first audio packet, stop on stream end / idle / shutdown.
    async fn run_publisher(self: Arc<Self>, p: Arc<WhepPublisher>, stream: Arc<LiveStream>) {
        let shutdown = self
            .running
            .get()
            .map(|r| r.shutdown.clone())
            .unwrap_or_default();
        let ended = stream.ended_token();
        let tx = p.sender();
        let mut video = VideoPacketizer::new();
        let mut audio_started = false;
        let mut process =
            |video: &mut VideoPacketizer, pkt: &MediaPacket| -> Option<PublisherState> {
                if pkt.is_audio() && !audio_started {
                    audio_started = true;
                    self.start_audio(&p, &stream);
                }
                match video.push(pkt) {
                    VideoEvent::Chunk(c) => {
                        let _ = tx.send(Arc::new(c));
                        None
                    }
                    VideoEvent::Config(profile) => Some(PublisherState::Ready(profile)),
                    VideoEvent::Unsupported(codec) => Some(PublisherState::Unsupported(format!(
                        "{codec} video cannot be sent over WebRTC (only H.264 is supported)"
                    ))),
                    VideoEvent::Nothing => None,
                }
            };

        let (snapshot, mut rx) = stream.subscribe();
        let mut state = None;
        for pkt in &snapshot {
            state = process(&mut video, pkt).or(state);
        }
        drop(snapshot);
        // Ready only after the whole catch-up, so the audio decision is known.
        if let Some(s) = state {
            p.set_state(s);
        }

        let idle_stop = Duration::from_secs(self.cfg().idle_stop_secs.max(1));
        let mut idle = Duration::ZERO;
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let reason = loop {
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => break "shutdown",
                _ = p.stop.cancelled() => break "replaced",
                _ = ended.cancelled() => break "stream ended",
                r = rx.recv() => match r {
                    Ok(pkt) => {
                        if let Some(s) = process(&mut video, &pkt) {
                            p.set_state(s);
                        }
                    }
                    Err(RecvError::Lagged(n)) => {
                        video.on_lag();
                        tracing::warn!(task = "webrtc-publisher", broadcast = %p.name, skipped = n, "WebRTC publisher lagging; resuming at the next keyframe");
                    }
                    Err(RecvError::Closed) => break "stream ended",
                },
                _ = tick.tick() => {
                    if p.sessions() > 0 {
                        idle = Duration::ZERO;
                    } else {
                        idle += Duration::from_secs(1);
                        if idle >= idle_stop && self.remove_if_idle(&p) {
                            break "idle";
                        }
                    }
                }
            }
        };
        drop(rx);
        drop(stream);
        p.set_state(PublisherState::Ended);
        p.stop.cancel();
        self.remove_publisher(&p);
        tracing::debug!(task = "webrtc-publisher", broadcast = %p.name, reason, "stopped");
    }

    fn start_audio(&self, p: &Arc<WhepPublisher>, stream: &Arc<LiveStream>) {
        let Some(ffmpeg) = self.ffmpeg.clone() else {
            if !self.ffmpeg_warned.swap(true, Ordering::Relaxed) {
                tracing::warn!(
                    task = "webrtc",
                    "ffmpeg not found: WebRTC sessions are video-only (AAC cannot be sent to browsers without an Opus transcode)"
                );
            }
            return;
        };
        p.enable_audio();
        tokio::spawn(audio::run(
            Arc::clone(stream),
            ffmpeg,
            self.cfg().audio_bitrate_kbps,
            p.sender(),
            p.stop.clone(),
        ));
    }

    /// Negotiate a WHEP session for the live `stream` of broadcast `name`.
    /// Returns `(session id, SDP answer)`.
    pub async fn create_session(
        self: &Arc<Self>,
        name: &str,
        stream: Arc<LiveStream>,
        offer: String,
        ip: IpAddr,
        user_agent: String,
    ) -> Result<(String, String), WhepError> {
        let running = self.running.get().ok_or_else(|| {
            WhepError::status(StatusCode::SERVICE_UNAVAILABLE, "WebRTC is not available")
        })?;
        if self.session_count() >= self.cfg().max_sessions {
            return Err(WhepError::status(
                StatusCode::SERVICE_UNAVAILABLE,
                "too many WebRTC sessions",
            ));
        }
        let slot = self.publisher_for(&stream);
        let info = stream.info();
        drop(stream);
        let publisher = Arc::clone(slot.publisher());
        let profile = match publisher.wait_ready(READY_WAIT).await {
            PublisherState::Ready(profile) => profile,
            PublisherState::Unsupported(reason) => {
                return Err(WhepError::status(
                    StatusCode::UNSUPPORTED_MEDIA_TYPE,
                    reason,
                ));
            }
            PublisherState::Ended => {
                return Err(WhepError::status(StatusCode::NOT_FOUND, "stream offline"));
            }
            PublisherState::Starting if info.has_audio && !info.has_video => {
                return Err(WhepError::status(
                    StatusCode::UNSUPPORTED_MEDIA_TYPE,
                    "audio-only streams are not supported over WebRTC",
                ));
            }
            PublisherState::Starting => {
                return Err(WhepError::status(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "stream is starting (no H.264 sequence header yet)",
                ));
            }
        };

        let ufrag = random_token(16);
        let pwd = random_token(32);
        let candidates = running.candidates.clone();
        let ice_lite = self.cfg().ice_lite;
        let audio = publisher.has_audio();
        let stream_id = format!("rms-{name}");
        // Certificate generation and SDP processing: off the async workers.
        let negotiated = tokio::task::spawn_blocking(move || {
            session::negotiate(
                &offer,
                &NegotiateOptions {
                    ice_lite,
                    ufrag: &ufrag,
                    pwd: &pwd,
                    candidates: &candidates,
                    audio,
                    profile: Some(profile),
                    stream_id: &stream_id,
                },
            )
            .map(|n| (n, ufrag))
        })
        .await
        .map_err(|e| WhepError::status(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let (negotiated, ufrag) = negotiated.map_err(|e| match e {
            NegotiateError::BadOffer(m) => WhepError::status(StatusCode::BAD_REQUEST, m),
            NegotiateError::Unsupported(m) => {
                WhepError::status(StatusCode::UNSUPPORTED_MEDIA_TYPE, m)
            }
            NegotiateError::Internal(m) => {
                tracing::error!(task = "webrtc", broadcast = %name, error = %m, "WHEP negotiation failed");
                WhepError::status(StatusCode::INTERNAL_SERVER_ERROR, "WebRTC negotiation failed")
            }
        })?;
        let answer = negotiated.answer.clone();

        let cluster_node = self
            .config
            .cluster
            .enabled
            .then_some(self.config.cluster.node_id.as_str());
        let id = new_session_id(cluster_node);
        let mux_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, inbound) = mpsc::channel(mux::SESSION_QUEUE);
        running.mux.register(mux_id, &ufrag, tx);
        let cancel = running.shutdown.child_token();
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                id.clone(),
                SessionEntry {
                    name: name.to_string(),
                    cancel: cancel.clone(),
                },
            );
        let task =
            SessionTask {
                id: id.clone(),
                mux_id,
                name: name.to_string(),
                negotiated,
                inbound,
                media: publisher.subscribe(),
                mux: Arc::clone(&running.mux),
                local_addr: running.candidates.first().copied().unwrap_or_else(|| {
                    SocketAddr::from((Ipv4Addr::LOCALHOST, self.cfg().udp_port))
                }),
                cancel,
                publisher_stop: publisher.stop.clone(),
                slot,
                tracker: Arc::clone(&self.tracker),
                ip,
                user_agent,
            };
        let this = Arc::clone(self);
        let session_id = id.clone();
        tracing::debug!(task = "webrtc", broadcast = %name, session = %id, audio = task.negotiated.audio.is_some(), "WHEP session created");
        tokio::spawn(async move {
            session::run(task).await;
            this.session_finished(&session_id);
        });
        Ok((id, answer))
    }

    /// Hub reference (for handlers that only have the manager).
    pub fn hub(&self) -> &Arc<Hub> {
        &self.hub
    }
}

/// A new WHEP session id: a UUID (32 hex digits), prefixed with the node id in cluster
/// mode (`<node_id>-<uuid>`, docs/CLUSTER.md §3.8) so any node can route a `DELETE`.
pub fn new_session_id(node_id: Option<&str>) -> String {
    let uuid = uuid::Uuid::new_v4().simple().to_string();
    match node_id.filter(|n| !n.is_empty()) {
        Some(node) => format!("{node}-{uuid}"),
        None => uuid,
    }
}

/// The node prefix of a session id made by [`new_session_id`] (`None` without prefix
/// or for malformed ids). Node ids may contain `-`: the UUID part is the last 32 hex
/// digits after the last `-`.
pub fn session_node(id: &str) -> Option<&str> {
    let (node, uuid) = id.rsplit_once('-')?;
    let is_uuid = uuid.len() == 32 && uuid.bytes().all(|b| b.is_ascii_hexdigit());
    (is_uuid && crate::config::is_valid_node_id(node)).then_some(node)
}

/// Host candidate addresses: `webrtc.public_ip` when set, otherwise the IPv4 addresses
/// of the local interfaces (the default-route address first), else loopback; with the
/// advertised port (`webrtc.public_port`, else the bound `udp_port`). Inbound datagrams
/// are tagged with the first of these addresses, so the ICE agent matches them against
/// the advertised candidate even when NAT maps another port to `udp_port`.
pub fn advertised_addrs(cfg: &WebRtcConfig) -> Vec<SocketAddr> {
    let port = cfg.advertised_port();
    let mut ips: Vec<IpAddr> = cfg
        .public_ip
        .iter()
        .filter_map(|ip| ip.parse::<Ipv4Addr>().ok())
        .map(IpAddr::V4)
        .collect();
    if ips.is_empty() {
        if let Some(ip) = default_route_ipv4() {
            ips.push(ip);
        }
        if let Ok(ifaces) = rtc::shared::ifaces::ifaces() {
            for ip in ifaces.into_iter().filter_map(|i| i.addr).map(|a| a.ip()) {
                if let IpAddr::V4(v4) = ip
                    && !v4.is_loopback()
                    && !v4.is_link_local()
                    && !v4.is_unspecified()
                    && !ips.contains(&ip)
                {
                    ips.push(ip);
                }
            }
        }
    }
    if ips.is_empty() {
        ips.push(IpAddr::V4(Ipv4Addr::LOCALHOST));
    }
    ips.into_iter()
        .map(|ip| SocketAddr::new(ip, port))
        .collect()
}

/// Source address the kernel would use towards the Internet (no packet is sent).
fn default_route_ipv4() -> Option<IpAddr> {
    let s = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    s.connect((Ipv4Addr::new(192, 0, 2, 1), 9)).ok()?;
    let ip = s.local_addr().ok()?.ip();
    (!ip.is_unspecified() && !ip.is_loopback()).then_some(ip)
}

/// `Link: <stun:host:port>; rel="ice-server"` values for a configured STUN server.
pub fn ice_server_links(stun_server: &str) -> Vec<String> {
    let s = stun_server.trim();
    if s.is_empty() {
        return Vec::new();
    }
    let url = if ["stun:", "stuns:", "turn:", "turns:"]
        .iter()
        .any(|p| s.starts_with(p))
    {
        s.to_string()
    } else {
        format!("stun:{s}")
    };
    if url.contains(['<', '>', '"', '\r', '\n']) {
        return Vec::new();
    }
    vec![format!("<{url}>; rel=\"ice-server\"")]
}

/// Random ICE credential (alphanumeric; ufrag ≥ 4 and pwd ≥ 22 chars per RFC 8445).
fn random_token(len: usize) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::rng();
    (0..len)
        .map(|_| CHARS[rng.random_range(0..CHARS.len())] as char)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_ids_carry_the_node() {
        let single = new_session_id(None);
        assert_eq!(single.len(), 32);
        assert_eq!(session_node(&single), None);
        assert_eq!(new_session_id(Some("")).len(), 32);
        let id = new_session_id(Some("rms-2"));
        assert!(id.starts_with("rms-2-"), "{id}");
        assert_eq!(session_node(&id), Some("rms-2"));
        assert_eq!(
            session_node(&new_session_id(Some("n1.local"))),
            Some("n1.local")
        );
        for bad in [
            "n1-xyz",
            "-0123456789abcdef0123456789abcdef",
            "n 1-0123456789abcdef0123456789abcdef",
            "n1-0123456789abcdef0123456789abcde",
        ] {
            assert_eq!(session_node(bad), None, "{bad}");
        }
    }

    #[test]
    fn link_headers() {
        assert!(ice_server_links("").is_empty());
        assert_eq!(
            ice_server_links("stun.l.google.com:19302"),
            vec!["<stun:stun.l.google.com:19302>; rel=\"ice-server\""]
        );
        assert_eq!(
            ice_server_links("stun:example.org:3478"),
            vec!["<stun:example.org:3478>; rel=\"ice-server\""]
        );
        assert!(ice_server_links("x>; rel=\"evil\"").is_empty());
    }

    #[test]
    fn advertised_addresses() {
        let mut cfg = WebRtcConfig {
            udp_port: 18443,
            public_ip: vec!["203.0.113.7".into()],
            ..Default::default()
        };
        assert_eq!(
            advertised_addrs(&cfg),
            vec!["203.0.113.7:18443".parse::<SocketAddr>().unwrap()]
        );
        cfg.public_ip.clear();
        let local = advertised_addrs(&cfg);
        assert!(!local.is_empty());
        assert!(local.iter().all(|a| a.port() == 18443 && a.is_ipv4()));
        // NAT port mapping: the advertised port differs from the bound one.
        cfg.public_ip = vec!["203.0.113.7".into()];
        cfg.public_port = 8444;
        assert_eq!(
            advertised_addrs(&cfg),
            vec!["203.0.113.7:8444".parse::<SocketAddr>().unwrap()]
        );
    }

    #[test]
    fn tokens() {
        let a = random_token(16);
        assert_eq!(a.len(), 16);
        assert!(a.bytes().all(|b| b.is_ascii_alphanumeric()));
        assert_ne!(a, random_token(16));
    }
}
