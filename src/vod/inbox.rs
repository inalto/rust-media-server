//! VOD inbox: a watched folder (`vod.inbox_dir`, default `data/vod/inbox`).
//!
//! Every `vod.scan_interval_secs` the folder and its sub-folders named after a ladder
//! (`inbox/mobile/` → ladder `mobile`; one level deep) are listed. A file is imported once
//! its size and modification time were unchanged for two consecutive scans; hidden and
//! partial files (`.name`, `*.part`, `*.tmp`, `*.crdownload`, `*.filepart`) are ignored,
//! as are sub-folders that are not a ladder (one warning each) and `_failed/`. The file
//! is moved (rename, or copy + delete across filesystems) to `data/vod/<id>/source.<ext>`
//! with the title taken from its name (`_` → space, no extension). Files that cannot be
//! imported (unsupported type, ffprobe error, no video stream) are moved to
//! `inbox/_failed/` with a `<name>.error.txt` next to them.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio_util::sync::CancellationToken;

use super::{ImportRequest, VodManager, ffmpeg, title_from_file_name, vod_extension};
use crate::db::vod::ORIGIN_INBOX;

/// Sub-folder for files that could not be imported.
pub const FAILED_DIR: &str = "_failed";
/// Unchanged scans needed before a file is imported.
pub const STABLE_SCANS: u32 = 2;

/// Hidden or partial (still being written) file names.
pub fn is_ignored_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    name.starts_with('.')
        || [".part", ".tmp", ".crdownload", ".filepart", ".error.txt"]
            .iter()
            .any(|s| lower.ends_with(s))
}

/// A file ready to be imported.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub path: PathBuf,
    /// Ladder of the sub-folder (`None` = inbox root → default ladder).
    pub ladder: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Observation {
    size: u64,
    mtime: SystemTime,
    stable: u32,
}

/// Stability tracking across scans.
pub struct InboxScanner {
    root: PathBuf,
    ladders: Vec<String>,
    seen: HashMap<PathBuf, Observation>,
    /// Sub-folders already warned about.
    warned: HashSet<String>,
    /// Files whose import failed and that could not be moved away: not retried while
    /// unchanged.
    stuck: HashMap<PathBuf, (u64, SystemTime)>,
}

impl InboxScanner {
    pub fn new(root: PathBuf, ladders: Vec<String>) -> Self {
        Self {
            root,
            ladders,
            seen: HashMap::new(),
            warned: HashSet::new(),
            stuck: HashMap::new(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// List the inbox (blocking I/O) and return the files stable for
    /// [`STABLE_SCANS`] consecutive scans. Returned files are forgotten (they are about
    /// to be moved away).
    pub fn scan(&mut self) -> Vec<Candidate> {
        let mut files: Vec<(PathBuf, Option<String>, u64, SystemTime)> = Vec::new();
        self.list_dir(&self.root.clone(), None, &mut files);
        let present: HashSet<PathBuf> = files.iter().map(|f| f.0.clone()).collect();
        self.seen.retain(|p, _| present.contains(p));
        self.stuck.retain(|p, _| present.contains(p));
        let mut ready = Vec::new();
        for (path, ladder, size, mtime) in files {
            if self.stuck.get(&path) == Some(&(size, mtime)) {
                continue;
            }
            let fresh = Observation {
                size,
                mtime,
                stable: 0,
            };
            match self.seen.get_mut(&path) {
                // Unchanged since the previous scan.
                Some(obs) if obs.size == size && obs.mtime == mtime => {
                    obs.stable += 1;
                    if obs.stable >= STABLE_SCANS && size > 0 {
                        self.seen.remove(&path);
                        ready.push(Candidate { path, ladder });
                    }
                }
                // Changed (still being written): start over.
                Some(obs) => *obs = fresh,
                None => {
                    self.seen.insert(path, fresh);
                }
            }
        }
        ready
    }

    /// Remember a file that failed and could not be moved to `_failed/`.
    pub fn mark_stuck(&mut self, path: &Path) {
        if let Ok(m) = std::fs::metadata(path)
            && let Ok(t) = m.modified()
        {
            self.stuck.insert(path.to_path_buf(), (m.len(), t));
        }
    }

    fn list_dir(
        &mut self,
        dir: &Path,
        ladder: Option<&str>,
        out: &mut Vec<(PathBuf, Option<String>, u64, SystemTime)>,
    ) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            let path = entry.path();
            if meta.is_dir() {
                if ladder.is_some() || name == FAILED_DIR {
                    continue; // one level only
                }
                if self.ladders.contains(&name) {
                    self.list_dir(&path, Some(&name), out);
                } else if self.warned.insert(name.clone()) {
                    tracing::warn!(folder = %path.display(), "VOD inbox: sub-folder is not a ladder name; ignored");
                }
                continue;
            }
            if !meta.is_file() || is_ignored_name(&name) {
                continue;
            }
            let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            out.push((path, ladder.map(str::to_string), meta.len(), mtime));
        }
    }
}

/// Move `src` to `dst`: rename, or copy + delete when they are on different filesystems.
pub async fn move_file(src: &Path, dst: &Path) -> std::io::Result<()> {
    match tokio::fs::rename(src, dst).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::CrossesDevices => {
            if let Err(e) = tokio::fs::copy(src, dst).await {
                let _ = tokio::fs::remove_file(dst).await;
                return Err(e);
            }
            tokio::fs::remove_file(src).await
        }
        Err(e) => Err(e),
    }
}

/// A free path `dir/name` (`name-1.ext`, `name-2.ext`, … when taken).
fn free_path(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (name.to_string(), String::new()),
    };
    (1..10_000)
        .map(|i| dir.join(format!("{stem}-{i}{ext}")))
        .find(|p| !p.exists())
        .unwrap_or(first)
}

/// Move a file that could not be imported to `inbox/_failed/` with `<name>.error.txt`.
pub async fn move_to_failed(root: &Path, path: &Path, error: &str) -> std::io::Result<PathBuf> {
    let dir = root.join(FAILED_DIR);
    tokio::fs::create_dir_all(&dir).await?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    let dest = free_path(&dir, &name);
    move_file(path, &dest).await?;
    let note = dest.with_file_name(format!(
        "{}.error.txt",
        dest.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    ));
    let text = format!(
        "{}\n\nfile: {}\nwhen: {}\n",
        error,
        path.display(),
        crate::db::now_ts()
    );
    tokio::fs::write(&note, text).await?;
    Ok(dest)
}

/// Import one stable inbox file.
async fn import(mgr: &VodManager, c: &Candidate) -> Result<(), String> {
    let name = c
        .path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = vod_extension(Some(&name), None).ok_or_else(|| {
        format!(
            "unsupported file type: accepted extensions are {}",
            super::VOD_EXTENSIONS.join(", ")
        )
    })?;
    let ffprobe = mgr
        .ffprobe()
        .ok_or_else(|| "ffprobe not available".to_string())?;
    let probe = ffmpeg::probe(ffprobe, &c.path).await?;
    if probe.video.is_none() {
        return Err("the file has no video stream".into());
    }
    let ladder = c
        .ladder
        .clone()
        .unwrap_or_else(|| mgr.config.vod.default_ladder.clone());
    mgr.import(
        &c.path,
        ImportRequest {
            title: title_from_file_name(&name),
            original_name: name,
            ext: ext.to_string(),
            description: String::new(),
            ladder,
            origin: ORIGIN_INBOX.to_string(),
            probe,
        },
    )
    .await
    .map(|_| ())
}

/// Create the inbox and one sub-folder per ladder (best effort).
async fn prepare(root: &Path, ladders: &[String]) {
    for dir in std::iter::once(root.to_path_buf()).chain(ladders.iter().map(|l| root.join(l))) {
        if let Err(e) = tokio::fs::create_dir_all(&dir).await {
            tracing::warn!(dir = %dir.display(), error = %e, "cannot create the VOD inbox folder");
        }
    }
}

/// The inbox task.
pub async fn run(mgr: Arc<VodManager>, shutdown: CancellationToken) {
    let root = mgr.config.vod_inbox_dir();
    let ladders: Vec<String> = mgr
        .config
        .vod
        .ladders
        .iter()
        .map(|l| l.name.clone())
        .collect();
    prepare(&root, &ladders).await;
    tracing::info!(task = "vod-inbox", dir = %root.display(), "watching the VOD inbox");
    let interval = Duration::from_secs(mgr.config.vod.scan_interval_secs.max(1));
    let mut scanner = InboxScanner::new(root.clone(), ladders);
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = tokio::time::sleep(interval) => {}
        }
        let (s, ready) = match tokio::task::spawn_blocking(move || {
            let ready = scanner.scan();
            (scanner, ready)
        })
        .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(task = "vod-inbox", error = %e, "scan task failed");
                scanner = InboxScanner::new(
                    root.clone(),
                    mgr.config
                        .vod
                        .ladders
                        .iter()
                        .map(|l| l.name.clone())
                        .collect(),
                );
                continue;
            }
        };
        scanner = s;
        for c in ready {
            if shutdown.is_cancelled() {
                break;
            }
            tracing::info!(file = %c.path.display(), ladder = ?c.ladder, "VOD inbox: importing");
            if let Err(err) = import(&mgr, &c).await {
                tracing::warn!(file = %c.path.display(), error = %err, "VOD inbox: import failed");
                match move_to_failed(&root, &c.path, &err).await {
                    Ok(dest) => {
                        tracing::info!(file = %dest.display(), "VOD inbox: moved to _failed")
                    }
                    Err(e) => {
                        tracing::warn!(file = %c.path.display(), error = %e, "VOD inbox: cannot move to _failed; ignoring the file until it changes");
                        scanner.mark_stuck(&c.path);
                    }
                }
            }
        }
    }
    tracing::info!(task = "vod-inbox", "stopped");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("rms-vod-inbox-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn ignored_names() {
        for n in [
            ".hidden.mp4",
            "a.mp4.part",
            "b.TMP",
            "c.crdownload",
            "d.filepart",
            "x.mp4.error.txt",
        ] {
            assert!(is_ignored_name(n), "{n}");
        }
        for n in ["clip.mp4", "part.mov", "tmp.mkv"] {
            assert!(!is_ignored_name(n), "{n}");
        }
    }

    #[test]
    fn stability_detection() {
        let root = tempdir();
        std::fs::create_dir_all(root.join("mobile")).unwrap();
        std::fs::create_dir_all(root.join("unknown")).unwrap();
        std::fs::create_dir_all(root.join(FAILED_DIR)).unwrap();
        std::fs::create_dir_all(root.join("mobile").join("deeper")).unwrap();
        std::fs::write(root.join("a.mp4"), b"aaaa").unwrap();
        std::fs::write(root.join("mobile").join("b.mov"), b"bbbb").unwrap();
        std::fs::write(root.join("unknown").join("c.mp4"), b"cccc").unwrap();
        std::fs::write(root.join(FAILED_DIR).join("d.mp4"), b"dddd").unwrap();
        std::fs::write(root.join("mobile").join("deeper").join("e.mp4"), b"eeee").unwrap();
        std::fs::write(root.join("f.mp4.part"), b"ffff").unwrap();
        std::fs::write(root.join(".g.mp4"), b"gggg").unwrap();
        std::fs::write(root.join("empty.mp4"), b"").unwrap();
        let mut s = InboxScanner::new(root.clone(), vec!["standard".into(), "mobile".into()]);
        // Scan 1: first observation; scan 2: unchanged once; scan 3: unchanged twice.
        assert!(s.scan().is_empty());
        assert!(s.scan().is_empty());
        // A file growing between scans is not picked up.
        std::fs::write(root.join("a.mp4"), b"aaaaaaaa").unwrap();
        let mut got = s.scan();
        got.sort_by(|x, y| x.path.cmp(&y.path));
        assert_eq!(
            got,
            [Candidate {
                path: root.join("mobile").join("b.mov"),
                ladder: Some("mobile".into())
            }]
        );
        std::fs::remove_file(root.join("mobile").join("b.mov")).unwrap(); // imported (moved away)
        assert!(
            s.scan().is_empty(),
            "a.mp4 changed: two more unchanged scans needed"
        );
        let got = s.scan();
        assert_eq!(
            got,
            [Candidate {
                path: root.join("a.mp4"),
                ladder: None
            }]
        );
        // Picked files are forgotten: if a file stays (import pending), it starts over.
        assert!(s.scan().is_empty());
        assert!(s.warned.contains("unknown"));
        // A stuck file is skipped until it changes.
        s.mark_stuck(&root.join("a.mp4"));
        for _ in 0..4 {
            assert!(s.scan().iter().all(|c| c.path != root.join("a.mp4")));
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn failed_files_and_moves() {
        let root = tempdir();
        let f = root.join("bad clip.mp4");
        std::fs::write(&f, b"x").unwrap();
        let dest = move_to_failed(&root, &f, "the file has no video stream")
            .await
            .unwrap();
        assert_eq!(dest, root.join(FAILED_DIR).join("bad clip.mp4"));
        assert!(!f.exists() && dest.exists());
        let note =
            std::fs::read_to_string(root.join(FAILED_DIR).join("bad clip.mp4.error.txt")).unwrap();
        assert!(note.starts_with("the file has no video stream"));
        // Same name again: a free name is chosen.
        std::fs::write(&f, b"y").unwrap();
        let dest2 = move_to_failed(&root, &f, "again").await.unwrap();
        assert_eq!(dest2, root.join(FAILED_DIR).join("bad clip-1.mp4"));
        assert!(
            root.join(FAILED_DIR)
                .join("bad clip-1.mp4.error.txt")
                .exists()
        );
        // Plain move.
        let src = root.join("m.mp4");
        std::fs::write(&src, b"m").unwrap();
        move_file(&src, &root.join("n.mp4")).await.unwrap();
        assert!(!src.exists() && root.join("n.mp4").exists());
        std::fs::remove_dir_all(root).unwrap();
    }
}
