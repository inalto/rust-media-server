//! Pure-Rust FLV file writer for recordings.
//!
//! - [`TimestampRebaser`]: the first media packet becomes `t = 0`, timestamps stay
//!   monotonic per track, sequence headers and script data are placed at the current
//!   position, and encoder timestamp resets/jumps are absorbed.
//! - [`FlvWriter`]: buffered async writer (`tokio::fs` + `BufWriter`) producing a
//!   standard FLV file (header, tags, PreviousTagSize fields).
//! - [`scan_flv`]: blocking scanner used by crash recovery: finds the last complete tag
//!   (so a truncated file can be repaired) and the media duration.

use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use bytes::BytesMut;
use tokio::io::{AsyncWriteExt, BufWriter};

use crate::hub::packet::{
    FLV_HEADER_LEN, FLV_TAG_AUDIO, FLV_TAG_HEADER_LEN, FLV_TAG_VIDEO, audio_is_sequence_header,
    flv_file_header, video_is_sequence_header, write_flv_tag,
};
use crate::hub::{MediaPacket, PacketKind};

/// Input jumps larger than these are treated as discontinuities (encoder restart,
/// timestamp reset): the output clock continues from where it was.
const MAX_BACKWARD_JUMP_MS: i64 = 1_000;
const MAX_FORWARD_JUMP_MS: i64 = 10_000;

/// Write buffer size of the recording file.
const WRITE_BUFFER: usize = 256 * 1024;

/// Maps live FLV timestamps (milliseconds, as received) to file timestamps.
///
/// The output clock follows the signed deltas between consecutive media packets
/// (which also handles the 32-bit wrap-around), starting at 0 with the first media
/// packet; per-track outputs never go backwards.
#[derive(Debug, Default, Clone)]
pub struct TimestampRebaser {
    last_in: Option<u32>,
    clock: i64,
    /// Last output per track: [video, audio].
    last_out: [u32; 2],
    max_out: u32,
}

impl TimestampRebaser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Output timestamp for `p` (updates the state).
    pub fn rebase(&mut self, p: &MediaPacket) -> u32 {
        let track = match p.kind {
            PacketKind::Video if !p.is_sequence_header => 0,
            PacketKind::Audio if !p.is_sequence_header => 1,
            // Decoder configuration and metadata carry no timing of their own.
            _ => return self.max_out,
        };
        if let Some(prev) = self.last_in {
            let delta = p.timestamp.wrapping_sub(prev) as i32 as i64;
            if (-MAX_BACKWARD_JUMP_MS..=MAX_FORWARD_JUMP_MS).contains(&delta) {
                self.clock += delta;
            } else {
                tracing::debug!(delta, "recording: timestamp discontinuity absorbed");
            }
        }
        self.last_in = Some(p.timestamp);
        let out = (self.clock.clamp(0, u32::MAX as i64) as u32).max(self.last_out[track]);
        self.last_out[track] = out;
        self.max_out = self.max_out.max(out);
        out
    }

    /// Highest output timestamp so far (≈ media duration in ms).
    pub fn max_out(&self) -> u32 {
        self.max_out
    }

    /// Whether at least one media packet was seen.
    pub fn started(&self) -> bool {
        self.last_in.is_some()
    }
}

/// Final figures of a closed [`FlvWriter`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlvStats {
    pub bytes: u64,
    pub duration_ms: u32,
    pub media_packets: u64,
}

/// Buffered FLV file writer with timestamp rebasing.
pub struct FlvWriter {
    path: PathBuf,
    out: BufWriter<tokio::fs::File>,
    rebaser: TimestampRebaser,
    buf: BytesMut,
    bytes: u64,
    media_packets: u64,
}

impl FlvWriter {
    /// Create `path` (must not exist) and write the FLV header.
    pub async fn create(path: &Path, has_video: bool, has_audio: bool) -> io::Result<Self> {
        let file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .await?;
        let mut w = Self {
            path: path.to_path_buf(),
            out: BufWriter::with_capacity(WRITE_BUFFER, file),
            rebaser: TimestampRebaser::new(),
            buf: BytesMut::with_capacity(64 * 1024),
            bytes: 0,
            media_packets: 0,
        };
        let header = flv_file_header(has_video, has_audio);
        w.out.write_all(&header).await?;
        w.bytes += header.len() as u64;
        Ok(w)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one packet as an FLV tag with a rebased timestamp.
    pub async fn write(&mut self, p: &MediaPacket) -> io::Result<()> {
        let ts = self.rebaser.rebase(p);
        if p.kind != PacketKind::Script && !p.is_sequence_header {
            self.media_packets += 1;
        }
        let mut tag = p.clone();
        tag.timestamp = ts;
        self.buf.clear();
        write_flv_tag(&mut self.buf, &tag);
        self.out.write_all(&self.buf).await?;
        self.bytes += self.buf.len() as u64;
        Ok(())
    }

    /// Flush buffered data to the OS (so the size on disk is current).
    pub async fn flush(&mut self) -> io::Result<()> {
        self.out.flush().await
    }

    /// Bytes written so far (header included).
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    pub fn duration_ms(&self) -> u32 {
        self.rebaser.max_out()
    }

    pub fn media_packets(&self) -> u64 {
        self.media_packets
    }

    /// Flush, fsync and close the file.
    pub async fn finish(mut self) -> io::Result<FlvStats> {
        self.out.flush().await?;
        let file = self.out.get_mut();
        file.sync_all().await?;
        Ok(FlvStats {
            bytes: self.bytes,
            duration_ms: self.rebaser.max_out(),
            media_packets: self.media_packets,
        })
    }
}

/// Result of [`scan_flv`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlvScan {
    /// Length of the valid prefix (header + complete tags).
    pub valid_len: u64,
    /// Total file length.
    pub file_len: u64,
    /// Highest audio/video frame timestamp (ms).
    pub duration_ms: u32,
    /// Number of complete audio/video frame tags (sequence headers excluded).
    pub media_tags: u64,
}

/// Scan an FLV file tag by tag (blocking). Errors when the FLV header is missing.
pub fn scan_flv(path: &Path) -> io::Result<FlvScan> {
    let mut f = std::io::BufReader::new(std::fs::File::open(path)?);
    let file_len = f.get_ref().metadata()?.len();
    let mut header = [0u8; FLV_HEADER_LEN];
    f.read_exact(&mut header)?;
    if &header[..3] != b"FLV" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not an FLV file",
        ));
    }
    let data_offset = u32::from_be_bytes([header[5], header[6], header[7], header[8]]) as u64;
    let mut pos = data_offset + 4;
    if data_offset != 9 {
        f.seek(SeekFrom::Start(pos))?;
    }
    let mut scan = FlvScan {
        valid_len: pos.min(file_len),
        file_len,
        duration_ms: 0,
        media_tags: 0,
    };
    let mut th = [0u8; FLV_TAG_HEADER_LEN];
    loop {
        if pos + FLV_TAG_HEADER_LEN as u64 > file_len {
            break;
        }
        f.read_exact(&mut th)?;
        let size = u32::from_be_bytes([0, th[1], th[2], th[3]]) as u64;
        let ts = u32::from_be_bytes([th[7], th[4], th[5], th[6]]);
        let end = pos + FLV_TAG_HEADER_LEN as u64 + size + 4;
        if end > file_len {
            break;
        }
        let mut skip = size as i64 + 4;
        if (th[0] == FLV_TAG_VIDEO || th[0] == FLV_TAG_AUDIO) && size >= 1 {
            // The first body bytes tell sequence headers (no media) from frames.
            let mut head = [0u8; 2];
            let n = size.min(2) as usize;
            f.read_exact(&mut head[..n])?;
            skip -= n as i64;
            let is_header = if th[0] == FLV_TAG_VIDEO {
                video_is_sequence_header(&head[..n])
            } else {
                audio_is_sequence_header(&head[..n])
            };
            if !is_header {
                scan.media_tags += 1;
                scan.duration_ms = scan.duration_ms.max(ts);
            }
        }
        f.seek_relative(skip)?;
        pos = end;
        scan.valid_len = pos;
    }
    Ok(scan)
}

/// Scan and truncate a partially written trailing tag (crash recovery). Blocking.
pub fn repair_flv(path: &Path) -> io::Result<FlvScan> {
    let scan = scan_flv(path)?;
    if scan.valid_len < scan.file_len {
        let f = std::fs::OpenOptions::new().write(true).open(path)?;
        f.set_len(scan.valid_len)?;
        f.sync_all()?;
    }
    Ok(FlvScan {
        file_len: scan.valid_len,
        ..scan
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    fn video(ts: u32, key: bool) -> MediaPacket {
        let b0 = if key { 0x17 } else { 0x27 };
        MediaPacket::video(ts, Bytes::from(vec![b0, 0x01, 0, 0, 0, 0xAA]))
    }
    fn video_header(ts: u32) -> MediaPacket {
        MediaPacket::video(
            ts,
            Bytes::from_static(&[0x17, 0x00, 0, 0, 0, 1, 0x64, 0, 0x1f]),
        )
    }
    fn audio(ts: u32) -> MediaPacket {
        MediaPacket::audio(ts, Bytes::from_static(&[0xAF, 0x01, 0x21, 0x10]))
    }
    fn audio_header(ts: u32) -> MediaPacket {
        MediaPacket::audio(ts, Bytes::from_static(&[0xAF, 0x00, 0x11, 0x90]))
    }

    #[test]
    fn rebase_starts_at_zero_with_headers_first() {
        let mut r = TimestampRebaser::new();
        // GOP snapshot of a stream live for 2 minutes.
        assert_eq!(
            r.rebase(&MediaPacket::script(0, Bytes::from_static(b"x"))),
            0
        );
        assert_eq!(r.rebase(&video_header(0)), 0);
        assert_eq!(r.rebase(&audio_header(0)), 0);
        assert!(!r.started());
        assert_eq!(r.rebase(&video(120_000, true)), 0);
        assert!(r.started());
        // Audio slightly behind the video keyframe: clamped to 0, then follows.
        assert_eq!(r.rebase(&audio(119_990)), 0);
        assert_eq!(r.rebase(&video(120_033, false)), 33);
        assert_eq!(r.rebase(&audio(120_011)), 11);
        assert_eq!(r.rebase(&video(120_066, false)), 66);
        assert_eq!(r.rebase(&audio(120_032)), 32);
        assert_eq!(r.max_out(), 66);
        // Mid-stream headers/metadata are placed at the current position.
        assert_eq!(r.rebase(&video_header(5)), 66);
    }

    #[test]
    fn rebase_is_monotonic_per_track_and_absorbs_jumps() {
        let mut r = TimestampRebaser::new();
        assert_eq!(r.rebase(&video(1000, true)), 0);
        assert_eq!(r.rebase(&video(1040, false)), 40);
        // Small backwards jitter on the same track: held.
        assert_eq!(r.rebase(&video(1030, false)), 40);
        assert_eq!(r.rebase(&video(1080, false)), 80);
        // Encoder timestamp reset (back to 0): continue from the current position.
        assert_eq!(r.rebase(&video(0, true)), 80);
        assert_eq!(r.rebase(&video(40, false)), 120);
        // Huge forward jump (e.g. bogus timestamp): absorbed as well.
        assert_eq!(r.rebase(&video(5_000_000, false)), 120);
        assert_eq!(r.rebase(&video(5_000_033, false)), 153);
        // 32-bit wrap-around is a small positive delta.
        let mut w = TimestampRebaser::new();
        assert_eq!(w.rebase(&audio(u32::MAX - 10)), 0);
        assert_eq!(w.rebase(&audio(12)), 23);
    }

    #[tokio::test]
    async fn writer_produces_scannable_file() {
        let dir = std::env::temp_dir().join(format!("rms-flvw-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.flv");
        let mut w = FlvWriter::create(&path, true, true).await.unwrap();
        // Exists already: create_new refuses to overwrite.
        assert!(FlvWriter::create(&path, true, true).await.is_err());
        w.write(&MediaPacket::script(0, Bytes::from_static(b"meta")))
            .await
            .unwrap();
        w.write(&video_header(0)).await.unwrap();
        w.write(&audio_header(0)).await.unwrap();
        for i in 0..90u32 {
            w.write(&video(50_000 + i * 33, i % 30 == 0)).await.unwrap();
            w.write(&audio(50_000 + i * 33 + 5)).await.unwrap();
        }
        assert_eq!(w.media_packets(), 180);
        let stats = w.finish().await.unwrap();
        let len = std::fs::metadata(&path).unwrap().len();
        assert_eq!(stats.bytes, len);
        assert_eq!(stats.duration_ms, 89 * 33 + 5);
        assert_eq!(stats.media_packets, 180);

        let scan = scan_flv(&path).unwrap();
        assert_eq!(scan.valid_len, len);
        assert_eq!(scan.media_tags, 180);
        assert_eq!(scan.duration_ms, stats.duration_ms);
        // First media tag (after script + 2 headers) has timestamp 0.
        let raw = std::fs::read(&path).unwrap();
        let mut pos = FLV_HEADER_LEN;
        for _ in 0..3 {
            let size = u32::from_be_bytes([0, raw[pos + 1], raw[pos + 2], raw[pos + 3]]) as usize;
            pos += FLV_TAG_HEADER_LEN + size + 4;
        }
        assert_eq!(raw[pos], FLV_TAG_VIDEO);
        assert_eq!(&raw[pos + 4..pos + 8], &[0, 0, 0, 0]);

        // Simulate a crash in the middle of a tag: repair truncates it.
        let f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.set_len(len + 7).unwrap();
        let repaired = repair_flv(&path).unwrap();
        assert_eq!(repaired.valid_len, len);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), len);
        assert_eq!(repaired.media_tags, 180);

        std::fs::write(dir.join("bad.flv"), b"nope").unwrap();
        assert!(scan_flv(&dir.join("bad.flv")).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
