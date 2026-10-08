//! LL-DASH routes (ARCHITECTURE.md §6.4), mounted by `packager::router()`:
//!
//! - `GET /dash/{name}/manifest.mpd`: dynamic MPD (SegmentTimeline, `$Time$`).
//! - `GET /dash/{name}/init-{video|audio}.m4s[?g=<generation>]`.
//! - `GET /dash/{name}/chunk-{video|audio}-{time}.m4s`: complete segment → bytes;
//!   in progress → chunked transfer, one CMAF chunk as soon as it is produced. A
//!   request for a time slightly in the future (the segment after the in-progress one)
//!   waits for that segment to start.
//! - `GET /dash/time`: server UTC time (`http-xsdate` UTCTiming).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::rejection::QueryRejection;
use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::get;
use bytes::Bytes;
use chrono::{SecondsFormat, Utc};
use serde::Deserialize;

use super::lowlat::{
    EgressResult, Proto, STARTUP_WAIT, bytes_response, not_found, resolve, streaming_response,
    text_response, touch, unavailable,
};
use crate::api::BaseUrl;
use crate::error::ApiError;
use crate::packager::StreamPackage;
use crate::packager::dash_mpd::{self, MpdOptions};
use crate::packager::store::{Chunk, StoreState};
use crate::state::AppState;

const MPD_TYPE: &str = "application/dash+xml";

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/dash/time", get(time))
        .route("/dash/{name}/{file}", get(handler))
}

#[derive(Debug, Default, Deserialize)]
pub struct DashQuery {
    /// Init segment generation (`init-video.m4s?g=N` after a codec change).
    pub g: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rep {
    Video,
    Audio,
}

impl Rep {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "video" => Some(Rep::Video),
            "audio" => Some(Rep::Audio),
            _ => None,
        }
    }

    fn content_type(self) -> &'static str {
        match self {
            Rep::Video => "video/mp4",
            Rep::Audio => "audio/mp4",
        }
    }

    fn pick(self) -> fn(&Chunk) -> Option<Bytes> {
        match self {
            Rep::Video => |c| Some(c.dash_video.clone()),
            Rep::Audio => |c| c.dash_audio.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DashFile {
    Manifest,
    Init(Rep),
    Chunk(Rep, u64),
}

fn parse_file(file: &str) -> Option<DashFile> {
    if file == "manifest.mpd" {
        return Some(DashFile::Manifest);
    }
    let stem = file.strip_suffix(".m4s")?;
    if let Some(rep) = stem.strip_prefix("init-") {
        return Rep::parse(rep).map(DashFile::Init);
    }
    let (rep, time) = stem.strip_prefix("chunk-")?.split_once('-')?;
    Some(DashFile::Chunk(Rep::parse(rep)?, time.parse().ok()?))
}

/// `GET /dash/time` → `2026-10-08T14:00:00.000Z`.
async fn time() -> Response {
    text_response(
        Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        "text/plain; charset=utf-8",
    )
}

async fn handler(
    State(state): State<AppState>,
    Path((name, file)): Path<(String, String)>,
    query: Result<Query<DashQuery>, QueryRejection>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> EgressResult<Response> {
    let Query(q) = query.map_err(ApiError::from)?;
    let file = parse_file(&file).ok_or_else(|| not_found("not found"))?;
    let pkg = resolve(&state, &name, Proto::Dash).await?;
    match file {
        DashFile::Manifest => {
            touch(&state, &name, Proto::Dash, peer, &headers);
            manifest(&state, &pkg, &headers).await
        }
        DashFile::Init(rep) => init(&state, &pkg, rep, q.g).await,
        DashFile::Chunk(rep, t) => {
            touch(&state, &name, Proto::Dash, peer, &headers);
            chunk(&state, &pkg, rep, t).await
        }
    }
}

async fn manifest(
    state: &AppState,
    pkg: &StreamPackage,
    headers: &HeaderMap,
) -> EgressResult<Response> {
    let ready = pkg
        .store
        .wait(STARTUP_WAIT, &state.shutdown, |s| {
            (s.hls_ready() || s.ended).then_some(())
        })
        .await;
    if !pkg.store.with(StoreState::hls_ready) {
        return Err(match ready {
            Some(()) => not_found("stream offline"),
            None => unavailable("stream is starting, retry shortly"),
        });
    }
    let info = pkg.stream_info();
    let base = BaseUrl::from_request(&state.config, headers);
    let mut opts = MpdOptions {
        utc_timing_url: format!("{}/dash/time", base.http),
        frame_rate: info.fps,
        ..Default::default()
    };
    // dash.js starts playing at `now - target latency`. Right after the stream started
    // (encoders often burst their first frames faster than real time) that instant can
    // precede the first segment, which dash.js reports as a gap: hold the MPD briefly.
    let early_ms = pkg.store.with(|s| {
        s.segments.front().map(|g| {
            (g.start / 90) as i64 + opts.latency_target_ms as i64 - Utc::now().timestamp_millis()
        })
    });
    if let Some(ms) = early_ms.filter(|ms| *ms > 0) {
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(ms.min(3000) as u64)) => {}
            _ = state.shutdown.cancelled() => {}
        }
    }
    opts.publish_time = Utc::now();
    let body = pkg.store.with(|s| dash_mpd::mpd(s, &opts));
    Ok(text_response(body, MPD_TYPE))
}

async fn init(
    state: &AppState,
    pkg: &StreamPackage,
    rep: Rep,
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
            Some(init.and_then(|i| match rep {
                Rep::Video => Some(i.video.clone()),
                Rep::Audio => i.audio.clone(),
            }))
        })
        .await;
    match found {
        Some(Some(bytes)) => Ok(bytes_response(bytes, rep.content_type(), false)),
        Some(None) => Err(not_found("init segment not found")),
        None => Err(unavailable("stream is starting, retry shortly")),
    }
}

enum Lookup {
    Complete(Bytes),
    InProgress(u64),
    Future,
    Gone,
}

fn lookup(s: &StoreState, rep: Rep, time: u64) -> Lookup {
    let audio = rep == Rep::Audio;
    match dash_mpd::segment_by_time(s, audio, time).and_then(|seq| s.segment(seq)) {
        Some(seg) if seg.complete => {
            let pick = rep.pick();
            let parts: Vec<Bytes> = seg.chunks.iter().filter_map(|c| pick(c)).collect();
            let mut out = Vec::with_capacity(parts.iter().map(Bytes::len).sum());
            for p in &parts {
                out.extend_from_slice(p);
            }
            Lookup::Complete(Bytes::from(out))
        }
        Some(seg) => Lookup::InProgress(seg.seq),
        None if dash_mpd::time_in_near_future(s, audio, time) => Lookup::Future,
        None => Lookup::Gone,
    }
}

async fn chunk(
    state: &AppState,
    pkg: &StreamPackage,
    rep: Rep,
    time: u64,
) -> EgressResult<Response> {
    let (mut found, wait) = pkg.store.with(|s| {
        // Generous: up to two expected segment durations.
        let wait = Duration::from_millis((2 * s.nominal_segment() / 90).max(1000));
        (lookup(s, rep, time), wait)
    });
    if let Lookup::Future = found {
        found = pkg
            .store
            .wait(wait, &state.shutdown, |s| match lookup(s, rep, time) {
                Lookup::Future => None,
                other => Some(other),
            })
            .await
            .ok_or_else(|| not_found("segment not available"))?;
    }
    match found {
        Lookup::Complete(bytes) => Ok(bytes_response(bytes, rep.content_type(), true)),
        Lookup::InProgress(seq) => {
            let idle = pkg
                .store
                .with(|s| Duration::from_millis((3 * s.nominal_segment() / 90).max(3000)));
            let stream =
                Arc::clone(&pkg.store).chunk_stream(seq, rep.pick(), idle, state.shutdown.clone());
            Ok(streaming_response(stream, rep.content_type()))
        }
        Lookup::Future | Lookup::Gone => {
            pkg.store.with(|s| {
                tracing::debug!(
                    broadcast = %pkg.name,
                    ?rep,
                    time,
                    first = ?s.segments.front().map(|g| (g.start, g.audio_start)),
                    last = ?s.segments.back().map(|g| (g.start, g.audio_start)),
                    "DASH segment not found"
                );
            });
            Err(not_found("segment not found"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names() {
        assert_eq!(parse_file("manifest.mpd"), Some(DashFile::Manifest));
        assert_eq!(
            parse_file("init-video.m4s"),
            Some(DashFile::Init(Rep::Video))
        );
        assert_eq!(
            parse_file("init-audio.m4s"),
            Some(DashFile::Init(Rep::Audio))
        );
        assert_eq!(
            parse_file("chunk-video-159050304000000.m4s"),
            Some(DashFile::Chunk(Rep::Video, 159_050_304_000_000))
        );
        assert_eq!(
            parse_file("chunk-audio-0.m4s"),
            Some(DashFile::Chunk(Rep::Audio, 0))
        );
        for bad in [
            "init-text.m4s",
            "chunk-video-.m4s",
            "chunk-video--1.m4s",
            "chunk-x-1.m4s",
            "manifest.xml",
            "",
        ] {
            assert_eq!(parse_file(bad), None, "{bad}");
        }
    }
}
