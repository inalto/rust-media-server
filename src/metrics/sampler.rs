//! Viewer sampler task (ARCHITECTURE.md §4.4): persists the live viewer counts as a
//! time series in `viewer_samples`.
//!
//! - Every `metrics.sample_interval_secs` it inserts one row per **live** stream (zeros
//!   included). Samples are taken in the middle of wall-clock slots of the interval
//!   (e.g. at :02.5, :07.5, … for 5 s) so that each epoch-aligned bucket of the series
//!   query gets exactly one sample despite timer jitter.
//! - When a stream ends it inserts a final zero sample.
//! - Retention: at startup and then daily, deletes samples (and closed viewer
//!   sessions) older than `metrics.retention_days` (in cluster mode only on the node
//!   holding the `metrics-retention` singleton lease); at startup it also closes the
//!   viewer sessions left open by a previous process of this node.
//!
//! `HubEvent::ViewerCountChanged` is *not* emitted here: the tracker already emits it
//! on every change.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::sync::broadcast::error::RecvError;

use super::{ViewerCounts, ViewerTracker};
use crate::config::Config;
use crate::db::{self, Db};
use crate::hub::{Hub, HubEvent};

const RETENTION_PERIOD: Duration = Duration::from_secs(24 * 3600);
/// Cluster-wide lease of the retention run: the first node after expiry runs it, the
/// others skip (at most about two runs a day whatever the number of nodes).
const RETENTION_LEASE: Duration = Duration::from_secs(12 * 3600);

/// Spawn the sampler task (call once at startup).
pub fn spawn_sampler(
    config: &Config,
    db: Db,
    hub: Arc<Hub>,
    tracker: Arc<ViewerTracker>,
) -> tokio::task::JoinHandle<()> {
    let interval = Duration::from_secs(config.metrics.sample_interval_secs.max(1));
    let retention_days = config.metrics.retention_days;
    let process_start = Utc::now();
    let mut events = hub.subscribe_events();
    let mut sampler = Sampler {
        db,
        hub,
        tracker,
        live: HashMap::new(),
    };
    tokio::spawn(async move {
        tracing::debug!(
            task = "metrics-sampler",
            interval_secs = interval.as_secs(),
            "started"
        );
        match db::metrics::close_dangling_viewer_sessions(&sampler.db, process_start).await {
            Ok(0) => {}
            Ok(n) => tracing::warn!(
                task = "metrics-sampler",
                count = n,
                "closed viewer sessions left open by a previous run"
            ),
            Err(e) => {
                tracing::warn!(task = "metrics-sampler", error = %e, "cannot close dangling viewer sessions")
            }
        }
        sampler.retention(retention_days).await;
        let mut retention = tokio::time::interval_at(
            tokio::time::Instant::now() + RETENTION_PERIOD,
            RETENTION_PERIOD,
        );
        retention.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut deadline = tokio::time::Instant::now() + until_next_slot(Utc::now(), interval);
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => {
                    sampler.tick(Utc::now()).await;
                    deadline = tokio::time::Instant::now() + until_next_slot(Utc::now(), interval);
                }
                ev = events.recv() => match ev {
                    Ok(HubEvent::StreamEnded { name }) => sampler.on_ended(&name, Utc::now()).await,
                    Ok(HubEvent::StreamStarted { name }) => sampler.on_started(&name),
                    Ok(_) => {}
                    Err(RecvError::Lagged(n)) => {
                        tracing::warn!(task = "metrics-sampler", skipped = n, "lagged on hub events");
                    }
                    Err(RecvError::Closed) => break,
                },
                _ = retention.tick() => sampler.retention(retention_days).await,
            }
        }
        tracing::debug!(task = "metrics-sampler", "stopped");
    })
}

/// Time until the middle of the next wall-clock slot of `interval`.
pub fn until_next_slot(now: DateTime<Utc>, interval: Duration) -> Duration {
    let period = (interval.as_millis() as i64).max(1);
    let offset = period / 2;
    let now_ms = now.timestamp_millis();
    let next = ((now_ms - offset).div_euclid(period) + 1) * period + offset;
    Duration::from_millis((next - now_ms).max(1) as u64)
}

struct Sampler {
    db: Db,
    hub: Arc<Hub>,
    tracker: Arc<ViewerTracker>,
    /// Streams sampled at least once (or seen starting): name -> broadcast id.
    live: HashMap<String, String>,
}

impl Sampler {
    async fn insert(&self, name: &str, broadcast_id: &str, ts: DateTime<Utc>, c: ViewerCounts) {
        if let Err(e) = db::metrics::insert_sample(&self.db, broadcast_id, ts, c).await {
            tracing::warn!(task = "metrics-sampler", broadcast = %name, error = %e, "cannot insert viewer sample");
        }
    }

    async fn tick(&mut self, now: DateTime<Utc>) {
        let streams = self.hub.list();
        let current: HashSet<&str> = streams.iter().map(|s| s.name.as_str()).collect();
        // Streams that ended without us seeing the event (lagged): final zero sample.
        let gone: Vec<(String, String)> = self
            .live
            .iter()
            .filter(|(n, _)| !current.contains(n.as_str()))
            .map(|(n, b)| (n.clone(), b.clone()))
            .collect();
        for (name, bid) in gone {
            self.live.remove(&name);
            self.insert(&name, &bid, now, ViewerCounts::default()).await;
        }
        for s in &streams {
            let counts = self.tracker.counts(&s.name);
            self.insert(&s.name, &s.broadcast_id, now, counts).await;
            self.live.insert(s.name.clone(), s.broadcast_id.clone());
        }
    }

    fn on_started(&mut self, name: &str) {
        if let Some(s) = self.hub.get(name) {
            self.live.insert(name.to_string(), s.broadcast_id.clone());
        }
    }

    async fn on_ended(&mut self, name: &str, now: DateTime<Utc>) {
        if self.hub.is_live(name) {
            // Already republished: nothing ended from the viewers' point of view.
            return;
        }
        if let Some(bid) = self.live.remove(name) {
            self.insert(name, &bid, now, ViewerCounts::default()).await;
        }
    }

    async fn retention(&self, days: u64) {
        match db::cluster::try_lease(
            &self.db,
            db::cluster::TASK_METRICS_RETENTION,
            RETENTION_LEASE,
        )
        .await
        {
            Ok(true) => {}
            Ok(false) => {
                tracing::debug!(task = "metrics-sampler", "retention runs on another node");
                return;
            }
            Err(e) => {
                tracing::warn!(task = "metrics-sampler", error = %e, "cannot take the retention lease");
                return;
            }
        }
        let days = days.min(365 * 100) as i64;
        let cutoff = Utc::now() - chrono::Duration::days(days);
        match db::metrics::delete_samples_older_than(&self.db, cutoff).await {
            Ok(0) => {}
            Ok(n) => tracing::info!(
                task = "metrics-sampler",
                deleted = n,
                "old viewer samples deleted"
            ),
            Err(e) => {
                tracing::warn!(task = "metrics-sampler", error = %e, "viewer samples retention failed")
            }
        }
        match db::metrics::delete_sessions_older_than(&self.db, cutoff).await {
            Ok(0) => {}
            Ok(n) => tracing::info!(
                task = "metrics-sampler",
                deleted = n,
                "old viewer sessions deleted"
            ),
            Err(e) => {
                tracing::warn!(task = "metrics-sampler", error = %e, "viewer sessions retention failed")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::broadcasts::{self, NewBroadcast};
    use crate::db::parse_ts;
    use crate::hub::PublishInfo;
    use crate::metrics::Protocol;

    #[test]
    fn slots_are_mid_interval() {
        let i = Duration::from_secs(5);
        let at = |s: &str| parse_ts(s);
        assert_eq!(
            until_next_slot(at("2026-10-08T12:00:00.000Z"), i),
            Duration::from_millis(2500)
        );
        assert_eq!(
            until_next_slot(at("2026-10-08T12:00:02.000Z"), i),
            Duration::from_millis(500)
        );
        assert_eq!(
            until_next_slot(at("2026-10-08T12:00:02.500Z"), i),
            Duration::from_millis(5000)
        );
        assert_eq!(
            until_next_slot(at("2026-10-08T12:00:04.900Z"), i),
            Duration::from_millis(2600)
        );
        let i = Duration::from_secs(60);
        assert_eq!(
            until_next_slot(at("2026-10-08T12:00:00.000Z"), i),
            Duration::from_secs(30)
        );
    }

    #[tokio::test]
    async fn tick_and_end_write_samples() {
        let db = Db::open_in_memory().unwrap();
        let b = broadcasts::create(&db, NewBroadcast::new("s", "S"))
            .await
            .unwrap();
        let hub = Hub::new();
        let tracker = ViewerTracker::new(&Config::default(), None, hub.events());
        let mut sampler = Sampler {
            db: db.clone(),
            hub: hub.clone(),
            tracker: tracker.clone(),
            live: HashMap::new(),
        };
        let t0 = parse_ts("2026-10-08T12:00:02.500Z");
        sampler.tick(t0).await; // nothing live
        let _s = hub
            .publish_start("s", PublishInfo::new(b.id.clone()))
            .unwrap();
        sampler.on_started("s");
        let _v = tracker.open("s", Protocol::Flv, "10.0.0.1".parse().unwrap(), "ua");
        sampler.tick(t0 + chrono::Duration::seconds(5)).await;
        hub.publish_end("s");
        sampler
            .on_ended("s", t0 + chrono::Duration::seconds(7))
            .await;
        // A second end (or a tick) must not duplicate the zero sample.
        sampler
            .on_ended("s", t0 + chrono::Duration::seconds(8))
            .await;
        sampler.tick(t0 + chrono::Duration::seconds(10)).await;

        let pts = db::metrics::series(
            &db,
            &b.id,
            t0 - chrono::Duration::hours(1),
            t0 + chrono::Duration::hours(1),
            1,
        )
        .await
        .unwrap();
        let totals: Vec<(i64, u32)> = pts
            .iter()
            .map(|p| (p.t.timestamp() - t0.timestamp(), p.total))
            .collect();
        assert_eq!(totals, [(5, 1), (7, 0)]);
    }

    #[tokio::test]
    async fn missed_end_event_is_caught_by_tick() {
        let db = Db::open_in_memory().unwrap();
        let hub = Hub::new();
        let tracker = ViewerTracker::new(&Config::default(), None, hub.events());
        let mut sampler = Sampler {
            db: db.clone(),
            hub: hub.clone(),
            tracker,
            live: HashMap::new(),
        };
        let t0 = parse_ts("2026-10-08T12:00:02.500Z");
        let _s = hub.publish_start("x", PublishInfo::new("bx")).unwrap();
        sampler.tick(t0).await;
        hub.publish_end("x");
        sampler.tick(t0 + chrono::Duration::seconds(5)).await;
        let pts = db::metrics::series(
            &db,
            "bx",
            t0 - chrono::Duration::hours(1),
            t0 + chrono::Duration::hours(1),
            5,
        )
        .await
        .unwrap();
        assert_eq!(pts.len(), 2);
        assert!(pts.iter().all(|p| p.total == 0));
        assert!(sampler.live.is_empty());
    }
}
