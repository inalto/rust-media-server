//! Read side of the viewer metrics (`GET /broadcasts/{name}/metrics[/summary]`,
//! ARCHITECTURE.md §8.1): query range validation, bucket size selection, the dense
//! time series and the summary.
//!
//! - Range: `from`/`to` are RFC 3339; defaults are the last hour ending now; `from < to`
//!   and at most [`MAX_RANGE_SECS`] (90 days).
//! - Step: chosen among [`ALLOWED_STEPS`] as the smallest value that is at least the
//!   requested step (if any) and the sampling interval, and that yields at most
//!   [`MAX_POINTS`] buckets. The contract lists 5…3600 s; longer steps (2 h, 6 h, 1 day)
//!   were added so that ranges longer than 25 h (up to 90 days) still fit in 600 points.
//! - Series: buckets aligned to the Unix epoch, `t` = bucket start, **max** of every
//!   column within the bucket; buckets without samples (stream offline) are filled with
//!   zeros so charts get an evenly spaced axis. No points are produced after "now".

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;

use super::{ViewerCounts, ViewerTracker};
use crate::db::metrics::{self as repo, Point};
use crate::db::{Broadcast, Db, DbResult};
use crate::hub::Hub;

/// Bucket sizes (seconds) the series can use.
pub const ALLOWED_STEPS: [u64; 10] = [5, 10, 30, 60, 300, 900, 3600, 7200, 21600, 86400];
/// Maximum number of points returned by the series endpoint.
pub const MAX_POINTS: u64 = 600;
/// Maximum query range (90 days).
pub const MAX_RANGE_SECS: i64 = 90 * 86_400;
/// Default range when `from` is absent (1 hour).
pub const DEFAULT_RANGE_SECS: i64 = 3_600;
/// Largest explicit `step` accepted.
pub const MAX_STEP_SECS: u64 = 86_400;

/// Parse an RFC 3339 query value. A `+` of a timezone offset that arrived unencoded
/// in a query string becomes a space; it is restored.
pub fn parse_time(field: &str, value: &str) -> Result<DateTime<Utc>, String> {
    let v = value.trim().replace(' ', "+");
    DateTime::parse_from_rfc3339(&v)
        .map(|d| d.with_timezone(&Utc))
        .map_err(|_| format!("invalid {field}: expected an RFC 3339 timestamp"))
}

/// Resolve and validate `from`/`to` (defaults: the hour ending at `now`).
pub fn resolve_range(
    from: Option<&str>,
    to: Option<&str>,
    now: DateTime<Utc>,
) -> Result<(DateTime<Utc>, DateTime<Utc>), String> {
    fn non_empty(v: Option<&str>) -> Option<&str> {
        v.filter(|s| !s.trim().is_empty())
    }
    let to = match non_empty(to) {
        Some(v) => parse_time("to", v)?,
        None => now,
    };
    let from = match non_empty(from) {
        Some(v) => parse_time("from", v)?,
        None => to - Duration::seconds(DEFAULT_RANGE_SECS),
    };
    if from >= to {
        return Err("from must be earlier than to".into());
    }
    if (to - from).num_seconds() > MAX_RANGE_SECS {
        return Err("range too large (maximum 90 days)".into());
    }
    // Millisecond precision, like the stored timestamps.
    let ms = |t: DateTime<Utc>| DateTime::from_timestamp_millis(t.timestamp_millis()).unwrap_or(t);
    Ok((ms(from), ms(to)))
}

/// Parse an explicit `step` query value (seconds, `1..=86400`).
pub fn parse_step(value: &str) -> Result<u64, String> {
    match value.trim().parse::<u64>() {
        Ok(s) if (1..=MAX_STEP_SECS).contains(&s) => Ok(s),
        _ => Err(format!(
            "invalid step: expected an integer number of seconds between 1 and {MAX_STEP_SECS}"
        )),
    }
}

fn bucket_start(epoch_secs: i64, step: u64) -> i64 {
    epoch_secs.div_euclid(step as i64) * step as i64
}

/// Number of epoch-aligned buckets of `step` seconds touched by `[from, to)`.
pub fn bucket_count(from: DateTime<Utc>, to: DateTime<Utc>, step: u64) -> u64 {
    if to <= from || step == 0 {
        return 0;
    }
    let first = bucket_start(from.timestamp(), step);
    let last_ms = to.timestamp_millis() - 1;
    let last = bucket_start(last_ms.div_euclid(1000), step);
    ((last - first) / step as i64 + 1).max(0) as u64
}

/// Bucket size for `[from, to)`: the smallest allowed step `>= max(requested, min_step)`
/// producing at most [`MAX_POINTS`] buckets (the largest allowed step otherwise).
pub fn choose_step(
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    requested: Option<u64>,
    min_step: u64,
) -> u64 {
    let floor = requested.unwrap_or(0).max(min_step);
    ALLOWED_STEPS
        .iter()
        .copied()
        .find(|&s| s >= floor && bucket_count(from, to, s) <= MAX_POINTS)
        .unwrap_or(ALLOWED_STEPS[ALLOWED_STEPS.len() - 1])
}

/// Dense series: one point per bucket from the bucket containing `from` to the one
/// containing `min(to, now)`; buckets missing from `sparse` are zero, except the
/// in-progress bucket containing "now", which is omitted until it has a sample.
pub fn fill_series(
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    now: DateTime<Utc>,
    step: u64,
    sparse: &[Point],
) -> Vec<Point> {
    let end = to.min(now);
    if end <= from || step == 0 {
        return Vec::new();
    }
    let by_bucket: HashMap<i64, &Point> = sparse.iter().map(|p| (p.t.timestamp(), p)).collect();
    let first = bucket_start(from.timestamp(), step);
    let mut last = bucket_start((end.timestamp_millis() - 1).div_euclid(1000), step);
    if now < to && !by_bucket.contains_key(&last) {
        // The bucket containing "now" is still being filled and has no sample yet:
        // reporting it as zero would make live charts dip at their right edge.
        last -= step as i64;
    }
    let mut out = Vec::with_capacity(((last - first) / step as i64 + 1).max(0) as usize);
    let mut b = first;
    while b <= last {
        let t = DateTime::from_timestamp(b, 0).unwrap_or_default();
        out.push(
            by_bucket
                .get(&b)
                .map(|p| **p)
                .unwrap_or_else(|| Point::zero(t)),
        );
        b += step as i64;
    }
    out
}

/// `GET /broadcasts/{name}/metrics` response.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Series {
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    pub step: u64,
    pub points: Vec<Point>,
}

/// Time series of a broadcast over `[from, to)` (see the module docs).
/// `min_step` is the sampling interval (no point in buckets smaller than that).
pub async fn series(
    db: &Db,
    broadcast_id: &str,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    requested_step: Option<u64>,
    min_step: u64,
) -> DbResult<Series> {
    let step = choose_step(from, to, requested_step, min_step);
    let sparse = repo::series(db, broadcast_id, from, to, step).await?;
    Ok(Series {
        from,
        to,
        step,
        points: fill_series(from, to, Utc::now(), step, &sparse),
    })
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct Peak {
    pub value: u32,
    pub at: DateTime<Utc>,
}

/// `GET /broadcasts/{name}/metrics/summary` response.
///
/// `current` and `live_since` describe "now"; `peak`, `sessions_total`,
/// `avg_watch_secs` and `bytes_in` cover `[from, to)`:
/// - `peak`: highest sampled total (the live count is included when "now" is in the
///   range), `null` when there is no sample;
/// - `sessions_total`: viewer sessions overlapping the range (all protocols);
/// - `avg_watch_secs`: average full duration of those sessions (open ones up to now);
/// - `bytes_in`: bytes received in the live periods overlapping the range (the current
///   one included, from the live counter).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Summary {
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    pub current: ViewerCounts,
    pub peak: Option<Peak>,
    pub sessions_total: u64,
    pub avg_watch_secs: f64,
    pub live_since: Option<DateTime<Utc>>,
    pub bytes_in: u64,
}

/// Pure combination step of [`summary`] (unit-tested).
pub fn merge_peak(
    sampled: Option<(u32, DateTime<Utc>)>,
    current: u32,
    now: DateTime<Utc>,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Option<Peak> {
    let sampled = sampled.map(|(value, at)| Peak { value, at });
    let now_in_range = now >= from && now < to;
    match sampled {
        Some(p) if !now_in_range || p.value >= current => Some(p),
        _ if now_in_range && current > 0 => Some(Peak {
            value: current,
            at: now,
        }),
        other => other,
    }
}

/// Round to milliseconds (keeps the JSON readable).
fn round_ms(secs: f64) -> f64 {
    (secs * 1000.0).round() / 1000.0
}

/// Summary of a broadcast over `[from, to)` (see [`Summary`]).
pub async fn summary(
    db: &Db,
    hub: &Arc<Hub>,
    tracker: &ViewerTracker,
    broadcast: &Broadcast,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> DbResult<Summary> {
    let now = Utc::now();
    let current = tracker.counts(&broadcast.name);
    let stream = hub.get(&broadcast.name);
    let sampled = repo::peak(db, &broadcast.id, from, to).await?;
    let sessions_total = repo::sessions_total(db, &broadcast.id, from, to).await?;
    let avg = repo::avg_watch_secs(db, &broadcast.id, from, to, now).await?;
    let mut bytes_in = repo::stream_bytes_in(db, &broadcast.id, from, to).await?;
    if let Some(s) = &stream
        && s.started_at < to
    {
        bytes_in += s.bytes_in();
    }
    Ok(Summary {
        from,
        to,
        current,
        peak: merge_peak(sampled, current.total(), now, from, to),
        sessions_total,
        avg_watch_secs: round_ms(avg),
        live_since: stream.map(|s| s.started_at),
        bytes_in,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::parse_ts;

    fn t(s: &str) -> DateTime<Utc> {
        parse_ts(s)
    }

    #[test]
    fn range_defaults_and_validation() {
        let now = t("2026-10-08T12:00:00.000Z");
        let (f, to) = resolve_range(None, None, now).unwrap();
        assert_eq!(to, now);
        assert_eq!(f, t("2026-10-08T11:00:00.000Z"));

        let (f, to) = resolve_range(Some("2026-10-08T10:00:00Z"), None, now).unwrap();
        assert_eq!((f, to), (t("2026-10-08T10:00:00.000Z"), now));

        let (f, to) = resolve_range(None, Some("2026-10-08T09:00:00Z"), now).unwrap();
        assert_eq!(
            (f, to),
            (t("2026-10-08T08:00:00.000Z"), t("2026-10-08T09:00:00.000Z"))
        );

        // Offsets, including a '+' decoded to a space by the query string parser.
        let (f, _) = resolve_range(Some("2026-10-08T12:00:00 02:00"), None, now).unwrap();
        assert_eq!(f, t("2026-10-08T10:00:00.000Z"));
        // Empty values mean "default".
        assert!(resolve_range(Some(""), Some(" "), now).is_ok());

        assert!(resolve_range(Some("yesterday"), None, now).is_err());
        assert!(resolve_range(None, Some("1700000000"), now).is_err());
        assert!(
            resolve_range(
                Some("2026-10-08T12:00:00Z"),
                Some("2026-10-08T12:00:00Z"),
                now
            )
            .is_err()
        );
        assert!(
            resolve_range(
                Some("2026-10-08T13:00:00Z"),
                Some("2026-10-08T12:00:00Z"),
                now
            )
            .is_err()
        );
        assert!(
            resolve_range(
                Some("2026-07-01T00:00:00Z"),
                Some("2026-10-08T00:00:00Z"),
                now
            )
            .is_err()
        );
        assert!(
            resolve_range(
                Some("2026-07-10T00:00:00Z"),
                Some("2026-10-08T00:00:00Z"),
                now
            )
            .is_ok()
        );

        assert_eq!(parse_step("30"), Ok(30));
        assert!(parse_step("0").is_err());
        assert!(parse_step("-5").is_err());
        assert!(parse_step("abc").is_err());
        assert!(parse_step("86401").is_err());
    }

    #[test]
    fn step_selection_keeps_at_most_600_points() {
        let to = t("2026-10-08T12:00:00.000Z");
        let ago = |secs: i64| to - Duration::seconds(secs);
        // 10 minutes -> 5 s (120 points).
        assert_eq!(choose_step(ago(600), to, None, 5), 5);
        assert_eq!(bucket_count(ago(600), to, 5), 120);
        // 50 minutes, aligned -> exactly 600 points at 5 s.
        assert_eq!(choose_step(ago(3000), to, None, 5), 5);
        // Unaligned 50 minutes would be 601 buckets -> 10 s.
        let unaligned = to + Duration::milliseconds(1500);
        assert_eq!(
            bucket_count(ago(3000) + Duration::milliseconds(1500), unaligned, 5),
            601
        );
        assert_eq!(
            choose_step(ago(3000) + Duration::milliseconds(1500), unaligned, None, 5),
            10
        );
        // Default hour -> 10 s (360 points).
        assert_eq!(choose_step(ago(3600), to, None, 5), 10);
        // A day -> 300 s (288 points); a week -> 3600 s (168); 30 days -> 7200 s; 90 days -> 21600 s.
        assert_eq!(choose_step(ago(86_400), to, None, 5), 300);
        assert_eq!(choose_step(ago(7 * 86_400), to, None, 5), 3600);
        assert_eq!(choose_step(ago(30 * 86_400), to, None, 5), 7200);
        assert_eq!(choose_step(ago(90 * 86_400), to, None, 5), 21600);
        // Explicit step: snapped up to an allowed value, raised when too many points.
        assert_eq!(choose_step(ago(600), to, Some(60), 5), 60);
        assert_eq!(choose_step(ago(600), to, Some(45), 5), 60);
        assert_eq!(choose_step(ago(600), to, Some(1), 5), 5);
        assert_eq!(choose_step(ago(3600), to, Some(5), 5), 10);
        // Never below the sampling interval.
        assert_eq!(choose_step(ago(600), to, None, 15), 30);
        for range in [60, 3599, 3601, 86_399, 7 * 86_400 + 13, 90 * 86_400] {
            let from = ago(range) + Duration::milliseconds(777);
            let s = choose_step(from, to, None, 5);
            assert!(
                bucket_count(from, to, s) <= MAX_POINTS,
                "range {range} step {s}"
            );
        }
    }

    #[test]
    fn dense_series_fills_gaps_and_stops_at_now() {
        let from = t("2026-10-08T12:00:01.000Z");
        let to = t("2026-10-08T12:01:00.000Z");
        let mut p = Point::zero(t("2026-10-08T12:00:10.000Z"));
        p.total = 3;
        p.flv = 3;
        let pts = fill_series(from, to, to + Duration::hours(1), 10, &[p]);
        assert_eq!(pts.len(), 6);
        assert_eq!(pts[0].t, t("2026-10-08T12:00:00.000Z"));
        assert_eq!(pts[1], p);
        assert!(pts.iter().filter(|x| x.t != p.t).all(|x| x.total == 0));
        assert_eq!(pts[5].t, t("2026-10-08T12:00:50.000Z"));
        // "now" in the middle of the range: no future points, and the in-progress
        // bucket (12:00:20) is omitted while it has no sample.
        let now = t("2026-10-08T12:00:25.000Z");
        let pts = fill_series(from, to, now, 10, &[p]);
        assert_eq!(pts.len(), 2);
        assert_eq!(pts[1], p);
        let mut cur = Point::zero(t("2026-10-08T12:00:20.000Z"));
        cur.total = 1;
        let pts = fill_series(from, to, now, 10, &[p, cur]);
        assert_eq!(pts.len(), 3);
        assert_eq!(pts[2], cur);
        // Range ending in the past: every bucket is complete, trailing zeros kept.
        let pts = fill_series(from, now, to, 10, &[p]);
        assert_eq!(pts.len(), 3);
        assert_eq!(pts[2].total, 0);
        // Entirely in the future.
        assert!(fill_series(now, to, from, 10, &[]).is_empty());
        // JSON shape.
        let v = serde_json::to_value(pts[1]).unwrap();
        assert_eq!(v["t"], "2026-10-08T12:00:10Z");
        for k in ["total", "flv", "ws_flv", "hls", "dash", "webrtc"] {
            assert!(v[k].is_u64(), "{k}");
        }
    }

    #[test]
    fn peak_merging() {
        let from = t("2026-10-08T12:00:00.000Z");
        let to = t("2026-10-08T13:00:00.000Z");
        let at = t("2026-10-08T12:10:00.000Z");
        let now = t("2026-10-08T12:30:00.000Z");
        // Sampled peak wins when higher or equal.
        assert_eq!(
            merge_peak(Some((4, at)), 2, now, from, to),
            Some(Peak { value: 4, at })
        );
        assert_eq!(
            merge_peak(Some((2, at)), 2, now, from, to),
            Some(Peak { value: 2, at })
        );
        // Live count above the samples (not sampled yet).
        assert_eq!(
            merge_peak(Some((1, at)), 3, now, from, to),
            Some(Peak { value: 3, at: now })
        );
        assert_eq!(
            merge_peak(None, 1, now, from, to),
            Some(Peak { value: 1, at: now })
        );
        // No samples and nobody watching -> null; zero samples -> peak 0.
        assert_eq!(merge_peak(None, 0, now, from, to), None);
        assert_eq!(
            merge_peak(Some((0, at)), 0, now, from, to),
            Some(Peak { value: 0, at })
        );
        // "now" outside the range: the live count is ignored.
        let later = t("2026-10-08T14:00:00.000Z");
        assert_eq!(merge_peak(None, 5, later, from, to), None);
        assert_eq!(
            merge_peak(Some((1, at)), 5, later, from, to),
            Some(Peak { value: 1, at })
        );
    }

    #[tokio::test]
    async fn summary_math() {
        use crate::config::Config;
        use crate::db::broadcasts::{self, NewBroadcast};
        use crate::db::metrics::{
            ViewerSessionRecord, close_viewer_session, insert_sample, insert_viewer_session,
        };
        use crate::hub::PublishInfo;
        use crate::metrics::Protocol;

        let db = Db::open_in_memory().unwrap();
        let b = broadcasts::create(&db, NewBroadcast::new("sum", "Sum"))
            .await
            .unwrap();
        let hub = Hub::new();
        let tracker = ViewerTracker::new(&Config::default(), None, hub.events());
        let now = Utc::now();
        let t0 = now - Duration::minutes(10);
        for (i, total) in [0u32, 2, 1].iter().enumerate() {
            let c = ViewerCounts {
                hls: *total,
                ..Default::default()
            };
            insert_sample(&db, &b.id, t0 + Duration::seconds(5 * i as i64), c)
                .await
                .unwrap();
        }
        for (id, start, len) in [("s1", 0, 20), ("s2", 5, 40)] {
            insert_viewer_session(
                &db,
                ViewerSessionRecord {
                    id: id.into(),
                    broadcast_name: "sum".into(),
                    protocol: Protocol::Hls,
                    ip: None,
                    user_agent: None,
                    started_at: t0 + Duration::seconds(start),
                },
            )
            .await
            .unwrap();
            close_viewer_session(&db, id, t0 + Duration::seconds(start + len))
                .await
                .unwrap();
        }
        let (from, to) = (now - Duration::hours(1), now + Duration::seconds(1));
        let s = summary(&db, &hub, &tracker, &b, from, to).await.unwrap();
        assert_eq!(
            s.peak,
            Some(Peak {
                value: 2,
                at: parse_ts(&crate::db::fmt_ts(&(t0 + Duration::seconds(5))))
            })
        );
        assert_eq!(s.sessions_total, 2);
        assert!(
            (s.avg_watch_secs - 30.0).abs() < 0.01,
            "{}",
            s.avg_watch_secs
        );
        assert_eq!(s.live_since, None);
        assert_eq!(s.bytes_in, 0);
        assert_eq!(s.current.total(), 0);

        // Live: live_since, bytes from the live counter, current counts.
        let stream = hub
            .publish_start("sum", PublishInfo::new(b.id.clone()))
            .unwrap();
        stream.push(crate::hub::MediaPacket::audio(
            0,
            bytes::Bytes::from_static(&[0xAF, 1, 1, 2, 3]),
        ));
        let ip = "10.0.0.9".parse().unwrap();
        let _v1 = tracker.open("sum", Protocol::Flv, ip, "a");
        let _v2 = tracker.open("sum", Protocol::Flv, ip, "b");
        let _v3 = tracker.open("sum", Protocol::WsFlv, ip, "c");
        let s = summary(&db, &hub, &tracker, &b, from, to).await.unwrap();
        assert_eq!(s.live_since, Some(stream.started_at));
        assert_eq!(s.bytes_in, 5);
        assert_eq!(s.current.total(), 3);
        assert_eq!(s.peak.map(|p| p.value), Some(3));
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["current"]["total"], 3);
        assert!(v["peak"]["at"].is_string());
    }
}
