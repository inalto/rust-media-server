//! Single-port UDP demultiplexer shared by every WebRTC session.
//!
//! All sessions use one socket (`webrtc.udp_port`). Each session registers its local
//! ICE username fragment; a STUN Binding Request carries `USERNAME = <local ufrag>:<remote
//! ufrag>`, which identifies the session and teaches the mux the peer's address. Every
//! other datagram (STUN responses/indications, DTLS, SRTP/SRTCP) is routed by peer
//! address. Unknown traffic is dropped. Per-session queues are bounded: a stalled session
//! loses datagrams instead of blocking the socket (ICE/DTLS retransmit, SRTP tolerates loss).

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::BytesMut;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Capacity of each session's inbound datagram queue.
pub const SESSION_QUEUE: usize = 512;
/// Peer addresses remembered per session (host, srflx, IPv4/IPv6 candidates...).
const MAX_ADDRS_PER_SESSION: usize = 8;
const STUN_MAGIC_COOKIE: [u8; 4] = [0x21, 0x12, 0xA4, 0x42];
const STUN_ATTR_USERNAME: u16 = 0x0006;
const STUN_BINDING_REQUEST: u16 = 0x0001;

/// One received datagram for a session.
#[derive(Debug)]
pub struct Inbound {
    pub peer: SocketAddr,
    pub data: BytesMut,
    pub at: Instant,
}

/// `(message type, USERNAME attribute)` of a STUN message, `None` if `buf` is not one
/// (RFC 5389 header with magic cookie, attributes within bounds).
pub fn parse_stun(buf: &[u8]) -> Option<(u16, Option<&str>)> {
    if buf.len() < 20 || buf[0] & 0xC0 != 0 || buf[4..8] != STUN_MAGIC_COOKIE {
        return None;
    }
    let msg_type = u16::from_be_bytes([buf[0], buf[1]]);
    let len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
    let body = buf.get(20..20 + len)?;
    let mut pos = 0;
    let mut username = None;
    while pos + 4 <= body.len() {
        let typ = u16::from_be_bytes([body[pos], body[pos + 1]]);
        let alen = u16::from_be_bytes([body[pos + 2], body[pos + 3]]) as usize;
        let value = body.get(pos + 4..pos + 4 + alen)?;
        if typ == STUN_ATTR_USERNAME {
            username = std::str::from_utf8(value).ok();
            break;
        }
        pos += 4 + alen.div_ceil(4) * 4;
    }
    Some((msg_type, username))
}

/// Local (server side) ufrag of a Binding Request `USERNAME` (`local:remote`).
pub fn local_ufrag(username: &str) -> &str {
    username.split(':').next().unwrap_or(username)
}

struct Route {
    ufrag: String,
    tx: mpsc::Sender<Inbound>,
    addrs: Vec<SocketAddr>,
}

#[derive(Default)]
struct Routes {
    sessions: HashMap<u64, Route>,
    by_ufrag: HashMap<String, u64>,
    by_addr: HashMap<SocketAddr, u64>,
}

impl Routes {
    /// Session for a datagram from `peer`; learns `peer` from Binding Requests.
    fn route(&mut self, peer: SocketAddr, data: &[u8]) -> Option<mpsc::Sender<Inbound>> {
        if let Some((typ, Some(username))) = parse_stun(data)
            && typ == STUN_BINDING_REQUEST
            && let Some(&id) = self.by_ufrag.get(local_ufrag(username))
        {
            if self.by_addr.get(&peer) != Some(&id) {
                let route = self.sessions.get_mut(&id)?;
                if route.addrs.len() < MAX_ADDRS_PER_SESSION {
                    route.addrs.push(peer);
                    if let Some(old) = self.by_addr.insert(peer, id)
                        && let Some(r) = self.sessions.get_mut(&old)
                    {
                        r.addrs.retain(|a| *a != peer);
                    }
                }
            }
            return self.sessions.get(&id).map(|r| r.tx.clone());
        }
        let id = self.by_addr.get(&peer)?;
        self.sessions.get(id).map(|r| r.tx.clone())
    }

    fn remove(&mut self, id: u64) {
        if let Some(r) = self.sessions.remove(&id) {
            if self.by_ufrag.get(&r.ufrag) == Some(&id) {
                self.by_ufrag.remove(&r.ufrag);
            }
            for a in r.addrs {
                if self.by_addr.get(&a) == Some(&id) {
                    self.by_addr.remove(&a);
                }
            }
        }
    }
}

/// The shared WebRTC UDP socket and its routing table.
pub struct UdpMux {
    socket: Arc<UdpSocket>,
    routes: Mutex<Routes>,
    dropped: AtomicU64,
}

impl UdpMux {
    pub fn new(socket: UdpSocket) -> Arc<Self> {
        Arc::new(Self {
            socket: Arc::new(socket),
            routes: Mutex::new(Routes::default()),
            dropped: AtomicU64::new(0),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Routes> {
        self.routes.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    /// Route datagrams whose STUN USERNAME starts with `ufrag` (and, afterwards, the
    /// peer addresses learnt from them) to `tx`.
    pub fn register(&self, id: u64, ufrag: &str, tx: mpsc::Sender<Inbound>) {
        let mut r = self.lock();
        r.remove(id);
        r.by_ufrag.insert(ufrag.to_string(), id);
        r.sessions.insert(
            id,
            Route {
                ufrag: ufrag.to_string(),
                tx,
                addrs: Vec::new(),
            },
        );
    }

    pub fn unregister(&self, id: u64) {
        self.lock().remove(id);
    }

    /// Datagrams dropped so far (unknown peer or full session queue).
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Send one datagram from the shared port.
    pub async fn send(&self, data: &[u8], peer: SocketAddr) -> io::Result<usize> {
        self.socket.send_to(data, peer).await
    }

    fn dispatch(&self, peer: SocketAddr, data: &[u8]) {
        let tx = self.lock().route(peer, data);
        let delivered = match tx {
            Some(tx) => tx
                .try_send(Inbound {
                    peer,
                    data: BytesMut::from(data),
                    at: Instant::now(),
                })
                .is_ok(),
            None => false,
        };
        if !delivered {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Receive loop; ends on shutdown. Transient socket errors (ICMP unreachable
    /// surfacing as `ConnectionRefused`, ...) are ignored.
    pub async fn run(self: Arc<Self>, shutdown: CancellationToken) {
        let mut buf = vec![0u8; 2048];
        tracing::debug!(task = "webrtc-udp", "started");
        loop {
            let r = tokio::select! {
                _ = shutdown.cancelled() => break,
                r = self.socket.recv_from(&mut buf) => r,
            };
            match r {
                Ok((n, peer)) => self.dispatch(peer, &buf[..n]),
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::ConnectionRefused
                            | io::ErrorKind::ConnectionReset
                            | io::ErrorKind::Interrupted
                            | io::ErrorKind::WouldBlock
                            | io::ErrorKind::TimedOut
                    ) => {}
                Err(e) => {
                    tracing::warn!(task = "webrtc-udp", error = %e, "UDP receive error");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
        tracing::debug!(task = "webrtc-udp", "stopped");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal STUN Binding Request with a USERNAME attribute (and a trailing
    /// attribute after it, like a real request).
    pub(crate) fn binding_request(username: &str) -> Vec<u8> {
        let mut attrs = Vec::new();
        attrs.extend_from_slice(&STUN_ATTR_USERNAME.to_be_bytes());
        attrs.extend_from_slice(&(username.len() as u16).to_be_bytes());
        attrs.extend_from_slice(username.as_bytes());
        while attrs.len() % 4 != 0 {
            attrs.push(0);
        }
        attrs.extend_from_slice(&[0x80, 0x29, 0, 8, 1, 2, 3, 4, 5, 6, 7, 8]); // ICE-CONTROLLING
        let mut m = Vec::new();
        m.extend_from_slice(&STUN_BINDING_REQUEST.to_be_bytes());
        m.extend_from_slice(&(attrs.len() as u16).to_be_bytes());
        m.extend_from_slice(&STUN_MAGIC_COOKIE);
        m.extend_from_slice(&[7; 12]);
        m.extend_from_slice(&attrs);
        m
    }

    #[test]
    fn stun_parsing() {
        let m = binding_request("srvufrag:abc");
        assert_eq!(
            parse_stun(&m),
            Some((STUN_BINDING_REQUEST, Some("srvufrag:abc")))
        );
        assert_eq!(local_ufrag("srvufrag:abc"), "srvufrag");
        assert_eq!(local_ufrag("solo"), "solo");
        // Not STUN: DTLS record, RTP, short, bad cookie, truncated attributes.
        assert!(
            parse_stun(&[
                22, 0xfe, 0xfd, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0
            ])
            .is_none()
        );
        assert!(parse_stun(&[0x80; 40]).is_none());
        assert!(parse_stun(&m[..10]).is_none());
        let mut bad = m.clone();
        bad[4] = 0;
        assert!(parse_stun(&bad).is_none());
        for n in 20..m.len() {
            let _ = parse_stun(&m[..n]); // never panics
        }
        let mut lying = m.clone();
        lying[2] = 0xff; // length beyond the buffer
        assert!(parse_stun(&lying).is_none());
    }

    #[tokio::test]
    async fn routes_by_ufrag_then_address() {
        let mut routes = Routes::default();
        let (tx1, mut rx1) = mpsc::channel(4);
        let (tx2, _rx2) = mpsc::channel(4);
        for (id, ufrag, tx) in [(1, "aaaa", tx1), (2, "bbbb", tx2)] {
            routes.by_ufrag.insert(ufrag.into(), id);
            routes.sessions.insert(
                id,
                Route {
                    ufrag: ufrag.into(),
                    tx,
                    addrs: vec![],
                },
            );
        }
        let peer: SocketAddr = "192.0.2.1:5000".parse().unwrap();
        // DTLS before any STUN from that address: unknown.
        assert!(routes.route(peer, &[22, 1, 2, 3]).is_none());
        // Binding request for session 1 teaches the address.
        let tx = routes.route(peer, &binding_request("aaaa:x")).unwrap();
        tx.try_send(Inbound {
            peer,
            data: BytesMut::new(),
            at: Instant::now(),
        })
        .unwrap();
        assert!(rx1.try_recv().is_ok());
        assert!(routes.route(peer, &[22, 1, 2, 3]).is_some());
        assert_eq!(routes.by_addr.get(&peer), Some(&1));
        // Unknown ufrag from a new address: dropped.
        let other: SocketAddr = "192.0.2.9:6000".parse().unwrap();
        assert!(routes.route(other, &binding_request("zzzz:x")).is_none());
        // Address limit per session.
        for p in 0..20u16 {
            let a = SocketAddr::from(([198, 51, 100, 1], 7000 + p));
            routes.route(a, &binding_request("bbbb:y"));
        }
        assert_eq!(routes.sessions[&2].addrs.len(), MAX_ADDRS_PER_SESSION);
        // Removing a session forgets its ufrag and addresses.
        routes.remove(1);
        assert!(routes.route(peer, &[22, 1, 2, 3]).is_none());
        assert!(!routes.by_ufrag.contains_key("aaaa"));
        assert_eq!(routes.by_addr.len(), MAX_ADDRS_PER_SESSION);
    }
}
