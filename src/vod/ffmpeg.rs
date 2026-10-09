//! ffprobe / ffmpeg for video on demand: source probing, rendition selection (never
//! upscale, even dimensions), the single-pass multi-rendition command line (CMAF segments
//! packaged by ffmpeg's `dash` muxer, which also writes the HLS playlists), progress
//! parsing and the process runner (timeout, cancellation, `kill_on_drop`, stderr tail).

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::config::VodRendition;

/// Maximum time for one ffprobe call.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(60);
/// Maximum time for the poster extraction.
pub const POSTER_TIMEOUT: Duration = Duration::from_secs(120);
/// Poster width limit (px).
pub const POSTER_MAX_WIDTH: u32 = 1280;
/// Constant-quality value used for `video_kbps = 0` renditions.
pub const CRF: u32 = 23;
/// Bytes of ffmpeg's stderr kept for error messages.
const STDERR_TAIL: usize = 2000;
/// Floor of the scaled bitrate of a fallback rendition (source smaller than every rung).
const MIN_FALLBACK_KBPS: u32 = 150;

// ---------------------------------------------------------------------------
// Probe
// ---------------------------------------------------------------------------

/// What the job needs to know about a source file.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceInfo {
    pub duration_secs: Option<f64>,
    pub size: Option<u64>,
    pub format_name: Option<String>,
    pub video: Option<VideoInfo>,
    pub audio: Option<AudioInfo>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct VideoInfo {
    /// Absolute stream index in the input (`-map 0:<index>`).
    pub index: u32,
    pub codec: String,
    /// Display size: sample aspect ratio applied and rotation (phones) taken into account,
    /// i.e. the size of the frames ffmpeg's filter graph receives (autorotate) once the
    /// pixels are made square.
    pub width: u32,
    pub height: u32,
    pub fps: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AudioInfo {
    pub index: u32,
    pub codec: String,
    pub channels: u32,
}

fn parse_ratio(s: Option<&str>, sep: char) -> Option<f64> {
    let (n, d) = s?.split_once(sep)?;
    let (n, d) = (n.trim().parse::<f64>().ok()?, d.trim().parse::<f64>().ok()?);
    (n > 0.0 && d > 0.0).then(|| n / d)
}

fn num(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Rotation in degrees from the display matrix side data or the legacy `rotate` tag.
fn rotation(stream: &Value) -> i64 {
    let side = stream["side_data_list"]
        .as_array()
        .into_iter()
        .flatten()
        .find_map(|sd| num(&sd["rotation"]));
    let tag = num(&stream["tags"]["rotate"]);
    side.or(tag).map(|r| r.round() as i64).unwrap_or(0)
}

/// Parse `ffprobe -show_streams -show_format -of json` output.
pub fn parse_probe(json: &str) -> Result<SourceInfo, String> {
    let v: Value =
        serde_json::from_str(json).map_err(|e| format!("invalid ffprobe output: {e}"))?;
    let streams = v["streams"].as_array().cloned().unwrap_or_default();
    let index = |s: &Value| s["index"].as_u64().map(|i| i as u32);
    let video = streams
        .iter()
        .filter(|s| s["codec_type"] == "video")
        .filter(|s| s["disposition"]["attached_pic"].as_u64().unwrap_or(0) == 0)
        .find_map(|s| {
            let (w, h) = (s["width"].as_u64()?, s["height"].as_u64()?);
            if w == 0 || h == 0 {
                return None;
            }
            let sar = parse_ratio(s["sample_aspect_ratio"].as_str(), ':').unwrap_or(1.0);
            let mut dw = ((w as f64) * sar).round().max(1.0) as u32;
            let mut dh = h as u32;
            if rotation(s).rem_euclid(180) == 90 {
                std::mem::swap(&mut dw, &mut dh);
            }
            let fps = parse_ratio(s["avg_frame_rate"].as_str(), '/')
                .or_else(|| parse_ratio(s["r_frame_rate"].as_str(), '/'))
                .filter(|f| *f > 0.0 && *f <= 1000.0);
            Some(VideoInfo {
                index: index(s)?,
                codec: s["codec_name"].as_str().unwrap_or("unknown").to_string(),
                width: dw,
                height: dh,
                fps,
            })
        });
    let audio = streams
        .iter()
        .filter(|s| s["codec_type"] == "audio")
        .find_map(|s| {
            let channels = s["channels"].as_u64().unwrap_or(0) as u32;
            (channels > 0).then(|| AudioInfo {
                index: index(s).unwrap_or(0),
                codec: s["codec_name"].as_str().unwrap_or("unknown").to_string(),
                channels,
            })
        });
    let duration_secs = num(&v["format"]["duration"])
        .or_else(|| {
            streams
                .iter()
                .find(|s| s["codec_type"] == "video")
                .and_then(|s| num(&s["duration"]))
        })
        .filter(|d| d.is_finite() && *d > 0.0);
    Ok(SourceInfo {
        duration_secs,
        size: num(&v["format"]["size"]).map(|s| s as u64),
        format_name: v["format"]["format_name"].as_str().map(str::to_string),
        video,
        audio,
    })
}

/// `ffprobe` the file (`-show_streams -show_format`, JSON).
pub async fn probe(ffprobe: &Path, file: &Path) -> Result<SourceInfo, String> {
    let out = tokio::time::timeout(
        PROBE_TIMEOUT,
        Command::new(ffprobe)
            .args([
                "-v",
                "error",
                "-show_streams",
                "-show_format",
                "-of",
                "json",
            ])
            .arg(file)
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| "ffprobe timed out".to_string())?
    .map_err(|e| format!("cannot run ffprobe: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let last = err
            .lines()
            .map(str::trim)
            .rfind(|l| !l.is_empty())
            .unwrap_or("");
        return Err(if last.is_empty() {
            "ffprobe cannot read the file".to_string()
        } else {
            format!("ffprobe cannot read the file: {last}")
        });
    }
    parse_probe(&String::from_utf8_lossy(&out.stdout))
}

// ---------------------------------------------------------------------------
// Rendition selection
// ---------------------------------------------------------------------------

/// A rendition that will be encoded.
#[derive(Debug, Clone, PartialEq)]
pub struct Selected {
    pub name: String,
    pub width: u32,
    pub height: u32,
    /// 0 = constant quality.
    pub video_kbps: u32,
    pub audio_kbps: u32,
}

/// Nearest even value (at least 2).
pub fn even(x: f64) -> u32 {
    (((x / 2.0).round() as i64).max(1) * 2) as u32
}

/// Output size for short side `short`, keeping the aspect ratio of `w`×`h`.
fn size_for(short: u32, w: u32, h: u32) -> (u32, u32) {
    let s = (short & !1).max(2);
    if w >= h {
        (even(s as f64 * w as f64 / h as f64), s)
    } else {
        (s, even(s as f64 * h as f64 / w as f64))
    }
}

/// Renditions of a ladder for a `w`×`h` (display size) source.
///
/// `height` is the short side (landscape height, portrait width); `0` = source size.
/// Rungs larger than the source are skipped (never upscale), duplicates of an already
/// selected size are dropped and dimensions are even. When every rung is larger than the
/// source, one rendition at the source size is kept with the smallest rung's settings
/// (bitrate scaled down with the pixel count, at least 150 kbit/s) and named `<short>p`.
pub fn select_renditions(ladder: &[VodRendition], w: u32, h: u32) -> Vec<Selected> {
    let (w, h) = (w.max(2), h.max(2));
    let short = w.min(h);
    let mut out: Vec<Selected> = Vec::new();
    for r in ladder {
        if r.height != 0 && r.height > short {
            continue;
        }
        let target = if r.height == 0 { short } else { r.height };
        let (ow, oh) = size_for(target, w, h);
        if out.iter().any(|s| s.width == ow && s.height == oh) {
            continue;
        }
        out.push(Selected {
            name: r.name.clone(),
            width: ow,
            height: oh,
            video_kbps: r.video_kbps,
            audio_kbps: r.audio_kbps,
        });
    }
    if out.is_empty()
        && let Some(r) = ladder.last()
    {
        let (ow, oh) = size_for(short, w, h);
        let ratio = (short as f64 / r.height.max(1) as f64).powi(2);
        let kbps = if r.video_kbps == 0 {
            0
        } else {
            ((r.video_kbps as f64 * ratio).round() as u32)
                .clamp(MIN_FALLBACK_KBPS.min(r.video_kbps), r.video_kbps)
        };
        out.push(Selected {
            name: format!("{}p", ow.min(oh)),
            width: ow,
            height: oh,
            video_kbps: kbps,
            audio_kbps: r.audio_kbps,
        });
    }
    out
}

/// Distinct audio bitrates in order of first use, and the audio output used by each
/// rendition (renditions sharing a bitrate share one AAC encode).
pub fn audio_outputs(renditions: &[Selected]) -> (Vec<u32>, Vec<usize>) {
    let mut unique: Vec<u32> = Vec::new();
    let mut map = Vec::with_capacity(renditions.len());
    for r in renditions {
        let i = match unique.iter().position(|k| *k == r.audio_kbps) {
            Some(i) => i,
            None => {
                unique.push(r.audio_kbps);
                unique.len() - 1
            }
        };
        map.push(i);
    }
    (unique, map)
}

/// H.264 profile: `high` from 720p (short side) up, `main` below.
pub fn profile_for(r: &Selected) -> &'static str {
    if r.width.min(r.height) >= 720 {
        "high"
    } else {
        "main"
    }
}

/// H.264 level: 4.1, or the lowest higher level whose frame size and macroblock rate
/// limits fit (1080p50/60, 1440p, 4K).
pub fn level_for(r: &Selected, fps: Option<f64>) -> &'static str {
    // (level, max frame size in macroblocks, max macroblocks per second)
    const LEVELS: [(&str, u64, u64); 6] = [
        ("4.1", 8_192, 245_760),
        ("4.2", 8_704, 522_240),
        ("5.0", 22_080, 589_824),
        ("5.1", 36_864, 983_040),
        ("5.2", 36_864, 2_073_600),
        ("6.0", 139_264, 4_177_920),
    ];
    let mbs = (r.width as u64).div_ceil(16) * (r.height as u64).div_ceil(16);
    let rate = (mbs as f64 * fps.unwrap_or(30.0)).ceil() as u64;
    LEVELS
        .iter()
        .find(|(_, fs, mbps)| mbs <= *fs && rate <= *mbps)
        .map(|(l, _, _)| *l)
        .unwrap_or("6.2")
}

// ---------------------------------------------------------------------------
// Command line
// ---------------------------------------------------------------------------

/// Encoder family (selects pixel format, quality and keyframe options).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderKind {
    Software,
    Nvenc,
    Qsv,
    Vaapi,
}

impl EncoderKind {
    pub fn of(encoder: &str) -> Self {
        if encoder.ends_with("_nvenc") {
            EncoderKind::Nvenc
        } else if encoder.ends_with("_qsv") {
            EncoderKind::Qsv
        } else if encoder.ends_with("_vaapi") {
            EncoderKind::Vaapi
        } else {
            EncoderKind::Software
        }
    }
}

/// Everything needed to build the transcode command.
#[derive(Debug, Clone)]
pub struct TranscodeSpec {
    pub source: PathBuf,
    /// Output directory (the MPD, HLS playlists, init and media segments go there).
    pub out_dir: PathBuf,
    pub video_index: u32,
    pub audio_index: Option<u32>,
    /// Source channels (1 = mono kept mono, otherwise stereo).
    pub channels: u32,
    pub fps: Option<f64>,
    pub renditions: Vec<Selected>,
    pub segment_duration: f64,
    pub encoder: String,
    pub preset: String,
    pub encoder_args: String,
    pub hwaccel_args: String,
}

fn kbps(n: f64) -> String {
    format!("{}k", n.round() as u64)
}

/// The single-pass ffmpeg command line (arguments after the executable).
///
/// One decode, `split` into N scaled branches, N video + M audio encodes (M = distinct
/// audio bitrates), packaged by the `dash` muxer as CMAF: fMP4 init + media segments with
/// a SegmentTemplate MPD (`$Number$`) and, with `-hls_playlist 1`, an HLS master and one
/// media playlist per representation over the same files.
pub fn transcode_args(s: &TranscodeSpec) -> Vec<String> {
    let kind = EncoderKind::of(&s.encoder);
    let n = s.renditions.len();
    let mut a: Vec<String> = Vec::new();
    let mut push = |items: &[&str]| a.extend(items.iter().map(|x| x.to_string()));
    push(&[
        "-nostdin",
        "-y",
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostats",
        "-progress",
        "pipe:1",
    ]);
    a.extend(s.hwaccel_args.split_whitespace().map(str::to_string));
    a.push("-i".into());
    a.push(s.source.to_string_lossy().into_owned());

    // Filter graph: [0:<v>]split=N[s0]..; [si]scale=W:H[vi] (VAAPI: + upload). No
    // `setsar=1`: the scale filter keeps the display aspect ratio exact through the
    // sample aspect ratio (854x480 of a 16:9 source gets SAR 1280:1281), which the dash
    // muxer requires to be identical across an adaptation set.
    let upload = if kind == EncoderKind::Vaapi {
        ",format=nv12,hwupload"
    } else {
        ""
    };
    let input = format!("[0:{}]", s.video_index);
    let graph = if n == 1 {
        let r = &s.renditions[0];
        format!("{input}scale={}:{}{upload}[v0]", r.width, r.height)
    } else {
        let mut g = format!("{input}split={n}");
        for i in 0..n {
            g.push_str(&format!("[s{i}]"));
        }
        for (i, r) in s.renditions.iter().enumerate() {
            g.push_str(&format!(
                ";[s{i}]scale={}:{}{upload}[v{i}]",
                r.width, r.height
            ));
        }
        g
    };
    a.push("-filter_complex".into());
    a.push(graph);
    for i in 0..n {
        a.push("-map".into());
        a.push(format!("[v{i}]"));
    }
    let (audio_kbps, _) = audio_outputs(&s.renditions);
    if let Some(ai) = s.audio_index {
        for _ in &audio_kbps {
            a.push("-map".into());
            a.push(format!("0:{ai}"));
        }
    }

    // Video encoders.
    a.push("-c:v".into());
    a.push(s.encoder.clone());
    if !s.preset.is_empty() {
        a.push("-preset".into());
        a.push(s.preset.clone());
    }
    match kind {
        EncoderKind::Software | EncoderKind::Nvenc => {
            a.extend(["-pix_fmt".to_string(), "yuv420p".to_string()])
        }
        EncoderKind::Qsv => a.extend(["-pix_fmt".to_string(), "nv12".to_string()]),
        EncoderKind::Vaapi => {}
    }
    let gop = ((s.segment_duration * s.fps.unwrap_or(30.0)).round() as u64)
        .max(1)
        .to_string();
    let seg = format!("{}", s.segment_duration);
    a.extend(["-g".into(), gop.clone(), "-keyint_min".into(), gop]);
    if kind == EncoderKind::Software {
        a.extend(["-sc_threshold".to_string(), "0".to_string()]);
    }
    if kind == EncoderKind::Nvenc {
        a.extend(["-forced-idr".to_string(), "1".to_string()]);
    }
    a.push("-force_key_frames".into());
    a.push(format!("expr:gte(t,n_forced*{seg})"));
    for (i, r) in s.renditions.iter().enumerate() {
        if r.video_kbps > 0 {
            let k = r.video_kbps as f64;
            a.extend([format!("-b:v:{i}"), kbps(k)]);
            a.extend([format!("-maxrate:v:{i}"), kbps(k * 1.07)]);
            a.extend([format!("-bufsize:v:{i}"), kbps(k * 1.5)]);
        } else {
            let q = CRF.to_string();
            match kind {
                EncoderKind::Software => a.extend([format!("-crf:v:{i}"), q]),
                EncoderKind::Nvenc => {
                    a.extend([format!("-cq:v:{i}"), q, format!("-b:v:{i}"), "0".into()])
                }
                EncoderKind::Qsv => a.extend([format!("-global_quality:v:{i}"), q]),
                EncoderKind::Vaapi => a.extend([format!("-qp:v:{i}"), q]),
            }
        }
        a.extend([format!("-profile:v:{i}"), profile_for(r).to_string()]);
        a.extend([format!("-level:v:{i}"), level_for(r, s.fps).to_string()]);
    }
    a.extend(s.encoder_args.split_whitespace().map(str::to_string));

    // Audio encoders: AAC-LC 48 kHz, mono kept mono, otherwise stereo.
    if s.audio_index.is_some() {
        let ch = if s.channels == 1 { "1" } else { "2" };
        a.extend(["-c:a", "aac", "-ar", "48000", "-ac", ch].map(str::to_string));
        for (j, k) in audio_kbps.iter().enumerate() {
            a.extend([format!("-b:a:{j}"), format!("{k}k")]);
        }
    }

    // CMAF packaging: DASH (static MPD) + HLS playlists over the same segments.
    let sets = if s.audio_index.is_some() {
        "id=0,streams=v id=1,streams=a"
    } else {
        "id=0,streams=v"
    };
    a.extend(
        [
            "-f",
            "dash",
            "-seg_duration",
            &seg,
            "-use_template",
            "1",
            "-use_timeline",
            "0",
            "-init_seg_name",
            "init-$RepresentationID$.m4s",
            "-media_seg_name",
            "chunk-$RepresentationID$-$Number%05d$.m4s",
            "-adaptation_sets",
            sets,
            "-hls_playlist",
            "1",
            "-hls_master_name",
            "master.m3u8",
        ]
        .map(str::to_string),
    );
    a.push(
        s.out_dir
            .join("manifest.mpd")
            .to_string_lossy()
            .into_owned(),
    );
    a
}

/// Poster size: at most [`POSTER_MAX_WIDTH`] wide, display aspect ratio, even.
pub fn poster_size(w: u32, h: u32) -> (u32, u32) {
    let (w, h) = (w.max(2), h.max(2));
    let pw = even(w.min(POSTER_MAX_WIDTH) as f64);
    (pw, even(pw as f64 * h as f64 / w as f64))
}

/// One JPEG frame at `seek` seconds.
pub fn poster_args(
    source: &Path,
    video_index: u32,
    seek: f64,
    size: (u32, u32),
    out: &Path,
) -> Vec<String> {
    let mut a: Vec<String> = [
        "-nostdin",
        "-y",
        "-hide_banner",
        "-loglevel",
        "error",
        "-ss",
    ]
    .map(str::to_string)
    .to_vec();
    a.push(format!("{seek:.3}"));
    a.push("-i".into());
    a.push(source.to_string_lossy().into_owned());
    a.extend([
        "-map".to_string(),
        format!("0:{video_index}"),
        "-frames:v".into(),
        "1".into(),
    ]);
    a.push("-vf".into());
    a.push(format!("scale={}:{},setsar=1", size.0, size.1));
    a.extend(["-q:v", "3", "-f", "image2", "-update", "1"].map(str::to_string));
    a.push(out.to_string_lossy().into_owned());
    a
}

// ---------------------------------------------------------------------------
// Progress
// ---------------------------------------------------------------------------

/// `out_time_us` of one `-progress` line (`out_time_ms` is also microseconds, a
/// historical misnomer); `None` for other keys and `N/A`.
pub fn parse_progress_line(line: &str) -> Option<u64> {
    let (k, v) = line.trim().split_once('=')?;
    match k {
        "out_time_us" | "out_time_ms" => v.trim().parse::<i64>().ok().map(|n| n.max(0) as u64),
        _ => None,
    }
}

/// Percentage (0-99 while running; 100 is set when the item is ready).
pub fn progress_percent(out_time_us: u64, duration_secs: Option<f64>) -> u32 {
    match duration_secs {
        Some(d) if d > 0.0 => {
            ((out_time_us as f64 / 1e6 / d * 100.0).floor() as i64).clamp(0, 99) as u32
        }
        _ => 0,
    }
}

// ---------------------------------------------------------------------------
// Process runner
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum RunError {
    Spawn(std::io::Error),
    Timeout,
    Cancelled,
    /// Non-zero exit: tail of stderr (or the exit status).
    Exit(String),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunError::Spawn(e) => write!(f, "cannot run ffmpeg: {e}"),
            RunError::Timeout => f.write_str("ffmpeg timed out"),
            RunError::Cancelled => f.write_str("cancelled"),
            RunError::Exit(m) => f.write_str(m),
        }
    }
}

/// Run ffmpeg with `args`. With `-progress pipe:1` in the arguments, every `out_time_us`
/// is published on `progress`. The process is killed on timeout, on cancellation and
/// when this future is dropped (`kill_on_drop`).
pub async fn run(
    ffmpeg: &Path,
    args: &[String],
    timeout: Duration,
    cancel: &CancellationToken,
    progress: Option<&watch::Sender<u64>>,
) -> Result<(), RunError> {
    let mut child = Command::new(ffmpeg)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(RunError::Spawn)?;
    let stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let read_progress = async {
        if let Some(out) = stdout {
            let mut lines = BufReader::new(out).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if let (Some(us), Some(tx)) = (parse_progress_line(&line), progress) {
                    let _ = tx.send(us);
                }
            }
        }
    };
    let read_err = async {
        let mut tail: Vec<u8> = Vec::new();
        if let Some(err) = stderr.as_mut() {
            let mut buf = [0u8; 4096];
            while let Ok(n) = err.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                tail.extend_from_slice(&buf[..n]);
                if tail.len() > 4 * STDERR_TAIL {
                    tail.drain(..tail.len() - STDERR_TAIL);
                }
            }
        }
        tail
    };
    let work = async {
        let (_, tail, status) = tokio::join!(read_progress, read_err, child.wait());
        (tail, status)
    };
    let (tail, status) = tokio::select! {
        r = tokio::time::timeout(timeout, work) => r.map_err(|_| RunError::Timeout)?,
        _ = cancel.cancelled() => return Err(RunError::Cancelled),
    };
    let status = status.map_err(RunError::Spawn)?;
    if status.success() {
        return Ok(());
    }
    let text = String::from_utf8_lossy(&tail);
    let text = crate::recorder::remux::tail(&text, STDERR_TAIL);
    Err(RunError::Exit(if text.is_empty() {
        format!("ffmpeg exited with {status}")
    } else {
        text
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(name: &str, height: u32, v: u32, a: u32) -> VodRendition {
        VodRendition {
            name: name.into(),
            height,
            video_kbps: v,
            audio_kbps: a,
        }
    }

    fn standard() -> Vec<VodRendition> {
        crate::config::VodConfig::default()
            .ladder("standard")
            .unwrap()
            .renditions
            .clone()
    }

    fn sizes(v: &[Selected]) -> Vec<(String, u32, u32)> {
        v.iter()
            .map(|s| (s.name.clone(), s.width, s.height))
            .collect()
    }

    #[test]
    fn rendition_selection_never_upscales() {
        let full = select_renditions(&standard(), 1920, 1080);
        assert_eq!(
            sizes(&full),
            [
                ("1080p".into(), 1920, 1080),
                ("720p".into(), 1280, 720),
                ("480p".into(), 854, 480),
                ("360p".into(), 640, 360)
            ]
        );
        // 640x360 source: only 360p (equal size is kept, larger rungs skipped).
        assert_eq!(
            sizes(&select_renditions(&standard(), 640, 360)),
            [("360p".into(), 640, 360)]
        );
        // 1280x720: no 1080p.
        assert_eq!(select_renditions(&standard(), 1280, 720).len(), 3);
        // Smaller than every rung: one rendition at the source size, bitrate scaled down.
        let tiny = select_renditions(&standard(), 320, 180);
        assert_eq!(sizes(&tiny), [("180p".into(), 320, 180)]);
        assert_eq!(tiny[0].video_kbps, 200, "800 kbit/s x (180/360)^2");
        assert_eq!(tiny[0].audio_kbps, 96);
        let micro = select_renditions(&standard(), 160, 90);
        assert_eq!(micro[0].video_kbps, MIN_FALLBACK_KBPS, "floor");
        // Odd source sizes give even dimensions.
        for s in select_renditions(&standard(), 1279, 719)
            .iter()
            .chain(&select_renditions(&[r("source", 0, 0, 128)], 161, 91))
        {
            assert!(s.width % 2 == 0 && s.height % 2 == 0, "{s:?}");
        }
        assert_eq!(
            sizes(&select_renditions(&[r("source", 0, 0, 128)], 161, 91)),
            [("source".into(), 160, 90)]
        );
        // Portrait: the rung is the short side (the width).
        assert_eq!(
            sizes(&select_renditions(&standard(), 1080, 1920))[..2],
            [("1080p".into(), 1080, 1920), ("720p".into(), 720, 1280)]
        );
        // Anamorphic / 4:3 aspect.
        assert_eq!(
            sizes(&select_renditions(&standard(), 768, 576))[0],
            ("480p".into(), 640, 480)
        );
        // Source size + a rung of the same size: the duplicate is dropped.
        let dup = select_renditions(
            &[
                r("src", 0, 0, 128),
                r("1080p", 1080, 5000, 128),
                r("720p", 720, 2800, 128),
            ],
            1920,
            1080,
        );
        assert_eq!(
            sizes(&dup),
            [("src".into(), 1920, 1080), ("720p".into(), 1280, 720)]
        );
    }

    #[test]
    fn audio_outputs_are_shared_by_bitrate() {
        let sel = select_renditions(&standard(), 1920, 1080);
        let (unique, map) = audio_outputs(&sel);
        assert_eq!(unique, [160, 128, 96]);
        assert_eq!(map, [0, 1, 2, 2]);
    }

    #[test]
    fn profiles_and_levels() {
        let s = |w, h| Selected {
            name: String::new(),
            width: w,
            height: h,
            video_kbps: 1,
            audio_kbps: 1,
        };
        assert_eq!(profile_for(&s(1280, 720)), "high");
        assert_eq!(profile_for(&s(854, 480)), "main");
        assert_eq!(profile_for(&s(720, 1280)), "high");
        assert_eq!(level_for(&s(1920, 1080), Some(30.0)), "4.1");
        assert_eq!(level_for(&s(1920, 1080), Some(60.0)), "4.2");
        assert_eq!(level_for(&s(640, 360), Some(25.0)), "4.1");
        assert_eq!(level_for(&s(3840, 2160), Some(30.0)), "5.1");
        assert_eq!(level_for(&s(1920, 1080), None), "4.1");
    }

    fn spec(renditions: Vec<Selected>, encoder: &str) -> TranscodeSpec {
        TranscodeSpec {
            source: "/data/vod/x/source.mp4".into(),
            out_dir: "/data/vod/x/abr.tmp".into(),
            video_index: 0,
            audio_index: Some(1),
            channels: 2,
            fps: Some(30.0),
            renditions,
            segment_duration: 4.0,
            encoder: encoder.into(),
            preset: "veryfast".into(),
            encoder_args: String::new(),
            hwaccel_args: String::new(),
        }
    }

    #[test]
    fn transcode_command_golden() {
        let sel = select_renditions(&standard(), 1920, 1080);
        let args = transcode_args(&spec(sel, "libx264")).join(" ");
        let golden = "-nostdin -y -hide_banner -loglevel error -nostats -progress pipe:1 \
-i /data/vod/x/source.mp4 \
-filter_complex [0:0]split=4[s0][s1][s2][s3];[s0]scale=1920:1080[v0];[s1]scale=1280:720[v1];[s2]scale=854:480[v2];[s3]scale=640:360[v3] \
-map [v0] -map [v1] -map [v2] -map [v3] -map 0:1 -map 0:1 -map 0:1 \
-c:v libx264 -preset veryfast -pix_fmt yuv420p -g 120 -keyint_min 120 -sc_threshold 0 -force_key_frames expr:gte(t,n_forced*4) \
-b:v:0 5000k -maxrate:v:0 5350k -bufsize:v:0 7500k -profile:v:0 high -level:v:0 4.1 \
-b:v:1 2800k -maxrate:v:1 2996k -bufsize:v:1 4200k -profile:v:1 high -level:v:1 4.1 \
-b:v:2 1400k -maxrate:v:2 1498k -bufsize:v:2 2100k -profile:v:2 main -level:v:2 4.1 \
-b:v:3 800k -maxrate:v:3 856k -bufsize:v:3 1200k -profile:v:3 main -level:v:3 4.1 \
-c:a aac -ar 48000 -ac 2 -b:a:0 160k -b:a:1 128k -b:a:2 96k \
-f dash -seg_duration 4 -use_template 1 -use_timeline 0 -init_seg_name init-$RepresentationID$.m4s \
-media_seg_name chunk-$RepresentationID$-$Number%05d$.m4s -adaptation_sets id=0,streams=v id=1,streams=a \
-hls_playlist 1 -hls_master_name master.m3u8 /data/vod/x/abr.tmp/manifest.mpd";
        assert_eq!(args, golden);
    }

    #[test]
    fn transcode_command_variants() {
        // Single CRF rendition, mono, no split, extra arguments, 25 fps GOP.
        let sel = select_renditions(&[r("source", 0, 0, 128)], 1280, 720);
        let mut s = spec(sel, "libx264");
        s.channels = 1;
        s.fps = Some(25.0);
        s.segment_duration = 2.5;
        s.encoder_args = " -tune  film ".into();
        s.hwaccel_args = "-threads 4".into();
        let a = transcode_args(&s).join(" ");
        assert!(a.contains("-threads 4 -i /data/vod/x/source.mp4"), "{a}");
        assert!(
            a.contains("-filter_complex [0:0]scale=1280:720[v0] -map [v0] -map 0:1 "),
            "{a}"
        );
        assert!(a.contains("-g 63 -keyint_min 63"), "{a}");
        assert!(
            a.contains("expr:gte(t,n_forced*2.5)") && a.contains("-seg_duration 2.5 "),
            "{a}"
        );
        assert!(a.contains("-crf:v:0 23 -profile:v:0 high -level:v:0 4.1 -tune film -c:a aac -ar 48000 -ac 1 -b:a:0 128k"), "{a}");
        assert!(!a.contains("-b:v:0"), "{a}");
        // No audio: no audio maps, video-only adaptation set.
        s.audio_index = None;
        let a = transcode_args(&s).join(" ");
        assert!(!a.contains("-c:a") && !a.contains("-map 0:1"), "{a}");
        assert!(
            a.contains("-adaptation_sets id=0,streams=v -hls_playlist"),
            "{a}"
        );
        // Hardware encoders.
        let mut s = spec(
            select_renditions(&[r("source", 0, 0, 128)], 1280, 720),
            "h264_nvenc",
        );
        s.preset = "p4".into();
        let a = transcode_args(&s).join(" ");
        assert!(
            a.contains("-c:v h264_nvenc -preset p4 -pix_fmt yuv420p"),
            "{a}"
        );
        assert!(
            a.contains("-forced-idr 1") && !a.contains("-sc_threshold"),
            "{a}"
        );
        assert!(a.contains("-cq:v:0 23 -b:v:0 0"), "{a}");
        s.encoder = "h264_vaapi".into();
        s.preset = String::new();
        let a = transcode_args(&s).join(" ");
        assert!(
            a.contains("scale=1280:720,format=nv12,hwupload[v0]")
                && !a.contains("-pix_fmt")
                && !a.contains("-preset"),
            "{a}"
        );
        assert!(a.contains("-qp:v:0 23"), "{a}");
        s.encoder = "h264_qsv".into();
        assert!(transcode_args(&s).join(" ").contains("-pix_fmt nv12"));
        assert_eq!(EncoderKind::of("libx264"), EncoderKind::Software);
    }

    #[test]
    fn poster_command() {
        assert_eq!(poster_size(1920, 1080), (1280, 720));
        assert_eq!(poster_size(640, 360), (640, 360));
        assert_eq!(poster_size(1080, 1920), (1080, 1920));
        assert_eq!(poster_size(161, 91), (162, 92));
        let a = poster_args(
            Path::new("/s.mp4"),
            2,
            3.0,
            (1280, 720),
            Path::new("/p.tmp.jpg"),
        )
        .join(" ");
        assert_eq!(
            a,
            "-nostdin -y -hide_banner -loglevel error -ss 3.000 -i /s.mp4 -map 0:2 -frames:v 1 \
             -vf scale=1280:720,setsar=1 -q:v 3 -f image2 -update 1 /p.tmp.jpg"
        );
    }

    #[test]
    fn progress_parsing() {
        assert_eq!(parse_progress_line("out_time_us=1500000"), Some(1_500_000));
        assert_eq!(
            parse_progress_line("out_time_ms=2000000\n"),
            Some(2_000_000)
        );
        assert_eq!(parse_progress_line("out_time_us=N/A"), None);
        assert_eq!(parse_progress_line("out_time_us=-5"), Some(0));
        assert_eq!(parse_progress_line("frame=12"), None);
        assert_eq!(parse_progress_line("progress=end"), None);
        assert_eq!(progress_percent(15_000_000, Some(30.0)), 50);
        assert_eq!(
            progress_percent(31_000_000, Some(30.0)),
            99,
            "never 100 while running"
        );
        assert_eq!(progress_percent(1_000_000, None), 0);
        assert_eq!(progress_percent(299_000, Some(30.0)), 0);
    }

    #[test]
    fn probe_parsing() {
        let json = r#"{
          "streams": [
            {"index": 0, "codec_type": "video", "codec_name": "mjpeg", "width": 300, "height": 300,
             "disposition": {"attached_pic": 1}},
            {"index": 1, "codec_type": "video", "codec_name": "h264", "width": 1920, "height": 1080,
             "sample_aspect_ratio": "1:1", "avg_frame_rate": "30000/1001", "r_frame_rate": "30000/1001",
             "disposition": {"attached_pic": 0},
             "side_data_list": [{"side_data_type": "Display Matrix", "rotation": -90}]},
            {"index": 2, "codec_type": "audio", "codec_name": "aac", "channels": 1}
          ],
          "format": {"duration": "30.033", "size": "12345678", "format_name": "mov,mp4,m4a,3gp,3g2,mj2"}
        }"#;
        let p = parse_probe(json).unwrap();
        let v = p.video.unwrap();
        assert_eq!(
            (v.index, v.codec.as_str(), v.width, v.height),
            (1, "h264", 1080, 1920),
            "rotated"
        );
        assert!((v.fps.unwrap() - 29.97).abs() < 0.01);
        assert_eq!(
            p.audio.unwrap(),
            AudioInfo {
                index: 2,
                codec: "aac".into(),
                channels: 1
            }
        );
        assert_eq!(p.duration_secs, Some(30.033));
        assert_eq!(p.size, Some(12_345_678));
        // Anamorphic PAL, legacy rotate tag of 180 (no swap), no audio, no duration.
        let json = r#"{"streams": [{"index": 0, "codec_type": "video", "codec_name": "mpeg2video",
            "width": 720, "height": 576, "sample_aspect_ratio": "16:15", "avg_frame_rate": "0/0",
            "r_frame_rate": "25/1", "tags": {"rotate": "180"}}], "format": {}}"#;
        let p = parse_probe(json).unwrap();
        let v = p.video.unwrap();
        assert_eq!((v.width, v.height, v.fps), (768, 576, Some(25.0)));
        assert!(p.audio.is_none() && p.duration_secs.is_none());
        // Audio only: no video.
        let p = parse_probe(r#"{"streams": [{"index": 0, "codec_type": "audio", "channels": 2}], "format": {"duration": "3"}}"#).unwrap();
        assert!(p.video.is_none());
        assert!(parse_probe("not json").is_err());
    }

    #[tokio::test]
    async fn runner_reports_errors() {
        let cancel = CancellationToken::new();
        let r = run(
            Path::new("/nonexistent/ffmpeg"),
            &[],
            Duration::from_secs(5),
            &cancel,
            None,
        )
        .await;
        assert!(matches!(r, Err(RunError::Spawn(_))));
        let Some(ffmpeg) = crate::config::find_in_path("ffmpeg") else {
            return;
        };
        let args: Vec<String> = [
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-i",
            "/nonexistent/in.mp4",
            "-f",
            "null",
            "-",
        ]
        .map(str::to_string)
        .to_vec();
        match run(&ffmpeg, &args, Duration::from_secs(20), &cancel, None).await {
            Err(RunError::Exit(m)) => assert!(m.contains("No such file"), "{m}"),
            other => panic!("{other:?}"),
        }
        // Cancellation kills the process.
        let args: Vec<String> = [
            "-nostdin",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=64x64:rate=5",
            "-f",
            "null",
            "-",
        ]
        .map(str::to_string)
        .to_vec();
        let c = CancellationToken::new();
        let c2 = c.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            c2.cancel();
        });
        let r = run(&ffmpeg, &args, Duration::from_secs(20), &c, None).await;
        assert!(matches!(r, Err(RunError::Cancelled)), "{r:?}");
    }
}
