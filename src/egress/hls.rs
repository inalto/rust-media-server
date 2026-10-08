//! LL-HLS routes (ARCHITECTURE.md §6.3), mounted by `packager::router()`:
//!
//! - `GET /hls/{name}/master.m3u8` (alias `index.m3u8`): one variant → `stream.m3u8`.
//! - `GET /hls/{name}/stream.m3u8[?_HLS_msn=N[&_HLS_part=P]]`: media playlist with
//!   blocking reload (waits until the requested segment/part exists; 400 when `N` is
//!   more than one segment beyond the in-progress one; 503 after 3 × target duration).
//! - `GET /hls/{name}/init.mp4[?g=<generation>]`.
//! - `GET /hls/{name}/seg-N.m4s`: complete → bytes; in progress → chunked, one complete
//!   part at a time, until the segment completes.
//! - `GET /hls/{name}/part-N-P.m4s`: blocks until the part is complete (preload hints),
//!   404 when it will never exist (evicted / beyond the segment), 503 on timeout.
//!
//! Status codes of [`super::lowlat::resolve`]: 404 unknown broadcast, 403 HLS disabled
//! for the broadcast, 404 `stream offline`, 415 unsupported codecs.

use std::net::SocketAddr;
use std::time::Duration;

use axum::Router;
use axum::extract::rejection::QueryRejection;
use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::get;
use serde::Deserialize;

use super::lowlat::{
    EgressResult, Proto, STARTUP_WAIT, bad_request, bytes_response, not_found, resolve,
    streaming_response, text_response, touch, unavailable,
};
use crate::error::ApiError;
use crate::packager::StreamPackage;
use crate::packager::hls_playlist::{self, VariantInfo};
use crate::packager::store::{StoreState, Wait};
use crate::state::AppState;

const PLAYLIST_TYPE: &str = "application/vnd.apple.mpegurl";
const INIT_TYPE: &str = "video/mp4";
const SEGMENT_TYPE: &str = "video/iso.segment";

pub fn routes() -> Router<AppState> {
    Router::new().route("/hls/{name}/{file}", get(handler))
}

#[derive(Debug, Default, Deserialize)]
pub struct HlsQuery {
    #[serde(rename = "_HLS_msn")]
    pub msn: Option<u64>,
    #[serde(rename = "_HLS_part")]
    pub part: Option<u32>,
    /// Init segment generation (`init.mp4?g=N` after a codec change).
    pub g: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HlsFile {
    Master,
    Media,
    Init,
    Segment(u64),
    Part(u64, u32),
}

fn parse_file(file: &str) -> Option<HlsFile> {
    match file {
        "master.m3u8" | "index.m3u8" => return Some(HlsFile::Master),
        "stream.m3u8" => return Some(HlsFile::Media),
        "init.mp4" => return Some(HlsFile::Init),
        _ => {}
    }
    let stem = file.strip_suffix(".m4s")?;
    if let Some(n) = stem.strip_prefix("seg-") {
        return n.parse().ok().map(HlsFile::Segment);
    }
    let (seq, part) = stem.strip_prefix("part-")?.split_once('-')?;
    Some(HlsFile::Part(seq.parse().ok()?, part.parse().ok()?))
}

/// Blocking timeout: 3 × target duration (RFC 8216bis).
fn block_timeout(s: &StoreState) -> Duration {
    Duration::from_secs(3 * hls_playlist::target_duration(s) as u64)
}

async fn handler(
    State(state): State<AppState>,
    Path((name, file)): Path<(String, String)>,
    query: Result<Query<HlsQuery>, QueryRejection>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> EgressResult<Response> {
    let Query(q) = query.map_err(ApiError::from)?;
    let file = parse_file(&file).ok_or_else(|| not_found("not found"))?;
    let pkg = resolve(&state, &name, Proto::Hls).await?;
    if file != HlsFile::Init {
        touch(&state, &name, Proto::Hls, peer, &headers);
    }
    match file {
        HlsFile::Master => master(&state, &pkg).await,
        HlsFile::Media => media(&state, &pkg, &q).await,
        HlsFile::Init => init(&state, &pkg, q.g).await,
        HlsFile::Segment(seq) => segment(&state, &pkg, seq).await,
        HlsFile::Part(seq, part) => part_file(&state, &pkg, seq, part).await,
    }
}

/// Wait (bounded) until the first segment exists so players never see an empty
/// playlist; 503 if the stream does not produce one in time.
async fn wait_ready(state: &AppState, pkg: &StreamPackage) -> EgressResult<()> {
    let ready = pkg
        .store
        .wait(STARTUP_WAIT, &state.shutdown, |s| {
            (s.hls_ready() || s.ended).then_some(())
        })
        .await;
    match (ready, pkg.store.with(StoreState::hls_ready)) {
        (_, true) => Ok(()),
        (Some(()), false) => Err(not_found("stream offline")),
        (None, false) => Err(unavailable("stream is starting, retry shortly")),
    }
}

async fn master(state: &AppState, pkg: &StreamPackage) -> EgressResult<Response> {
    wait_ready(state, pkg).await?;
    let info = pkg.stream_info();
    let declared = match (info.video_bitrate_kbps, info.audio_bitrate_kbps) {
        (Some(v), a) => Some(v + a.unwrap_or(0.0)),
        _ => None,
    };
    let variant = pkg.store.with(|s| {
        let init = s.init()?;
        let (bandwidth, average_bandwidth) = hls_playlist::variant_bandwidth(s, declared);
        let v = &init.video_config;
        Some(VariantInfo {
            bandwidth,
            average_bandwidth,
            codecs: init.codecs(),
            resolution: Some((v.width, v.height)),
            frame_rate: info.fps,
        })
    });
    let variant = variant.ok_or_else(|| unavailable("stream is starting, retry shortly"))?;
    Ok(text_response(
        hls_playlist::master_playlist(&variant),
        PLAYLIST_TYPE,
    ))
}

/// Whether the playlist satisfies a blocking request for `msn` (and `part`).
fn satisfies(s: &StoreState, msn: u64, part: Option<u32>) -> bool {
    if s.ended {
        return true;
    }
    match s.segment(msn) {
        Some(seg) => match part {
            Some(p) => seg.complete || seg.chunks.len() > p as usize,
            None => seg.complete,
        },
        // Older than the window: already satisfied. Newer: not started yet.
        None => s.last_seq().is_some_and(|last| last > msn),
    }
}

async fn media(state: &AppState, pkg: &StreamPackage, q: &HlsQuery) -> EgressResult<Response> {
    if q.part.is_some() && q.msn.is_none() {
        return Err(bad_request("_HLS_part requires _HLS_msn"));
    }
    wait_ready(state, pkg).await?;
    if let Some(msn) = q.msn {
        let (limit, timeout) = pkg.store.with(|s| {
            let last_complete = s.complete_segments().next_back().map_or(0, |g| g.seq);
            (last_complete + 2, block_timeout(s))
        });
        if msn > limit {
            return Err(bad_request(
                "_HLS_msn is more than one segment beyond the live edge",
            ));
        }
        let ok = pkg
            .store
            .wait(timeout, &state.shutdown, |s| {
                satisfies(s, msn, q.part).then_some(())
            })
            .await;
        if ok.is_none() {
            return Err(unavailable("playlist update timed out"));
        }
    }
    let body = pkg.store.with(hls_playlist::media_playlist);
    Ok(text_response(body, PLAYLIST_TYPE))
}

async fn init(
    state: &AppState,
    pkg: &StreamPackage,
    generation: Option<u32>,
) -> EgressResult<Response> {
    let found = pkg
        .store
        .wait(STARTUP_WAIT, &state.shutdown, |s| {
            if s.inits.is_empty() {
                return s.ended.then_some(None);
            }
            let init = match generation {
                Some(g) => s.init_gen(g),
                None => s.init_gen(0).or_else(|| s.init()),
            };
            Some(init.map(|i| i.hls.clone()))
        })
        .await;
    match found {
        Some(Some(bytes)) => Ok(bytes_response(bytes, INIT_TYPE, false)),
        Some(None) => Err(not_found("init segment not found")),
        None => Err(unavailable("stream is starting, retry shortly")),
    }
}

enum SegmentState {
    Complete(bytes::Bytes),
    InProgress,
    NotYet,
    Gone,
}

async fn segment(state: &AppState, pkg: &StreamPackage, seq: u64) -> EgressResult<Response> {
    let lookup = |s: &StoreState| match s.segment(seq) {
        Some(seg) if seg.complete => SegmentState::Complete(seg.hls_bytes()),
        Some(_) => SegmentState::InProgress,
        None if !s.ended && s.last_seq().is_some_and(|l| l + 1 == seq) => SegmentState::NotYet,
        None => SegmentState::Gone,
    };
    let (mut st, timeout) = pkg.store.with(|s| (lookup(s), block_timeout(s)));
    if let SegmentState::NotYet = st {
        // Requested right before it starts: wait for it to open.
        st = pkg
            .store
            .wait(timeout, &state.shutdown, |s| match lookup(s) {
                SegmentState::NotYet => None,
                other => Some(other),
            })
            .await
            .ok_or_else(|| unavailable("segment not available yet"))?;
    }
    match st {
        SegmentState::Complete(bytes) => Ok(bytes_response(bytes, SEGMENT_TYPE, true)),
        SegmentState::InProgress => {
            let stream = std::sync::Arc::clone(&pkg.store).chunk_stream(
                seq,
                |c| Some(c.hls.clone()),
                timeout,
                state.shutdown.clone(),
            );
            Ok(streaming_response(stream, SEGMENT_TYPE))
        }
        SegmentState::NotYet | SegmentState::Gone => Err(not_found("segment not found")),
    }
}

async fn part_file(
    state: &AppState,
    pkg: &StreamPackage,
    seq: u64,
    part: u32,
) -> EgressResult<Response> {
    let timeout = pkg.store.with(block_timeout);
    match pkg
        .store
        .wait_for_part(seq, part, timeout, &state.shutdown)
        .await
    {
        Wait::Ready(chunk) => Ok(bytes_response(chunk.hls.clone(), SEGMENT_TYPE, true)),
        Wait::NotFound => Err(not_found("part not found")),
        Wait::Timeout => Err(unavailable("part not available yet")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names() {
        assert_eq!(parse_file("master.m3u8"), Some(HlsFile::Master));
        assert_eq!(parse_file("index.m3u8"), Some(HlsFile::Master));
        assert_eq!(parse_file("stream.m3u8"), Some(HlsFile::Media));
        assert_eq!(parse_file("init.mp4"), Some(HlsFile::Init));
        assert_eq!(parse_file("seg-17.m4s"), Some(HlsFile::Segment(17)));
        assert_eq!(parse_file("part-17-3.m4s"), Some(HlsFile::Part(17, 3)));
        for bad in [
            "seg-.m4s",
            "seg-x.m4s",
            "part-1.m4s",
            "part-1-2-3.m4s",
            "part--1.m4s",
            "seg-1.mp4",
            "foo.m3u8",
            "",
        ] {
            assert_eq!(parse_file(bad), None, "{bad}");
        }
    }
}
