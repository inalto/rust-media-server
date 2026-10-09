# rust-media-server (rms) — Architecture & Implementation Contract

This document is the binding contract for everyone (humans and agents) implementing
the project. Module boundaries, data shapes, URL layout and the API contract below
must be respected so that independently developed parts integrate without surprises.
When something is underspecified, prefer the simplest robust choice and document it
in code comments.

Language conventions: code, comments, commit messages, API field names and this
document are in **English**. The management dashboard UI, the embed player texts and
`README.md` are in **Italian** (the operator is Italian).

## 1. Goals

A single-binary live media server written in Rust, inspired by Ant Media Server and
by `xiu` (pure Rust live server):

1. **Ingest from OBS Studio** (and ffmpeg / any RTMP encoder) via RTMP with
   per-broadcast secret stream keys. SRT ingest is optional (ffmpeg bridge).
2. **Low-latency delivery** to browsers with open standards:
   - HTTP-FLV and WebSocket-FLV (≈1–2 s, played with mpegts.js)
   - LL-HLS (Apple Low-Latency HLS, CMAF/fMP4, EXT-X-PART, blocking playlist reload — ≈2–3 s, hls.js / Safari native)
   - LL-DASH (CMAF chunked transfer, `availabilityTimeOffset` — ≈2–3 s, dash.js)
   - WebRTC egress via WHEP (sub-second) is a **stretch goal** (phase 4) and must not block the rest.
3. **Management dashboard** (axum, server-side static SPA + JSON API): create broadcasts,
   show OBS settings, generate embed codes, pause/resume, record on demand,
   upload waiting video, view metrics charts.
4. **Metrics**: concurrent viewers per broadcast per protocol, sampled to SQLite,
   charted over time (Chart.js), peak/total/avg watch time.
5. **Recording**: automatic (per-broadcast flag) or manual start/stop while live;
   pure-Rust FLV writer, remuxed to MP4 with ffmpeg when available.
6. **Waiting video**: per-broadcast (or global default) video looped by the embed
   player when the broadcast is Offline or Paused (admin forced pause), switching
   automatically to the live stream when it arrives.

Non-goals (for now): transcoding/ABR ladders, clustering, HEVC/AV1 packaging
(they pass through FLV paths only), authentication for viewers (tokens), TLS
termination (put nginx/caddy in front).

## 2. Toolchain & dependencies

- Rust stable 1.99 (edition 2024), `cargo` in `~/.cargo/bin`.
- `ffmpeg`/`ffprobe` static builds in `~/.local/bin` (optional at runtime; detect with `which`
  or config path; every feature must degrade gracefully when missing).
- Crates (versions verified on crates.io 2026-10-08):
  - `tokio` 1 (full), `axum` 0.8 (features: ws, multipart, macros), `tower-http` 0.7
    (fs, cors, trace, compression? no — never compress media), `tower` 0.5
  - `rtmp-rs` 0.6 — RTMP server with `RtmpHandler` callbacks (`on_publish` auth,
    `on_media_tag` raw FLV tags, `on_metadata`, `on_unpublish`). Source is downloaded by cargo
    to `~/.cargo/registry/src/*/rtmp-rs-0.6.0/` — read it when in doubt (also a copy in the
    session scratchpad). Use `MediaDeliveryMode::RawFlv`.
  - `rusqlite` 0.40 (features: bundled) — SQLite, accessed via a dedicated blocking thread/`spawn_blocking`
    behind an async wrapper (`db::Db`).
  - `serde`, `serde_json`, `toml`, `uuid` (v4), `chrono`, `bytes`, `anyhow`, `thiserror`,
    `tracing`, `tracing-subscriber` (env-filter), `rand`, `sha2`/`hex` (fingerprints), `mime_guess`,
    `tokio-util` (io), `futures`, `async-stream`, `clap` 4 (derive) for CLI flags,
    `mp4-atom` 0.16 (ISOBMFF box encoding for the CMAF packager) — optional; hand-rolled
    box writer is acceptable if simpler.
  - Frontend libs via CDN (cdnjs / jsdelivr), pinned versions: hls.js ≥1.5, mpegts.js ≥1.7, dash.js ≥4.7, Chart.js ≥4.4.
    They must be loaded from the CDN with fixed versions; no build step for the frontend.

## 3. Repository layout

```
rust-media-server/
  Cargo.toml                 crate "rust-media-server", binary "rms"
  README.md                  Italian user guide (install, OBS setup, URLs, API, embed)
  config.example.toml
  docs/ARCHITECTURE.md       this file
  docs/PROTOCOLS.md          Italian: proposed low-latency standards and why (phase 3)
  docs/API.md                REST/SSE API reference (phase 3; keep in sync with §8)
  scripts/test-publish.sh    ffmpeg test-pattern publisher to rtmp://localhost:1935/live/<key>
  scripts/smoke.sh           curl-based smoke test of the HTTP surface
  src/
    main.rs                  CLI (clap): --config, --data-dir, --http-bind, --rtmp-bind; startup wiring
    config.rs                Config (TOML + env overrides RMS_*), defaults
    state.rs                 AppState (Arc) shared by axum handlers and tasks
    error.rs                 ApiError -> axum response mapping ({"error": "..."})
    db/mod.rs                Db wrapper + migrations (schema §7) + repositories
    db/broadcasts.rs, db/metrics.rs, db/recordings.rs, db/settings.rs
    hub/mod.rs               Hub: registry of live streams, events channel
    hub/stream.rs            LiveStream: broadcast channel, GOP cache, stats
    hub/packet.rs            MediaPacket + FLV tag helpers (serialize FLV header/tags)
    ingest/rtmp.rs           rtmp-rs handler -> hub
    ingest/srt.rs            optional ffmpeg SRT->RTMP bridge (phase 4)
    egress/httpflv.rs        GET /live/{name}.flv
    egress/wsflv.rs          GET /ws/live/{name}.flv (WebSocket binary FLV)
    egress/hls.rs            LL-HLS routes (playlists, init, segments, parts, blocking reload)
    egress/dash.rs           LL-DASH routes (MPD, init, chunked segments, utc time)
    packager/mod.rs          PackagerManager: one Packager task per live stream
    packager/cmaf.rs         fMP4/CMAF muxer (H.264 + AAC) -> init segment + chunks
    packager/store.rs        SegmentStore: segments/parts in memory + notify for blocking reads
    packager/hls_playlist.rs LL-HLS media/master playlist generation
    packager/dash_mpd.rs     LL-DASH MPD generation
    recorder/mod.rs          RecorderManager: FLV writer per stream, ffmpeg remux to MP4
    metrics/mod.rs           ViewerTracker (live counts) + Sampler task -> db
    api/mod.rs               /api/v1 router; auth middleware; JSON types
    api/auth.rs, api/broadcasts.rs, api/metrics.rs, api/recordings.rs, api/waiting.rs,
    api/system.rs, api/events.rs (SSE), api/public.rs (unauthenticated player API)
    web/mod.rs               dashboard + embed routes (static assets via include_str!/include_bytes!)
    web/static/index.html, app.js, style.css           dashboard SPA (Italian UI)
    web/static/embed.html, player.js, player.css       embeddable player
  data/ (runtime, gitignored)  rms.sqlite, recordings/, waiting/, admin_token
```

Each `src/<module>/mod.rs` exposes at most: a constructor/`spawn` function, an
`axum::Router<AppState>` builder named `router()` when it serves HTTP, and the public
types listed in this document. `main.rs` only wires modules together.

## 4. Core domain

### 4.1 Broadcast (persisted)

```
Broadcast {
  id: String (uuid v4),
  name: String       // URL-safe slug [a-z0-9-_]{1,64}, unique, public identifier used in all playback URLs
  title: String,
  description: String,
  stream_key: String // secret, 32 hex chars, used by OBS; regenerable
  record: bool       // auto-record when live
  enable_flv, enable_hls, enable_dash: bool (default true)
  paused: bool       // admin forced "waiting" state
  waiting_video: Option<String>   // relative path under data/waiting/, normalized mp4
  waiting_text: String            // text shown on the waiting screen (default "La diretta inizierà a breve")
  created_at, updated_at: DateTime<Utc>
}
```

### 4.2 Effective status

`status = if paused { Paused } else if hub.is_live(name) { Live } else { Offline }`.
Pause does **not** disconnect the publisher; the ingest keeps running (and recording keeps
recording). Pause only affects what players are told (public status API/SSE) — players show
the waiting video. Playback endpoints keep serving (so an admin preview still works), except
that the public status reports `paused`.

### 4.3 MediaPacket and Hub

```rust
pub enum PacketKind { Video, Audio, Script }
pub struct MediaPacket {
  pub kind: PacketKind,
  pub timestamp: u32,          // FLV timestamp, milliseconds, as received (monotonic-ish)
  pub data: Bytes,             // FLV tag *body* (no 11-byte tag header), shared across subscribers
  pub is_keyframe: bool,       // video only (frame type 1)
  pub is_sequence_header: bool // AVC/HEVC/AAC sequence header (config record)
}
```

- `Hub` keeps `RwLock<HashMap<String /*broadcast name*/, Arc<LiveStream>>>` plus
  `tokio::sync::broadcast::Sender<HubEvent>` with
  `HubEvent::{StreamStarted{name}, StreamEnded{name}, Metadata{name}, ViewerCountChanged{name, counts}, BroadcastUpdated{name}}`.
- `LiveStream` holds: `name`, `started_at`, `publisher_addr`, `tx: broadcast::Sender<Arc<MediaPacket>>`
  (capacity 2048), `GopCache` (script tag + video seq header + audio seq header + all packets since the last keyframe,
  bounded to e.g. 4 s / 4 MB), `StreamInfo` (video codec, audio codec, width, height, fps, bitrates,
  from `onMetaData` and from sequence headers), counters (`bytes_in`, `packets`, last packet time).
- `LiveStream::subscribe() -> (Vec<Arc<MediaPacket>> /*catch-up from GOP cache, headers first*/, broadcast::Receiver<Arc<MediaPacket>>)`
  taken atomically under the GOP cache lock so no packet is lost/duplicated between the snapshot and the receiver.
- Slow subscriber policy: on `RecvError::Lagged`, the consumer drops packets until the next video keyframe
  (for FLV outputs) and logs a warning. Never block the publisher.
- Only one publisher per broadcast; a second publish with the same key is rejected (`AuthResult::Reject`).
- `Hub::publish_start(name, info) -> Arc<LiveStream>`, `Hub::publish_end(name)`, `Hub::push(name, MediaPacket)`,
  `Hub::get(name) -> Option<Arc<LiveStream>>`, `Hub::list()`, `Hub::subscribe_events()`.

### 4.4 Viewer tracking (metrics::ViewerTracker)

```rust
pub enum Protocol { Flv, WsFlv, Hls, Dash, WebRtc }
pub struct ViewerCounts { pub flv: u32, pub ws_flv: u32, pub hls: u32, pub dash: u32, pub webrtc: u32 } // + total()
```
- Connection-oriented protocols (FLV, WS-FLV, WebRTC): `tracker.open(name, protocol, peer_ip, user_agent) -> ViewerSession`
  (guard; `Drop` closes it; also records a `viewer_sessions` row with started_at/ended_at).
- Request-oriented protocols (HLS, DASH): `tracker.touch(name, protocol, peer_ip, user_agent)` on every
  media playlist/segment request; a session is `sha256(ip|ua|name|protocol)` and expires after
  `metrics.session_timeout_secs` (default 30) without touches (reaper task every 5 s).
- `tracker.counts(name) -> ViewerCounts`, `tracker.sessions(name) -> Vec<SessionInfo>`.
- Sampler task (metrics module): every `metrics.sample_interval_secs` (default 5) for each live stream
  insert a `viewer_samples` row and emit `HubEvent::ViewerCountChanged`. When a stream ends insert a final zero sample.
- Peer IP: when `server.trust_proxy = true`, `X-Real-IP`, else the last `X-Forwarded-For` element (§20), else socket addr.

## 5. Ingest

- RTMP via `rtmp-rs`: bind `rtmp.bind` (default `0.0.0.0:1935`), app name configurable (default `live`,
  but accept any app — OBS users often leave it empty or wrong). `on_fc_publish`/`on_publish`: look up the
  broadcast by `stream_key` (the RTMP stream name); reject unknown keys and already-live broadcasts.
  Map stream key → broadcast `name` in the handler (`DashMap`/`RwLock<HashMap<session_id, name>>`).
- `on_metadata` → store `onMetaData` fields in `StreamInfo` and also keep the raw script tag (if available
  through `on_media_tag` with `FlvTagType::ScriptData`) in the GOP cache so FLV outputs can replay it.
  If the raw script tag is not exposed, re-serialize an `onMetaData` AMF0 script tag from the map
  (there is an AMF0 encoder in `rtmp_rs::amf`).
- `on_media_tag` → `Hub::push`. `on_unpublish`/`on_disconnect` → `Hub::publish_end`.
- A `db.stream_sessions` row is created on start and closed on end (peak viewers, bytes).
- Optional SRT ingest (phase 4): spawn `ffmpeg -i "srt://0.0.0.0:<port>?mode=listener&streamid=<key>" -c copy -f flv rtmp://127.0.0.1:1935/live/<key>`
  per broadcast on demand — keep it isolated in `ingest/srt.rs`.

## 6. Egress

### 6.1 HTTP-FLV — `GET /live/{name}.flv`
Headers: `Content-Type: video/x-flv`, `Cache-Control: no-cache`, `Access-Control-Allow-Origin: *`,
chunked transfer (streaming body). Body: FLV header (9 bytes + PreviousTagSize0) then tags:
script metadata (if any), video seq header, audio seq header, GOP cache, then live packets.
Timestamps are passed through as-is. Register a `ViewerSession` (Protocol::Flv). 404 when not live
(`{"error":"stream offline"}`), 404 when the broadcast does not exist, 403 when `enable_flv=false`.
### 6.2 WebSocket-FLV — `GET /ws/live/{name}.flv`
Same byte stream, as binary WebSocket messages (one message per tag; header + first tags in the first message).
Supported by mpegts.js (`type: 'flv'`, `url: ws://...`).
### 6.3 LL-HLS — `/hls/{name}/...`
- `master.m3u8` (EXT-X-STREAM-INF with BANDWIDTH, RESOLUTION, CODECS e.g. `avc1.64001f,mp4a.40.2`,
  FRAME-RATE) → `stream.m3u8`.
- `stream.m3u8`: `#EXT-X-VERSION:9`, `#EXT-X-TARGETDURATION`, `#EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=YES,PART-HOLD-BACK=<3*part>,CAN-SKIP-UNTIL=<6*target>`,
  `#EXT-X-PART-INF:PART-TARGET=<part_duration>`, `#EXT-X-MEDIA-SEQUENCE`, `#EXT-X-MAP:URI="init.mp4"`,
  `#EXT-X-PROGRAM-DATE-TIME`, then for each segment its `#EXT-X-PART:DURATION=..,URI="part-<seg>-<n>.m4s"[,INDEPENDENT=YES]`
  lines followed by `#EXTINF:<dur>,` + `seg-<seg>.m4s`; the in-progress segment lists its completed parts and ends with
  `#EXT-X-PRELOAD-HINT:TYPE=PART,URI="part-<seg>-<next>.m4s"`. Support delta updates (`_HLS_skip=YES`) only if cheap; otherwise omit CAN-SKIP-UNTIL.
- Blocking playlist reload: `stream.m3u8?_HLS_msn=N&_HLS_part=P` waits (tokio `Notify`/`watch`) until that part exists;
  timeout after 3×target duration → `503`.
- `init.mp4` (ftyp+moov), `seg-N.m4s` (complete segment = concatenation of its parts; while in progress, serve what exists
  and keep streaming (chunked) until complete), `part-N-P.m4s` (one CMAF chunk: moof+mdat).
- Headers: `Content-Type: application/vnd.apple.mpegurl` / `video/mp4` / `video/iso.segment`, `Cache-Control: no-cache` for playlists,
  `max-age=3600` for finished segments/parts, CORS `*`. Every playlist/segment request calls `tracker.touch(.., Protocol::Hls, ..)`.
- Segment = one GOP (new segment at every keyframe; if a GOP exceeds `hls.max_segment_duration` (default 6 s), split anyway
  at the next part boundary). Target segment duration follows the encoder keyframe interval (OBS default 2 s). Part duration
  `hls.part_duration` (default 0.5 s): a part is closed when the next video frame would exceed the duration or at a keyframe.
  Audio samples are interleaved in the same track-separate fragments (one `moof` with two `traf`s per chunk is allowed
  but **prefer one chunk containing both tracks' samples**, i.e. moof with traf(video)+traf(audio), mdat with both — hls.js and Safari accept it).
- When the stream ends, keep serving the last window for `hls.linger_secs` (default 30) then drop; `#EXT-X-ENDLIST` is appended.
### 6.4 LL-DASH — `/dash/{name}/...`
- `manifest.mpd`: `type="dynamic"`, `profiles="urn:mpeg:dash:profile:isoff-live:2011"`, `availabilityStartTime` (stream start, UTC),
  `publishTime`, `minimumUpdatePeriod="PT2S"`, `minBufferTime="PT1S"`, `timeShiftBufferDepth` (window), `maxSegmentDuration`,
  `<ServiceDescription><Latency target="2000" min="1500" max="6000"/><PlaybackRate min="0.96" max="1.04"/></ServiceDescription>`,
  `<UTCTiming schemeIdUri="urn:mpeg:dash:utc:http-xsdate:2014" value="/dash/time"/>`, one `Period` with one `AdaptationSet` per
  track (video avc1, audio mp4a), `SegmentTemplate` with `initialization="init-$RepresentationID$.m4s"`, `media="chunk-$RepresentationID$-$Number$.m4s"`,
  `timescale`, `duration` (nominal segment duration in timescale units), `startNumber`, `availabilityTimeOffset="<seg - chunk>"`, `availabilityTimeComplete="false"`.
  Because DASH `$Number$` addressing assumes constant segment durations, the DASH packager uses **fixed-duration segments**
  (`dash.segment_duration`, default 2 s) cut at the keyframe closest to the boundary; if GOPs are longer than the segment duration,
  fall back to `SegmentTimeline` (`$Time$`) — implement SegmentTimeline from the start if simpler: it is fully supported by dash.js low-latency mode.
- Segments are served with chunked transfer while being produced (same `SegmentStore` as HLS: a segment is a list of chunks + "complete" flag; readers stream existing chunks then await new ones).
- `GET /dash/time` → current UTC in ISO-8601 (`2026-10-08T14:00:00.000Z`), `Content-Type: text/plain`.
- Headers as HLS (`application/dash+xml` for the MPD). Every MPD/segment request → `tracker.touch(.., Protocol::Dash, ..)`.
### 6.5 Shared SegmentStore (packager/store.rs)
Single source for HLS parts and DASH chunks: a `Chunk { seq, part_index, duration_ms, bytes: Bytes, independent: bool }`;
`Segment { seq, start_ts_ms, duration_ms, chunks: Vec<Chunk>, complete: bool, program_date_time }`; `init: Bytes`;
`tokio::sync::watch<Version>` to wake blocking readers; window of N complete segments (default 6) + the in-progress one.

## 7. Database schema (SQLite, `data/rms.sqlite`, WAL mode)

```sql
CREATE TABLE IF NOT EXISTS broadcasts (
  id TEXT PRIMARY KEY, name TEXT UNIQUE NOT NULL, title TEXT NOT NULL, description TEXT NOT NULL DEFAULT '',
  stream_key TEXT UNIQUE NOT NULL, record INTEGER NOT NULL DEFAULT 0,
  enable_flv INTEGER NOT NULL DEFAULT 1, enable_hls INTEGER NOT NULL DEFAULT 1, enable_dash INTEGER NOT NULL DEFAULT 1,
  paused INTEGER NOT NULL DEFAULT 0, waiting_video TEXT, waiting_text TEXT NOT NULL DEFAULT 'La diretta inizierà a breve',
  created_at TEXT NOT NULL, updated_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS stream_sessions (   -- one row per live period
  id TEXT PRIMARY KEY, broadcast_id TEXT NOT NULL, started_at TEXT NOT NULL, ended_at TEXT,
  publisher_ip TEXT, video_codec TEXT, audio_codec TEXT, width INTEGER, height INTEGER, fps REAL,
  bytes_in INTEGER NOT NULL DEFAULT 0, peak_viewers INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS viewer_samples (    -- time series, one row per sample per broadcast
  broadcast_id TEXT NOT NULL, ts TEXT NOT NULL, total INTEGER NOT NULL, flv INTEGER NOT NULL, ws_flv INTEGER NOT NULL,
  hls INTEGER NOT NULL, dash INTEGER NOT NULL, webrtc INTEGER NOT NULL, PRIMARY KEY (broadcast_id, ts));
CREATE TABLE IF NOT EXISTS viewer_sessions (   -- one row per viewer session (all protocols)
  id TEXT PRIMARY KEY, broadcast_id TEXT NOT NULL, protocol TEXT NOT NULL, ip TEXT, user_agent TEXT,
  started_at TEXT NOT NULL, ended_at TEXT);
CREATE TABLE IF NOT EXISTS recordings (
  id TEXT PRIMARY KEY, broadcast_id TEXT NOT NULL, stream_session_id TEXT, path TEXT NOT NULL,
  format TEXT NOT NULL, status TEXT NOT NULL,  -- recording|processing|ready|failed
  started_at TEXT NOT NULL, ended_at TEXT, size_bytes INTEGER NOT NULL DEFAULT 0, duration_secs REAL, error TEXT);
CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);  -- e.g. default_waiting_video, default_waiting_text
CREATE INDEX IF NOT EXISTS idx_samples_ts ON viewer_samples(broadcast_id, ts);
CREATE INDEX IF NOT EXISTS idx_vsessions ON viewer_sessions(broadcast_id, started_at);
```
Timestamps are RFC 3339 UTC strings. `db::Db` offers `call<F, T>(f) -> T` running `f(&mut Connection)` on a blocking thread.
Retention: delete `viewer_samples` older than `metrics.retention_days` (default 90) daily.

## 8. HTTP surface

Authentication: admin token (`server.admin_token` or env `RMS_ADMIN_TOKEN`; if empty, generate a random
one at first start, store it in `data/admin_token`, print it in the log). Admin endpoints require
`Authorization: Bearer <token>` **or** cookie `rms_token=<token>` (set by `POST /api/v1/auth/login {token}` → `204` + cookie;
`POST /api/v1/auth/logout`). `GET /api/v1/auth/check` → 204/401. Everything under `/api/v1/public/*`, all playback
paths, `/embed/*`, `/waiting/*`, `/recordings/*/download`? (no: downloads are admin) and the dashboard static files are public.
CORS: `*` on playback and public API. JSON errors: `{"error": "message"}` with proper status codes.

### 8.1 Admin API (`/api/v1`)
| Method & path | Body / query | Response |
|---|---|---|
| `GET /broadcasts` | | `[BroadcastView]` |
| `POST /broadcasts` | `{name?, title, description?, record?, enable_flv?, enable_hls?, enable_dash?, waiting_text?}` (name auto-slugified from title if absent) | `201 BroadcastView` |
| `GET /broadcasts/{name}` | | `BroadcastView` |
| `PATCH /broadcasts/{name}` | any subset of the POST fields except name | `BroadcastView` |
| `DELETE /broadcasts/{name}` | | `204` (disconnects publisher, deletes waiting video; recordings kept) |
| `POST /broadcasts/{name}/regenerate-key` | | `BroadcastView` |
| `POST /broadcasts/{name}/pause` / `/resume` | | `BroadcastView` |
| `POST /broadcasts/{name}/disconnect` | | `204` (kick publisher) |
| `POST /broadcasts/{name}/record/start` / `/stop` | | `Recording` / `204` |
| `GET /broadcasts/{name}/recordings` | | `[Recording]` |
| `GET /recordings` | `?broadcast=` | `[Recording]` |
| `DELETE /recordings/{id}` | | `204` (deletes file) |
| `GET /recordings/{id}/download` | | file (`Content-Disposition: attachment`) |
| `GET /broadcasts/{name}/metrics` | `?from=<rfc3339>&to=<rfc3339>&step=<secs>` (defaults: last 1 h, step auto so ≤ 600 points; aggregate by max) | `{from,to,step,points:[{t,total,flv,ws_flv,hls,dash,webrtc}]}` |
| `GET /broadcasts/{name}/metrics/summary` | `?from&to` | `{current: ViewerCounts, peak:{value,at}, sessions_total, avg_watch_secs, live_since?, bytes_in}` |
| `GET /broadcasts/{name}/sessions` | | `[{id, protocol, ip, user_agent, started_at}]` (currently connected) |
| `GET /metrics/overview` | | `{live_broadcasts, total_viewers: ViewerCounts, per_broadcast:[{name,title,status,viewers}]}` |
| `POST /broadcasts/{name}/waiting-video` | multipart `file` (≤ `server.max_upload_mb`, default 1024) | `BroadcastView` (normalized with ffmpeg to H.264/AAC mp4 + poster when available; otherwise stored as-is if mp4) |
| `DELETE /broadcasts/{name}/waiting-video` | | `BroadcastView` |
| `POST /settings/waiting-video` / `DELETE` | multipart `file` | `Settings` (global default) |
| `GET /settings` / `PATCH /settings` | `{default_waiting_text?}` | `Settings` |
| `GET /broadcasts/{name}/embed` | | `EmbedInfo` (see below) |
| `GET /system` | | `{version, uptime_secs, ffmpeg: {available, path, version?}, rtmp_bind, http_bind, public_url, hub:{live_streams}}` |
| `GET /events` | SSE | events `broadcast` (BroadcastView on any change), `viewers` (`{name, counts}`), `stream` (`{name, status}`), heartbeat every 15 s |

`BroadcastView = Broadcast (without stream_key? NO — admins need it) + {status: "offline"|"live"|"paused", viewers: ViewerCounts, live: Option<{started_at, publisher_ip, video_codec, audio_codec, width, height, fps, bytes_in, recording: Option<Recording>}>, urls: PlaybackUrls, obs: {server: "rtmp://<host>:1935/live", stream_key}, waiting_video_url: Option<String>}`.
`PlaybackUrls = {flv, ws_flv, hls, dash, embed, player}` absolute, built from `server.public_url` (default `http://<Host header>`; ws scheme derived).
`EmbedInfo = {iframe: "<iframe src=... allow=\"autoplay; fullscreen\" ...></iframe>", urls: PlaybackUrls, obs, snippets: {hlsjs, mpegtsjs, dashjs}}` (ready-to-paste examples).

### 8.2 Public API (no auth)
- `GET /api/v1/public/broadcasts/{name}` → `{name, title, status, waiting_video_url, waiting_text, poster_url, sources:{flv, ws_flv, hls, dash}, enable_*}` (no stream key!).
- `GET /api/v1/public/broadcasts/{name}/events` → SSE `status` events `{status}` whenever it changes + heartbeat.

### 8.3 Pages
- `/` dashboard SPA (login view when 401), `/embed/{name}` player page for iframes (query: `autoplay=1|0`, `muted=1|0`, `src=auto|flv|hls|dash`, `controls=1|0`),
  `/player/{name}` same as embed but full page with title, `/waiting/{...}` static waiting videos & posters (`data/waiting/`), `/static/*` assets.

## 9. Embed player behaviour (web/static/player.js)

1. Fetch public broadcast info; open SSE for status.
2. `offline`/`paused` → show waiting screen: looped muted `<video>` with the waiting video (or a styled placeholder
   with `waiting_text` and a pulsing "In attesa della diretta" badge when none).
3. `live` → pick a source: `src` query param, otherwise: if `mpegts.js` supported (MSE + not iOS) → HTTP-FLV (`liveBufferLatencyChasing: true`),
   else if Safari/iOS → native LL-HLS, else hls.js `lowLatencyMode: true`. Expose `window.rmsPlayer` with `setSource()` for debugging.
   On fatal player errors fall back to the next source (flv → hls → dash); on `offline`/`paused` tear the player down and return to step 2.
4. Show a small latency/health indicator (optional) and a "LIVE" badge.

## 10. Configuration (`config.toml`, env override `RMS_<SECTION>_<KEY>`, CLI flags win)

```toml
[server]
http_bind = "0.0.0.0:8080"
public_url = ""            # e.g. "https://stream.example.com"; empty = derive from Host header
admin_token = ""           # empty = generate at first start (stored in data/admin_token)
data_dir = "./data"
trust_proxy = false
max_upload_mb = 1024
[rtmp]
bind = "0.0.0.0:1935"
app = "live"
[hls]
enabled = true
part_duration = 0.5
max_segment_duration = 6.0
window_segments = 6
linger_secs = 30
[dash]
enabled = true
segment_duration = 2.0
[recording]
remux_to_mp4 = true        # needs ffmpeg
keep_flv = false
[metrics]
sample_interval_secs = 5
session_timeout_secs = 30
retention_days = 90
[ffmpeg]
path = ""                  # empty = search PATH
[log]
level = "info"             # RUST_LOG overrides
```

## 11. Phases & ownership

- **Phase 1 (foundation, sequential)**: Cargo.toml with *all* deps above, config, error, state, db (full schema + broadcasts/settings repos),
  hub (packets, GOP cache, subscribe), ingest/rtmp, egress/httpflv + wsflv, metrics::ViewerTracker (live counts only, no DB sampler),
  api (auth, broadcasts CRUD, pause/resume/disconnect/regenerate-key, embed, system, events SSE, public), minimal web (placeholder dashboard
  showing broadcasts JSON + embed page that plays HTTP-FLV with mpegts.js), stub modules `packager`, `recorder` (with the public API
  signatures used by main.rs), `scripts/test-publish.sh`, e2e verified with ffmpeg → RTMP → `curl /live/x.flv | ffprobe`.
- **Phase 2 (parallel, git worktrees, one branch each)**:
  - `feat/packager`: packager (cmaf, store, hls_playlist, dash_mpd) + egress/hls + egress/dash, validated with ffprobe/ffmpeg and hls.js/dash.js in a headless browser.
  - `feat/metrics-recording`: metrics sampler + db/metrics + api/metrics; recorder + db/recordings + api/recordings; retention.
  - `feat/dashboard`: web/static dashboard SPA + embed player + api/waiting (upload/normalize) + api/settings; Italian UI; Chart.js charts (against the API contract in §8).
- **Phase 3 (integration, sequential)**: merge, fix, e2e tests, README.md, docs/API.md, docs/PROTOCOLS.md, code review pass.
- **Phase 4 (stretch)**: WHEP egress (webrtc-rs), SRT ingest bridge.

## 12. Quality bar

- `cargo build --release` warning-free (`#![deny(warnings)]` is NOT required but `cargo clippy` should be clean of errors), `cargo test` green.
- No `unwrap()` on untrusted input in request handlers; never panic in tasks (log and continue).
- Media paths never buffer unbounded data: broadcast channels, bounded GOP caches, bounded segment windows.
- Every long-running task is spawned with a name in logs and survives errors (loop with backoff where appropriate).
- Keep handlers small; business logic lives in modules, not in axum handlers.

## 13. Phase 1 as-built notes (binding for later phases)

Deviations/additions made while implementing Phase 1 — these supersede the sections above where they differ:

- `src/lib.rs` exists (library target: `rust_media_server::{api, config, db, egress, hub, ingest, metrics, packager, recorder, state, web, error}`); `main.rs` uses it.
- Router builders: `api::router(&state)` and `web::router(&state)` take the state; `egress::router()` and `packager::router()` take none. `main.rs` merges them at `/` — never change the wiring, only fill the modules.
- `Hub::publish_start(..) -> Result<Arc<LiveStream>, HubError::AlreadyLive>`; extra API: `publish_end_stream(&stream)`, `kick(name)`, `emit(event)`, `events()`, `live_count()`. `LiveStream::ended_token() -> CancellationToken` fires when the publisher goes away — **every consumer task (packager, recorder) must `select!` on it**, because holding an `Arc<LiveStream>` keeps the channel open. `LiveStream::info() -> StreamInfo` carries `video_codec`/`audio_codec` names (`h264`, `hevc`, `av1`, `aac`...), RFC 6381 `video_codec_string`/`audio_codec_string` (e.g. `avc1.64001f`, `mp4a.40.2`), width/height/fps, bitrates, sample rate/channels. `hub::packet` has `parse_avc_sequence_header`, `parse_aac_sequence_header`, `flv_file_header`, `flv_tag_bytes`, `on_metadata_body`.
- Publisher kick is real: `ingest/rtmp.rs` runs its own accept loop around rtmp-rs `Connection` with a `CancellationToken` per connection; a publisher idle > 5 s is replaced when the same key publishes again.
- `ViewerTracker::new(&config, Option<Db>, events)`; `open(name, protocol, ip, ua) -> ViewerSession` guard, `touch(..)`, `counts`, `all_counts`, `sessions`, `peak`/`take_peak`; `metrics::client_ip(headers, peer, trust_proxy)` and `metrics::user_agent(headers)` helpers. `ViewerCounts` serializes an extra `total` field. The tracker already calls `db::metrics::insert_viewer_session/close_viewer_session` (stubs to be implemented).
- `db/stream_sessions.rs` holds the per-live-period rows; `db::Recording` type is final (see `db/recordings.rs`); `db::Db::call(|conn| ..)` runs on a blocking thread (single connection behind a mutex).
- `api/settings.rs` is a separate route file (`GET/PATCH /settings`, `POST/DELETE /settings/waiting-video`); `api/waiting.rs` holds the per-broadcast upload. Poster convention: `api::poster_path_for(video)` = same file name with `.jpg`.
- `api/metrics.rs` already implements `/broadcasts/{name}/sessions` and `/metrics/overview`; the series/summary endpoints return 501 until the metrics branch lands.
- Admin SSE (`/api/v1/events`) emits `broadcast`, `broadcast_deleted {name}`, `viewers {name, counts}`, `stream {name, status}`, `heartbeat {ts}` — cookie auth only (EventSource cannot set headers). Public `sources.*` are `null` for disabled protocols.
- `AppState` (Deref to `Inner`): `config`, `db`, `hub`, `tracker`, `packager`, `recorder`, `events`, `admin_token`, `ffmpeg: Option<PathBuf>`, `shutdown: CancellationToken`, `ffmpeg_version().await`.
- Behaviour: disconnect on a non-live broadcast → 409; regenerate-key does not kick; static assets served `no-cache` + ETag; default log filter appends `rtmp_rs=warn`.
- Known limits: HEVC/AV1 (Enhanced RTMP) only flow through FLV paths; the CMAF packager is H.264 + AAC. OBS defaults to keyframe interval "auto" (up to 250 frames): the packager must measure the GOP and expose it (`gop_secs`) so the dashboard can warn the operator to set 1–2 s.

## 14. Phase 2 as-built notes (binding; supersede §§6-9 where they differ; field-level detail in docs/API.md)

**Packager (§6.3-6.5)**
- Hand-rolled ISO-BMFF writer in `packager/cmaf.rs`; the `mp4-atom` dependency is declared in `Cargo.toml` but unused. Extra files: `packager/segmenter.rs` (FLV packets to GOP-aligned segments and parts) and `egress/lowlat.rs` (shared HLS/DASH plumbing: `resolve()` status codes, response builders, viewer `touch`). `packager::router()` mounts `egress::hls` + `egress::dash` behind permissive CORS.
- Timescales: video 90 kHz (track 1), audio = AAC sample rate (track 2). A chunk is one `moof`+`mdat`: HLS parts mux both tracks (one `moof`, two `traf`), DASH uses one single-track fragment per Representation built from the same samples.
- Media time is anchored to the Unix epoch (first keyframe's FLV timestamp is mapped to its arrival wall-clock time). DASH `availabilityStartTime` = `1970-01-01T00:00:00Z`; the first HLS media sequence / segment number is the Unix second of the first keyframe, then +1 per segment, so segment, part and `$Time$` URLs never repeat across publishing sessions.
- DASH uses `SegmentTimeline` with `$Time$` addressing (`chunk-$RepresentationID$-$Time$.m4s`), not `$Number$`; segments follow the encoder GOP and `dash.segment_duration` is only the initial estimate. `minimumUpdatePeriod` is 1 s (not 2 s), `UTCTiming` is the absolute URL `<base>/dash/time`, one `Period` per codec generation (`init-*.m4s?g=N`).
- LL-HLS: no `CAN-SKIP-UNTIL` / `_HLS_skip` (no delta updates); `PART-HOLD-BACK` = 3 x part; blocking reload answers 503 after 3 x target duration and 400 if `_HLS_msn` > newest complete segment + 2 or `_HLS_part` lacks `_HLS_msn`.
- Startup wait: playlist, MPD and init requests wait up to 8 s for the first complete segment, then answer 503 (`stream is starting`); 404 `stream offline` if the stream ended first. Only H.264 + AAC are packaged; other codecs give `supported: false` and 415 on HLS/DASH.
- `PackagerStatus` (`packager/mod.rs`): `supported`, `reason`, `hls_ready`, `dash_ready`, `ended`, `segments`, `parts`, `measured_gop_secs` (this is the `gop_secs` of §13), `last_segment_secs`, `part_target_secs`, `target_duration_secs`, `latency_hint_secs`. `LiveInfo` (defined in `api/mod.rs`) gained `packager: Option<PackagerStatus>`, filled by `PackagerManager::status(name)`.

**Metrics and recording (§4.4, §7, §8.1)**
- `record/start` and `record/stop` both return `200` with the `Recording` (§8.1 said `204` for stop); stop waits (<= 15 s) for the file to close and returns the post-stop state (`processing` while the MP4 remux runs, else `ready`/`failed`). `GET /recordings/{id}/download` is inline by default (`?download=1|true|yes` gives `attachment`), with `Range` support.
- `metrics` `step`: any integer 1..=86400 is accepted and snapped up to one of 5, 10, 30, 60, 300, 900, 3600, 7200, 21600, 86400 (the contract stopped at 3600; the larger steps keep 90 days within 600 points), never below `metrics.sample_interval_secs`. Range <= 90 days, buckets aligned to the Unix epoch, aggregation by max, series dense and zero-filled (no points after now; the bucket containing now is omitted until it has a sample).
- Summary additionally returns `from`/`to`. `peak` includes the live count when now is in the range (`null` if nothing to report); `sessions_total` / `avg_watch_secs` use viewer sessions overlapping the range; `bytes_in` sums the live periods overlapping the range, current one included.
- Retention (at startup, then daily) deletes `viewer_samples` and closed `viewer_sessions` older than `metrics.retention_days`; at startup the sampler also closes viewer sessions left open by a previous process, and `main.rs` closes dangling `stream_sessions`.
- `RecorderError`: `NotLive`, `AlreadyRecording`, `NotRecording`, `Busy` map to 409, `NotFound` to 404, `Other` to 500. Crash recovery at startup finalizes `recording`/`processing` rows (FLV truncated to its last complete tag by `repair_flv`, then remuxed) or marks them `failed` when the file is missing.
- Files are named in UTC: `data/recordings/<name>/<YYYYMMDD-HHMMSS>[-n].flv`; the MP4 remux is `-c copy +faststart` and the FLV is deleted unless `recording.keep_flv`.

**Dashboard, player, waiting video (§8.3, §9)**
- `src/web/static/`: `index.html`, `app.js`, `core.js`, `detail.js`, `metrics.js`, `mock.js`, `style.css`, `favicon.svg` (dashboard) and `embed.html`, `player.js`, `player.css` (player); embedded with `include_str!`, served `no-cache` + content-hash `ETag`.
- CDN (pinned): mpegts.js 1.8.2, hls.js 1.7.3 and dash.js 5.2.1 from jsDelivr, loaded lazily by `player.js` (same URLs in the `EmbedInfo` snippets); Chart.js 4.5.1 from cdnjs in `metrics.js` (`api::cdn::CHART_JS` is unused).
- dash.js 5 settings: `streaming.delay.liveDelay: 2` + `streaming.liveCatchup.enabled: true`; low-latency mode comes from the MPD (`lowLatencyEnabled` no longer exists in v5).
- Player (§9): source order is `flv` -> `hls` -> `dash`, and `hls` -> `dash` on iOS (desktop Safari has MSE, so it starts with mpegts.js; the `hls` engine is native on iOS/Safari, hls.js elsewhere). `src=` means "start here, then fall through" (`src=dash` tries DASH only). Extra query parameter `latency=1|0` (default on for `/player`, off for `/embed`).
- Client-side mock: `localStorage.rmsMock = "1"` makes `mock.js` answer `GET` metrics, summary and recordings in the browser only (UI demos); it is not a server feature.
- Waiting-video upload: `data/waiting/<name>/<uuid>.mp4`, global default in `data/waiting/_default/`, in-flight files in `data/waiting/tmp/`, poster `<stem>.jpg`. Body limit `server.max_upload_mb` MiB + 64 KiB (413 over it), 415 unsupported type (non-MP4 without ffmpeg), 422 ffmpeg cannot convert, 503 conversion timeout (15 min, two conversions at a time).

## 15. Known limits and roadmap (after the phase 3 review)

Fixed in the review merge: HLS/DASH viewer double counting (race in `ViewerTracker::touch`), out-of-order
`viewer_sessions` writes (now a single ordered writer task), encoder timestamp jumps breaking the segmenter,
oversized LL-HLS parts at 29.97 fps, SPS/AAC parser overflows on hostile input, partial-file cleanup on aborted
uploads, audio-only FLV lag recovery, stream keys logged by `rtmp_rs` at INFO. Added afterwards: `rtmp.max_connections`
cap (default 256) and in-flight uploads moved to `data/upload-tmp/` (outside the public `/waiting/*` tree).

Open (low/medium-low severity, documented for future work):
- Segmenter: an encoder sending frozen (identical) timestamps keeps the in-progress segment open indefinitely;
  a frame gap of 0.5–10 s yields one part longer than `PART-TARGET`; `EXT-X-TARGETDURATION` may grow mid-stream
  (spec says it must not change); segments split from long GOPs are not keyframe-aligned (DASH still says `startWithSAP=1`);
  lag gaps are not signalled as discontinuities.
- Viewer tracking: the in-memory request-session map is keyed by (IP, UA) and only bounded by the 30 s timeout;
  a client stopping to read an HTTP-FLV/SSE response stays counted until TCP gives up (no send-side watchdog).
- Hot paths: broadcast lookups on every FLV/public request hit SQLite through `spawn_blocking` (cache candidate);
  complete HLS segments / DASH segments are re-concatenated per request under the store mutex (cache candidate).
- Packager task has no panic guard; two concurrent waiting-video uploads to the same broadcast may orphan a file.
- Not tested: Safari native LL-HLS (no macOS available), HTTP/2 (needs a reverse proxy), HEVC/AV1 only via FLV.

Roadmap (phase 4+): WebRTC egress via WHEP (`webrtc` crate; H.264 pass-through + AAC→Opus via ffmpeg RTP),
native SRT ingest (`srt-tokio`) or the ffmpeg bridge, WHIP ingest, ABR/transcoding ladder, viewer tokens,
delta playlists (`CAN-SKIP-UNTIL`), MoQ when the IETF spec settles.

## 16. Phase 4 as-built: WebRTC egress via WHEP (binding; field-level detail in docs/API.md §6.5)

**Stack.** `src/webrtc/` uses `rtc` 0.21, the sans-I/O core of webrtc-rs 0.21, directly instead of the
async `webrtc` 0.21 crate: the async crate's driver binds its own sockets per peer connection (no UDP mux:
`SettingEngine::with_udp_network` is a TODO and `nat_1to1_ips` is stored but never applied in 0.21) and its
`TrackLocal*` objects bind to a single peer connection. With `rtc` the server owns the socket, chooses the
ICE candidates and drives each peer connection from its own task (`handle_read` / `poll_write` /
`poll_event` / `handle_timeout`).

**Modules.**
- `webrtc/mod.rs` — `WebRtcManager` (`AppState.webrtc`): `new(config, hub, tracker)`, `spawn(shutdown)`
  (binds `0.0.0.0:webrtc.udp_port`; a bind failure disables WHEP and is reported by `status()` /
  `GET /system`), `router()` (merged in `main.rs`), `create_session`, `delete_session`,
  `close_broadcast` (called by the broadcast PATCH/DELETE handlers), publishers map, sessions map plus
  2-minute tombstones (a late `DELETE` after a server-side close answers 200).
- `webrtc/mux.rs` — `UdpMux`: one socket for every session. A STUN Binding Request is routed by the local
  ufrag of its `USERNAME` (each session registers its random ufrag) and teaches the mux the peer address
  (≤ 8 per session); every other datagram (DTLS, SRTP/SRTCP, STUN responses) is routed by peer address;
  bounded per-session queues (512, drop when full); unknown traffic dropped.
- `webrtc/session.rs` — `negotiate()` builds the `RTCPeerConnection` (media engine: H.264
  packetization-mode=1 profiles 42001f/42e01f/4d001f/64001f with nack/pli/fir feedback, Opus 48 kHz
  stereo; default interceptors: NACK responder, RTCP sender/receiver reports, TWCC receiver;
  `SettingEngine`: ICE-lite per config, the session's ICE credentials, mDNS disabled), adds one video and
  (when the publisher has audio) one audio track with random SSRCs, applies the offer, adds one host
  candidate per advertised address plus the end-of-candidates sentinel, and returns the answer. It runs in
  `spawn_blocking` (certificate generation). `run()` is the session task: inbound datagrams are tagged with
  the first advertised address as local address; media is written only once the peer connection is
  `Connected`, video starting at an IDR (and again after a `Lagged`); each packet gets the session's SSRC,
  negotiated payload type and its own contiguous sequence numbers. RTCP from the browser is handled by the
  interceptors and otherwise drained (no keyframe can be forced upstream: PLI is ignored).
- `webrtc/publisher.rs` — `WhepPublisher` per live stream (created by the first session; identity checked
  with `Weak<LiveStream>` like the packager): `VideoPacketizer` (FLV tag → `cmaf::parse_video_tag` →
  `h264::avcc_to_annexb` with SPS/PPS before IDRs → `H264Payloader`, MTU 1200 → RTP packets with
  timestamp = random base + 90 × continuous DTS in ms (`MediaClock` absorbs negative steps and jumps
  > 10 s)), fan-out of `Arc<MediaChunk>` (one per access unit / Opus packet) on a `broadcast` channel
  (1024). State `Starting → Ready(profile) | Unsupported(reason) → Ended` in a `watch` channel; `Ready` is
  published only after the whole GOP-cache catch-up so the audio decision is known.
- `webrtc/audio.rs` — Opus via ffmpeg: audio-only FLV (header, AAC sequence header, frames rebased to 0)
  written to stdin, `-probesize 32 -analyzeduration 0` (each probed byte would be permanent delay:
  32 KB ≈ 2 s), `libopus -application lowdelay -frame_duration 20`, `-f rtp -payload_type 111` to a
  socket on `127.0.0.1:0` (RTCP to the same port, ignored). Restart with backoff 1 → 30 s while the stream
  is live. Started on the first audio packet only when ffmpeg exists; otherwise sessions are video-only
  (the answer rejects the audio m-line) and a warning is logged once.
- `webrtc/h264.rs` — `parse_avcc` (SPS/PPS lists, `lengthSizeMinusOne`), `split_avcc`, `avcc_to_annexb`,
  `choose_h264_payload_type` (stream profile match > same profile_idc > Constrained Baseline > first);
  `webrtc/whep.rs` — HTTP handlers, `WhepError`, `validate_offer`.

**Lifecycle.** Publisher: stops on stream end (`ended_token`), after `webrtc.idle_stop_secs` without
sessions (checked every second; removal under the map lock re-checks the session count), when replaced by
a newer publishing session, or on shutdown; its `stop` token ends the encoder and every session. Session:
`ViewerTracker::open(name, WebRtc, ip, ua)` when the peer connection first reaches `Connected` (failed
attempts are not counted); closed on DELETE, ICE/peer connection `Failed`/`Closed`, `Disconnected` > 10 s,
not connected within 30 s, publisher stop, `enable_webrtc` set false / broadcast deleted, shutdown.

**Player.** `webrtc` engine first in `auto` (`webrtc → flv → hls → dash`, iOS `webrtc → hls → dash`),
`src=webrtc|whep`; failover on POST error, ICE not connected in 6 s, `failed`/`closed`, no frame in 8 s,
no video packet for 4 s (`getStats`); teardown = `pc.close()` + `DELETE` (keepalive, also on `pagehide`).
`rmsPlayer.latency` = jitter-buffer delay + RTT/2.

**Deviations from the phase 4 brief.** `rtc` instead of the async `webrtc` crate (above); one packetizer per
stream with per-session SSRC/PT/sequence stamping instead of shared `TrackLocalStatic*` objects; RTP video
timestamps from the FLV DTS instead of `Sample.duration` accumulation; `PATCH` answers 405 (no trickle).

**Known limits.** IPv4 only; host candidates only (no STUN gathering, no TURN: viewers need UDP reachability
of `webrtc.udp_port`); with several advertised addresses all inbound traffic is attributed to the first one
(fine for one public IP or a single-homed host); B-frames are sent in decode order (not supported by
browsers' WebRTC decoders: encoders must disable them); new viewers wait for the next IDR (≤ keyframe
interval); audio/video sync relies on RTCP sender reports of the send time (Opus leaves ffmpeg ≈ 30 ms
after the AAC frame arrives); the unauthenticated POST can hold a session slot for up to 30 s without
connecting (`webrtc.max_sessions` bounds it; per-IP limits are future work).

**Measured (2026-10-09, localhost, Chrome 150 headless, x264 veryfast zerolatency 720p30 2.5 Mbit/s + AAC
128 kbit/s).** Glass-to-glass (wall-clock code burned into the frame before encoding, decoded from the
rendered frame): p50 35-40 ms, p90 ≤ 54 ms. Chrome `getStats`: video jitter buffer 7-8 ms, audio 50-60 ms,
0 packets lost, 0 freezes over 20 s. WHEP POST → ICE connected ≈ 240 ms. ffmpeg Opus path adds ≈ 30 ms
(it was 2.1 s with the default probe size). Server CPU ≈ 1.5 % of one core idle, 13.6 % with 10 viewers,
25.8 % with 30 viewers (all at 30 fps, no loss), RSS ≈ 49 MiB; the Opus encoder process ≈ 4 % CPU per
broadcast. A browser killed without `DELETE` is reaped after ≈ 15 s (5 s ICE disconnected + 10 s grace).

## 17. Phase 5 as-built: UI language (binding; field-level detail in docs/API.md)

- Setting `ui_language` (`it` default | `en`) in the `settings` table (`db::settings::UiLanguage`); `PATCH /api/v1/settings {ui_language}` accepts exactly `"it"`/`"en"` (else 400, nothing written) and emits `HubEvent::SettingsChanged` → admin SSE `settings {ui_language, default_waiting_text}`. Exposed in `Settings`, `GET /api/v1/public/ui` → `{language}` (no auth) and `PublicBroadcast.ui_language`.
- Built-in waiting text per language (`UiLanguage::builtin_waiting_text`: "La diretta inizierà a breve" / "The live stream will start shortly"). A stored text equal to any built-in text (the `POST /broadcasts` default, or a default text saved unchanged) counts as "no text" (`ViewContext::waiting_text`); `PATCH /settings` deletes such a default text. Custom texts are never rewritten; Italian output is unchanged.
- `web/mod.rs` sets `<html lang>` on `/`, `/embed`, `/player` from the setting (one SQLite read per page, no cache); `?lang=it|en` (case-insensitive) on the player pages overrides it.
- `static/i18n.js` (ES module, in the asset table): `t`, `tn` (Intl.PluralRules), `setLanguage`/`getLanguage`/`onLanguageChange`, `translateDom` (`data-i18n`, `data-i18n-title|-aria-label|-placeholder`), `format{Number,Date,Bytes,Duration}` with `it-IT`/`en-US`; dictionaries `it` (reference wording) and `en`, one `'key': 'text',` per line. Missing keys fall back to Italian with one `console.warn` per key.
- Dashboard flow: `<html lang>` → `GET /public/ui` (before the login view) → `GET /settings` (after login) → SSE `settings` / Settings page "Lingua"; a change re-translates the shell and re-mounts the current view (no reload). Embed options gain "Lingua del player" (`?lang=`).
- Player: `player.js` is now an ES module (`<script type="module">`) importing `i18n.js`; language = `?lang=` else `<html lang>` then `ui_language` of every info refresh (every 60 s while waiting); a built-in `waiting_text` is shown in the player's language.
- Tests: `web::tests` parse `i18n.js` (same key set, same `{placeholders}`, built-in texts equal to the server's, every `t()`/`tn()`/`data-i18n*` key in the other static files exists, no unused key) and check `<html lang>`; `api::settings` tests cover validation, persistence, the SSE event and the per-language built-in text.
- Not translated by design: server error messages (API stays English; the dashboard maps known ones to the UI language), protocol names, OBS values (`CBR`, `zerolatency`), user data (titles, custom texts).

## 18. Phase 6 as-built: themes (binding; field-level detail in docs/API.md)

- Setting `ui_theme` (`system` default | `light` | `dark`, `db::settings::UiTheme`), exposed like `ui_language`: `GET/PATCH /settings` (anything else → 400, nothing written), `GET /public/ui` → `{language, theme}`, `PublicBroadcast.ui_theme`, admin SSE `settings {ui_language, ui_theme, default_waiting_text}`.
- `web/mod.rs` renders `<html lang data-theme>` (one `settings::ui_prefs` read); `?theme=` (case-insensitive) overrides it on `/embed` and `/player`. The attribute always holds the *choice*, never a resolved value.
- CSS (`style.css`, `player.css`): every colour is a token. `:root` = dark palette (unchanged look), `:root[data-theme="light"]` and `@media (prefers-color-scheme: light) { :root:not([data-theme="dark"]):not([data-theme="light"]) }` = the same light palette (so `system` follows the OS live without JS); `color-scheme` per block, `<meta name="color-scheme" content="light dark">`.
- No flash: an inline `<head>` script (before the stylesheet) applies `localStorage['rms.theme']` (dashboard) or `?theme=` (player) to `<html data-theme>`.
- `static/theme.js` (ES module): override (dashboard toggle, persisted in `rms.theme` with try/catch; player `?theme=`) → server default → `system`; `onThemeChange` also fires on OS changes under `system`; keeps `<meta name="theme-color">` = `--bg`.
- Dashboard: top-bar toggle system → light → dark (icon of the current choice, aria-label/title name the next); Settings "Lingua e tema" saves the server default and offers "Usa il tema predefinito" to drop this browser's override; embed options gain "Tema del player" (`?theme=`); the preview iframe gets `&theme=` and `postMessage({type: 'rms-theme'})` on changes (no reload).
- Charts (`metrics.js`): series, grid, ticks, tooltip from tokens (`--series-*`, `--chart-grid`, `--text-2`, `--bg`, `--border`, `--text`), repainted in place on theme change; the light palette darkens WS-FLV and LL-HLS (≥ 4.2:1 on white).
- The player's video area stays black in both themes; waiting screen, title and overlays (LIVE, latency, buttons) have light variants with their own backgrounds.
- Tests: `web::tests::css_colours_are_theme_tokens` (no colour literal outside `:root` token blocks, exception list empty, dark/light token sets equal, both light blocks identical, every `var(--x)` defined), `pages_follow_the_ui_theme`, `api::settings::tests::ui_theme_setting`, SSE payload.

## 19. Phase 7B as-built: VOD/ABR (binding; field-level detail in docs/API.md §4.7, §5, §6.6, §7)

- **Model.** `vod_items` (schema v3, `db::vod`): source probe data, `status` queued → processing → ready | failed, `progress`, `ladder`, produced `renditions` (JSON, with measured peak `bandwidth`), `plays`, `origin` upload|inbox. Files in `data/vod/<id>/`: `source.<ext>`, `poster.jpg` (10 % of the duration, ≤ 1280 px), `abr/` (output), `abr.tmp/` (being written, swapped in on success). `[vod]` config with validated `[[vod.ladders]]` (`height` = short side, 0 = source size; `video_kbps` 0 = CRF 23).
- **Pipeline (`src/vod/`).** `VodManager` (`AppState.vod`, built in `AppState::new`, tasks spawned by `main.rs`). Job: ffprobe (display size with SAR and rotation) → `select_renditions` (never upscale, even sizes, duplicates dropped, fallback rung at source size) → poster → **one ffmpeg pass**: `[0:v]split=N` + `scale=W:H` per rung (no `setsar=1`: the dash muxer needs one display aspect ratio per adaptation set), N video encodes + one AAC encode per *distinct* audio bitrate (48 kHz, mono kept mono), GOP = `segment_duration` (`-g`/`-keyint_min`/`-sc_threshold 0` + `-force_key_frames`), High@4.1 from 720p, Main below (level raised when 4.1 is too small), `-f dash … -hls_playlist 1` = CMAF segments shared by HLS and DASH. Progress from `-progress pipe:1` (≥ 1 % steps, ≤ 2/s → DB + `HubEvent::VodChanged` → admin SSE `vod`/`vod_deleted`). Timeout max(30 min, 10 × duration), `kill_on_drop`, cancellation token per job (child of shutdown).
- **Post-processing (`vod/hls.rs`).** ffmpeg's HLS master is rebuilt: one audio group per bitrate, measured `BANDWIDTH`/`AVERAGE-BANDWIDTH` from segment sizes, `FRAME-RATE`, `INDEPENDENT-SEGMENTS`, the largest rung ≤ 720p first. Media playlists get `PLAYLIST-TYPE:VOD` and lose `PROGRAM-DATE-TIME`; playlist and MPD segment URLs carry `?v=<revision>` (segments are `immutable`, file names repeat across re-transcodes); the public player info versions the master/MPD/poster URLs with `ready_at`.
- **Queue.** One runner per process: `max_parallel_jobs` slots; claims with `UPDATE … SET status='processing' WHERE id=? AND status='queued'` (1 row or nothing). Startup re-queues `processing` items, removes orphan `data/vod/<uuid>/` dirs (> 1 h, no row); graceful shutdown un-claims running jobs. Forced delete deletes the row first, then cancels and waits for the job, then removes the directory.
- **Inbox (`vod/inbox.rs`).** Scan every `scan_interval_secs`; a file is imported when size and mtime were unchanged for two consecutive scans; hidden/partial names ignored; `inbox/<ladder>/` selects the ladder (sub-folders created at startup; unknown ones warned once); failures moved to `inbox/_failed/` + `<name>.error.txt`.
- **Serving (`vod/serve.rs`).** `/vod/{id}/hls/*` and `/vod/{id}/dash/*` both map to `abr/` (relative manifest URLs stay valid), file names allow-listed per prefix; `ServeFile` (Range), CORS `*`, segments `max-age=31536000, immutable`, playlists/MPD/poster `max-age=60`. Tracker name `vod:<id>` (never a broadcast name: no `viewer_sessions` rows, not sampled); `ViewerTracker::touch_is_new` counts a play when a master/MPD request opens a session.
- **API/pages.** `api/vod.rs` (admin + public), upload streamed with the waiting-video helpers (`waiting::save_field`, `upload_tmp_dir`); `/embed/vod/{id}`, `/watch/{id}` (`vod.html`, `player.js` → `vod-player.js`), `/vod` catalogue; dashboard `#/vod` (`vod.js`) and Settings "Ladder VOD".
- **Multi-instance.** Instances may share the `data/vod` volume and the job queue: claims are atomic and every path is derived from the item id. Limits: SQLite is not meant to be shared over a network filesystem (a shared queue needs a DB that is), the startup re-queue cannot tell a crashed job from one running on another instance (no owner/heartbeat column yet), and each instance runs its own inbox scanner (only one should watch a given folder).
- **Measured (2026-10-09, 8 cores, libx264 veryfast, release build).** 1080p30 30 s → standard ladder (4 rungs, 3 AAC): 16.8 s per job (≈ 1.8× real time); 360p 10 s → mobile ladder: 0.8 s. Output 38 MB for the 30 s ladder.
- **Known limits.** H.264 only; hardware encoders are wired (`EncoderKind`: pixel format, quality option, VAAPI `hwupload`) but untested here; `ffmpeg -i master.m3u8` prints "Invalid NAL unit size" for the variants its HLS demuxer drops (also with ffmpeg's own fMP4 HLS with a separate audio group; single variants and the MPD decode silently); no thumbnails track, subtitles or multi-language audio; the admin preview counts as a play.

## 20. Phase 7C-1 as-built: store backends, node registry (binding; detail in docs/CLUSTER.md §3, docs/API.md §4.5)

- **Store.** `db::Db` = SQLite (`Db::open`, behaviour unchanged, rusqlite behind `spawn_blocking`) or PostgreSQL (`Db::connect_postgres(url)`: async sqlx pool, dynamic queries only). Every `db::<repo>::<fn>(&db, …)` keeps its signature and dispatches with `db.pg()`; SQLite and PostgreSQL statements sit side by side. `sqlx-core` + `sqlx-postgres` are used directly (the `sqlx` facade's optional `sqlx-sqlite` pins another `libsqlite3-sys`). `Db::call` is SQLite-only; `db::pg::{q, q_dyn}` build PostgreSQL queries (`q_dyn` = SQL assembled from constants).
- **Schema.** SQLite v4 adds `node_id` (viewer_samples, viewer_sessions, stream_sessions, recordings), `viewer_samples.ts_epoch` (backfilled), `recordings.lease_until`, `vod_items.{claimed_by, lease_until, attempts, storage_node}`, tables `nodes`, `live_streams`, `singleton_tasks` (text timestamps; `viewer_samples` keeps PK `(broadcast_id, ts)`). PostgreSQL gets the same schema fresh as migration 1 (`db::pg::MIGRATIONS`, `rms_schema_migrations`, one transaction under `pg_advisory_xact_lock`): BOOLEAN/BIGINT, `COLLATE "C"` on compared text, PK `(broadcast_id, node_id, ts)`, TIMESTAMPTZ + `now()` for nodes, live_streams, singleton_tasks and leases, `live_streams.node_id → nodes ON DELETE CASCADE`.
- **Node tag.** `Db::with_node_id` / `db.node_id()` is written into every node column: `""` on a single node (results identical to before), `cluster.node_id` in cluster mode. Crash recovery (stream/viewer sessions, unfinished recordings, VOD re-queue) only touches this node's rows. Metrics series = MAX per (node, bucket) then SUM; `peak(…, bucket_secs)` = max of the per-slot sum (`ViewerTracker::sample_interval_secs`).
- **Broadcasts cache** (`db::cache::BroadcastCache`, `db.broadcast_cache()`): serves `find_by_name`/`find_by_key` hits; invalidated after every `db::broadcasts` write (before handlers emit `BroadcastUpdated`), cleared by `db::settings` writes; a generation counter drops stale re-inserts; entries expire after 5 s on PostgreSQL until the 7C-2 NOTIFY listener calls `invalidate(name)` / `invalidate_all()`.
- **Leases.** A VOD claim sets `claimed_by`, `lease_until = now + 60 s`, `attempts + 1`; the job renews it every 15 s; `mark_ready`/`mark_failed`/`unclaim` require `claimed_by = me`; a lost lease cancels the job without touching its files; the runner re-queues other nodes' expired leases every 15 s; the startup re-queue takes own/unclaimed/expired rows and fails them after 3 attempts. `db::cluster::try_lease` (singleton_tasks UPSERT on the DB clock, no-op on SQLite) guards the VOD inbox scan and the metrics retention.
- **Cluster.** `[cluster]` (CLUSTER.md §3.1, `RMS_CLUSTER_*`) is validated at startup (postgres URL, advertise URL, secret ≥ 32 chars, admin token mandatory: `data/admin_token` is not used). `cluster::ClusterNode` (`AppState.cluster`): registry task (register, heartbeat every `heartbeat_secs` with live_streams/viewers/draining, re-register when reaped, reaper of rows older than `node_ttl_secs`, draining heartbeat on shutdown, `deregister()` at exit), `info()` for `/system`, `set_draining`, `last_heartbeat()` (for auto-fencing). `cluster::http::serve_internal` binds `cluster.internal_bind`: only `GET /internal/ping` behind `Authorization: Bearer <internal_secret>`.
- **HTTP.** `GET /healthz` (public): `{node_id, cluster, db, live_streams, version, draining}`, 200 even with the database down, 503 once `state.shutdown` fired or the node drains; Dockerfile/compose health checks use it. `/system` gains `cluster` and `webrtc.public_port`.
- **Prerequisites.** `metrics::client_ip` (trusted proxy): `X-Real-IP`, else the last `X-Forwarded-For` element, else the socket. `webrtc.public_port` (0 = `udp_port`) is the port of the advertised ICE candidates and of the local address the ICE agent matches; the socket stays on `udp_port`.
- **Open (7C-2+).** `live_streams` ownership, NOTIFY, relay, cluster SSE, per-node WHEP routing, shared files; HLS/DASH viewer session ids are not node-scoped (the same viewer on two nodes archives the other node's row). — Done in 7C-2 (§21).

## 21. Phase 7C-2 as-built: cluster data plane (binding; detail in docs/CLUSTER.md §3, §7, docs/API.md §4.8)

- **Ownership** (`db::live_streams`, both backends): `claim(name, node, session, ttl)` = insert, or take over a row whose heartbeat is older than the TTL or that the same node owns (`Claim::Claimed | OwnedBy(row)`); `heartbeat_all`, `owned_by`, `release(name, node, session)` (session-scoped: a newer publish on the same node keeps its row), `owner`, `list_alive`, `reap`. `RtmpIngest::on_publish` calls `ClusterNode::claim_publish` after the local stale-publisher check and before `Hub::publish_start`: when another node owns the key it `POST`s `/internal/kick/{name}` there and retries every 100 ms for ~3 s (the newest publisher wins), stops a local relay of the name, then `announce_start` (NOTIFY). `finish_publisher` → `release_publish` (+ NOTIFY `end`). Single node: every hook returns at once.
- **Registry tick** (`heartbeat_secs`): node row, then the node's stream rows: fewer rows than local streams → re-claim with the stream's `claim_id` (DB outage) or kick the local publisher (another node owns it); rows without a local stream older than 10 s are released; then reap dead streams (NOTIFY `reaped` with `owner`) and dead nodes. `deregister` NOTIFYs `end` for its rows directly.
- **Hub**: `StreamOrigin { Local, Remote(node) }` and `claim_id` on `PublishInfo`/`LiveStream`; `Hub::local_count` (healthz, node load, `/system hub.live_streams`), `Hub::kick` only kicks local publishers; `Hub::emit_shared` = local event + NOTIFY (`BroadcastUpdated`, `SettingsChanged`, `VodChanged`; used by the API handlers, the recorder and `VodManager::changed`); events received from other nodes are re-emitted with `Hub::emit` → no loop. New `HubEvent::RemoteStreamChanged{name}` (admin `stream`/`broadcast`, public `status`). `hub::flv_parser::FlvStreamParser`: tolerant incremental FLV demuxer (DataOffset, extended timestamp, PreviousTagSize ignored, unknown/encrypted tags skipped, partial reads).
- **Bus** (`cluster::bus`): `Outbox` (bounded 1024, try_send) → sender task `SELECT pg_notify` (3 s grace after shutdown); `PgListener` on `rms_broadcasts|rms_streams|rms_settings|rms_vod`, payload `{"name","op","node"[,"owner"]}` ≤ 200 B, own notices ignored; reconnect with backoff 250 ms → 10 s and `resync` (cache cleared, directory reloaded, relays reconciled, VOD runner woken). Receivers: invalidate cache entry / clear cache, re-emit, kick+close WebRTC on a remote delete, close WebRTC when `enable_webrtc` turned off, wake the VOD runner, directory update + relay stop + kick of a local publisher when another node `start`s the same name.
- **Directory** (`cluster::directory`, sync reads on request paths): fresh `live_streams` rows + alive nodes (reloaded every 2 s by `spawn_refresh`, diffed → `RemoteStreamChanged`, `reconcile_relays`), other nodes' viewers = latest sample per (broadcast, node) within 2 × `sample_interval` + 2 s (`db::metrics::recent_counts_of_other_nodes`, PG migration 2 indexes `ts_epoch`), owners' `/internal/status` (TTL 3 s). `AppState::is_live` / `viewer_counts` / `live_stream` are the cluster-aware entry points; `BroadcastStatus::of(b, state)`; `ViewContext::view` is async (remote `LiveInfo` with `node_id`).
- **Relay** (`cluster::relay`): `ClusterNode::live_stream(name)` = local stream, else (cluster, not draining, remote owner) single-flight `RelayHandle` per name: `reqwest` (rustls + ring provider installed at first use, no proxy, connect 2 s, read 10 s) `GET /internal/relay/{name}.flv` → parser → local `LiveStream` (`origin = Remote`, no `stream_sessions`, no recording); ready at the first keyframe (or first audio frame without video header, or after 3 s); waiters wait ≤ 6 s. Ends on cancel/owner change, `relay_idle_secs` without tracker viewers, or EOF/error → retry 250 ms → 5 s while the owner row lives and someone watched within `relay_idle_secs`. Used by FLV/WS-FLV (`resolve_flv`), HLS/DASH (`lowlat::resolve`, after the flag checks; waits ≤ 2 s for the package) and WHEP.
- **Internal listener** (`cluster::http::internal_router(AppState)`): ping, relay (local streams only, no viewer session), kick, status, drain/undrain, files (`recordings|waiting|vod`, `ServeFile` with Range, plain relative paths only), whep/delete. `X-Rms-Node` on every public and internal response in cluster mode.
- **Other**: WHEP ids `<node>-<uuid>` (`webrtc::new_session_id`/`session_node`), foreign `DELETE` forwarded, `307` to another node's `whep_url` when WebRTC is unusable (`cluster.whep_url`, `cluster.whep_redirect`; the public info then offers `whep`; player resolves `Location` on `res.url`); HLS/DASH viewer session ids `<node>-<sha256>`; VOD temp dir `abr.<node>.tmp`; recorder refuses relays (`RecorderError::Remote` → 409); recording download falls back to `/internal/files` on the writing node; `metrics::query::summary(…, RemoteLive)`; disconnect API kicks on the owner.
- **Measured** (one host, PG 16, 720p30): status on the other node 0.35 s after publish; relay TTFB 4–17 ms, steady-state delay +0.31 ms median; edge LL-HLS first playlist 1.1 s; end of stream → relay closed + offline 0.45 s; takeover by the other node 0.29 s; owner SIGKILL → offline after the TTL (10.0 s with ttl 10/hb 2, 14.3 s with 15/5), republish on the survivor 0.2 s; HAProxy compose: smoke 148/148, HLS sticky per `X-Real-IP`.
- **Gotchas**: two processes with the same `node_id` (restart overlap) → the old one's `deregister` deletes the new row; the registry re-registers within one tick. A draining node answers `404 stream offline` for streams it would have to relay. Relayed streams restart (viewers reconnect) at every upstream EOF; HLS/DASH timelines differ per node (strategy A). Rootless Podman on a host routing 10.0.0.0/8 needs an explicit compose subnet.

## 22. Phase 8A as-built: accounts, API keys, buckets, storage API (binding; detail in docs/ACCOUNTS.md, docs/API.md §9)

- **Schema** (SQLite v5, PostgreSQL migration 3): `users`, `api_keys`, `buckets`, `bucket_objects` as in ACCOUNTS.md §1, plus `vod_items.{user_id, bucket_id, object_id, visibility, output_bytes}` (`output_bytes` = renditions + poster, counted in quotas: an addition). SQLite runs with `foreign_keys=OFF`, so `db::users::delete` / `db::buckets::delete` cascade explicitly in one transaction on both backends. Repositories `db::{users, api_keys, buckets, objects}` (+ the storage queries of `db::vod`), every function covered by `dual_db_test!`. Directory queries use byte-ordered ranges (`["/d/", "/d0")`, direct children = no further `/` via `instr`/`strpos`).
- **Principals** (`api::principal`): `Principal::{Admin, User(UserPrincipal{user_id, key_id, scopes, permissions, …})}` resolved per request from `Bearer` (admin token, else key) → `AccessKey` → cookie `rms_token` → cookie `rms_key`; keys looked up by SHA-256 joined with the owner; revoked/expired/unknown 401, suspended 403, `last_used_at` ≤ 1/min (background). `require_admin` (user keys 403) guards the admin API incl. `/users`; `require_principal` the account and storage routes. `Principal::access(bucket, access_path)` = admin everywhere / account scope → admin on own buckets / storage scope → max level of the entries (`*` or bucket id, plain string prefix of the file path or of the directory path with its trailing `/`).
- **Auth**: login/check answer `200 {role, user?, scopes?, key_id?}` (was `204`); login accepts a key (`rms_key` cookie, other cookie cleared); logout clears both.
- **Storage** (`src/storage/`): `paths` (strict percent-decoding, NFC, rejects `..`, controls, `\`, encoded `/`, `.rms*`, empty segments, > 255 B segments, > 1024 B paths; trailing `/` = directory), `sign` (HMAC-SHA256 tokens, constant-time `verify_slice`, HLS/MPD rewriting), `api` (the `/storage/{bucket}/{*path}` handler, thin), `mod` (operations). PUT streams to `data/upload-tmp/.upload-<uuid>.bin` (1 MiB `BufWriter`, SHA-256 on the fly), checks `Content-Length` then the running size against `storage.max_object_mb` (413) and the quota room (507), verifies `Checksum` (400), then under a per-object async lock (this node) renames into `<storage.root>/<bucket id>/<path>`, upserts the row (overwrite keeps the id) and applies the encoding policy; every write ends with `db::buckets::recount` (bytes = files + `output_bytes`, count = files). `repair_user` re-stats files/outputs (`POST /users/{id}/recount`).
- **Encodings** (`VodManager`): `create_storage_item` (origin `storage`, title = file name, `source_path` = object path, no copy), `source_file` resolves bucket items to the storage root, `reencode` = hold id (runner skips held ids) → cancel local job → clear output → `requeue_any` (any status; a remote job loses its claim and cancels at its next lease renewal), `delete_item` (row, job, `data/vod/<id>`). Jobs measure the output and recount the bucket; `keep_source` never deletes a bucket original. Ladder: header → bucket → user → `vod.default_ladder`.
- **Signed playback** (`vod::serve::authorize`): visibility + signing key cached 5 s per id (`VodManager::playback_access`, invalidated by `changed()` and key rotation on this node); private items need `?token&expires` or cookie `rms_play_<id>` (set by the first query-authorized request, `Path=/vod/<id>/`); playlists/MPD of private items are rewritten with the token (`private, no-store`), segments `private, max-age=31536000, immutable`. Public info 401 / token-bearing URLs; catalogue = public only. `vod-player.js` forwards `token`/`expires` (hls.js `xhrSetup` re-opens the XHR, dash.js `addRequestInterceptor`), shows a "private video" card on 401.
- **Management API**: `api::users` (admin: users, keys, buckets, usage, encodings, recount) and `api::account` (account scope or admin `?user=`; keys, buckets, rotate, sign, encodings incl. `PATCH` (addition), SSE `/account/events` filtered by owner with a known-id set for `vod_deleted`). Validation errors `400 {error, field}`.
- **Deviations from ACCOUNTS.md**: PostgreSQL migration 3 (2 was taken by 7C-2); `output_bytes` column; login/check status 200; `?action=encode` answers the ObjectView (with `encoding`); a missing directory lists `[]`; `DELETE` of the root → 400; key/bucket ids accept names; storage-only keys need ≥ 1 permission; admin-library items cannot be private; added `GET/PATCH /account/encodings/{id}`; rotation reaches other cluster nodes within 5 s.
- **Known limits**: quotas are checked at upload time (parallel uploads and later encodings may overshoot; no per-bucket quota); the per-object lock is per node (two nodes writing the same path concurrently: last rename wins, `recount` repairs counters, not contents); bucket files need a shared `storage.root` in cluster mode (no `/internal/files` fallback); a bucket deleted while an upload is streaming answers 404 at the end; 507 is logged at ERROR by the HTTP trace layer (5xx class).
- **Measured** (2026-10-09, release build, local disk): 1 GiB `PUT` with `Checksum` in 8.8 s (122 MB/s), server RSS 22.5 → 24.8 MB; 413 with `storage.max_object_mb = 100`: immediate with `Content-Length` (0 bytes sent, `Expect: 100-continue`), after 113 MB when chunked, no temp left; headless Chrome 150: private 12 s clip via hls.js and dash.js with `?token&expires` plays (40/32 requests, all signed), with the playlists stripped of tokens by the test harness 12/12 segment requests still carry the token (player-side), without the token the page shows the 401 card; smoke 212/212 on SQLite and on a PostgreSQL cluster node.
