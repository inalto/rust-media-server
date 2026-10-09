//! In-memory cache of `broadcasts` rows (`name → Broadcast`, `stream_key → name`).
//!
//! Every FLV/WS-FLV/HLS/DASH/WHEP request, public info request and RTMP publish looks a
//! broadcast up by name or key; the cache answers those from memory
//! (`db::broadcasts::find_by_name` / `find_by_key`). Only hits are cached (a lookup of an
//! unknown name always reaches the database), so the cache is bounded by the table.
//!
//! **Invalidation.** Every write of `db::broadcasts` invalidates the row it changed
//! *after* the write is committed (and before the repository function returns, hence
//! before the handler emits `HubEvent::BroadcastUpdated`); `db::settings` writes clear
//! the whole cache. A reader that loaded a row while a write was in flight cannot put
//! the stale row back: inserts carry the [`BroadcastCache::generation`] observed before
//! the read and are dropped when an invalidation happened meanwhile. Writes made by
//! *other* processes are not seen: on PostgreSQL entries also expire after a TTL
//! ([`super::PG_BROADCAST_CACHE_TTL`]) until the cluster NOTIFY listener (phase 7C-2)
//! calls [`BroadcastCache::invalidate`] / [`BroadcastCache::invalidate_all`].

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::Broadcast;

#[derive(Default)]
struct Inner {
    by_name: HashMap<String, (Broadcast, Instant)>,
    /// stream key → name
    by_key: HashMap<String, String>,
    generation: u64,
}

pub struct BroadcastCache {
    inner: Mutex<Inner>,
    /// `None` = entries never expire (single writer).
    ttl: Option<Duration>,
}

impl BroadcastCache {
    pub fn new(ttl: Option<Duration>) -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            ttl,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn fresh(&self, at: Instant) -> bool {
        self.ttl.is_none_or(|ttl| at.elapsed() < ttl)
    }

    /// Current generation: read it *before* querying the database and pass it to
    /// [`Self::insert`].
    pub fn generation(&self) -> u64 {
        self.lock().generation
    }

    pub fn get_by_name(&self, name: &str) -> Option<Broadcast> {
        let inner = self.lock();
        let (b, at) = inner.by_name.get(name)?;
        self.fresh(*at).then(|| b.clone())
    }

    pub fn get_by_key(&self, stream_key: &str) -> Option<Broadcast> {
        let inner = self.lock();
        let name = inner.by_key.get(stream_key)?;
        let (b, at) = inner.by_name.get(name)?;
        (self.fresh(*at) && b.stream_key == stream_key).then(|| b.clone())
    }

    /// Cache a row read from the database, unless an invalidation happened since
    /// `generation` was read.
    pub fn insert(&self, generation: u64, b: &Broadcast) {
        let mut inner = self.lock();
        if inner.generation != generation {
            return;
        }
        if let Some((old, _)) = inner.by_name.get(&b.name)
            && old.stream_key != b.stream_key
        {
            let old_key = old.stream_key.clone();
            inner.by_key.remove(&old_key);
        }
        inner.by_key.insert(b.stream_key.clone(), b.name.clone());
        inner
            .by_name
            .insert(b.name.clone(), (b.clone(), Instant::now()));
    }

    /// Forget one broadcast (by name), including its stream key mapping.
    pub fn invalidate(&self, name: &str) {
        let mut inner = self.lock();
        inner.generation = inner.generation.wrapping_add(1);
        if inner.by_name.remove(name).is_some() {
            inner.by_key.retain(|_, n| n != name);
        }
    }

    /// Forget everything (settings changes, NOTIFY resync).
    pub fn invalidate_all(&self) {
        let mut inner = self.lock();
        inner.generation = inner.generation.wrapping_add(1);
        inner.by_name.clear();
        inner.by_key.clear();
    }

    /// Cached rows (fresh or not).
    pub fn len(&self) -> usize {
        self.lock().by_name.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::broadcasts::{self, NewBroadcast};
    use crate::db::{Db, parse_ts};

    fn row(name: &str, key: &str) -> Broadcast {
        let t = parse_ts("2026-10-09T10:00:00.000Z");
        Broadcast {
            id: format!("id-{name}"),
            name: name.into(),
            title: name.into(),
            description: String::new(),
            stream_key: key.into(),
            record: false,
            enable_flv: true,
            enable_hls: true,
            enable_dash: true,
            enable_webrtc: true,
            paused: false,
            waiting_video: None,
            waiting_text: String::new(),
            created_at: t,
            updated_at: t,
        }
    }

    #[test]
    fn insert_lookup_invalidate() {
        let c = BroadcastCache::new(None);
        let g = c.generation();
        c.insert(g, &row("a", "k1"));
        assert_eq!(c.get_by_name("a").unwrap().stream_key, "k1");
        assert_eq!(c.get_by_key("k1").unwrap().name, "a");
        assert!(c.get_by_key("k2").is_none());
        // Key change: the old key no longer resolves.
        c.insert(c.generation(), &row("a", "k2"));
        assert!(c.get_by_key("k1").is_none());
        assert_eq!(c.get_by_key("k2").unwrap().name, "a");
        c.invalidate("a");
        assert!(c.get_by_name("a").is_none() && c.get_by_key("k2").is_none());
        assert!(c.is_empty());
    }

    #[test]
    fn stale_insert_after_invalidation_is_dropped() {
        let c = BroadcastCache::new(None);
        let g = c.generation();
        // A write commits and invalidates while a reader holds an old row.
        c.invalidate("a");
        c.insert(g, &row("a", "old"));
        assert!(c.get_by_name("a").is_none());
        c.insert(c.generation(), &row("b", "kb"));
        c.invalidate_all();
        assert!(c.get_by_key("kb").is_none());
    }

    #[test]
    fn ttl_expires_entries() {
        let c = BroadcastCache::new(Some(Duration::from_millis(0)));
        c.insert(c.generation(), &row("a", "k"));
        assert!(c.get_by_name("a").is_none());
        assert!(c.get_by_key("k").is_none());
    }

    #[tokio::test]
    async fn repository_writes_invalidate_the_cache() {
        let db = Db::open_in_memory().unwrap();
        let b = broadcasts::create(&db, NewBroadcast::new("show", "Show"))
            .await
            .unwrap();
        // Fill the cache through both lookups.
        assert_eq!(
            broadcasts::find_by_name(&db, "show")
                .await
                .unwrap()
                .unwrap(),
            b
        );
        assert!(
            broadcasts::find_by_key(&db, &b.stream_key)
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(db.broadcast_cache().len(), 1);
        let p = broadcasts::set_paused(&db, "show", true)
            .await
            .unwrap()
            .unwrap();
        assert!(db.broadcast_cache().is_empty(), "write invalidates");
        assert!(
            broadcasts::find_by_name(&db, "show")
                .await
                .unwrap()
                .unwrap()
                .paused
        );
        assert!(p.paused);
        let rk = broadcasts::regenerate_key(&db, "show")
            .await
            .unwrap()
            .unwrap();
        assert!(
            broadcasts::find_by_key(&db, &b.stream_key)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            broadcasts::find_by_key(&db, &rk.stream_key)
                .await
                .unwrap()
                .unwrap()
                .name,
            "show"
        );
        crate::db::settings::set(&db, crate::db::settings::UI_LANGUAGE, "en")
            .await
            .unwrap();
        assert!(db.broadcast_cache().is_empty(), "settings clear the cache");
        broadcasts::delete(&db, "show").await.unwrap();
        assert!(
            broadcasts::find_by_name(&db, "show")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            broadcasts::find_by_key(&db, &rk.stream_key)
                .await
                .unwrap()
                .is_none()
        );
    }
}
