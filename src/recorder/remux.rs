//! ffmpeg/ffprobe helpers for recordings: FLV → MP4 stream-copy remux and duration probe.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::process::Command;

/// Maximum time allowed for one remux.
pub const REMUX_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// Maximum time allowed for one ffprobe call.
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);
/// Keep at most this many bytes of ffmpeg's stderr in error messages.
const STDERR_TAIL: usize = 1000;

/// `ffprobe` next to the given `ffmpeg` executable, else from `PATH`.
pub fn ffprobe_for(ffmpeg: &Path) -> Option<PathBuf> {
    let name = if cfg!(windows) {
        "ffprobe.exe"
    } else {
        "ffprobe"
    };
    ffmpeg
        .parent()
        .map(|dir| dir.join(name))
        .filter(|p| p.is_file())
        .or_else(|| crate::config::find_in_path("ffprobe"))
}

/// Last `max` bytes of `s` (on a char boundary), trimmed.
pub fn tail(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.len() <= max {
        return s.to_string();
    }
    let mut start = s.len() - max;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    format!("…{}", s[start..].trim_start())
}

/// Temporary output name used while ffmpeg writes (`<out>.tmp`); renamed on success.
pub fn temp_output(out: &Path) -> PathBuf {
    let mut s = out.as_os_str().to_owned();
    s.push(".tmp");
    PathBuf::from(s)
}

/// `ffmpeg -y -v error -i <input> -c copy -movflags +faststart -f mp4 <output>`
/// with a timeout. The MP4 is written to `<output>.tmp` and renamed when complete, so
/// a partial file is never visible under the final name. On failure, returns the tail
/// of ffmpeg's stderr (or a description of what went wrong).
pub async fn remux_to_mp4(
    ffmpeg: &Path,
    input: &Path,
    output: &Path,
    timeout: Duration,
) -> Result<(), String> {
    let tmp = temp_output(output);
    let mut child = Command::new(ffmpeg)
        .arg("-nostdin")
        .arg("-hide_banner")
        .args(["-y", "-v", "error", "-i"])
        .arg(input)
        .args(["-c", "copy", "-movflags", "+faststart", "-f", "mp4"])
        .arg(&tmp)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("cannot run ffmpeg: {e}"))?;
    let mut stderr = child.stderr.take();
    let read_err = async {
        let mut buf = Vec::new();
        if let Some(s) = stderr.as_mut() {
            // Bounded by ffmpeg's -v error output; never huge.
            let _ = s.take(1 << 20).read_to_end(&mut buf).await;
        }
        buf
    };
    let result = tokio::time::timeout(timeout, async {
        let (status, err) = tokio::join!(child.wait(), read_err);
        (status, err)
    })
    .await;
    let fail = |msg: String| {
        let _ = std::fs::remove_file(&tmp);
        Err(msg)
    };
    match result {
        Err(_) => {
            let _ = child.kill().await;
            fail(format!(
                "ffmpeg remux timed out after {} s",
                timeout.as_secs()
            ))
        }
        Ok((Err(e), _)) => fail(format!("ffmpeg failed: {e}")),
        Ok((Ok(status), err)) => {
            let err = String::from_utf8_lossy(&err);
            if !status.success() {
                let detail = tail(&err, STDERR_TAIL);
                return fail(if detail.is_empty() {
                    format!("ffmpeg exited with {status}")
                } else {
                    detail
                });
            }
            match tokio::fs::metadata(&tmp).await {
                Ok(m) if m.len() > 0 => {}
                _ => return fail("ffmpeg produced no output".into()),
            }
            tokio::fs::rename(&tmp, output)
                .await
                .or_else(|e| fail(format!("cannot rename remuxed file: {e}")))
        }
    }
}

/// Container duration in seconds according to ffprobe (`None` on any failure).
pub async fn probe_duration(ffprobe: &Path, file: &Path) -> Option<f64> {
    let out = tokio::time::timeout(
        PROBE_TIMEOUT,
        Command::new(ffprobe)
            .args([
                "-v",
                "error",
                "-show_entries",
                "format=duration",
                "-of",
                "default=noprint_wrappers=1:nokey=1",
            ])
            .arg(file)
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|d| d.is_finite() && *d >= 0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_and_temp_names() {
        assert_eq!(tail("  short  ", 10), "short");
        assert_eq!(tail("0123456789abc", 3), "…abc");
        // Multi-byte boundary.
        assert_eq!(tail("ààà", 3), "…à");
        assert_eq!(
            temp_output(Path::new("/x/a.mp4")),
            PathBuf::from("/x/a.mp4.tmp")
        );
    }

    #[tokio::test]
    async fn missing_ffmpeg_is_an_error() {
        let r = remux_to_mp4(
            Path::new("/nonexistent/ffmpeg"),
            Path::new("/tmp/in.flv"),
            Path::new("/tmp/rms-never.mp4"),
            Duration::from_secs(5),
        )
        .await;
        assert!(r.unwrap_err().contains("cannot run ffmpeg"));
        assert!(
            probe_duration(Path::new("/nonexistent/ffprobe"), Path::new("/x"))
                .await
                .is_none()
        );
    }
}
