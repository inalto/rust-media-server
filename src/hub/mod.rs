//! Hub: registry of live streams keyed by broadcast name, plus the global event bus
//! (ARCHITECTURE.md §4.3).
//!
//! Producers (RTMP ingest) call [`Hub::publish_start`] / [`LiveStream::push`] /
//! [`Hub::publish_end_stream`]; consumers (FLV egress, packager, recorder) call
//! [`Hub::get`] + [`LiveStream::subscribe`] and watch [`Hub::subscribe_events`].

pub mod flv_parser;
pub mod packet;
pub mod stream;

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use tokio::sync::broadcast;

pub use packet::{MediaPacket, PacketKind, flv_file_header, flv_tag_bytes};
pub use stream::{GopCache, LiveStream, PublishInfo, StreamInfo, StreamOrigin};

use crate::cluster::bus::{Notice, Outbox};
use crate::metrics::ViewerCounts;

/// Capacity of the hub event channel.
pub const EVENTS_CAPACITY: usize = 1024;

/// Events broadcast to every interested task (SSE endpoints, packager, recorder...).
#[derive(Debug, Clone, PartialEq)]
pub enum HubEvent {
    /// A publisher started streaming to the broadcast `name`.
    StreamStarted { name: String },
    /// The stream ended (publisher disconnected or kicked).
    StreamEnded { name: String },
    /// `StreamInfo` changed (onMetaData or new sequence headers).
    Metadata { name: String },
    /// Concurrent viewer counts changed.
    ViewerCountChanged { name: String, counts: ViewerCounts },
    /// The persisted broadcast was created, modified (pause, key, settings...) or deleted.
    BroadcastUpdated { name: String },
    /// Global settings changed (`PATCH /api/v1/settings`: UI language, default waiting
    /// text). Not tied to a broadcast.
    SettingsChanged,
    /// A video on demand item was created, changed (status, progress, title, plays) or
    /// deleted (admin SSE `vod` / `vod_deleted`). Not tied to a broadcast.
    VodChanged { id: String },
    /// Cluster mode: a stream owned by another node started, ended or changed owner
    /// (from the `rms_streams` NOTIFY or the reaper). The local hub is unchanged; the
    /// SSE endpoints recompute the effective status.
    RemoteStreamChanged { name: String },
}

impl HubEvent {
    /// Broadcast the event refers to; `""` for [`HubEvent::SettingsChanged`] and
    /// [`HubEvent::VodChanged`] (never a valid broadcast name).
    pub fn name(&self) -> &str {
        match self {
            HubEvent::StreamStarted { name }
            | HubEvent::StreamEnded { name }
            | HubEvent::Metadata { name }
            | HubEvent::ViewerCountChanged { name, .. }
            | HubEvent::BroadcastUpdated { name }
            | HubEvent::RemoteStreamChanged { name } => name,
            HubEvent::SettingsChanged | HubEvent::VodChanged { .. } => "",
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum HubError {
    #[error("broadcast {0} is already live")]
    AlreadyLive(String),
}

pub struct Hub {
    streams: RwLock<HashMap<String, Arc<LiveStream>>>,
    events: broadcast::Sender<HubEvent>,
    /// Cluster NOTIFY queue (detached on a single node: [`Self::emit_shared`] is then
    /// [`Self::emit`]).
    outbox: Outbox,
}

impl Hub {
    pub fn new() -> Arc<Self> {
        let (events, _) = broadcast::channel(EVENTS_CAPACITY);
        Arc::new(Self {
            streams: RwLock::new(HashMap::new()),
            events,
            outbox: Outbox::default(),
        })
    }

    /// The cluster NOTIFY queue (attached by `cluster::bus` in cluster mode).
    pub fn outbox(&self) -> &Outbox {
        &self.outbox
    }

    /// Register a new live stream for broadcast `name` and emit `StreamStarted`.
    /// Fails if the broadcast is already live (one publisher per broadcast).
    pub fn publish_start(
        &self,
        name: &str,
        info: PublishInfo,
    ) -> Result<Arc<LiveStream>, HubError> {
        let stream = {
            let mut map = self.streams.write().unwrap_or_else(|e| e.into_inner());
            if map.contains_key(name) {
                return Err(HubError::AlreadyLive(name.to_string()));
            }
            let stream = Arc::new(LiveStream::new(name, info, self.events.clone()));
            map.insert(name.to_string(), Arc::clone(&stream));
            stream
        };
        tracing::info!(broadcast = %name, "stream started");
        self.emit(HubEvent::StreamStarted {
            name: name.to_string(),
        });
        Ok(stream)
    }

    /// End whatever stream is live for `name`; emits `StreamEnded`.
    pub fn publish_end(&self, name: &str) -> Option<Arc<LiveStream>> {
        let removed = self
            .streams
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(name);
        if let Some(s) = &removed {
            self.finish(s);
        }
        removed
    }

    /// End `stream` only if it is still the registered stream for its name (a stale
    /// publisher must not end a newer stream). Returns true if it was removed.
    pub fn publish_end_stream(&self, stream: &Arc<LiveStream>) -> bool {
        let removed = {
            let mut map = self.streams.write().unwrap_or_else(|e| e.into_inner());
            match map.get(&stream.name) {
                Some(cur) if Arc::ptr_eq(cur, stream) => map.remove(&stream.name).is_some(),
                _ => false,
            }
        };
        if removed {
            self.finish(stream);
        } else {
            stream.mark_ended();
        }
        removed
    }

    fn finish(&self, stream: &Arc<LiveStream>) {
        stream.mark_ended();
        tracing::info!(
            broadcast = %stream.name,
            bytes_in = stream.bytes_in(),
            packets = stream.packets(),
            "stream ended"
        );
        self.emit(HubEvent::StreamEnded {
            name: stream.name.clone(),
        });
    }

    /// Push a packet to the live stream `name`. Returns false if it is not live.
    pub fn push(&self, name: &str, packet: MediaPacket) -> bool {
        match self.get(name) {
            Some(s) => {
                s.push(packet);
                true
            }
            None => false,
        }
    }

    pub fn get(&self, name: &str) -> Option<Arc<LiveStream>> {
        self.streams
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .cloned()
    }

    pub fn is_live(&self, name: &str) -> bool {
        self.streams
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(name)
    }

    /// All live streams, sorted by name.
    pub fn list(&self) -> Vec<Arc<LiveStream>> {
        let mut v: Vec<_> = self
            .streams
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    pub fn live_count(&self) -> usize {
        self.streams.read().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// Streams published on this node (relays of other nodes' streams excluded).
    pub fn local_count(&self) -> usize {
        self.streams
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .filter(|s| s.origin.is_local())
            .count()
    }

    /// Ask the ingest to disconnect the publisher of `name`. Returns false if not live
    /// here (a relay of another node's stream is not a publisher: false).
    pub fn kick(&self, name: &str) -> bool {
        match self.get(name).filter(|s| s.origin.is_local()) {
            Some(s) => {
                tracing::info!(broadcast = %name, "kicking publisher");
                s.kick();
                true
            }
            None => false,
        }
    }

    pub fn subscribe_events(&self) -> broadcast::Receiver<HubEvent> {
        self.events.subscribe()
    }

    /// Sender side of the event bus (for components that emit events).
    pub fn events(&self) -> broadcast::Sender<HubEvent> {
        self.events.clone()
    }

    pub fn emit(&self, event: HubEvent) {
        // No receivers is fine.
        let _ = self.events.send(event);
    }

    /// [`Self::emit`] a change made on this node (broadcast, settings, video on demand)
    /// and, in cluster mode, queue the matching NOTIFY so the other nodes invalidate
    /// their caches and re-emit the event locally. Events received from other nodes are
    /// re-emitted with [`Self::emit`] only, so nothing loops.
    pub fn emit_shared(&self, event: HubEvent) {
        if let Some(notice) = Notice::for_event(&event) {
            self.outbox.send(notice);
        }
        self.emit(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    #[tokio::test]
    async fn lifecycle_and_events() {
        let hub = Hub::new();
        let mut ev = hub.subscribe_events();
        let s = hub.publish_start("a", PublishInfo::new("id-a")).unwrap();
        assert_eq!(
            hub.publish_start("a", PublishInfo::new("id-a"))
                .unwrap_err(),
            HubError::AlreadyLive("a".into())
        );
        assert!(hub.is_live("a"));
        assert_eq!(hub.list().len(), 1);
        assert!(hub.push(
            "a",
            MediaPacket::audio(0, Bytes::from_static(&[0xAF, 1, 0]))
        ));
        assert!(!hub.push(
            "b",
            MediaPacket::audio(0, Bytes::from_static(&[0xAF, 1, 0]))
        ));
        assert_eq!(
            ev.recv().await.unwrap(),
            HubEvent::StreamStarted { name: "a".into() }
        );

        // kick only signals; the ingest ends the stream.
        assert!(hub.kick("a"));
        assert!(s.is_kicked());
        assert!(!hub.kick("zzz"));

        // A stale stream object cannot end a newer one.
        hub.publish_end("a");
        assert!(s.is_ended());
        let s2 = hub.publish_start("a", PublishInfo::new("id-a")).unwrap();
        assert!(!hub.publish_end_stream(&s));
        assert!(hub.is_live("a"));
        assert!(hub.publish_end_stream(&s2));
        assert!(!hub.is_live("a"));
        assert_eq!(
            ev.recv().await.unwrap(),
            HubEvent::StreamEnded { name: "a".into() }
        );
    }
}
