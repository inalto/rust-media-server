//! fMP4 / CMAF muxer for H.264 (AVC) + AAC (ARCHITECTURE.md §6.3–6.5).
//!
//! Hand-rolled ISO-BMFF box writer (`BytesMut` + big-endian puts): everything the
//! packager emits is produced here.
//!
//! - [`init_segment`]: `ftyp` + `moov` (`mvhd`, one `trak` per track with empty sample
//!   tables, `mvex`/`trex`). The `avcC` payload is the AVCDecoderConfigurationRecord
//!   copied verbatim from the FLV sequence header; `esds` wraps the AudioSpecificConfig.
//! - [`fragment`]: one CMAF chunk = one `moof` (`mfhd` + one `traf` per track with
//!   `tfhd` (default-base-is-moof), `tfdt` v1, `trun` v1) followed by one `mdat`
//!   holding the samples of every track in `traf` order.
//!
//! Track ids are fixed: video = [`VIDEO_TRACK_ID`] (timescale [`VIDEO_TIMESCALE`]),
//! audio = [`AUDIO_TRACK_ID`] (timescale = AAC sample rate). Video samples are the
//! AVCC (length-prefixed) NAL units exactly as carried in the FLV body.
//!
//! The FLV tag parsers ([`parse_video_tag`], [`parse_audio_tag`]) classify tag bodies
//! (legacy FLV and Enhanced RTMP) into codec configs, frames, ignorable packets or
//! unsupported codecs.

use bytes::{BufMut, Bytes, BytesMut};

use crate::hub::packet::{
    audio_codec_name, parse_aac_sequence_header, parse_avc_sequence_header, video_codec_name,
};

pub const VIDEO_TRACK_ID: u32 = 1;
pub const AUDIO_TRACK_ID: u32 = 2;
/// Video media timescale (90 kHz, MPEG convention).
pub const VIDEO_TIMESCALE: u32 = 90_000;
/// Samples per AAC frame (AAC-LC; HE-AAC core frames are treated the same way).
pub const AAC_FRAME_SAMPLES: u32 = 1024;

/// `sample_flags` of a sync sample: sample_depends_on = 2 (independent), is_non_sync = 0.
pub const SAMPLE_FLAGS_SYNC: u32 = 0x0200_0000;
/// `sample_flags` of a non-sync sample: sample_depends_on = 1, is_non_sync = 1.
pub const SAMPLE_FLAGS_NON_SYNC: u32 = 0x0101_0000;

// ---------------------------------------------------------------------------
// Codec configuration
// ---------------------------------------------------------------------------

/// H.264 decoder configuration (from the FLV AVC sequence header).
#[derive(Debug, Clone, PartialEq)]
pub struct VideoConfig {
    /// AVCDecoderConfigurationRecord, verbatim.
    pub avcc: Bytes,
    pub width: u32,
    pub height: u32,
    /// RFC 6381 codec string, e.g. `avc1.64001f`.
    pub codec: String,
}

/// AAC decoder configuration (from the FLV AAC sequence header).
#[derive(Debug, Clone, PartialEq)]
pub struct AudioConfig {
    /// AudioSpecificConfig, verbatim.
    pub asc: Bytes,
    pub object_type: u8,
    pub sample_rate: u32,
    pub channels: u8,
    /// RFC 6381 codec string, e.g. `mp4a.40.2`.
    pub codec: String,
}

/// Classified FLV video tag body.
#[derive(Debug, Clone, PartialEq)]
pub enum VideoTag {
    Config(VideoConfig),
    Frame {
        keyframe: bool,
        /// Composition time offset in milliseconds (FLV CTS, signed 24 bit).
        cts_ms: i32,
        /// AVCC length-prefixed NAL units (FLV headers stripped).
        data: Bytes,
    },
    /// End of sequence, command frames, metadata... (nothing to package).
    Ignore,
    /// A codec the packager cannot handle (name for the status message).
    Unsupported(String),
}

/// Classified FLV audio tag body.
#[derive(Debug, Clone, PartialEq)]
pub enum AudioTag {
    Config(AudioConfig),
    /// One raw AAC access unit.
    Frame(Bytes),
    Ignore,
    Unsupported(String),
}

fn i24(b: &[u8]) -> i32 {
    // Sign-extend a big-endian 24-bit value.
    ((((b[0] as u32) << 24) | ((b[1] as u32) << 16) | ((b[2] as u32) << 8)) as i32) >> 8
}

fn video_config(tag: &[u8], record: &[u8]) -> Option<VideoConfig> {
    let info = parse_avc_sequence_header(tag)?;
    if record.len() < 7 || record[0] != 1 {
        return None;
    }
    Some(VideoConfig {
        avcc: Bytes::copy_from_slice(record),
        width: info.width.unwrap_or(0),
        height: info.height.unwrap_or(0),
        codec: info.codec_string,
    })
}

/// Classify an FLV video tag body (legacy `0x17/0x27 AVCPacketType CTS ...` or
/// Enhanced RTMP `ExHeader FourCC ...`). Only H.264 is supported.
pub fn parse_video_tag(data: &Bytes) -> VideoTag {
    let Some(&b0) = data.first() else {
        return VideoTag::Ignore;
    };
    if b0 & 0x80 != 0 {
        // Enhanced RTMP: [IsExHeader|FrameType(3)|PacketType(4)] [FourCC] ...
        if data.len() < 5 {
            return VideoTag::Ignore;
        }
        let frame_type = (b0 >> 4) & 0x07;
        let packet_type = b0 & 0x0F;
        if &data[1..5] != b"avc1" {
            if packet_type == 6 {
                // Multitrack: not supported by the packager.
                return VideoTag::Unsupported("multitrack video".into());
            }
            let name = video_codec_name(data)
                .map(str::to_string)
                .unwrap_or_else(|| String::from_utf8_lossy(&data[1..5]).into_owned());
            return VideoTag::Unsupported(name);
        }
        if frame_type == 5 {
            return VideoTag::Ignore; // command frame
        }
        let keyframe = frame_type == 1;
        return match packet_type {
            0 => match video_config(data, &data[5..]) {
                Some(c) => VideoTag::Config(c),
                None => VideoTag::Ignore,
            },
            1 if data.len() >= 8 => VideoTag::Frame {
                keyframe,
                cts_ms: i24(&data[5..8]),
                data: data.slice(8..),
            },
            3 => VideoTag::Frame {
                keyframe,
                cts_ms: 0,
                data: data.slice(5..),
            },
            _ => VideoTag::Ignore,
        };
    }
    let codec = b0 & 0x0F;
    if codec != 7 {
        let name = video_codec_name(data).unwrap_or("unknown");
        return VideoTag::Unsupported(name.to_string());
    }
    let frame_type = b0 >> 4;
    if frame_type == 5 || data.len() < 5 {
        return VideoTag::Ignore;
    }
    match data[1] {
        0 => match video_config(data, &data[5..]) {
            Some(c) => VideoTag::Config(c),
            None => VideoTag::Ignore,
        },
        1 => VideoTag::Frame {
            keyframe: frame_type == 1,
            cts_ms: i24(&data[2..5]),
            data: data.slice(5..),
        },
        _ => VideoTag::Ignore,
    }
}

fn audio_config(asc: &[u8]) -> Option<AudioConfig> {
    if asc.len() < 2 {
        return None;
    }
    let mut legacy = Vec::with_capacity(asc.len() + 2);
    legacy.extend_from_slice(&[0xAF, 0x00]);
    legacy.extend_from_slice(asc);
    let info = parse_aac_sequence_header(&legacy)?;
    if info.sample_rate == 0 {
        return None;
    }
    Some(AudioConfig {
        asc: Bytes::copy_from_slice(asc),
        object_type: info.object_type,
        sample_rate: info.sample_rate,
        channels: info.channels,
        codec: info.codec_string,
    })
}

/// Classify an FLV audio tag body (legacy AAC `0xAF AACPacketType ...` or Enhanced
/// RTMP `0x9? FourCC ...`). Only AAC is supported.
pub fn parse_audio_tag(data: &Bytes) -> AudioTag {
    let Some(&b0) = data.first() else {
        return AudioTag::Ignore;
    };
    match b0 >> 4 {
        10 => {
            if data.len() < 2 {
                return AudioTag::Ignore;
            }
            match data[1] {
                0 => audio_config(&data[2..]).map_or(AudioTag::Ignore, AudioTag::Config),
                1 if data.len() > 2 => AudioTag::Frame(data.slice(2..)),
                _ => AudioTag::Ignore,
            }
        }
        9 => {
            if data.len() < 5 {
                return AudioTag::Ignore;
            }
            if &data[1..5] != b"mp4a" {
                let name = audio_codec_name(data)
                    .map(str::to_string)
                    .unwrap_or_else(|| String::from_utf8_lossy(&data[1..5]).into_owned());
                return AudioTag::Unsupported(name);
            }
            match b0 & 0x0F {
                0 => audio_config(&data[5..]).map_or(AudioTag::Ignore, AudioTag::Config),
                1 if data.len() > 5 => AudioTag::Frame(data.slice(5..)),
                _ => AudioTag::Ignore,
            }
        }
        _ => AudioTag::Unsupported(audio_codec_name(data).unwrap_or("unknown").to_string()),
    }
}

// ---------------------------------------------------------------------------
// Box writer
// ---------------------------------------------------------------------------

/// Write a box: 32-bit size placeholder + type, body from `f`, then patch the size.
fn put_box(buf: &mut BytesMut, typ: &[u8; 4], f: impl FnOnce(&mut BytesMut)) {
    let start = buf.len();
    buf.put_u32(0);
    buf.put_slice(typ);
    f(buf);
    let size = (buf.len() - start) as u32;
    buf[start..start + 4].copy_from_slice(&size.to_be_bytes());
}

/// Write a full box (version + 24-bit flags).
fn put_full_box(
    buf: &mut BytesMut,
    typ: &[u8; 4],
    version: u8,
    flags: u32,
    f: impl FnOnce(&mut BytesMut),
) {
    put_box(buf, typ, |b| {
        b.put_u32(((version as u32) << 24) | (flags & 0x00FF_FFFF));
        f(b);
    });
}

const UNITY_MATRIX: [u32; 9] = [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000];

fn put_matrix(b: &mut BytesMut) {
    for v in UNITY_MATRIX {
        b.put_u32(v);
    }
}

/// ISO-639-2/T "und" packed as 3×5 bits.
const LANGUAGE_UND: u16 =
    ((b'u' - 0x60) as u16) << 10 | ((b'n' - 0x60) as u16) << 5 | (b'd' - 0x60) as u16;

fn put_ftyp(b: &mut BytesMut, with_avc1: bool) {
    put_box(b, b"ftyp", |b| {
        b.put_slice(b"iso6");
        b.put_u32(0);
        let mut brands: Vec<&[u8; 4]> = vec![b"isom", b"iso6", b"mp41", b"cmfc"];
        if with_avc1 {
            brands.push(b"avc1");
        }
        for brand in brands {
            b.put_slice(brand);
        }
    });
}

fn put_mvhd(b: &mut BytesMut, next_track_id: u32) {
    put_full_box(b, b"mvhd", 0, 0, |b| {
        b.put_u32(0); // creation_time
        b.put_u32(0); // modification_time
        b.put_u32(1000); // timescale
        b.put_u32(0); // duration (fragmented)
        b.put_u32(0x0001_0000); // rate 1.0
        b.put_u16(0x0100); // volume 1.0
        b.put_u16(0);
        b.put_u64(0); // reserved
        put_matrix(b);
        for _ in 0..6 {
            b.put_u32(0); // pre_defined
        }
        b.put_u32(next_track_id);
    });
}

fn put_tkhd(b: &mut BytesMut, track_id: u32, audio: bool, width: u32, height: u32) {
    // flags: track_enabled | track_in_movie
    put_full_box(b, b"tkhd", 0, 0x000003, |b| {
        b.put_u32(0); // creation_time
        b.put_u32(0); // modification_time
        b.put_u32(track_id);
        b.put_u32(0); // reserved
        b.put_u32(0); // duration
        b.put_u64(0); // reserved
        b.put_u16(0); // layer
        b.put_u16(0); // alternate_group
        b.put_u16(if audio { 0x0100 } else { 0 }); // volume
        b.put_u16(0);
        put_matrix(b);
        b.put_u32(width.min(0xFFFF) << 16);
        b.put_u32(height.min(0xFFFF) << 16);
    });
}

fn put_mdhd(b: &mut BytesMut, timescale: u32) {
    put_full_box(b, b"mdhd", 0, 0, |b| {
        b.put_u32(0);
        b.put_u32(0);
        b.put_u32(timescale);
        b.put_u32(0); // duration
        b.put_u16(LANGUAGE_UND);
        b.put_u16(0);
    });
}

fn put_hdlr(b: &mut BytesMut, handler: &[u8; 4], name: &str) {
    put_full_box(b, b"hdlr", 0, 0, |b| {
        b.put_u32(0); // pre_defined
        b.put_slice(handler);
        b.put_u32(0);
        b.put_u32(0);
        b.put_u32(0);
        b.put_slice(name.as_bytes());
        b.put_u8(0);
    });
}

fn put_dinf(b: &mut BytesMut) {
    put_box(b, b"dinf", |b| {
        put_full_box(b, b"dref", 0, 0, |b| {
            b.put_u32(1); // entry_count
            put_full_box(b, b"url ", 0, 0x000001, |_| {}); // self-contained
        });
    });
}

fn put_empty_sample_tables(b: &mut BytesMut) {
    put_full_box(b, b"stts", 0, 0, |b| b.put_u32(0));
    put_full_box(b, b"stsc", 0, 0, |b| b.put_u32(0));
    put_full_box(b, b"stsz", 0, 0, |b| {
        b.put_u32(0); // sample_size
        b.put_u32(0); // sample_count
    });
    put_full_box(b, b"stco", 0, 0, |b| b.put_u32(0));
}

fn put_avc1(b: &mut BytesMut, v: &VideoConfig) {
    put_box(b, b"avc1", |b| {
        b.put_slice(&[0; 6]); // reserved
        b.put_u16(1); // data_reference_index
        b.put_u16(0); // pre_defined
        b.put_u16(0); // reserved
        b.put_slice(&[0; 12]); // pre_defined
        b.put_u16(v.width.min(0xFFFF) as u16);
        b.put_u16(v.height.min(0xFFFF) as u16);
        b.put_u32(0x0048_0000); // 72 dpi
        b.put_u32(0x0048_0000);
        b.put_u32(0); // reserved
        b.put_u16(1); // frame_count
        b.put_slice(&[0; 32]); // compressorname (empty pascal string)
        b.put_u16(0x0018); // depth
        b.put_i16(-1); // pre_defined
        put_box(b, b"avcC", |b| b.put_slice(&v.avcc));
    });
}

/// MPEG-4 descriptor header (tag + expandable size, 1 byte when < 128, else 4 bytes).
fn put_descriptor(b: &mut BytesMut, tag: u8, body: &[u8]) {
    b.put_u8(tag);
    let len = body.len() as u32;
    if len < 0x80 {
        b.put_u8(len as u8);
    } else {
        b.put_u8(0x80 | ((len >> 21) & 0x7F) as u8);
        b.put_u8(0x80 | ((len >> 14) & 0x7F) as u8);
        b.put_u8(0x80 | ((len >> 7) & 0x7F) as u8);
        b.put_u8((len & 0x7F) as u8);
    }
    b.put_slice(body);
}

fn esds_payload(a: &AudioConfig) -> BytesMut {
    let mut dsi = BytesMut::new();
    put_descriptor(&mut dsi, 0x05, &a.asc);
    let mut dcd = BytesMut::new();
    dcd.put_u8(0x40); // objectTypeIndication: Audio ISO/IEC 14496-3
    dcd.put_u8((0x05 << 2) | 0x01); // streamType audio, upStream 0, reserved 1
    dcd.put_slice(&[0, 0, 0]); // bufferSizeDB
    dcd.put_u32(0); // maxBitrate (unknown)
    dcd.put_u32(0); // avgBitrate (unknown)
    dcd.put_slice(&dsi);
    let mut es = BytesMut::new();
    es.put_u16(0); // ES_ID
    es.put_u8(0); // flags
    put_descriptor(&mut es, 0x04, &dcd);
    put_descriptor(&mut es, 0x06, &[0x02]); // SLConfigDescriptor, predefined = MP4
    let mut out = BytesMut::new();
    put_descriptor(&mut out, 0x03, &es);
    out
}

fn put_mp4a(b: &mut BytesMut, a: &AudioConfig) {
    put_box(b, b"mp4a", |b| {
        b.put_slice(&[0; 6]); // reserved
        b.put_u16(1); // data_reference_index
        b.put_u64(0); // version, revision, vendor
        let channels = if a.channels == 0 {
            2
        } else {
            a.channels as u16
        };
        b.put_u16(channels);
        b.put_u16(16); // samplesize
        b.put_u16(0); // pre_defined
        b.put_u16(0); // reserved
        b.put_u32(a.sample_rate.min(0xFFFF) << 16);
        let payload = esds_payload(a);
        put_full_box(b, b"esds", 0, 0, |b| b.put_slice(&payload));
    });
}

fn put_video_trak(b: &mut BytesMut, v: &VideoConfig) {
    put_box(b, b"trak", |b| {
        put_tkhd(b, VIDEO_TRACK_ID, false, v.width, v.height);
        put_box(b, b"mdia", |b| {
            put_mdhd(b, VIDEO_TIMESCALE);
            put_hdlr(b, b"vide", "VideoHandler");
            put_box(b, b"minf", |b| {
                put_full_box(b, b"vmhd", 0, 0x000001, |b| {
                    b.put_u16(0); // graphicsmode
                    b.put_slice(&[0; 6]); // opcolor
                });
                put_dinf(b);
                put_box(b, b"stbl", |b| {
                    put_full_box(b, b"stsd", 0, 0, |b| {
                        b.put_u32(1);
                        put_avc1(b, v);
                    });
                    put_empty_sample_tables(b);
                });
            });
        });
    });
}

fn put_audio_trak(b: &mut BytesMut, a: &AudioConfig) {
    put_box(b, b"trak", |b| {
        put_tkhd(b, AUDIO_TRACK_ID, true, 0, 0);
        put_box(b, b"mdia", |b| {
            put_mdhd(b, a.sample_rate);
            put_hdlr(b, b"soun", "SoundHandler");
            put_box(b, b"minf", |b| {
                put_full_box(b, b"smhd", 0, 0, |b| {
                    b.put_u16(0); // balance
                    b.put_u16(0);
                });
                put_dinf(b);
                put_box(b, b"stbl", |b| {
                    put_full_box(b, b"stsd", 0, 0, |b| {
                        b.put_u32(1);
                        put_mp4a(b, a);
                    });
                    put_empty_sample_tables(b);
                });
            });
        });
    });
}

fn put_trex(b: &mut BytesMut, track_id: u32) {
    put_full_box(b, b"trex", 0, 0, |b| {
        b.put_u32(track_id);
        b.put_u32(1); // default_sample_description_index
        b.put_u32(0); // default_sample_duration
        b.put_u32(0); // default_sample_size
        b.put_u32(0); // default_sample_flags
    });
}

/// Initialization segment (`ftyp` + `moov`) with the given tracks (at least one).
pub fn init_segment(video: Option<&VideoConfig>, audio: Option<&AudioConfig>) -> Bytes {
    let mut b = BytesMut::with_capacity(1024);
    put_ftyp(&mut b, video.is_some());
    put_box(&mut b, b"moov", |b| {
        put_mvhd(b, AUDIO_TRACK_ID + 1);
        if let Some(v) = video {
            put_video_trak(b, v);
        }
        if let Some(a) = audio {
            put_audio_trak(b, a);
        }
        put_box(b, b"mvex", |b| {
            if video.is_some() {
                put_trex(b, VIDEO_TRACK_ID);
            }
            if audio.is_some() {
                put_trex(b, AUDIO_TRACK_ID);
            }
        });
    });
    b.freeze()
}

// ---------------------------------------------------------------------------
// Fragments (CMAF chunks)
// ---------------------------------------------------------------------------

/// One media sample of a fragment.
#[derive(Debug, Clone, PartialEq)]
pub struct Sample {
    /// Duration in track timescale units.
    pub duration: u32,
    /// Composition time offset (pts - dts) in track timescale units.
    pub cts: i32,
    pub sync: bool,
    pub data: Bytes,
}

/// The samples of one track inside a fragment.
#[derive(Debug, Clone, Copy)]
pub struct TrackRun<'a> {
    pub track_id: u32,
    /// `tfdt` baseMediaDecodeTime (decode time of the first sample).
    pub base_decode_time: u64,
    pub samples: &'a [Sample],
    /// Audio runs omit per-sample flags / composition offsets (every sample is a
    /// sync sample, signalled through `tfhd` default-sample-flags).
    pub audio: bool,
}

const TFHD_DEFAULT_BASE_IS_MOOF: u32 = 0x020000;
const TFHD_DEFAULT_SAMPLE_FLAGS: u32 = 0x000020;
const TRUN_DATA_OFFSET: u32 = 0x000001;
const TRUN_SAMPLE_DURATION: u32 = 0x000100;
const TRUN_SAMPLE_SIZE: u32 = 0x000200;
const TRUN_SAMPLE_FLAGS: u32 = 0x000400;
const TRUN_SAMPLE_CTO: u32 = 0x000800;

/// One CMAF chunk: `moof` (one `traf` per non-empty run) + `mdat` with the samples
/// of every run, in run order. `sequence` is the `mfhd` sequence number.
pub fn fragment(sequence: u32, runs: &[TrackRun<'_>]) -> Bytes {
    let runs: Vec<&TrackRun<'_>> = runs.iter().filter(|r| !r.samples.is_empty()).collect();
    let data_len: usize = runs
        .iter()
        .flat_map(|r| r.samples.iter())
        .map(|s| s.data.len())
        .sum();
    let mut b = BytesMut::with_capacity(256 + runs.len() * 64 + data_len);
    // Positions of the trun data_offset fields, patched once the moof size is known.
    let mut offset_fields = Vec::with_capacity(runs.len());
    put_box(&mut b, b"moof", |b| {
        put_full_box(b, b"mfhd", 0, 0, |b| b.put_u32(sequence));
        for run in &runs {
            put_box(b, b"traf", |b| {
                let mut tfhd_flags = TFHD_DEFAULT_BASE_IS_MOOF;
                if run.audio {
                    tfhd_flags |= TFHD_DEFAULT_SAMPLE_FLAGS;
                }
                put_full_box(b, b"tfhd", 0, tfhd_flags, |b| {
                    b.put_u32(run.track_id);
                    if run.audio {
                        b.put_u32(SAMPLE_FLAGS_SYNC);
                    }
                });
                put_full_box(b, b"tfdt", 1, 0, |b| b.put_u64(run.base_decode_time));
                let mut flags = TRUN_DATA_OFFSET | TRUN_SAMPLE_DURATION | TRUN_SAMPLE_SIZE;
                if !run.audio {
                    flags |= TRUN_SAMPLE_FLAGS | TRUN_SAMPLE_CTO;
                }
                put_full_box(b, b"trun", 1, flags, |b| {
                    b.put_u32(run.samples.len() as u32);
                    offset_fields.push(b.len());
                    b.put_i32(0); // data_offset, patched below
                    for s in run.samples {
                        b.put_u32(s.duration);
                        b.put_u32(s.data.len() as u32);
                        if !run.audio {
                            b.put_u32(if s.sync {
                                SAMPLE_FLAGS_SYNC
                            } else {
                                SAMPLE_FLAGS_NON_SYNC
                            });
                            b.put_i32(s.cts);
                        }
                    }
                });
            });
        }
    });
    let moof_len = b.len();
    let mut offset = moof_len + 8; // first sample byte, relative to the moof start
    for (pos, run) in offset_fields.iter().zip(&runs) {
        b[*pos..*pos + 4].copy_from_slice(&(offset as i32).to_be_bytes());
        offset += run.samples.iter().map(|s| s.data.len()).sum::<usize>();
    }
    b.put_u32((8 + data_len) as u32);
    b.put_slice(b"mdat");
    for run in &runs {
        for s in run.samples {
            b.put_slice(&s.data);
        }
    }
    b.freeze()
}

// ---------------------------------------------------------------------------
// Minimal box reader (tests, diagnostics)
// ---------------------------------------------------------------------------

/// A parsed box: type, payload (after the 8-byte header) and children for the known
/// container types.
#[derive(Debug, Clone)]
pub struct ParsedBox {
    pub typ: [u8; 4],
    pub payload: Bytes,
    pub children: Vec<ParsedBox>,
}

impl ParsedBox {
    pub fn type_str(&self) -> String {
        String::from_utf8_lossy(&self.typ).into_owned()
    }

    /// First descendant (depth-first) of the given type.
    pub fn find(&self, typ: &[u8; 4]) -> Option<&ParsedBox> {
        for c in &self.children {
            if &c.typ == typ {
                return Some(c);
            }
            if let Some(f) = c.find(typ) {
                return Some(f);
            }
        }
        None
    }

    /// All descendants of the given type (depth-first order).
    pub fn find_all<'a>(&'a self, typ: &[u8; 4], out: &mut Vec<&'a ParsedBox>) {
        for c in &self.children {
            if &c.typ == typ {
                out.push(c);
            }
            c.find_all(typ, out);
        }
    }
}

const CONTAINERS: [&[u8; 4]; 10] = [
    b"moov", b"trak", b"mdia", b"minf", b"stbl", b"mvex", b"moof", b"traf", b"dinf", b"edts",
];

/// Parse a sequence of boxes; `None` on any size inconsistency.
pub fn parse_boxes(data: &Bytes) -> Option<Vec<ParsedBox>> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos < data.len() {
        if data.len() - pos < 8 {
            return None;
        }
        let size = u32::from_be_bytes(data[pos..pos + 4].try_into().ok()?) as usize;
        if size < 8 || pos + size > data.len() {
            return None;
        }
        let typ: [u8; 4] = data[pos + 4..pos + 8].try_into().ok()?;
        let payload = data.slice(pos + 8..pos + size);
        let children = if CONTAINERS.contains(&&typ) {
            parse_boxes(&payload)?
        } else if &typ == b"stsd" && payload.len() >= 8 {
            // full box header + entry_count, then sample entries
            let entries = payload.slice(8..);
            let mut v = parse_boxes(&entries)?;
            for e in &mut v {
                // avc1 / mp4a: fixed sample-entry header then child boxes
                let skip = match &e.typ {
                    b"avc1" => 78,
                    b"mp4a" => 28,
                    _ => continue,
                };
                if e.payload.len() >= skip {
                    e.children = parse_boxes(&e.payload.slice(skip..))?;
                }
            }
            v
        } else {
            Vec::new()
        };
        out.push(ParsedBox {
            typ,
            payload,
            children,
        });
        pos += size;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real SPS/PPS from libx264 (High profile, 1280x720).
    const SPS_720P: [u8; 26] = [
        0x67, 0x64, 0x00, 0x1f, 0xac, 0xd9, 0x40, 0x50, 0x05, 0xbb, 0x01, 0x10, 0x00, 0x00, 0x03,
        0x00, 0x10, 0x00, 0x00, 0x03, 0x03, 0xc0, 0xf1, 0x83, 0x19, 0x60,
    ];
    const PPS: [u8; 4] = [0x68, 0xeb, 0xe3, 0xcb];

    pub(crate) fn avc_seq_header() -> Bytes {
        let mut body = vec![0x17, 0x00, 0, 0, 0, 1, 0x64, 0x00, 0x1f, 0xff, 0xe1];
        body.extend_from_slice(&(SPS_720P.len() as u16).to_be_bytes());
        body.extend_from_slice(&SPS_720P);
        body.extend_from_slice(&[1, 0, PPS.len() as u8]);
        body.extend_from_slice(&PPS);
        Bytes::from(body)
    }

    fn u32_at(b: &[u8], i: usize) -> u32 {
        u32::from_be_bytes(b[i..i + 4].try_into().unwrap())
    }

    fn configs() -> (VideoConfig, AudioConfig) {
        let VideoTag::Config(v) = parse_video_tag(&avc_seq_header()) else {
            panic!("expected config")
        };
        let AudioTag::Config(a) = parse_audio_tag(&Bytes::from_static(&[0xAF, 0x00, 0x11, 0x90]))
        else {
            panic!("expected config")
        };
        (v, a)
    }

    #[test]
    fn video_tag_classification() {
        let (v, _) = configs();
        assert_eq!(v.codec, "avc1.64001f");
        assert_eq!((v.width, v.height), (1280, 720));
        assert_eq!(v.avcc[0], 1);
        assert_eq!(&v.avcc[..], &avc_seq_header()[5..]);

        // Legacy NALU packet, keyframe, CTS = 66 ms.
        let t = parse_video_tag(&Bytes::from_static(&[
            0x17, 0x01, 0, 0, 66, 0, 0, 0, 1, 0x65,
        ]));
        assert_eq!(
            t,
            VideoTag::Frame {
                keyframe: true,
                cts_ms: 66,
                data: Bytes::from_static(&[0, 0, 0, 1, 0x65])
            }
        );
        // Negative CTS (sign extension).
        let t = parse_video_tag(&Bytes::from_static(&[0x27, 0x01, 0xFF, 0xFF, 0xFE, 9]));
        assert!(matches!(
            t,
            VideoTag::Frame {
                keyframe: false,
                cts_ms: -2,
                ..
            }
        ));
        // End of sequence.
        assert_eq!(
            parse_video_tag(&Bytes::from_static(&[0x17, 0x02, 0, 0, 0])),
            VideoTag::Ignore
        );
        // HEVC legacy (codec id 12) and enhanced hvc1 / av01 are unsupported.
        assert_eq!(
            parse_video_tag(&Bytes::from_static(&[0x1C, 0x01, 0, 0, 0])),
            VideoTag::Unsupported("hevc".into())
        );
        assert_eq!(
            parse_video_tag(&Bytes::from_static(&[
                0x91, b'h', b'v', b'c', b'1', 0, 0, 0
            ])),
            VideoTag::Unsupported("hevc".into())
        );
        assert_eq!(
            parse_video_tag(&Bytes::from_static(&[0x91, b'a', b'v', b'0', b'1', 0])),
            VideoTag::Unsupported("av1".into())
        );
        // Enhanced avc1: CodedFrames (with CTS) and CodedFramesX (no CTS).
        let t = parse_video_tag(&Bytes::from_static(&[
            0x91, b'a', b'v', b'c', b'1', 0, 0, 33, 0, 0, 0, 1, 0x65,
        ]));
        assert!(
            matches!(t, VideoTag::Frame { keyframe: true, cts_ms: 33, ref data } if data.len() == 5)
        );
        let t = parse_video_tag(&Bytes::from_static(&[
            0xA3, b'a', b'v', b'c', b'1', 0, 0, 0, 1, 0x41,
        ]));
        assert!(
            matches!(t, VideoTag::Frame { keyframe: false, cts_ms: 0, ref data } if data.len() == 5)
        );
        // Enhanced avc1 sequence start.
        let mut enh = vec![0x90, b'a', b'v', b'c', b'1'];
        enh.extend_from_slice(&avc_seq_header()[5..]);
        assert!(matches!(
            parse_video_tag(&Bytes::from(enh)),
            VideoTag::Config(_)
        ));
        // Garbage never panics.
        for n in 0..12 {
            let _ = parse_video_tag(&avc_seq_header().slice(..n));
        }
        assert_eq!(parse_video_tag(&Bytes::new()), VideoTag::Ignore);
    }

    #[test]
    fn audio_tag_classification() {
        let (_, a) = configs();
        assert_eq!((a.sample_rate, a.channels, a.object_type), (48000, 2, 2));
        assert_eq!(a.codec, "mp4a.40.2");
        assert_eq!(
            parse_audio_tag(&Bytes::from_static(&[0xAF, 0x01, 0x21, 0x10])),
            AudioTag::Frame(Bytes::from_static(&[0x21, 0x10]))
        );
        assert_eq!(
            parse_audio_tag(&Bytes::from_static(&[0x2F, 0xFF, 0xFB])),
            AudioTag::Unsupported("mp3".into())
        );
        assert_eq!(
            parse_audio_tag(&Bytes::from_static(&[0x91, b'O', b'p', b'u', b's', 1])),
            AudioTag::Unsupported("opus".into())
        );
        assert!(matches!(
            parse_audio_tag(&Bytes::from_static(&[
                0x90, b'm', b'p', b'4', b'a', 0x11, 0x90
            ])),
            AudioTag::Config(_)
        ));
        assert_eq!(
            parse_audio_tag(&Bytes::from_static(&[0x91, b'm', b'p', b'4', b'a', 7])),
            AudioTag::Frame(Bytes::from_static(&[7]))
        );
        assert_eq!(
            parse_audio_tag(&Bytes::from_static(&[0xAF])),
            AudioTag::Ignore
        );
        assert_eq!(
            parse_audio_tag(&Bytes::from_static(&[0xAF, 0x00, 0x11])),
            AudioTag::Ignore
        );
    }

    #[test]
    fn init_segment_structure() {
        let (v, a) = configs();
        let init = init_segment(Some(&v), Some(&a));
        let boxes = parse_boxes(&init).expect("well-formed");
        let types: Vec<String> = boxes.iter().map(ParsedBox::type_str).collect();
        assert_eq!(types, ["ftyp", "moov"]);
        let ftyp = &boxes[0].payload;
        assert_eq!(&ftyp[..4], b"iso6");
        let brands: Vec<&[u8]> = ftyp[8..].chunks(4).collect();
        assert!(brands.contains(&&b"cmfc"[..]) && brands.contains(&&b"isom"[..]));

        let moov = &boxes[1];
        let kids: Vec<String> = moov.children.iter().map(ParsedBox::type_str).collect();
        assert_eq!(kids, ["mvhd", "trak", "trak", "mvex"]);
        let mvhd = &moov.children[0].payload;
        assert_eq!(mvhd.len(), 100);
        assert_eq!(u32_at(mvhd, 96), 3, "next_track_ID");

        // Video track.
        let vtrak = &moov.children[1];
        let tkhd = &vtrak.children[0].payload;
        assert_eq!(u32_at(tkhd, 0) & 0xFFFFFF, 3, "tkhd flags");
        assert_eq!(u32_at(tkhd, 12), VIDEO_TRACK_ID);
        assert_eq!(u32_at(tkhd, 76) >> 16, 1280);
        assert_eq!(u32_at(tkhd, 80) >> 16, 720);
        let mdhd = &vtrak.find(b"mdhd").unwrap().payload;
        assert_eq!(u32_at(mdhd, 12), VIDEO_TIMESCALE);
        assert_eq!(&vtrak.find(b"hdlr").unwrap().payload[8..12], b"vide");
        assert!(vtrak.find(b"vmhd").is_some());
        let dref = &vtrak.find(b"dinf").unwrap().payload;
        assert_eq!(&dref[4..8], b"dref");
        assert_eq!(&dref[16..24], &[0, 0, 0, 12, b'u', b'r', b'l', b' ']);
        for t in [b"stts", b"stsc", b"stsz", b"stco"] {
            assert!(vtrak.find(t).is_some(), "missing {t:?}");
        }
        let avc1 = vtrak.find(b"avc1").unwrap();
        assert_eq!(
            u16::from_be_bytes([avc1.payload[24], avc1.payload[25]]),
            1280
        );
        assert_eq!(
            u16::from_be_bytes([avc1.payload[26], avc1.payload[27]]),
            720
        );
        let avcc = vtrak.find(b"avcC").unwrap();
        assert_eq!(&avcc.payload[..], &v.avcc[..]);

        // Audio track.
        let atrak = &moov.children[2];
        assert_eq!(u32_at(&atrak.children[0].payload, 12), AUDIO_TRACK_ID);
        assert_eq!(u32_at(&atrak.find(b"mdhd").unwrap().payload, 12), 48000);
        assert_eq!(&atrak.find(b"hdlr").unwrap().payload[8..12], b"soun");
        assert!(atrak.find(b"smhd").is_some());
        let mp4a = atrak.find(b"mp4a").unwrap();
        assert_eq!(u16::from_be_bytes([mp4a.payload[16], mp4a.payload[17]]), 2);
        assert_eq!(u32_at(&mp4a.payload, 24) >> 16, 48000);
        let esds = &atrak.find(b"esds").unwrap().payload;
        // version/flags, ES_Descriptor (tag 3, ES_ID, flags), DecoderConfigDescriptor
        // (tag 4, OTI 0x40, stream type audio), DecoderSpecificInfo (tag 5) = ASC, SLConfig.
        assert_eq!(&esds[..4], &[0, 0, 0, 0]);
        assert_eq!(esds[4], 0x03);
        assert_eq!(esds[5] as usize, esds.len() - 6);
        assert_eq!(esds[9], 0x04);
        assert_eq!((esds[11], esds[12]), (0x40, 0x15));
        assert_eq!(&esds[24..28], &[0x05, 2, a.asc[0], a.asc[1]]);
        assert_eq!(&esds[esds.len() - 3..], &[0x06, 0x01, 0x02]);

        // mvex/trex per track.
        let mut trex = Vec::new();
        moov.find_all(b"trex", &mut trex);
        assert_eq!(trex.len(), 2);
        assert_eq!(u32_at(&trex[0].payload, 4), VIDEO_TRACK_ID);
        assert_eq!(u32_at(&trex[1].payload, 4), AUDIO_TRACK_ID);

        // Single-track inits (DASH).
        let vi = parse_boxes(&init_segment(Some(&v), None)).unwrap();
        let kids: Vec<String> = vi[1].children.iter().map(ParsedBox::type_str).collect();
        assert_eq!(kids, ["mvhd", "trak", "mvex"]);
        let ai = parse_boxes(&init_segment(None, Some(&a))).unwrap();
        assert!(ai[1].find(b"mp4a").is_some() && ai[1].find(b"avc1").is_none());
    }

    #[test]
    fn fragment_layout_and_offsets() {
        let video = [
            Sample {
                duration: 3000,
                cts: 6000,
                sync: true,
                data: Bytes::from_static(&[0, 0, 0, 2, 0x65, 0x88]),
            },
            Sample {
                duration: 3000,
                cts: -3000,
                sync: false,
                data: Bytes::from_static(&[0, 0, 0, 1, 0x41]),
            },
        ];
        let audio = [
            Sample {
                duration: 1024,
                cts: 0,
                sync: true,
                data: Bytes::from_static(&[0xA1, 0xA2, 0xA3]),
            },
            Sample {
                duration: 1024,
                cts: 0,
                sync: true,
                data: Bytes::from_static(&[0xB1]),
            },
        ];
        let base_v = 162_000_000_000_000u64;
        let frag = fragment(
            7,
            &[
                TrackRun {
                    track_id: VIDEO_TRACK_ID,
                    base_decode_time: base_v,
                    samples: &video,
                    audio: false,
                },
                TrackRun {
                    track_id: AUDIO_TRACK_ID,
                    base_decode_time: 4096,
                    samples: &audio,
                    audio: true,
                },
            ],
        );
        let boxes = parse_boxes(&frag).unwrap();
        assert_eq!(boxes.len(), 2);
        assert_eq!(boxes[0].type_str(), "moof");
        assert_eq!(boxes[1].type_str(), "mdat");
        let moof = &boxes[0];
        let moof_size = moof.payload.len() + 8;
        assert_eq!(u32_at(&moof.children[0].payload, 4), 7, "mfhd sequence");
        let mut trafs = Vec::new();
        moof.find_all(b"traf", &mut trafs);
        assert_eq!(trafs.len(), 2);

        // Video traf.
        let tfhd = &trafs[0].children[0];
        assert_eq!(tfhd.type_str(), "tfhd");
        assert_eq!(u32_at(&tfhd.payload, 0), 0x020000);
        assert_eq!(u32_at(&tfhd.payload, 4), VIDEO_TRACK_ID);
        let tfdt = &trafs[0].children[1].payload;
        assert_eq!(tfdt[0], 1, "tfdt version 1");
        assert_eq!(u64::from_be_bytes(tfdt[4..12].try_into().unwrap()), base_v);
        let trun = &trafs[0].children[2].payload;
        assert_eq!(trun[0], 1, "trun version 1");
        assert_eq!(u32_at(trun, 0) & 0xFFFFFF, 0x000F01);
        assert_eq!(u32_at(trun, 4), 2, "sample_count");
        let v_offset = u32_at(trun, 8) as usize;
        assert_eq!(v_offset, moof_size + 8);
        // sample 0: duration, size, flags, cts
        assert_eq!(
            [u32_at(trun, 12), u32_at(trun, 16), u32_at(trun, 20)],
            [3000, 6, SAMPLE_FLAGS_SYNC]
        );
        assert_eq!(u32_at(trun, 24) as i32, 6000);
        assert_eq!(u32_at(trun, 36), SAMPLE_FLAGS_NON_SYNC);
        assert_eq!(u32_at(trun, 40) as i32, -3000);

        // Audio traf: default sample flags in tfhd, no per-sample flags / cto.
        let tfhd = &trafs[1].children[0].payload;
        assert_eq!(u32_at(tfhd, 0), 0x020020);
        assert_eq!(u32_at(tfhd, 8), SAMPLE_FLAGS_SYNC);
        let trun = &trafs[1].children[2].payload;
        assert_eq!(u32_at(trun, 0) & 0xFFFFFF, 0x000301);
        let a_offset = u32_at(trun, 8) as usize;
        assert_eq!(a_offset, v_offset + 11);
        assert_eq!(trun.len(), 12 + 2 * 8);

        // Offsets point at the right bytes.
        assert_eq!(&frag[v_offset..v_offset + 6], &[0, 0, 0, 2, 0x65, 0x88]);
        assert_eq!(&frag[a_offset..a_offset + 4], &[0xA1, 0xA2, 0xA3, 0xB1]);
        assert_eq!(boxes[1].payload.len(), 6 + 5 + 3 + 1);

        // Empty runs are skipped; single-track fragment.
        let single = fragment(
            1,
            &[
                TrackRun {
                    track_id: AUDIO_TRACK_ID,
                    base_decode_time: 0,
                    samples: &audio,
                    audio: true,
                },
                TrackRun {
                    track_id: VIDEO_TRACK_ID,
                    base_decode_time: 0,
                    samples: &[],
                    audio: false,
                },
            ],
        );
        let b = parse_boxes(&single).unwrap();
        let mut trafs = Vec::new();
        b[0].find_all(b"traf", &mut trafs);
        assert_eq!(trafs.len(), 1);
    }

    #[test]
    fn descriptor_sizes() {
        let mut b = BytesMut::new();
        put_descriptor(&mut b, 0x05, &[1, 2]);
        assert_eq!(&b[..], &[0x05, 2, 1, 2]);
        let mut b = BytesMut::new();
        put_descriptor(&mut b, 0x05, &[0u8; 200]);
        assert_eq!(&b[..5], &[0x05, 0x80, 0x80, 0x81, 0x48]);
        assert_eq!(b.len(), 205);
    }
}
