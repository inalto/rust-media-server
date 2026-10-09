//! In-memory view of the cluster state used on the request paths (docs/CLUSTER.md
//! §3.3, §3.6): the fresh `live_streams` rows (who owns which stream), the alive
//! `nodes`, the live viewers of the other nodes (from their latest samples) and the
//! owners' `/internal/status` answers.
//!
//! The refresh task of `ClusterNode` reloads the tables every [`REFRESH`] (the 2 s TTL
//! of docs/CLUSTER.md §3.3); the bus applies `rms_streams` notices immediately. All
//! reads are synchronous (no database round trip on the request paths).

use std::collections::HashMap;
use std::sync::{Mutex, RwLock};
use std::time::{Duration, Instant};

use crate::db::live_streams::LiveStreamRow;
use crate::db::nodes::NodeInfo;
use crate::metrics::ViewerCounts;

use super::RemoteStreamStatus;

/// Reload period of the tables (and TTL of the cached view).
pub const REFRESH: Duration = Duration::from_secs(2);
/// Lifetime of an owner's `/internal/status` answer.
pub const STATUS_TTL: Duration = Duration::from_secs(3);

/// The owner of a stream that is live on another node.
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteOwner {
    pub node_id: String,
    pub session_id: String,
    /// Internal URL of the owner (`None` until the node row was loaded).
    pub advertise_url: Option<String>,
    pub row: LiveStreamRow,
}

#[derive(Default)]
pub struct Directory {
    me: String,
    streams: RwLock<HashMap<String, LiveStreamRow>>,
    nodes: RwLock<HashMap<String, NodeInfo>>,
    remote_viewers: RwLock<HashMap<String, ViewerCounts>>,
    status: Mutex<HashMap<String, (Instant, Option<RemoteStreamStatus>)>>,
    loaded: Mutex<Option<Instant>>,
}

impl Directory {
    pub fn new(me: &str) -> Self {
        Self {
            me: me.to_string(),
            ..Default::default()
        }
    }

    /// Replace the stream rows and the alive nodes (full reload).
    pub fn replace(&self, rows: Vec<LiveStreamRow>, nodes: Vec<NodeInfo>) {
        *self.streams.write().unwrap_or_else(|e| e.into_inner()) =
            rows.into_iter().map(|r| (r.name.clone(), r)).collect();
        *self.nodes.write().unwrap_or_else(|e| e.into_inner()) = nodes
            .into_iter()
            .filter(|n| n.alive)
            .map(|n| (n.node_id.clone(), n))
            .collect();
        *self.loaded.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
    }

    /// When the tables were last loaded.
    pub fn loaded_at(&self) -> Option<Instant> {
        *self.loaded.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `name` started on `node` (notice): the row is fresh now.
    pub fn upsert_stream(&self, name: &str, node: &str) {
        let now = chrono::Utc::now();
        let mut map = self.streams.write().unwrap_or_else(|e| e.into_inner());
        let entry = map
            .entry(name.to_string())
            .or_insert_with(|| LiveStreamRow {
                name: name.to_string(),
                node_id: node.to_string(),
                session_id: String::new(),
                started_at: now,
                heartbeat: now,
            });
        if entry.node_id != node {
            entry.node_id = node.to_string();
            entry.session_id.clear();
            entry.started_at = now;
        }
        entry.heartbeat = now;
        self.status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(name);
    }

    /// `name` ended (released or reaped). With `node`, only if that node owns the row.
    pub fn remove_stream(&self, name: &str, node: Option<&str>) -> Option<LiveStreamRow> {
        let mut map = self.streams.write().unwrap_or_else(|e| e.into_inner());
        let matches = map
            .get(name)
            .is_some_and(|r| node.is_none_or(|n| r.node_id == n));
        let removed = if matches { map.remove(name) } else { None };
        drop(map);
        if removed.is_some() {
            self.status
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(name);
        }
        removed
    }

    /// The owner row of `name` (any node, this one included).
    pub fn owner_row(&self, name: &str) -> Option<LiveStreamRow> {
        self.streams
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .cloned()
    }

    /// `name` is live on another node: fresh row and owner not known to be dead.
    pub fn remote_owner(&self, name: &str) -> Option<RemoteOwner> {
        let row = self.owner_row(name)?;
        if row.node_id == self.me || row.node_id.is_empty() {
            return None;
        }
        let advertise_url = self
            .nodes
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&row.node_id)
            .map(|n| n.advertise_url.clone());
        Some(RemoteOwner {
            node_id: row.node_id.clone(),
            session_id: row.session_id.clone(),
            advertise_url,
            row,
        })
    }

    /// Every fresh row, by name.
    pub fn rows(&self) -> Vec<LiveStreamRow> {
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

    pub fn node(&self, node_id: &str) -> Option<NodeInfo> {
        self.nodes
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(node_id)
            .cloned()
    }

    /// Alive nodes, by id.
    pub fn nodes(&self) -> Vec<NodeInfo> {
        let mut v: Vec<_> = self
            .nodes
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect();
        v.sort_by(|a, b| a.node_id.cmp(&b.node_id));
        v
    }

    /// Replace the other nodes' live viewer counts; returns the names whose counts
    /// changed (the caller re-emits `ViewerCountChanged` for them).
    pub fn set_remote_viewers(&self, counts: Vec<(String, ViewerCounts)>) -> Vec<String> {
        let new: HashMap<String, ViewerCounts> = counts.into_iter().collect();
        let mut cur = self
            .remote_viewers
            .write()
            .unwrap_or_else(|e| e.into_inner());
        let mut changed: Vec<String> = new
            .iter()
            .filter(|(n, c)| cur.get(*n) != Some(*c))
            .map(|(n, _)| n.clone())
            .chain(cur.keys().filter(|n| !new.contains_key(*n)).cloned())
            .collect();
        changed.sort();
        *cur = new;
        changed
    }

    /// Live viewers of `name` on the other nodes.
    pub fn remote_viewers(&self, name: &str) -> ViewerCounts {
        self.remote_viewers
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .copied()
            .unwrap_or_default()
    }

    /// Cached `/internal/status` answer of the owner of `name`: `Some(answer)` while
    /// fresh (the answer itself may be `None`: not live there), `None` when stale.
    pub fn cached_status(&self, name: &str) -> Option<Option<RemoteStreamStatus>> {
        let map = self.status.lock().unwrap_or_else(|e| e.into_inner());
        map.get(name)
            .filter(|(at, _)| at.elapsed() < STATUS_TTL)
            .map(|(_, s)| s.clone())
    }

    /// The last answer, fresh or not.
    pub fn last_status(&self, name: &str) -> Option<RemoteStreamStatus> {
        let map = self.status.lock().unwrap_or_else(|e| e.into_inner());
        map.get(name).and_then(|(_, s)| s.clone())
    }

    pub fn set_status(&self, name: &str, status: Option<RemoteStreamStatus>) {
        let mut map = self.status.lock().unwrap_or_else(|e| e.into_inner());
        if map.len() > 4096 {
            map.retain(|_, (at, _)| at.elapsed() < STATUS_TTL);
        }
        map.insert(name.to_string(), (Instant::now(), status));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn row(name: &str, node: &str) -> LiveStreamRow {
        LiveStreamRow {
            name: name.into(),
            node_id: node.into(),
            session_id: "s".into(),
            started_at: Utc::now(),
            heartbeat: Utc::now(),
        }
    }

    fn node(id: &str, alive: bool) -> NodeInfo {
        NodeInfo {
            node_id: id.into(),
            advertise_url: format!("http://{id}:8081"),
            whep_url: None,
            version: "t".into(),
            started_at: Utc::now(),
            heartbeat: Utc::now(),
            live_streams: 0,
            viewers: 0,
            draining: false,
            alive,
        }
    }

    #[test]
    fn owners_rows_and_notices() {
        let d = Directory::new("n2");
        d.replace(
            vec![row("a", "n1"), row("b", "n2")],
            vec![node("n1", true), node("n2", true), node("n3", false)],
        );
        let o = d.remote_owner("a").unwrap();
        assert_eq!(o.node_id, "n1");
        assert_eq!(o.advertise_url.as_deref(), Some("http://n1:8081"));
        assert!(d.remote_owner("b").is_none(), "own stream");
        assert!(d.remote_owner("zz").is_none());
        assert_eq!(d.nodes().len(), 2, "dead nodes dropped");

        // Owner change by notice; removal only for the named owner.
        d.upsert_stream("a", "n3");
        assert_eq!(d.remote_owner("a").unwrap().node_id, "n3");
        assert!(d.remote_owner("a").unwrap().advertise_url.is_none());
        assert!(d.remote_stream_kept_for_wrong_node());
        assert!(d.remove_stream("a", Some("n3")).is_some());
        assert!(d.owner_row("a").is_none());
        d.upsert_stream("c", "n1");
        assert_eq!(d.rows().len(), 2);
    }

    impl Directory {
        fn remote_stream_kept_for_wrong_node(&self) -> bool {
            self.remove_stream("a", Some("n1")).is_none() && self.owner_row("a").is_some()
        }
    }

    #[test]
    fn remote_viewers_changes() {
        let d = Directory::new("n1");
        let c = |n| ViewerCounts {
            flv: n,
            ..Default::default()
        };
        assert_eq!(
            d.set_remote_viewers(vec![("a".into(), c(1)), ("b".into(), c(2))]),
            ["a", "b"]
        );
        assert_eq!(d.remote_viewers("a").total(), 1);
        assert_eq!(
            d.set_remote_viewers(vec![("a".into(), c(1)), ("c".into(), c(1))]),
            ["b", "c"]
        );
        assert_eq!(d.remote_viewers("b").total(), 0);
        assert!(
            d.set_remote_viewers(vec![("a".into(), c(1)), ("c".into(), c(1))])
                .is_empty()
        );
    }

    #[test]
    fn status_cache_expires() {
        let d = Directory::new("n1");
        assert!(d.cached_status("a").is_none());
        d.set_status("a", None);
        assert_eq!(d.cached_status("a"), Some(None));
        assert!(d.last_status("a").is_none());
    }
}
