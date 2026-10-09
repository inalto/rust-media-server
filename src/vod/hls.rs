//! Post-processing of ffmpeg's CMAF output (`dash` muxer with `-hls_playlist 1`).
//!
//! ffmpeg writes a usable MPD, one HLS media playlist per representation and an HLS
//! master whose variants all reference one audio group (every audio bitrate in it, only
//! the first marked DEFAULT) with the *configured* bitrate as BANDWIDTH. The master is
//! rebuilt here:
//!
//! - one audio group per audio bitrate (`aud<kbps>`), each variant referencing the group
//!   of its own rung, so lower video rungs also get the lighter audio;
//! - `BANDWIDTH` = measured peak segment bitrate (video + audio), `AVERAGE-BANDWIDTH` =
//!   measured average, `RESOLUTION`, `FRAME-RATE`, `CODECS` (from ffmpeg);
//! - `#EXT-X-INDEPENDENT-SEGMENTS` (every segment starts with a forced keyframe);
//! - the variant played first by native players is the largest one up to 720p.
//!
//! Media playlists get `#EXT-X-PLAYLIST-TYPE:VOD`, lose the wall-clock
//! `#EXT-X-PROGRAM-DATE-TIME` tags and, like the MPD's `SegmentTemplate`, reference the
//! segments with a `?v=<revision>` query: segment URLs are cached as immutable, and a
//! re-transcode reuses the same file names.

use std::path::Path;

use super::ffmpeg::{Selected, audio_outputs};
use crate::db::vod::Rendition;

/// `KEY=VALUE,KEY="quoted, value"` → `[(KEY, value)]` (quotes removed).
pub fn parse_attributes(s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = s.trim();
    while !rest.is_empty() {
        let Some((key, after)) = rest.split_once('=') else {
            break;
        };
        let (value, next) = if let Some(q) = after.strip_prefix('"') {
            match q.find('"') {
                Some(end) => (&q[..end], q[end + 1..].trim_start_matches(',')),
                None => (q, ""),
            }
        } else {
            match after.find(',') {
                Some(end) => (&after[..end], &after[end + 1..]),
                None => (after, ""),
            }
        };
        out.push((key.trim().to_string(), value.to_string()));
        rest = next.trim_start();
    }
    out
}

fn attr<'a>(attrs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

/// The parts of ffmpeg's master playlist that are reused.
#[derive(Debug, Default, PartialEq)]
pub struct FfmpegMaster {
    /// `#EXT-X-STREAM-INF` attributes and URI.
    pub variants: Vec<(Vec<(String, String)>, String)>,
    /// `#EXT-X-MEDIA` attributes.
    pub media: Vec<Vec<(String, String)>>,
}

impl FfmpegMaster {
    pub fn variant(&self, uri: &str) -> Option<&[(String, String)]> {
        self.variants
            .iter()
            .find(|(_, u)| u == uri)
            .map(|(a, _)| a.as_slice())
    }
    pub fn media(&self, uri: &str) -> Option<&[(String, String)]> {
        self.media
            .iter()
            .find(|a| attr(a, "URI") == Some(uri))
            .map(Vec::as_slice)
    }
}

pub fn parse_master(text: &str) -> FfmpegMaster {
    let mut m = FfmpegMaster::default();
    let mut pending: Option<Vec<(String, String)>> = None;
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if let Some(a) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            pending = Some(parse_attributes(a));
        } else if let Some(a) = line.strip_prefix("#EXT-X-MEDIA:") {
            m.media.push(parse_attributes(a));
        } else if !line.starts_with('#')
            && let Some(attrs) = pending.take()
        {
            m.variants.push((attrs, line.to_string()));
        }
    }
    m
}

fn with_version(uri: &str, version: &str) -> String {
    if uri.contains('?') {
        uri.to_string()
    } else {
        format!("{uri}?v={version}")
    }
}

/// VOD media playlist: `PLAYLIST-TYPE:VOD`, no `PROGRAM-DATE-TIME`, versioned URIs,
/// `ENDLIST`.
pub fn rewrite_media_playlist(text: &str, version: &str) -> String {
    let mut out = String::with_capacity(text.len() + 256);
    let mut has_type = text.contains("#EXT-X-PLAYLIST-TYPE:");
    for line in text.lines().map(str::trim_end) {
        if line.is_empty() || line.starts_with("#EXT-X-PROGRAM-DATE-TIME") {
            continue;
        }
        if let Some(a) = line.strip_prefix("#EXT-X-MAP:") {
            let attrs: Vec<String> = parse_attributes(a)
                .into_iter()
                .map(|(k, v)| {
                    if k == "URI" {
                        format!("URI=\"{}\"", with_version(&v, version))
                    } else {
                        format!("{k}={v}")
                    }
                })
                .collect();
            out.push_str(&format!("#EXT-X-MAP:{}\n", attrs.join(",")));
            continue;
        }
        if line.starts_with('#') {
            out.push_str(line);
            out.push('\n');
            if line.starts_with("#EXT-X-VERSION") && !has_type {
                out.push_str("#EXT-X-PLAYLIST-TYPE:VOD\n");
                has_type = true;
            }
        } else {
            out.push_str(&with_version(line.trim(), version));
            out.push('\n');
        }
    }
    if !out.contains("#EXT-X-ENDLIST") {
        out.push_str("#EXT-X-ENDLIST\n");
    }
    out
}

/// `(peak, average)` bit rates (bit/s) of a media playlist, `size_of(uri)` giving each
/// segment's size. Segments shorter than half the longest one (the last) are left out
/// of the peak.
pub fn playlist_bitrates(text: &str, size_of: impl Fn(&str) -> Option<u64>) -> Option<(u64, u64)> {
    let mut segments: Vec<(f64, u64)> = Vec::new();
    let mut duration: Option<f64> = None;
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if let Some(d) = line.strip_prefix("#EXTINF:") {
            duration = d.split(',').next().and_then(|d| d.trim().parse().ok());
        } else if !line.starts_with('#')
            && let Some(d) = duration.take()
        {
            let uri = line.split('?').next().unwrap_or(line);
            segments.push((d, size_of(uri)?));
        }
    }
    let total_dur: f64 = segments.iter().map(|(d, _)| d).sum();
    if segments.is_empty() || total_dur <= 0.0 {
        return None;
    }
    let total_bits: f64 = segments.iter().map(|(_, s)| *s as f64 * 8.0).sum();
    let longest = segments.iter().map(|(d, _)| *d).fold(0.0, f64::max);
    let peak = segments
        .iter()
        .filter(|(d, _)| *d >= longest / 2.0 && *d > 0.0)
        .map(|(d, s)| *s as f64 * 8.0 / d)
        .fold(0.0, f64::max);
    Some((peak.ceil() as u64, (total_bits / total_dur).ceil() as u64))
}

#[derive(Debug, Clone, PartialEq)]
pub struct VariantOut {
    pub uri: String,
    pub width: u32,
    pub height: u32,
    pub codecs: String,
    pub bandwidth: u64,
    pub average_bandwidth: u64,
    pub frame_rate: Option<f64>,
    pub audio_group: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AudioOut {
    pub group: String,
    pub name: String,
    pub uri: String,
    pub channels: Option<String>,
}

/// Index of the variant listed first: the largest rung up to 720p (short side), else the
/// smallest one. Native players (Safari, iOS) start with the first variant.
pub fn first_variant(renditions: &[Selected]) -> usize {
    renditions
        .iter()
        .enumerate()
        .filter(|(_, r)| r.width.min(r.height) <= 720)
        .max_by_key(|(_, r)| r.width * r.height)
        .map(|(i, _)| i)
        .unwrap_or(renditions.len().saturating_sub(1))
}

/// The rebuilt master playlist (variants in the given order).
pub fn build_master(variants: &[VariantOut], audio: &[AudioOut], version: &str) -> String {
    let mut out = String::from("#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-INDEPENDENT-SEGMENTS\n");
    for a in audio {
        out.push_str(&format!(
            "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"{}\",NAME=\"{}\",DEFAULT=YES,AUTOSELECT=YES,",
            a.group, a.name
        ));
        if let Some(ch) = &a.channels {
            out.push_str(&format!("CHANNELS=\"{ch}\","));
        }
        out.push_str(&format!("URI=\"{}\"\n", with_version(&a.uri, version)));
    }
    for v in variants {
        out.push_str(&format!(
            "#EXT-X-STREAM-INF:BANDWIDTH={},AVERAGE-BANDWIDTH={},RESOLUTION={}x{},",
            v.bandwidth, v.average_bandwidth, v.width, v.height
        ));
        if let Some(fps) = v.frame_rate {
            out.push_str(&format!("FRAME-RATE={fps:.3},"));
        }
        out.push_str(&format!("CODECS=\"{}\"", v.codecs));
        if let Some(g) = &v.audio_group {
            out.push_str(&format!(",AUDIO=\"{g}\""));
        }
        out.push('\n');
        out.push_str(&with_version(&v.uri, version));
        out.push('\n');
    }
    out
}

/// Add `?v=<version>` to the `initialization` and `media` templates of the MPD.
pub fn rewrite_mpd(text: &str, version: &str) -> String {
    let mut out = text.to_string();
    for key in ["initialization=\"", "media=\""] {
        let mut result = String::with_capacity(out.len() + 64);
        let mut rest = out.as_str();
        while let Some(i) = rest.find(key) {
            // Only whole attribute names (`media=` but not `xmedia=`).
            let boundary = i == 0 || rest.as_bytes()[i - 1].is_ascii_whitespace();
            let start = i + key.len();
            result.push_str(&rest[..start]);
            rest = &rest[start..];
            let Some(end) = rest.find('"') else { break };
            let value = &rest[..end];
            if boundary {
                result.push_str(&with_version(value, version));
            } else {
                result.push_str(value);
            }
            rest = &rest[end..];
        }
        result.push_str(rest);
        out = result;
    }
    out
}

fn read(dir: &Path, name: &str) -> Result<String, String> {
    std::fs::read_to_string(dir.join(name)).map_err(|e| format!("cannot read {name}: {e}"))
}

fn write(dir: &Path, name: &str, text: &str) -> Result<(), String> {
    std::fs::write(dir.join(name), text).map_err(|e| format!("cannot write {name}: {e}"))
}

/// Rewrite the playlists and the MPD in `dir` (blocking I/O: call from
/// `spawn_blocking`). `renditions` are the encoded video rungs in output order (video
/// representation `i` = `media_<i>.m3u8`, audio output `j` = `media_<N+j>.m3u8`).
/// Returns the renditions with their measured peak bandwidth.
pub fn finalize(
    dir: &Path,
    renditions: &[Selected],
    has_audio: bool,
    frame_rate: Option<f64>,
    version: &str,
) -> Result<Vec<Rendition>, String> {
    let n = renditions.len();
    let master = parse_master(&read(dir, "master.m3u8")?);
    let size_of = |uri: &str| -> Option<u64> {
        let safe = !uri.is_empty() && !uri.contains('/') && !uri.contains("..");
        safe.then(|| std::fs::metadata(dir.join(uri)).ok().map(|m| m.len()))
            .flatten()
    };
    let (audio_kbps, audio_map) = audio_outputs(renditions);
    let mut audio: Vec<AudioOut> = Vec::new();
    let mut audio_rates: Vec<(u64, u64)> = Vec::new();
    if has_audio {
        for (j, kbps) in audio_kbps.iter().enumerate() {
            let uri = format!("media_{}.m3u8", n + j);
            let text = read(dir, &uri)?;
            let rates =
                playlist_bitrates(&text, size_of).ok_or_else(|| format!("{uri}: no segments"))?;
            write(dir, &uri, &rewrite_media_playlist(&text, version))?;
            audio_rates.push(rates);
            audio.push(AudioOut {
                group: format!("aud{kbps}"),
                name: format!("AAC {kbps} kbit/s"),
                channels: master
                    .media(&uri)
                    .and_then(|a| attr(a, "CHANNELS"))
                    .map(str::to_string),
                uri,
            });
        }
    }
    let mut variants = Vec::with_capacity(n);
    let mut out = Vec::with_capacity(n);
    for (i, r) in renditions.iter().enumerate() {
        let uri = format!("media_{i}.m3u8");
        let text = read(dir, &uri)?;
        let (peak, avg) =
            playlist_bitrates(&text, size_of).ok_or_else(|| format!("{uri}: no segments"))?;
        write(dir, &uri, &rewrite_media_playlist(&text, version))?;
        let codecs = master
            .variant(&uri)
            .and_then(|a| attr(a, "CODECS"))
            .ok_or_else(|| format!("{uri} is missing from ffmpeg's master playlist"))?
            .to_string();
        let (a_peak, a_avg, group) = match (has_audio, audio_map.get(i)) {
            (true, Some(&j)) => (
                audio_rates[j].0,
                audio_rates[j].1,
                Some(audio[j].group.clone()),
            ),
            _ => (0, 0, None),
        };
        variants.push(VariantOut {
            uri,
            width: r.width,
            height: r.height,
            codecs,
            bandwidth: peak + a_peak,
            average_bandwidth: avg + a_avg,
            frame_rate,
            audio_group: group,
        });
        out.push(Rendition {
            name: r.name.clone(),
            width: r.width,
            height: r.height,
            video_kbps: r.video_kbps,
            audio_kbps: if has_audio { r.audio_kbps } else { 0 },
            bandwidth: peak + a_peak,
        });
    }
    let first = first_variant(renditions);
    let mut ordered = vec![variants[first].clone()];
    ordered.extend(
        variants
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != first)
            .map(|(_, v)| v.clone()),
    );
    write(dir, "master.m3u8", &build_master(&ordered, &audio, version))?;
    let mpd = read(dir, "manifest.mpd")?;
    write(dir, "manifest.mpd", &rewrite_mpd(&mpd, version))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FF_MASTER: &str = "#EXTM3U\n#EXT-X-VERSION:7\n\
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"group_A1\",NAME=\"audio_2\",DEFAULT=YES,CHANNELS=\"2\",URI=\"media_2.m3u8\"\n\
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"group_A1\",NAME=\"audio_3\",DEFAULT=NO,CHANNELS=\"2\",URI=\"media_3.m3u8\"\n\
#EXT-X-STREAM-INF:BANDWIDTH=2932168,RESOLUTION=1280x720,CODECS=\"avc1.640029,mp4a.40.2\",AUDIO=\"group_A1\"\nmedia_0.m3u8\n\n\
#EXT-X-STREAM-INF:BANDWIDTH=932168,RESOLUTION=640x360,CODECS=\"avc1.4d401e,mp4a.40.2\",AUDIO=\"group_A1\"\nmedia_1.m3u8\n\n";

    const FF_MEDIA: &str = "#EXTM3U\n#EXT-X-VERSION:6\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:1\n\
#EXT-X-MAP:URI=\"init-0.m4s\"\n#EXTINF:4.000000,\n#EXT-X-PROGRAM-DATE-TIME:2026-10-09T18:35:36.000+0200\nchunk-0-00001.m4s\n\
#EXTINF:4.000000,\n#EXT-X-PROGRAM-DATE-TIME:2026-10-09T18:35:40.000+0200\nchunk-0-00002.m4s\n\
#EXTINF:1.000000,\nchunk-0-00003.m4s\n#EXT-X-ENDLIST\n";

    #[test]
    fn attribute_lists() {
        let a = parse_attributes(
            "BANDWIDTH=1,CODECS=\"avc1.64001f,mp4a.40.2\",RESOLUTION=640x360,AUDIO=\"g\"",
        );
        assert_eq!(
            a,
            [
                ("BANDWIDTH".to_string(), "1".to_string()),
                ("CODECS".into(), "avc1.64001f,mp4a.40.2".into()),
                ("RESOLUTION".into(), "640x360".into()),
                ("AUDIO".into(), "g".into())
            ]
        );
        assert!(parse_attributes("").is_empty());
    }

    #[test]
    fn ffmpeg_master_parsing() {
        let m = parse_master(FF_MASTER);
        assert_eq!(m.variants.len(), 2);
        assert_eq!(m.media.len(), 2);
        assert_eq!(
            attr(m.variant("media_1.m3u8").unwrap(), "CODECS"),
            Some("avc1.4d401e,mp4a.40.2")
        );
        assert_eq!(
            attr(m.media("media_3.m3u8").unwrap(), "CHANNELS"),
            Some("2")
        );
        assert!(m.variant("media_9.m3u8").is_none());
    }

    #[test]
    fn media_playlist_rewrite() {
        let out = rewrite_media_playlist(FF_MEDIA, "ab12");
        assert_eq!(
            out,
            "#EXTM3U\n#EXT-X-VERSION:6\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:1\n\
#EXT-X-MAP:URI=\"init-0.m4s?v=ab12\"\n#EXTINF:4.000000,\nchunk-0-00001.m4s?v=ab12\n#EXTINF:4.000000,\nchunk-0-00002.m4s?v=ab12\n\
#EXTINF:1.000000,\nchunk-0-00003.m4s?v=ab12\n#EXT-X-ENDLIST\n"
        );
        // Idempotent, ENDLIST added when missing.
        assert_eq!(rewrite_media_playlist(&out, "zz"), out);
        let open = rewrite_media_playlist("#EXTM3U\n#EXT-X-VERSION:6\n#EXTINF:4,\na.m4s\n", "v");
        assert!(open.ends_with("a.m4s?v=v\n#EXT-X-ENDLIST\n"), "{open}");
    }

    #[test]
    fn bitrates_from_segment_sizes() {
        let sizes = |uri: &str| match uri {
            "chunk-0-00001.m4s" => Some(1_000_000),
            "chunk-0-00002.m4s" => Some(2_000_000),
            "chunk-0-00003.m4s" => Some(1_000_000),
            _ => None,
        };
        let (peak, avg) = playlist_bitrates(FF_MEDIA, sizes).unwrap();
        // Peak over full-length segments: 2 MB in 4 s; the 1 s tail (8 Mbit/s) is ignored.
        assert_eq!(peak, 4_000_000);
        assert_eq!(avg, 32_000_000 / 9 + 1);
        assert!(playlist_bitrates(FF_MEDIA, |_| None).is_none());
        assert!(playlist_bitrates("#EXTM3U\n", sizes).is_none());
    }

    #[test]
    fn master_rebuild() {
        let sel = |name: &str, w, h, a| Selected {
            name: name.into(),
            width: w,
            height: h,
            video_kbps: 1000,
            audio_kbps: a,
        };
        let ladder = [
            sel("1080p", 1920, 1080, 160),
            sel("720p", 1280, 720, 128),
            sel("360p", 640, 360, 96),
        ];
        assert_eq!(first_variant(&ladder), 1);
        assert_eq!(first_variant(&ladder[..1]), 0, "only rung above 720p");
        assert_eq!(first_variant(&ladder[2..]), 0);
        let v = VariantOut {
            uri: "media_1.m3u8".into(),
            width: 1280,
            height: 720,
            codecs: "avc1.640029,mp4a.40.2".into(),
            bandwidth: 3_100_000,
            average_bandwidth: 2_900_000,
            frame_rate: Some(29.97),
            audio_group: Some("aud128".into()),
        };
        let a = AudioOut {
            group: "aud128".into(),
            name: "AAC 128 kbit/s".into(),
            uri: "media_4.m3u8".into(),
            channels: Some("2".into()),
        };
        assert_eq!(
            build_master(&[v], &[a], "r1"),
            "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-INDEPENDENT-SEGMENTS\n\
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud128\",NAME=\"AAC 128 kbit/s\",DEFAULT=YES,AUTOSELECT=YES,CHANNELS=\"2\",URI=\"media_4.m3u8?v=r1\"\n\
#EXT-X-STREAM-INF:BANDWIDTH=3100000,AVERAGE-BANDWIDTH=2900000,RESOLUTION=1280x720,FRAME-RATE=29.970,CODECS=\"avc1.640029,mp4a.40.2\",AUDIO=\"aud128\"\n\
media_1.m3u8?v=r1\n"
        );
    }

    #[test]
    fn mpd_templates_are_versioned() {
        let mpd = r#"<SegmentTemplate timescale="1000000" duration="4000000" initialization="init-$RepresentationID$.m4s" media="chunk-$RepresentationID$-$Number%05d$.m4s" startNumber="1">"#;
        assert_eq!(
            rewrite_mpd(mpd, "r1"),
            r#"<SegmentTemplate timescale="1000000" duration="4000000" initialization="init-$RepresentationID$.m4s?v=r1" media="chunk-$RepresentationID$-$Number%05d$.m4s?v=r1" startNumber="1">"#
        );
        assert_eq!(rewrite_mpd("<x xmedia=\"a\"/>", "r"), "<x xmedia=\"a\"/>");
    }

    #[test]
    fn finalize_directory() {
        let dir = std::env::temp_dir().join(format!("rms-vod-hls-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("master.m3u8"), FF_MASTER).unwrap();
        let media = |i: usize| {
            FF_MEDIA
                .replace("-0-", &format!("-{i}-"))
                .replace("init-0", &format!("init-{i}"))
        };
        for i in 0..4 {
            std::fs::write(dir.join(format!("media_{i}.m3u8")), media(i)).unwrap();
            for (k, size) in [(1, 100_000), (2, 150_000), (3, 10_000)] {
                std::fs::write(
                    dir.join(format!("chunk-{i}-0000{k}.m4s")),
                    vec![0u8; size * (4 - i)],
                )
                .unwrap();
            }
        }
        std::fs::write(dir.join("manifest.mpd"), r#"<SegmentTemplate initialization="init-$RepresentationID$.m4s" media="chunk-$RepresentationID$-$Number%05d$.m4s"/>"#).unwrap();
        let sel = |name: &str, w, h, a| Selected {
            name: name.into(),
            width: w,
            height: h,
            video_kbps: 1000,
            audio_kbps: a,
        };
        let ladder = [sel("720p", 1280, 720, 128), sel("360p", 640, 360, 96)];
        let out = finalize(&dir, &ladder, true, Some(30.0), "rv").unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].name, "720p");
        // 720p: 600 kB in 4 s = 1.2 Mbit/s + audio media_2 (300 kB / 4 s = 0.6 Mbit/s).
        assert_eq!(out[0].bandwidth, 1_200_000 + 600_000);
        let master = std::fs::read_to_string(dir.join("master.m3u8")).unwrap();
        assert!(
            master.contains("GROUP-ID=\"aud128\"") && master.contains("GROUP-ID=\"aud96\""),
            "{master}"
        );
        assert!(master.contains("RESOLUTION=640x360,FRAME-RATE=30.000,CODECS=\"avc1.4d401e,mp4a.40.2\",AUDIO=\"aud96\"\nmedia_1.m3u8?v=rv"), "{master}");
        // 720p is listed first.
        assert!(master.find("media_0.m3u8").unwrap() < master.find("media_1.m3u8?v").unwrap());
        assert!(
            std::fs::read_to_string(dir.join("media_3.m3u8"))
                .unwrap()
                .contains("chunk-3-00001.m4s?v=rv")
        );
        assert!(
            std::fs::read_to_string(dir.join("manifest.mpd"))
                .unwrap()
                .contains("m4s?v=rv\"")
        );
        // A missing variant is an error.
        std::fs::write(dir.join("master.m3u8"), "#EXTM3U\n").unwrap();
        assert!(
            finalize(&dir, &ladder, true, None, "rv")
                .unwrap_err()
                .contains("missing")
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
