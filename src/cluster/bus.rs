//! Cluster event bus on PostgreSQL `LISTEN/NOTIFY` (docs/CLUSTER.md §3.3).
//!
//! Channels and payloads (JSON, at most [`MAX_PAYLOAD`] bytes):
//! `{"name": <broadcast name | vod id | "">, "op": <operation>, "node": <sender>}`.
//!
//! | channel          | ops                         | receiver                                         |
//! |------------------|-----------------------------|--------------------------------------------------|
//! | `rms_broadcasts` | `updated`                   | invalidate the cache entry, `BroadcastUpdated`   |
//! | `rms_settings`   | `changed`                   | clear the broadcasts cache, `SettingsChanged`    |
//! | `rms_vod`        | `changed`                   | `VodChanged`, wake the VOD runner                |
//! | `rms_streams`    | `start`, `end`, `reaped`    | directory update, relays, `RemoteStreamChanged`  |
//!
//! **Sending.** Local changes go through [`Outbox`] (a bounded queue: a full queue drops
//! the notice, the receivers' caches expire anyway) to one sender task that runs
//! `SELECT pg_notify(channel, payload)`. Broadcast/settings/VOD notices come from
//! [`crate::hub::Hub::emit_shared`]; stream notices from the ownership code
//! (`ClusterNode`). **Receiving.** A dedicated `PgListener` connection; notices sent by
//! this node are ignored; events are re-emitted with `Hub::emit` (never
//! `emit_shared`), so nothing loops. After a lost connection (notifications are
//! transient) the listener reconnects with backoff and resyncs: live streams table,
//! relays, broadcasts cache, VOD runner.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::db::PgPool;
use crate::hub::HubEvent;
use crate::state::AppState;

/// Upper bound of a payload (PostgreSQL allows 8000 bytes; docs/CLUSTER.md says ~200).
pub const MAX_PAYLOAD: usize = 200;
/// Capacity of the outbound queue.
pub const OUTBOX_CAPACITY: usize = 1024;
/// How long the sender keeps going after shutdown (stream `end` notices of the
/// publishers being disconnected).
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(3);

/// NOTIFY channels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    Broadcasts,
    Streams,
    Settings,
    Vod,
}

impl Channel {
    pub const ALL: [Channel; 4] = [
        Channel::Broadcasts,
        Channel::Streams,
        Channel::Settings,
        Channel::Vod,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Channel::Broadcasts => "rms_broadcasts",
            Channel::Streams => "rms_streams",
            Channel::Settings => "rms_settings",
            Channel::Vod => "rms_vod",
        }
    }

    pub fn parse(s: &str) -> Option<Channel> {
        Channel::ALL.into_iter().find(|c| c.as_str() == s)
    }
}

/// Stream operations on [`Channel::Streams`].
pub const OP_START: &str = "start";
pub const OP_END: &str = "end";
pub const OP_REAPED: &str = "reaped";

/// A change to announce to the other nodes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub channel: Channel,
    pub name: String,
    pub op: &'static str,
    /// `reaped`: the dead owner (the sender is the reaping node).
    pub owner: Option<String>,
}

impl Notice {
    /// The notice of a local hub event that other nodes must see (`None` for events
    /// that stay local: stream start/end are announced by the ownership code, viewer
    /// counts are read from the samples).
    pub fn for_event(event: &HubEvent) -> Option<Notice> {
        let (channel, name, op) = match event {
            HubEvent::BroadcastUpdated { name } => (Channel::Broadcasts, name.clone(), "updated"),
            HubEvent::SettingsChanged => (Channel::Settings, String::new(), "changed"),
            HubEvent::VodChanged { id } => (Channel::Vod, id.clone(), "changed"),
            _ => return None,
        };
        Some(Notice {
            channel,
            name,
            op,
            owner: None,
        })
    }

    pub fn stream(name: &str, op: &'static str) -> Notice {
        Notice {
            channel: Channel::Streams,
            name: name.to_string(),
            op,
            owner: None,
        }
    }
}

/// Wire format of a payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Payload {
    pub name: String,
    pub op: String,
    pub node: String,
    /// `reaped` only: the dead owner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
}

impl Payload {
    /// JSON payload, `None` when it would exceed [`MAX_PAYLOAD`] (names and node ids
    /// have at most 64 characters: only a `reaped` notice with two very long node ids
    /// can exceed it, and is then sent without `owner`).
    pub fn encode(&self) -> Option<String> {
        serde_json::to_string(self)
            .ok()
            .filter(|s| s.len() <= MAX_PAYLOAD)
    }

    pub fn decode(s: &str) -> Option<Payload> {
        if s.len() > MAX_PAYLOAD * 4 {
            return None;
        }
        serde_json::from_str(s).ok()
    }
}

/// Handle on the outbound NOTIFY queue. Detached (a no-op) on a single node.
#[derive(Clone, Default)]
pub struct Outbox(Arc<OnceLock<mpsc::Sender<Notice>>>);

impl Outbox {
    /// Attach the sender task's queue (cluster mode, once).
    pub fn attach(&self, tx: mpsc::Sender<Notice>) {
        let _ = self.0.set(tx);
    }

    pub fn is_attached(&self) -> bool {
        self.0.get().is_some()
    }

    /// Queue a notice (never blocks; dropped when detached or full).
    pub fn send(&self, notice: Notice) {
        let Some(tx) = self.0.get() else { return };
        if let Err(mpsc::error::TrySendError::Full(n)) = tx.try_send(notice) {
            tracing::warn!(task = "cluster-bus", channel = n.channel.as_str(), name = %n.name, "NOTIFY queue full; notice dropped");
        }
    }
}

/// A notification received from another node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Received {
    pub channel: Channel,
    pub payload: Payload,
}

/// Parse a raw notification; `None` for unknown channels, malformed payloads and the
/// node's own notices.
pub fn parse_notification(channel: &str, payload: &str, me: &str) -> Option<Received> {
    let channel = Channel::parse(channel)?;
    let payload = Payload::decode(payload)?;
    (payload.node != me).then_some(Received { channel, payload })
}

/// Start the sender and listener tasks (cluster mode on PostgreSQL only).
pub fn spawn(state: &AppState) -> Vec<tokio::task::JoinHandle<()>> {
    let Some(pool) = state.db.pg().cloned() else {
        return Vec::new();
    };
    if !state.cluster.enabled() {
        return Vec::new();
    }
    let (tx, rx) = mpsc::channel(OUTBOX_CAPACITY);
    state.hub.outbox().attach(tx);
    let node = state.cluster.node_id().to_string();
    let sender = tokio::spawn(sender_loop(pool.clone(), node, rx, state.shutdown.clone()));
    let listener = tokio::spawn(listener_loop(state.clone(), pool));
    vec![sender, listener]
}

async fn sender_loop(
    pool: PgPool,
    node: String,
    mut rx: mpsc::Receiver<Notice>,
    shutdown: CancellationToken,
) {
    tracing::debug!(task = "cluster-bus-sender", "started");
    let mut failing = false;
    // After shutdown the publishers are still being finalized (stream `end` notices):
    // keep sending for a short grace period.
    let mut deadline: Option<tokio::time::Instant> = None;
    loop {
        if deadline.is_none() && shutdown.is_cancelled() {
            deadline = Some(tokio::time::Instant::now() + SHUTDOWN_GRACE);
        }
        let notice = tokio::select! {
            n = rx.recv() => match n { Some(n) => n, None => break },
            _ = shutdown.cancelled(), if deadline.is_none() => continue,
            _ = tokio::time::sleep_until(deadline.unwrap_or_else(tokio::time::Instant::now)), if deadline.is_some() => break,
        };
        let payload = Payload {
            name: notice.name.clone(),
            op: notice.op.to_string(),
            node: node.clone(),
            owner: notice.owner.clone(),
        };
        // Long node ids: a `reaped` notice falls back to "whoever owned it".
        let text = payload.encode().or_else(|| {
            Payload {
                owner: None,
                ..payload.clone()
            }
            .encode()
        });
        let Some(text) = text else {
            tracing::warn!(
                task = "cluster-bus-sender",
                channel = notice.channel.as_str(),
                "payload too large; notice dropped"
            );
            continue;
        };
        let r = tokio::time::timeout(
            Duration::from_secs(3),
            crate::db::pg::q("SELECT pg_notify($1, $2)")
                .bind(notice.channel.as_str())
                .bind(text)
                .execute(&pool),
        )
        .await;
        match r {
            Ok(Ok(_)) => {
                if failing {
                    tracing::info!(task = "cluster-bus-sender", "NOTIFY restored");
                    failing = false;
                }
            }
            Ok(Err(e)) => {
                if !failing {
                    tracing::warn!(task = "cluster-bus-sender", error = %e, "NOTIFY failed (the other nodes catch up through their cache TTLs)");
                    failing = true;
                }
            }
            Err(_) => {
                if !failing {
                    tracing::warn!(task = "cluster-bus-sender", "NOTIFY timed out");
                    failing = true;
                }
            }
        }
    }
    tracing::debug!(task = "cluster-bus-sender", "stopped");
}

async fn listener_loop(state: AppState, pool: PgPool) {
    let channels: Vec<&str> = Channel::ALL.iter().map(|c| c.as_str()).collect();
    let me = state.cluster.node_id().to_string();
    let mut backoff = Duration::from_millis(250);
    let shutdown = state.shutdown.clone();
    tracing::info!(task = "cluster-bus", channels = ?channels, "listening");
    'outer: loop {
        if shutdown.is_cancelled() {
            break;
        }
        let connect = async {
            let mut l = sqlx_postgres::PgListener::connect_with(&pool).await?;
            l.listen_all(channels.iter().copied()).await?;
            Ok::<_, sqlx_core::Error>(l)
        };
        let mut listener = tokio::select! {
            _ = shutdown.cancelled() => break,
            r = connect => match r {
                Ok(l) => l,
                Err(e) => {
                    tracing::warn!(task = "cluster-bus", error = %e, retry_ms = backoff.as_millis() as u64, "cannot LISTEN; retrying");
                    tokio::select! {
                        _ = shutdown.cancelled() => break,
                        _ = tokio::time::sleep(backoff) => {}
                    }
                    backoff = (backoff * 2).min(Duration::from_secs(10));
                    continue;
                }
            },
        };
        // (Re)connected: anything sent meanwhile was lost.
        backoff = Duration::from_millis(250);
        state.cluster.resync(&state).await;
        loop {
            let next = tokio::select! {
                _ = shutdown.cancelled() => break 'outer,
                n = listener.try_recv() => n,
            };
            match next {
                Ok(Some(n)) => {
                    if let Some(rx) = parse_notification(n.channel(), n.payload(), &me) {
                        state.cluster.on_notification(&state, rx).await;
                    }
                }
                Ok(None) => {
                    // Connection lost and re-established by sqlx: resync.
                    tracing::warn!(task = "cluster-bus", "LISTEN connection lost; resyncing");
                    state.cluster.resync(&state).await;
                }
                Err(e) => {
                    if shutdown.is_cancelled() {
                        break 'outer;
                    }
                    tracing::warn!(task = "cluster-bus", error = %e, "LISTEN failed; reconnecting");
                    drop(listener);
                    tokio::select! {
                        _ = shutdown.cancelled() => break 'outer,
                        _ = tokio::time::sleep(backoff) => {}
                    }
                    backoff = (backoff * 2).min(Duration::from_secs(10));
                    continue 'outer;
                }
            }
        }
    }
    tracing::debug!(task = "cluster-bus", "stopped");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notices_of_local_events() {
        let n = Notice::for_event(&HubEvent::BroadcastUpdated { name: "a".into() }).unwrap();
        assert_eq!(
            (n.channel, n.name.as_str(), n.op),
            (Channel::Broadcasts, "a", "updated")
        );
        let n = Notice::for_event(&HubEvent::SettingsChanged).unwrap();
        assert_eq!(n.channel, Channel::Settings);
        let n = Notice::for_event(&HubEvent::VodChanged { id: "v".into() }).unwrap();
        assert_eq!((n.channel, n.name.as_str()), (Channel::Vod, "v"));
        for local in [
            HubEvent::StreamStarted { name: "a".into() },
            HubEvent::StreamEnded { name: "a".into() },
            HubEvent::Metadata { name: "a".into() },
            HubEvent::RemoteStreamChanged { name: "a".into() },
        ] {
            assert!(Notice::for_event(&local).is_none(), "{local:?}");
        }
    }

    #[test]
    fn payload_round_trip_and_bounds() {
        let p = Payload {
            name: "x".repeat(64),
            op: OP_REAPED.into(),
            node: "n".repeat(32),
            owner: Some("o".repeat(32)),
        };
        let text = p.encode().unwrap();
        assert!(text.len() <= MAX_PAYLOAD, "{}", text.len());
        assert_eq!(Payload::decode(&text).unwrap(), p);
        let big = Payload {
            name: "x".repeat(300),
            ..p.clone()
        };
        assert!(big.encode().is_none());
        assert!(Payload::decode("nope").is_none());

        // Own notices and unknown channels are ignored.
        let own = Payload {
            name: "a".into(),
            op: OP_START.into(),
            node: "n1".into(),
            owner: None,
        }
        .encode()
        .unwrap();
        assert!(parse_notification("rms_streams", &own, "n1").is_none());
        assert!(parse_notification("other", &own, "n2").is_none());
        let r = parse_notification("rms_streams", &own, "n2").unwrap();
        assert_eq!(r.channel, Channel::Streams);
        assert_eq!(r.payload.op, OP_START);
    }

    #[tokio::test]
    async fn outbox_is_bounded_and_detached_by_default() {
        let o = Outbox::default();
        o.send(Notice::stream("a", OP_START)); // detached: no-op
        let (tx, mut rx) = mpsc::channel(1);
        o.attach(tx);
        assert!(o.is_attached());
        o.send(Notice::stream("a", OP_START));
        o.send(Notice::stream("b", OP_END)); // full: dropped
        assert_eq!(rx.recv().await.unwrap().name, "a");
        assert!(rx.try_recv().is_err());
    }

    /// NOTIFY round trip between two handles on PostgreSQL (skipped without
    /// `RMS_TEST_DATABASE_URL`): a notice sent through `pg_notify` by "node A" reaches a
    /// listener of "node B"; B's own notices are filtered out.
    #[tokio::test]
    async fn notify_round_trip_on_postgres() {
        let Some(a) = crate::db::testkit::postgres().await else {
            eprintln!("skipped: {} not set", crate::db::testkit::PG_URL_ENV);
            return;
        };
        let Some(b) = crate::db::testkit::postgres().await else {
            return;
        };
        let pool_a = a.db.pg().unwrap().clone();
        let pool_b = b.db.pg().unwrap().clone();
        let tag = uuid::Uuid::new_v4().simple().to_string();
        let (node_a, node_b) = (format!("a{}", &tag[..8]), format!("b{}", &tag[..8]));
        let mut listener = sqlx_postgres::PgListener::connect_with(&pool_b)
            .await
            .unwrap();
        listener
            .listen_all(Channel::ALL.iter().map(|c| c.as_str()))
            .await
            .unwrap();

        let (tx, rx) = mpsc::channel(8);
        let outbox = Outbox::default();
        outbox.attach(tx);
        let shutdown = CancellationToken::new();
        let sender = tokio::spawn(sender_loop(
            pool_a.clone(),
            node_a.clone(),
            rx,
            shutdown.clone(),
        ));
        // B's own notice (filtered) then A's.
        crate::db::pg::q("SELECT pg_notify($1, $2)")
            .bind(Channel::Broadcasts.as_str())
            .bind(
                Payload {
                    name: tag.clone(),
                    op: "updated".into(),
                    node: node_b.clone(),
                    owner: None,
                }
                .encode()
                .unwrap(),
            )
            .execute(&pool_b)
            .await
            .unwrap();
        outbox.send(Notice::stream(&tag, OP_START));
        let got = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let n = listener.recv().await.unwrap();
                // Other tests may share the database: only this test's names count.
                if let Some(r) = parse_notification(n.channel(), n.payload(), &node_b)
                    && r.payload.name == tag
                {
                    return r;
                }
            }
        })
        .await
        .expect("notification within 5 s");
        assert_eq!(got.channel, Channel::Streams);
        assert_eq!(got.payload.op, OP_START);
        assert_eq!(got.payload.node, node_a);
        shutdown.cancel();
        drop(outbox);
        let _ = sender.await;
        drop(listener);
        a.cleanup().await;
        b.cleanup().await;
    }
}
