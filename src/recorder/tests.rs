//! Recorder state-transition tests (in-memory DB, temporary data dir, no ffmpeg).

use std::time::Duration;

use bytes::Bytes;

use super::*;
use crate::db::broadcasts::{self, BroadcastPatch, NewBroadcast};
use crate::hub::PublishInfo;

struct Env {
    dir: PathBuf,
    db: Db,
    hub: Arc<Hub>,
    mgr: Arc<RecorderManager>,
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn env() -> Env {
    let dir = std::env::temp_dir().join(format!("rms-rec-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut config = Config::default();
    config.server.data_dir = dir.clone();
    config.ffmpeg.path = "/nonexistent/ffmpeg".into();
    let db = Db::open_in_memory().unwrap();
    let hub = Hub::new();
    let mgr = RecorderManager::new(Arc::new(config), db.clone(), hub.clone());
    assert!(mgr.shared.ffmpeg.is_none());
    Env { dir, db, hub, mgr }
}

fn video_header() -> MediaPacket {
    MediaPacket::video(
        0,
        Bytes::from_static(&[0x17, 0x00, 0, 0, 0, 1, 0x64, 0, 0x1f]),
    )
}
fn audio_header() -> MediaPacket {
    MediaPacket::audio(0, Bytes::from_static(&[0xAF, 0x00, 0x11, 0x90]))
}
fn video(ts: u32, key: bool) -> MediaPacket {
    MediaPacket::video(
        ts,
        Bytes::from(vec![if key { 0x17 } else { 0x27 }, 1, 0, 0, 0, 7, 7]),
    )
}
fn audio(ts: u32) -> MediaPacket {
    MediaPacket::audio(ts, Bytes::from_static(&[0xAF, 0x01, 0x21, 0x10]))
}

/// Push `secs` seconds of 30 fps video (keyframe every second) + audio from `start`.
fn feed(s: &LiveStream, start: u32, secs: u32) {
    for i in 0..secs * 30 {
        let ts = start + i * 1000 / 30;
        s.push(video(ts, i % 30 == 0));
        s.push(audio(ts + 3));
    }
}

async fn wait_for<F: Fn() -> bool>(what: &str, f: F) {
    for _ in 0..200 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {what}");
}

async fn wait_status(db: &Db, id: &str, status: &str) -> Recording {
    for _ in 0..200 {
        if let Some(r) = repo::get(db, id).await.unwrap()
            && r.status == status
        {
            return r;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("recording {id} never reached {status}");
}

#[test]
fn keyframe_gate() {
    let mut g = KeyframeGate::new(false);
    // Unknown stream type: audio passes, video waits for a keyframe.
    assert!(g.accept(&audio(0)));
    assert!(!g.accept(&video(0, false)));
    assert!(g.accept(&video_header()));
    // Video now known: audio waits for the first keyframe too.
    assert!(!g.accept(&audio(1)));
    assert!(g.accept(&video(2, true)));
    assert!(g.accept(&audio(3)));
    assert!(g.accept(&video(4, false)));
    g.lagged();
    assert!(!g.accept(&video(5, false)));
    assert!(g.accept(&audio(5)));
    assert!(g.accept(&audio_header()));
    assert!(g.accept(&video(6, true)));
    assert!(g.accept(&video(7, false)));
}

#[test]
fn path_helpers() {
    assert_eq!(with_extension("a/2026.flv", "mp4"), "a/2026.mp4");
    assert_eq!(with_extension("a/2026.flv", "mp4.tmp"), "a/2026.mp4.tmp");
    assert_eq!(name_of("show/x.flv"), "show");
    let e = env();
    assert!(e.mgr.shared.abs("show/x.flv").is_some());
    assert!(e.mgr.shared.abs("../x.flv").is_none());
    assert!(e.mgr.shared.abs("/etc/passwd").is_none());
    assert!(e.mgr.shared.abs("").is_none());
}

#[tokio::test]
async fn manual_start_stop_lifecycle() {
    let e = env();
    let b = broadcasts::create(&e.db, NewBroadcast::new("show", "Show"))
        .await
        .unwrap();
    assert!(matches!(
        e.mgr.start_manual("show").await,
        Err(RecorderError::NotLive)
    ));
    assert!(matches!(
        e.mgr.stop_manual("show").await,
        Err(RecorderError::NotRecording)
    ));

    let s = e
        .hub
        .publish_start("show", PublishInfo::new(b.id.clone()))
        .unwrap();
    s.push(MediaPacket::script(
        0,
        Bytes::from_static(b"\x02\x00\x0aonMetaData"),
    ));
    s.push(video_header());
    s.push(audio_header());
    // Two minutes into the stream: the GOP snapshot starts at 120 s.
    feed(&s, 120_000, 1);

    let rec = e.mgr.start_manual("show").await.unwrap();
    assert_eq!(rec.status, STATUS_RECORDING);
    assert_eq!(rec.format, FORMAT_FLV);
    assert_eq!(rec.broadcast_id, b.id);
    assert!(
        rec.path.starts_with("show/") && rec.path.ends_with(".flv"),
        "{}",
        rec.path
    );
    assert_eq!(
        repo::get(&e.db, &rec.id).await.unwrap().unwrap().status,
        STATUS_RECORDING
    );
    assert_eq!(e.mgr.current("show").map(|r| r.id), Some(rec.id.clone()));
    assert!(matches!(
        e.mgr.start_manual("show").await,
        Err(RecorderError::AlreadyRecording)
    ));
    assert!(matches!(
        e.mgr.delete(&rec.id).await,
        Err(RecorderError::Busy)
    ));

    feed(&s, 121_000, 2);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let stopped = e.mgr.stop_manual("show").await.unwrap();
    // No ffmpeg: straight to ready, FLV kept.
    assert_eq!(stopped.status, STATUS_READY);
    assert_eq!(stopped.format, FORMAT_FLV);
    assert!(stopped.ended_at.is_some());
    assert!(e.mgr.current("show").is_none());
    assert!(matches!(
        e.mgr.stop_manual("show").await,
        Err(RecorderError::NotRecording)
    ));

    let row = repo::get(&e.db, &rec.id).await.unwrap().unwrap();
    assert_eq!(row, stopped);
    let file = e.mgr.file_path(&row).unwrap();
    let scan = flv_writer::scan_flv(&file).unwrap();
    assert_eq!(scan.file_len, row.size_bytes);
    assert_eq!(scan.media_tags, 3 * 30 * 2);
    // Rebased: 3 s of media starting at 0.
    assert!(
        (2900..=3010).contains(&scan.duration_ms),
        "{}",
        scan.duration_ms
    );
    assert!((row.duration_secs.unwrap() - scan.duration_ms as f64 / 1000.0).abs() < 1e-9);

    // A second manual recording in the same live period gets a new file.
    let rec2 = e.mgr.start_manual("show").await.unwrap();
    assert_ne!(rec2.path, rec.path);
    feed(&s, 124_000, 1);
    tokio::time::sleep(Duration::from_millis(50)).await;
    // Stream end finalizes it.
    e.hub.publish_end("show");
    let done = wait_status(&e.db, &rec2.id, STATUS_READY).await;
    assert!(done.size_bytes > 0);
    assert!(e.mgr.current("show").is_none());
    let list = repo::list(&e.db, Some(&b.id)).await.unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].id, rec2.id, "newest first");

    // Delete: row and file.
    let deleted = e.mgr.delete(&rec.id).await.unwrap();
    assert_eq!(deleted.id, rec.id);
    assert!(!file.exists());
    assert!(repo::get(&e.db, &rec.id).await.unwrap().is_none());
    assert!(matches!(
        e.mgr.delete(&rec.id).await,
        Err(RecorderError::NotFound)
    ));
}

#[tokio::test]
async fn auto_record_and_manual_stop_is_not_undone() {
    let e = env();
    let b = broadcasts::create(&e.db, NewBroadcast::new("auto", "Auto"))
        .await
        .unwrap();
    broadcasts::update(
        &e.db,
        "auto",
        BroadcastPatch {
            record: Some(true),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let mut events = e.hub.subscribe_events();
    e.mgr.spawn();
    tokio::task::yield_now().await;

    let s = e
        .hub
        .publish_start("auto", PublishInfo::new(b.id.clone()))
        .unwrap();
    wait_for("auto recording", || e.mgr.current("auto").is_some()).await;
    let rec = e.mgr.current("auto").unwrap();
    s.push(video_header());
    s.push(audio_header());
    feed(&s, 0, 2);
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Manual stop ends the file; a lagged-event rescan must not restart it.
    let stopped = e.mgr.stop_manual("auto").await.unwrap();
    assert_eq!(stopped.status, STATUS_READY);
    e.mgr.shared.check_live_streams();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(e.mgr.current("auto").is_none());

    // Next live period records again.
    e.hub.publish_end("auto");
    let s2 = e
        .hub
        .publish_start("auto", PublishInfo::new(b.id.clone()))
        .unwrap();
    wait_for("second auto recording", || e.mgr.current("auto").is_some()).await;
    let rec2 = e.mgr.current("auto").unwrap();
    assert_ne!(rec2.id, rec.id);
    // Nothing pushed: an empty recording is marked failed and its file removed.
    e.hub.publish_end_stream(&s2);
    let failed = wait_status(&e.db, &rec2.id, STATUS_FAILED).await;
    assert_eq!(failed.error.as_deref(), Some("no media received"));
    assert!(!e.mgr.file_path(&failed).unwrap().exists());

    // BroadcastUpdated was emitted for the recording changes.
    let mut updates = 0;
    while let Ok(ev) = events.try_recv() {
        if ev
            == (HubEvent::BroadcastUpdated {
                name: "auto".into(),
            })
        {
            updates += 1;
        }
    }
    assert!(updates >= 4, "updates = {updates}");
}

#[tokio::test]
async fn record_flag_off_does_not_record() {
    let e = env();
    let b = broadcasts::create(&e.db, NewBroadcast::new("off", "Off"))
        .await
        .unwrap();
    e.mgr.spawn();
    tokio::task::yield_now().await;
    let _s = e
        .hub
        .publish_start("off", PublishInfo::new(b.id.clone()))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(e.mgr.current("off").is_none());
    assert!(repo::list(&e.db, None).await.unwrap().is_empty());
}

#[tokio::test]
async fn crash_recovery_finalizes_dangling_rows() {
    let e = env();
    let dir = e.dir.join("recordings").join("show");
    std::fs::create_dir_all(&dir).unwrap();
    // A recording interrupted mid-tag.
    let flv = dir.join("20261008-100000.flv");
    let mut w = FlvWriter::create(&flv, true, true).await.unwrap();
    w.write(&video_header()).await.unwrap();
    for i in 0..60u32 {
        w.write(&video(i * 33, i % 30 == 0)).await.unwrap();
    }
    let good_len = w.finish().await.unwrap().bytes;
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(&flv).unwrap();
        f.write_all(&[9, 0, 1, 0]).unwrap(); // partial tag header
    }
    let base = |id: &str, path: &str, status: &str| Recording {
        id: id.into(),
        broadcast_id: "b".into(),
        stream_session_id: None,
        path: path.into(),
        format: FORMAT_FLV.into(),
        status: status.into(),
        started_at: now_ms(),
        ended_at: None,
        size_bytes: 0,
        duration_secs: None,
        error: None,
    };
    repo::insert(
        &e.db,
        &base("r1", "show/20261008-100000.flv", STATUS_RECORDING),
    )
    .await
    .unwrap();
    repo::insert(&e.db, &base("r2", "show/missing.flv", STATUS_PROCESSING))
        .await
        .unwrap();
    repo::insert(&e.db, &base("r3", "show/done.flv", STATUS_READY))
        .await
        .unwrap();

    Arc::clone(&e.mgr.shared).recover().await;

    let r1 = repo::get(&e.db, "r1").await.unwrap().unwrap();
    assert_eq!(r1.status, STATUS_READY);
    assert_eq!(r1.size_bytes, good_len);
    assert_eq!(
        std::fs::metadata(&flv).unwrap().len(),
        good_len,
        "partial tag truncated"
    );
    assert!((r1.duration_secs.unwrap() - 59.0 * 0.033).abs() < 1e-9);
    assert!(r1.ended_at.is_some());
    let r2 = repo::get(&e.db, "r2").await.unwrap().unwrap();
    assert_eq!(r2.status, STATUS_FAILED);
    assert!(r2.error.unwrap().contains("missing"));
    assert_eq!(
        repo::get(&e.db, "r3").await.unwrap().unwrap().status,
        STATUS_READY
    );
    assert!(e.mgr.shared.lock().busy.is_empty());
}

#[tokio::test]
async fn shutdown_closes_active_recordings() {
    let e = env();
    let b = broadcasts::create(&e.db, NewBroadcast::new("sd", "Sd"))
        .await
        .unwrap();
    let s = e
        .hub
        .publish_start("sd", PublishInfo::new(b.id.clone()))
        .unwrap();
    s.push(video_header());
    let rec = e.mgr.start_manual("sd").await.unwrap();
    feed(&s, 0, 1);
    tokio::time::sleep(Duration::from_millis(50)).await;
    e.mgr.shutdown().await;
    let row = repo::get(&e.db, &rec.id).await.unwrap().unwrap();
    assert_eq!(row.status, STATUS_READY);
    assert!(e.mgr.current("sd").is_none());
}
