# rms HTTP API reference

Reference for every HTTP surface of rust-media-server (`rms`): the admin REST API, the Server-Sent
Events streams, the public player API, the playback endpoints (HTTP-FLV, WS-FLV, LL-HLS, LL-DASH,
WebRTC via WHEP) and the HTML pages. It is derived from the code (`src/api/*.rs`, `src/egress/*.rs`,
`src/packager/*.rs`, `src/webrtc/*.rs`, `src/web/mod.rs`, `src/main.rs`); where it and
[ARCHITECTURE.md](ARCHITECTURE.md) §8 differ, this file describes the behaviour of the code (see also
ARCHITECTURE.md §13, §14, §16 and §17).

Conventions used below:

- Examples use `http://127.0.0.1:8080` as the server and `evento-2026` as the broadcast name.
- `{name}` is a broadcast name: a slug `[a-z0-9_-]{1,64}`. A malformed or unknown name is always a
  `404 broadcast not found` (never a 400).
- Timestamps are RFC 3339 UTC strings (`2026-10-08T14:00:00.123Z`; the fraction is omitted when it is
  zero). Durations are seconds (JSON numbers), sizes are bytes.
- JSON request bodies must be sent with `Content-Type: application/json`; unknown fields are ignored.
- All JSON bodies are UTF-8. Admin responses do not set CORS headers (same-origin only); see
  [CORS](#cors) for what is cross-origin readable.

## Contents

1. [Authentication](#1-authentication)
2. [Errors](#2-errors)
3. [Data types](#3-data-types)
4. [Admin API](#4-admin-api): [Broadcasts](#41-broadcasts) - [Recording](#42-recording) - [Metrics](#43-metrics) -
   [Waiting video and settings](#44-waiting-video-and-settings) - [System](#45-system) - [Events (SSE)](#46-events-sse)
5. [Public player API](#5-public-player-api)
6. [Playback endpoints](#6-playback-endpoints)
7. [Pages](#7-pages)
8. [curl cookbook](#8-curl-cookbook)

---

## 1. Authentication

### Token bootstrap

The admin token is resolved once at startup, in this order:

1. `server.admin_token` in `config.toml`, or the environment variables `RMS_SERVER_ADMIN_TOKEN` /
   `RMS_ADMIN_TOKEN` (alias);
2. the contents (trimmed) of `<data_dir>/admin_token` (default `./data/admin_token`) if the file exists and is not empty;
3. otherwise a new token is generated (24 random bytes, 48 hex characters), written to
   `<data_dir>/admin_token` with mode `0600`, and printed once in the log at WARN level
   (`generated a new admin token: ...`).

There is a single shared token (no users, no expiry). It is compared in constant time.

### Bearer header or cookie

An admin endpoint accepts either:

- `Authorization: Bearer <token>` (the scheme is matched as `Bearer ` or `bearer `), or
- the cookie `rms_token=<token>` set by `POST /api/v1/auth/login`.

If a `Bearer` header is present it is the only credential considered: a wrong bearer token is a 401
even when a valid cookie is also sent. The cookie *is* the token (it is not a separate session id).
Browsers cannot set headers on `EventSource`, so the SSE streams are used with the cookie.

### `POST /api/v1/auth/login`

Public. Sets the login cookie.

| | |
|---|---|
| Body | `{"token": "<admin token>"}` (surrounding whitespace is trimmed) |
| Success | `204 No Content` with `Set-Cookie: rms_token=<token>; Path=/; HttpOnly; SameSite=Lax; Max-Age=2592000` (30 days); `; Secure` is added when the request is HTTPS (`server.public_url` starts with `https://`, or `X-Forwarded-Proto: https`) |
| `401` | wrong token (answered after a 400 ms delay to slow down brute force; logged as a warning) |
| `400` | the token is correct but contains characters that cannot go in a cookie (non-printable, `;`, `,`): `token contains characters unsuitable for a cookie; use the Authorization header`; or the body is not valid JSON / lacks `token` |

### `POST /api/v1/auth/logout`

Public, no body. Always `204 No Content` with `Set-Cookie: rms_token=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0`.

### `GET /api/v1/auth/check`

Public in the sense that it never needs a valid token to be called: `204 No Content` when the request
carries a valid token (header or cookie), `401` otherwise. The dashboard uses it to decide between the
login view and the panel.

### Which routes need the token

| Route | Auth |
|---|---|
| `/api/v1/auth/login`, `/api/v1/auth/logout`, `/api/v1/auth/check` | none (see above) |
| `/api/v1/public/**` | none |
| `/live/{name}.flv`, `/ws/live/{name}.flv`, `/hls/**`, `/dash/**`, `/whep/**` | none |
| `/`, `/embed/{name}`, `/player/{name}`, `/static/*`, `/waiting/*`, `/favicon.ico` | none |
| everything else under `/api/v1` (broadcasts, recordings incl. downloads, metrics, settings, system, events) | admin token, else `401` |

### CORS

`Access-Control-Allow-Origin: *` (permissive CORS, including preflight) is sent on the public player API,
all playback endpoints (FLV, WS-FLV, HLS, DASH, WHEP) and the pages (`/`, `/embed`, `/player`, `/static`,
`/waiting`). WHEP responses also carry `Access-Control-Expose-Headers: Location, Link, ETag` (see
[6.5](#65-webrtc-whep)). The admin API and the auth endpoints send no CORS headers.

---

## 2. Errors

Every error produced by the API handlers is JSON:

```json
{"error": "broadcast not found"}
```

with `Cache-Control: no-store` (and `WWW-Authenticate: Bearer` on 401). Internal errors (500) are logged
in detail and returned as the generic `{"error":"internal server error"}`.

| Status | Meaning in this API |
|---|---|
| `400` | invalid input: malformed JSON or wrong `Content-Type`, invalid field/query value, malformed multipart, `name cannot be changed`, blocking-reload parameters out of range |
| `401` | missing or wrong admin token |
| `403` | playback of a protocol is disabled for that broadcast (`enable_flv` / `enable_hls` / `enable_dash` / `enable_webrtc` = false), or WebRTC is disabled on the server |
| `404` | unknown or malformed broadcast name, unknown recording id or WHEP session, stream offline, unknown path (`{"error":"not found"}` from the global fallback), protocol disabled on the server |
| `405` | `PATCH` of a WHEP session (trickle ICE is not supported; JSON body and `Allow` header) |
| `409` | conflict: duplicate broadcast name, `broadcast is not live`, `broadcast is already being recorded`, `broadcast is not being recorded`, deleting a recording that is still recording/processing |
| `413` | upload larger than `server.max_upload_mb` |
| `415` | unsupported media: waiting-video upload that is not an accepted video; HLS/DASH requested for a stream whose codecs cannot be packaged; WHEP offer without `Content-Type: application/sdp` or without H.264, or a stream whose video is not H.264 |
| `422` | waiting-video upload that ffmpeg cannot decode/convert |
| `500` | internal error |
| `503` | not ready yet: playlist/MPD/part not available in time, the waiting-video conversion timed out; WHEP: WebRTC unavailable, too many sessions, stream starting |

(`501` exists in the error type but no endpoint returns it any more.)

Responses that are *not* JSON errors: `/static/*` 404 (plain text `not found`), `/embed/{name}` and
`/player/{name}` with a malformed name (plain text `broadcast not found`), `/waiting/*` 404 (empty
body), and axum's own `405 Method Not Allowed` for a known path with an unsupported method.

---

## 3. Data types

### BroadcastView

Returned by most broadcast endpoints and by the SSE `broadcast` event. The persisted broadcast is
flattened into the object, followed by the computed fields.

| Field | Type | Notes |
|---|---|---|
| `id` | string | UUID v4 |
| `name` | string | slug; used in every playback URL; immutable |
| `title` | string | 1-200 characters |
| `description` | string | up to 5000 characters, default `""` |
| `stream_key` | string | 32 lowercase hex characters; secret; regenerable |
| `record` | boolean | auto-record when a publisher connects |
| `enable_flv` | boolean | HTTP-FLV and WS-FLV playback |
| `enable_hls` | boolean | LL-HLS playback |
| `enable_dash` | boolean | LL-DASH playback |
| `enable_webrtc` | boolean | WebRTC playback via WHEP (default `true`) |
| `paused` | boolean | admin-forced "waiting" state (does not disconnect the publisher) |
| `waiting_video` | string \| null | the broadcast's *own* waiting video, relative to `data/waiting/` (`<name>/<uuid>.mp4`) |
| `waiting_text` | string | the broadcast's own waiting text; `""` means "use the global default or the built-in text". A text equal to a built-in text (`La diretta inizierà a breve`, `The live stream will start shortly`) counts as `""` |
| `created_at`, `updated_at` | timestamp | |
| `status` | `"offline"` \| `"live"` \| `"paused"` | `paused` wins over `live` |
| `viewers` | [ViewerCounts](#viewercounts) | current concurrent viewers |
| `live` | [LiveInfo](#liveinfo) \| null | non-null while a publisher is connected (also when paused) |
| `urls` | [PlaybackUrls](#playbackurls) | absolute URLs |
| `obs` | [ObsSettings](#obssettings) | what to type into OBS |
| `waiting_video_url` | string \| null | the *effective* waiting video: the broadcast's own, else the global default, else `null` |

```json
{
  "id": "5d1f0c2e-8a64-4c0b-9d57-3f2f6d0a7b11",
  "name": "evento-2026",
  "title": "Evento 2026",
  "description": "",
  "stream_key": "9f86d081884c7d659a2feaa0c55ad015",
  "record": false,
  "enable_flv": true,
  "enable_hls": true,
  "enable_dash": true,
  "enable_webrtc": true,
  "paused": false,
  "waiting_video": null,
  "waiting_text": "La diretta inizierà a breve",
  "created_at": "2026-10-08T14:00:00.123Z",
  "updated_at": "2026-10-08T14:00:00.123Z",
  "status": "live",
  "viewers": {"flv": 2, "ws_flv": 0, "hls": 1, "dash": 0, "webrtc": 0, "total": 3},
  "live": {
    "started_at": "2026-10-08T14:05:00.456Z",
    "publisher_ip": "203.0.113.7",
    "video_codec": "h264",
    "audio_codec": "aac",
    "width": 1280,
    "height": 720,
    "fps": 30.0,
    "bytes_in": 18340211,
    "recording": null,
    "packager": {
      "supported": true,
      "reason": null,
      "hls_ready": true,
      "dash_ready": true,
      "ended": false,
      "segments": 12,
      "parts": 48,
      "measured_gop_secs": 2.0,
      "last_segment_secs": 2.0,
      "part_target_secs": 0.5,
      "target_duration_secs": 2,
      "latency_hint_secs": 2.0
    }
  },
  "urls": {
    "flv": "http://127.0.0.1:8080/live/evento-2026.flv",
    "ws_flv": "ws://127.0.0.1:8080/ws/live/evento-2026.flv",
    "hls": "http://127.0.0.1:8080/hls/evento-2026/master.m3u8",
    "dash": "http://127.0.0.1:8080/dash/evento-2026/manifest.mpd",
    "whep": "http://127.0.0.1:8080/whep/evento-2026",
    "embed": "http://127.0.0.1:8080/embed/evento-2026",
    "player": "http://127.0.0.1:8080/player/evento-2026"
  },
  "obs": {"server": "rtmp://127.0.0.1:1935/live", "stream_key": "9f86d081884c7d659a2feaa0c55ad015"},
  "waiting_video_url": null
}
```

### ViewerCounts

`{"flv": u32, "ws_flv": u32, "hls": u32, "dash": u32, "webrtc": u32, "total": u32}`. `total` is the sum
of the five protocol counters. `webrtc` counts WHEP sessions whose peer connection is connected (a
session is counted from the DTLS handshake until it closes).
HLS/DASH viewers are request-based: a viewer is counted from its first playlist/segment request until
`metrics.session_timeout_secs` (default 30) pass without one.

### LiveInfo

Present in `BroadcastView.live` while a publisher is connected.

| Field | Type | Notes |
|---|---|---|
| `started_at` | timestamp | start of the current live period |
| `publisher_ip` | string \| null | IP of the RTMP publisher (no port) |
| `video_codec`, `audio_codec` | string \| null | `h264`, `hevc`, `av1`, `aac`, `mp3`, ... as detected from the sequence headers / `onMetaData` |
| `width`, `height` | integer \| null | pixels |
| `fps` | number \| null | |
| `bytes_in` | integer | bytes received from the publisher so far |
| `recording` | [Recording](#recording) \| null | the in-progress recording (`status = "recording"`), if any |
| `packager` | [PackagerStatus](#packagerstatus) \| null | LL-HLS/LL-DASH packaging status; `null` when both packagers are disabled or nothing is packaged yet |

### PackagerStatus

| Field | Type | Notes |
|---|---|---|
| `supported` | boolean | `false` when the codecs cannot be packaged (only H.264 + AAC are) |
| `reason` | string \| null | why unsupported (e.g. `video codec hevc is not supported for LL-HLS/LL-DASH (H.264 only)`), or `waiting for H.264/AAC sequence headers and a keyframe` while the packager is still probing |
| `hls_ready`, `dash_ready` | boolean | at least one complete segment exists and the protocol is enabled on the server; always `false` when unsupported |
| `ended` | boolean | the publisher left; the last window is served until `hls.linger_secs` expire |
| `segments`, `parts` | integer | segments / parts produced since the stream started |
| `measured_gop_secs` | number \| null | average keyframe interval of the encoder, two decimals (the dashboard warns above 2-3 s) |
| `last_segment_secs` | number \| null | duration of the newest complete segment |
| `part_target_secs` | number | configured `hls.part_duration` |
| `target_duration_secs` | integer | `EXT-X-TARGETDURATION` in effect |
| `latency_hint_secs` | number | expected added latency: `PART-HOLD-BACK` (3 x part) + one part |

### PlaybackUrls

`{flv, ws_flv, hls, dash, whep, embed, player}`, absolute URLs built from `server.public_url` when set,
otherwise from the request: scheme `http` (or `https` if the first `X-Forwarded-Proto` value is
`https`) plus the `Host` header (a malformed `Host` falls back to `localhost`). `ws_flv` uses `ws`/`wss`.
The URLs are shown above. They are built for every broadcast whether or not the protocol is enabled.

### ObsSettings

`{"server": "rtmp://<host>:<rtmp port>/<rtmp.app>", "stream_key": "<key>"}`. The host is the base-URL
host without its port; the port comes from `rtmp.bind`; the app from `rtmp.app` (default `live`; the
ingest accepts any app name).

### Recording

One row of the `recordings` table.

| Field | Type | Notes |
|---|---|---|
| `id` | string | UUID v4 |
| `broadcast_id` | string | UUID of the broadcast (not its name) |
| `stream_session_id` | string \| null | the live period it belongs to |
| `path` | string | relative to `data/recordings/`: `<name>/<YYYYMMDD-HHMMSS>.flv` or `.mp4` (UTC start time; `-2`, `-3`... on collisions) |
| `format` | `"flv"` \| `"mp4"` | |
| `status` | `"recording"` \| `"processing"` \| `"ready"` \| `"failed"` | `processing` = FLV closed, MP4 remux running |
| `started_at` | timestamp | |
| `ended_at` | timestamp \| null | |
| `size_bytes` | integer | refreshed every 5 s while recording |
| `duration_secs` | number \| null | |
| `error` | string \| null | failure reason; also set on `ready` rows whose MP4 remux failed (the FLV is kept) |

```json
{
  "id": "b2f6a0f4-6f4f-4b6e-9a1e-0b6f3a8d1c11",
  "broadcast_id": "5d1f0c2e-8a64-4c0b-9d57-3f2f6d0a7b11",
  "stream_session_id": "0b4c93a2-2f55-4d0e-8d6f-6a5c2f1b9e77",
  "path": "evento-2026/20261008-140500.mp4",
  "format": "mp4",
  "status": "ready",
  "started_at": "2026-10-08T14:05:00.512Z",
  "ended_at": "2026-10-08T15:02:11.087Z",
  "size_bytes": 1873412096,
  "duration_secs": 3430.575,
  "error": null
}
```

### Settings

| Field | Type | Notes |
|---|---|---|
| `default_waiting_video` | string \| null | global default video, relative to `data/waiting/` (`_default/<uuid>.mp4`) |
| `default_waiting_video_url` | string \| null | absolute `/waiting/...` URL of it |
| `default_poster_url` | string \| null | URL of its poster (`.jpg`), only if the file exists |
| `default_waiting_text` | string | effective global text: the custom one, else the built-in text of `ui_language` (`it`: `La diretta inizierà a breve`, `en`: `The live stream will start shortly`) |
| `max_upload_mb` | integer | `server.max_upload_mb` |
| `ffmpeg_available` | boolean | whether ffmpeg was found at startup |
| `ui_language` | `"it"` \| `"en"` | language of the dashboard and of the player texts (setting key `ui_language`, default `"it"`) |

```json
{
  "default_waiting_video": "_default/3c2d1e0f9a8b4c7d8e6f5a4b3c2d1e0f.mp4",
  "default_waiting_video_url": "http://127.0.0.1:8080/waiting/_default/3c2d1e0f9a8b4c7d8e6f5a4b3c2d1e0f.mp4",
  "default_poster_url": "http://127.0.0.1:8080/waiting/_default/3c2d1e0f9a8b4c7d8e6f5a4b3c2d1e0f.jpg",
  "default_waiting_text": "La diretta inizierà a breve",
  "max_upload_mb": 1024,
  "ffmpeg_available": true,
  "ui_language": "it"
}
```

---

## 4. Admin API

Base path `/api/v1`. Every endpoint in this section needs the admin token (`401` otherwise; not repeated
below). `{name}` problems are always `404 broadcast not found`.

### 4.1 Broadcasts

#### `GET /api/v1/broadcasts`

List all broadcasts, newest first (`created_at` descending, then `name`).

- Response `200`: `[BroadcastView]`

#### `POST /api/v1/broadcasts`

Create a broadcast.

| Body field | Type | Default | Notes |
|---|---|---|---|
| `title` | string | (required) | trimmed; 1-200 characters |
| `name` | string | derived from `title` | `[a-z0-9_-]{1,64}`. If absent/blank, a slug is derived from the title (lowercase ASCII, accents transliterated, other characters collapsed to `-`) and made unique with `-2`, `-3`...; if the title yields no characters the name is `live-<6 hex>` |
| `description` | string | `""` | up to 5000 characters |
| `record` | boolean | `false` | auto-record |
| `enable_flv`, `enable_hls`, `enable_dash`, `enable_webrtc` | boolean | `true` | |
| `waiting_text` | string | `La diretta inizierà a breve` | up to 500 characters; `""` = use the global default. The default (a built-in text) also counts as `""`, so the player shows the global default or the built-in text in the UI language |

- Response `201`: `BroadcastView` (status `offline`, a fresh `stream_key`).
- `400`: missing/empty title, title/description/waiting_text too long, invalid `name`
  (`name must match [a-z0-9-_]{1,64} (lowercase letters, digits, '-' and '_')`), bad JSON.
- `409`: `a broadcast named "evento-2026" already exists` (or no free slug could be found).

```json
{"title": "Evento 2026", "record": true, "enable_dash": false}
```

#### `GET /api/v1/broadcasts/{name}`

- Response `200`: `BroadcastView`. `404` unknown.

#### `PATCH /api/v1/broadcasts/{name}`

Partial update; only the supplied fields change.

| Body field | Type | Notes |
|---|---|---|
| `title` | string | 1-200 characters, not empty |
| `description` | string | up to 5000 |
| `record`, `enable_flv`, `enable_hls`, `enable_dash`, `enable_webrtc` | boolean | |
| `waiting_text` | string | up to 500; `""` = use the global default |
| `name` | string | accepted only when equal to the current name; otherwise `400 name cannot be changed` |

- Response `200`: `BroadcastView`. `400`, `404` as above.
- `enable_*` changes take effect on new requests (the packager caches the flags for at most 5 s).
  Setting `enable_webrtc` to `false` also closes the broadcast's open WebRTC sessions (players fall back
  to the next source).

#### `DELETE /api/v1/broadcasts/{name}`

Disconnects the publisher (if live), closes its WebRTC sessions, deletes the broadcast's waiting video
and poster, and removes the row. Recordings and their files are kept.

- Response `204 No Content`. `404` unknown.

#### `POST /api/v1/broadcasts/{name}/regenerate-key`

New 32-hex `stream_key`. The current publisher stays connected; use `/disconnect` to kick it.

- Response `200`: `BroadcastView` (with the new key in `stream_key` and `obs.stream_key`).

#### `POST /api/v1/broadcasts/{name}/pause` and `/resume`

Set / clear `paused`. Idempotent. Pause does not disconnect the publisher, stop recording or stop the
playback endpoints: it only makes the public status `paused` so that players show the waiting video.

- Response `200`: `BroadcastView`.

#### `POST /api/v1/broadcasts/{name}/disconnect`

Kicks the RTMP publisher.

- Response `204 No Content`. `409 broadcast is not live`.

#### `GET /api/v1/broadcasts/{name}/embed`

Ready-to-paste embed code.

- Response `200`:

```json
{
  "iframe": "<iframe src=\"http://127.0.0.1:8080/embed/evento-2026\" width=\"640\" height=\"360\" style=\"border:0;max-width:100%;aspect-ratio:16/9;height:auto\" allow=\"autoplay; fullscreen; picture-in-picture\" allowfullscreen></iframe>",
  "urls": { "flv": "...", "ws_flv": "...", "hls": "...", "dash": "...", "whep": "...", "embed": "...", "player": "..." },
  "obs": { "server": "rtmp://127.0.0.1:1935/live", "stream_key": "9f86d081884c7d659a2feaa0c55ad015" },
  "snippets": {
    "hlsjs": "<video id=\"rms-player\" ...></video>\n<script src=\"https://cdn.jsdelivr.net/npm/hls.js@1.7.3/dist/hls.min.js\"></script>...",
    "mpegtsjs": "... mpegts.js@1.8.2 ... (HTTP-FLV)",
    "dashjs": "... dashjs@5.2.1 ... (LL-DASH)",
    "whep": "<video id=\"rms-player\" ...></video>\n<script>... RTCPeerConnection ... fetch(endpoint, {method: 'POST', ...}) ...</script>"
  }
}
```

`urls` is a [PlaybackUrls](#playbackurls), `obs` an [ObsSettings](#obssettings). The `snippets` are
self-contained HTML (a `<video>` element, the pinned CDN script and a few lines of player setup; `whep`
needs no library: ~25 lines of vanilla JavaScript with `RTCPeerConnection`, recvonly video and audio
transceivers, the WHEP `POST`, and a `DELETE` of the session on `pagehide`). The
iframe `src` has no query string; see [`/embed/{name}`](#embedname-and-playername) for the parameters
you can append.

### 4.2 Recording

Business rules: a recording is an FLV file written by the server while the broadcast is live
(`data/recordings/<name>/<YYYYMMDD-HHMMSS>.flv`, UTC time). When ffmpeg is available and
`recording.remux_to_mp4` is true, the FLV is remuxed to MP4 (`-c copy -movflags +faststart`) after the
recording stops and the FLV is deleted unless `recording.keep_flv`. Without ffmpeg (or if the remux
fails) the FLV stays and the row is `ready` (with `error` set after a failed remux). Automatic
recording (`record = true`) is evaluated once per live period: a manual stop is not undone.

#### `POST /api/v1/broadcasts/{name}/record/start`

No body. Starts a manual recording of the live stream.

- Response `200`: [Recording](#recording) with `status: "recording"`, `format: "flv"`.
- `409`: `broadcast is not live` / `broadcast is already being recorded`.

#### `POST /api/v1/broadcasts/{name}/record/stop`

No body. Closes the file (waits up to 15 s for it to be flushed and fsynced).

- Response `200`: the [Recording](#recording) in its post-stop state: `processing` while the MP4 remux
  runs (it continues in the background; poll the list), `ready` when no remux is needed, `failed` if
  no media was received or a write error occurred.
- `409 broadcast is not being recorded`.

#### `GET /api/v1/broadcasts/{name}/recordings`

- Response `200`: `[Recording]` of that broadcast, newest first (`started_at` descending).

#### `GET /api/v1/recordings`

| Query | Type | Default | Notes |
|---|---|---|---|
| `broadcast` | string | all | broadcast *name*; blank = all; unknown name = `404` |

- Response `200`: `[Recording]`, newest first.

#### `GET /api/v1/recordings/{id}/download`

Serves the recording file (`ServeFile`: `Range` requests / `206`, `Accept-Ranges: bytes`, `Content-Type`
from the extension: `video/mp4` or `video/x-flv`).

| Query | Type | Default | Notes |
|---|---|---|---|
| `download` | string | absent | `1`, `true` or `yes` -> `Content-Disposition: attachment`; otherwise `inline` (so a `<video>` can play it) |

- Response `200`/`206`: the bytes, with `Content-Disposition: <inline|attachment>; filename="<name>-<YYYYMMDD-HHMMSS>.<ext>"`
  and `Cache-Control: private, no-cache`.
- `404`: unknown id (`recording not found`) or the file is gone (`recording file not found`).
- There is no status check: while a recording is in progress or being processed the (partial) file at
  its current path can be fetched.

#### `DELETE /api/v1/recordings/{id}`

Deletes the row and its files (the recorded file plus a sibling `.flv` / `.mp4` / `.mp4.tmp` of the same stem).

- Response `204 No Content`.
- `404 recording not found`. `409 recording is still in progress or being processed`.

### 4.3 Metrics

Viewer counts are sampled every `metrics.sample_interval_secs` (default 5) for every *live* stream and
stored in SQLite (`viewer_samples`); every viewer session is stored in `viewer_sessions`. Both are
pruned after `metrics.retention_days` (default 90).

#### `GET /api/v1/metrics/overview`

- Response `200`:

```json
{
  "live_broadcasts": 1,
  "total_viewers": {"flv": 2, "ws_flv": 0, "hls": 1, "dash": 0, "webrtc": 0, "total": 3},
  "per_broadcast": [
    {"name": "evento-2026", "title": "Evento 2026", "status": "live",
     "viewers": {"flv": 2, "ws_flv": 0, "hls": 1, "dash": 0, "webrtc": 0, "total": 3}}
  ]
}
```

`live_broadcasts` counts connected publishers (a paused live broadcast is counted); `per_broadcast`
lists every broadcast, newest first.

#### `GET /api/v1/broadcasts/{name}/sessions`

Currently connected viewers, oldest first.

- Response `200`:

```json
[
  {"id": "7f1d2c9e4b8a4f6c9d3e5a1b0c2d4e6f", "protocol": "flv", "ip": "198.51.100.23",
   "user_agent": "Mozilla/5.0 ...", "started_at": "2026-10-08T14:06:12.004Z"}
]
```

`protocol` is one of `flv`, `ws_flv`, `hls`, `dash`, `webrtc`. For `flv`/`ws_flv` the `id` is a random
32-hex id; for `hls`/`dash` it is `sha256(ip|user_agent|name|protocol)` in hex (64 characters). The IP
honours `X-Forwarded-For` / `X-Real-IP` only when `server.trust_proxy = true`. `user_agent` is cut at
512 characters.

#### `GET /api/v1/broadcasts/{name}/metrics`

Viewer time series.

| Query | Type | Default | Notes |
|---|---|---|---|
| `from` | RFC 3339 | `to` minus 1 hour | blank = default; a `+` in a timezone offset that arrived unencoded is accepted |
| `to` | RFC 3339 | now | |
| `step` | integer seconds | automatic | `1..=86400`. Rounded *up* to the next allowed bucket size |

Rules (`400` with an explanatory message when violated): `from < to`, range at most 90 days, `step` an
integer in `1..=86400`.

Bucket size: the smallest of `5, 10, 30, 60, 300, 900, 3600, 7200, 21600, 86400` seconds that is at
least `max(step, metrics.sample_interval_secs)` and yields at most 600 points (the largest if none
does). Buckets are aligned to the Unix epoch; each point holds the **maximum** of every column over
the samples of its bucket. The series is *dense*: buckets without samples (stream offline) are
returned as zeros, no point lies after "now", and the bucket containing "now" is omitted until it has a
sample.

- Response `200`:

```json
{
  "from": "2026-10-08T13:10:00Z",
  "to": "2026-10-08T14:10:00Z",
  "step": 10,
  "points": [
    {"t": "2026-10-08T14:09:40Z", "total": 3, "flv": 2, "ws_flv": 0, "hls": 1, "dash": 0, "webrtc": 0},
    {"t": "2026-10-08T14:09:50Z", "total": 4, "flv": 2, "ws_flv": 0, "hls": 1, "dash": 1, "webrtc": 0}
  ]
}
```

`from`/`to` are the resolved range (millisecond precision), `step` the bucket size actually used,
`points[].t` the bucket start.

#### `GET /api/v1/broadcasts/{name}/metrics/summary`

| Query | Type | Default | Notes |
|---|---|---|---|
| `from`, `to` | RFC 3339 | last hour ending now | same rules as above |

- Response `200`:

```json
{
  "from": "2026-10-08T13:10:00Z",
  "to": "2026-10-08T14:10:00Z",
  "current": {"flv": 2, "ws_flv": 0, "hls": 1, "dash": 0, "webrtc": 0, "total": 3},
  "peak": {"value": 7, "at": "2026-10-08T13:41:25Z"},
  "sessions_total": 19,
  "avg_watch_secs": 842.317,
  "live_since": "2026-10-08T13:00:02.456Z",
  "bytes_in": 1048576000
}
```

- `current` and `live_since` describe "now" (`live_since` is `null` when the broadcast is not live).
- `peak` is the highest sampled total in the range, or `null` when there is nothing to report; when
  "now" falls inside the range the live count is also considered (`at` = now if it is the maximum).
- `sessions_total` counts viewer sessions (all protocols) that overlap the range; `avg_watch_secs` is
  the average full duration of those sessions (open ones measured up to now), rounded to milliseconds.
- `bytes_in` sums the bytes received during live periods overlapping the range, including the current one.

### 4.4 Waiting video and settings

The waiting video is shown by the player while the broadcast is offline or paused. A broadcast without
its own video and/or text falls back to the global defaults, then to the built-in text.

Uploads (`POST .../waiting-video`) are `multipart/form-data` with a part named `file`:

- The part is streamed to `data/waiting/tmp/` (never buffered in memory). The request body limit is
  `server.max_upload_mb` MiB + 64 KiB of framing (default 1024 MiB).
- Accepted containers (by file extension, else by MIME type): `mp4`, `mov`, `m4v`, `webm`, `mkv`, `avi`.
- With ffmpeg the video is normalised to H.264 (libx264, veryfast, CRF 23) + AAC 128 kbit/s MP4 with
  `+faststart`, at most 1920 px wide (even dimensions), first video stream plus the audio if any; a poster
  frame (at 1 s, else 0 s) is saved next to it as `<same name>.jpg`. At most two conversions run at once;
  each is limited to 15 minutes.
- Without ffmpeg only MP4 files (`ftyp` signature) are accepted and stored as-is, with no poster.
- Stored as `data/waiting/<broadcast name>/<uuid>.mp4`; the global default goes to
  `data/waiting/_default/<uuid>.mp4`. Replacing or deleting a video removes the old file and its poster.

Common error codes of both upload endpoints: `400` (not multipart, missing `file` part, empty file,
malformed multipart), `413` (larger than the limit), `415` (`unsupported file type: upload a video (mp4,
mov, m4v, webm, mkv, avi)`, or `ffmpeg not available: upload an MP4 (H.264/AAC)`), `422`
(`could not process the video: <ffmpeg's last error line>`), `503` (`video processing timed out`), `500`.

#### `POST /api/v1/broadcasts/{name}/waiting-video`

- Request: multipart `file`.
- Response `200`: `BroadcastView` with `waiting_video` = `"evento-2026/<uuid>.mp4"` and the matching `waiting_video_url`.
- `404` unknown broadcast (checked before the body is read).

#### `DELETE /api/v1/broadcasts/{name}/waiting-video`

Removes the broadcast's own video (idempotent); the global default applies again.

- Response `200`: `BroadcastView`.

#### `GET /api/v1/settings`

- Response `200`: [Settings](#settings).

#### `PATCH /api/v1/settings`

| Body field | Type | Notes |
|---|---|---|
| `default_waiting_text` | string | trimmed; up to 500 characters; `""` (or a built-in text) restores the built-in text, which follows `ui_language`; omitted = unchanged |
| `ui_language` | string | `"it"` or `"en"` (exact, lowercase); omitted = unchanged |

- Response `200`: [Settings](#settings). `400` if the text is too long or `ui_language` is anything else
  (`ui_language must be "it" or "en"`); nothing is written on a `400`.
- A request that sets either field sends a `settings` event on the admin SSE stream
  ([4.6](#46-events-sse)), so open dashboards switch language without a reload.

#### `POST /api/v1/settings/waiting-video`

Upload of the global default video (same pipeline and limits as above, stored under `_default/`).

- Request: multipart `file`. Response `200`: [Settings](#settings).

#### `DELETE /api/v1/settings/waiting-video`

- Response `200`: [Settings](#settings) (idempotent; the file and poster are deleted).

### 4.5 System

#### `GET /api/v1/system`

- Response `200`:

```json
{
  "version": "0.1.0",
  "uptime_secs": 86412,
  "ffmpeg": {"available": true, "path": "/home/media/.local/bin/ffmpeg", "version": "ffmpeg version 7.1 Copyright (c) 2000-2025 the FFmpeg developers"},
  "rtmp_bind": "0.0.0.0:1935",
  "http_bind": "0.0.0.0:8080",
  "public_url": "",
  "hub": {"live_streams": 1},
  "webrtc": {
    "enabled": true,
    "available": true,
    "udp_port": 8443,
    "public_ip": [],
    "candidates": ["192.168.1.20:8443"],
    "ice_lite": true,
    "sessions": 2,
    "publishers": 1,
    "audio": true,
    "error": null
  }
}
```

`ffmpeg.path` / `ffmpeg.version` are `null` when ffmpeg is unavailable (`version` is the first line of
`ffmpeg -version`). `public_url` is `server.public_url` as configured (empty = derived from `Host`).

`webrtc`: `enabled` is `webrtc.enabled`; `available` is false when disabled or when the UDP port could
not be bound (`error` says why); `candidates` are the ICE host candidates given to viewers
(`webrtc.public_ip`, else the IPv4 addresses of the local interfaces, with `udp_port`); `sessions` counts
WHEP sessions (connecting or connected); `publishers` the broadcasts with a running WebRTC publisher;
`audio` is false without ffmpeg (sessions are then video-only).

### 4.6 Events (SSE)

#### `GET /api/v1/events`

Admin Server-Sent Events stream (`Content-Type: text/event-stream`, `Cache-Control: no-cache`,
`X-Accel-Buffering: no`). Authenticate with the cookie (browser `EventSource`) or a Bearer header
(`curl -N`). A `heartbeat` is sent immediately, then every 15 s. The stream ends when the server shuts down.

| Event | Data (JSON) | When |
|---|---|---|
| `heartbeat` | `{"ts": "2026-10-08T14:00:00.000Z"}` | on connect and every 15 s |
| `broadcast` | a full [BroadcastView](#broadcastview) | any change of a broadcast: create, patch, pause/resume, regenerate-key, waiting video, recording start/stop/finalize, stream start/end, stream metadata (codec info) update |
| `stream` | `{"name": "evento-2026", "status": "live"}` | stream start/end; `status` is the new `offline`/`live`/`paused`; sent right before the matching `broadcast` event |
| `viewers` | `{"name": "evento-2026", "counts": ViewerCounts}` | the viewer counts of a broadcast changed |
| `broadcast_deleted` | `{"name": "evento-2026"}` | a broadcast was deleted |
| `settings` | `{"ui_language": "en", "default_waiting_text": "The live stream will start shortly"}` | after a `PATCH /api/v1/settings` that set `ui_language` and/or `default_waiting_text` (effective values) |

```
event: viewers
data: {"name":"evento-2026","counts":{"flv":3,"ws_flv":0,"hls":1,"dash":0,"webrtc":0,"total":4}}

event: stream
data: {"name":"evento-2026","status":"live"}
```

Events are not replayed: after a reconnect, re-read `GET /api/v1/broadcasts`.

---

## 5. Public player API

No authentication, permissive CORS. These endpoints never expose the stream key; they are what the
embed player and the dashboard login page use.

#### `GET /api/v1/public/ui`

UI settings needed before login (the dashboard reads the language here before showing the login view).

- Response `200`: `{"language": "it"}` (`"it"` or `"en"`, the `ui_language` setting).

#### `GET /api/v1/public/broadcasts/{name}`

- Response `200`:

```json
{
  "name": "evento-2026",
  "title": "Evento 2026",
  "status": "live",
  "waiting_video_url": "http://127.0.0.1:8080/waiting/evento-2026/3c2d1e0f9a8b4c7d8e6f5a4b3c2d1e0f.mp4",
  "waiting_text": "La diretta inizierà a breve",
  "poster_url": "http://127.0.0.1:8080/waiting/evento-2026/3c2d1e0f9a8b4c7d8e6f5a4b3c2d1e0f.jpg",
  "sources": {
    "flv": "http://127.0.0.1:8080/live/evento-2026.flv",
    "ws_flv": "ws://127.0.0.1:8080/ws/live/evento-2026.flv",
    "hls": "http://127.0.0.1:8080/hls/evento-2026/master.m3u8",
    "dash": null,
    "whep": "http://127.0.0.1:8080/whep/evento-2026"
  },
  "enable_flv": true,
  "enable_hls": true,
  "enable_dash": false,
  "enable_webrtc": true,
  "ui_language": "it"
}
```

| Field | Type | Notes |
|---|---|---|
| `name`, `title` | string | |
| `status` | `"offline"` \| `"live"` \| `"paused"` | |
| `waiting_video_url` | string \| null | effective waiting video (own, else global default) |
| `waiting_text` | string | *effective* text: the broadcast's, else the global default, else the built-in text in `ui_language` (`La diretta inizierà a breve` / `The live stream will start shortly`) |
| `poster_url` | string \| null | the video's `.jpg` poster, only if that file exists |
| `sources` | object | `flv`, `ws_flv`, `hls`, `dash`, `whep`: URL, or `null` when the protocol is disabled. `flv` and `ws_flv` follow `enable_flv`; `hls` / `dash` are `null` when disabled for the broadcast *or* on the server (`hls.enabled` / `dash.enabled`); `whep` is `null` when `enable_webrtc` is false, WebRTC is disabled or unavailable on the server (`webrtc.enabled`, UDP port not bound), or the live video codec is known and is not H.264 |
| `enable_flv`, `enable_hls`, `enable_dash`, `enable_webrtc` | boolean | `enable_hls` / `enable_dash` / `enable_webrtc` already include the server-wide conditions (`enable_webrtc` = `sources.whep` is non-null) |
| `ui_language` | `"it"` \| `"en"` | the UI language setting: the player's texts follow it unless the page has `?lang=` |

`404 broadcast not found` for an unknown name.

#### `GET /api/v1/public/broadcasts/{name}/events`

Server-Sent Events with the status of one broadcast (`Content-Type: text/event-stream`,
`X-Accel-Buffering: no`). `404` (JSON) if the broadcast does not exist when connecting.

| Event | Data | When |
|---|---|---|
| `status` | `{"status": "offline" \| "live" \| "paused"}` | immediately on connect, then only when the status changes (a broadcast deleted while connected reports `offline`) |
| `heartbeat` | `{"ts": "2026-10-08T14:00:00.000Z"}` | every 15 s |

```
event: status
data: {"status":"live"}
```

---

## 6. Playback endpoints

All playback endpoints are public, answer with `Access-Control-Allow-Origin: *`, and count the viewer
in the metrics (`viewers` in the [BroadcastView](#broadcastview)). A broadcast being `paused` does
**not** stop playback: only the public status changes. The URLs of a broadcast are in `urls`
([PlaybackUrls](#playbackurls)).

### 6.1 HTTP-FLV

`GET /live/{name}.flv`

- Response `200`, `Content-Type: video/x-flv`, `Cache-Control: no-cache`, `X-Accel-Buffering: no`,
  chunked, endless until the stream ends, the client disconnects or the server shuts down.
- Body: FLV header, then (in the first chunk) the script/metadata tag, the video and audio sequence
  headers and the GOP cache (everything since the last keyframe), then one chunk per live tag.
  Timestamps are passed through as received. HEVC/AV1 (Enhanced RTMP) streams are carried as-is.
- A slow client that falls behind skips ahead to the next video keyframe (the publisher is never blocked).
- Errors (JSON): `404 broadcast not found` (unknown name, or the file name does not end in `.flv`),
  `403 FLV playback is disabled for this broadcast`, `404 stream offline`.
- Viewer counted as `flv` for the lifetime of the connection.

### 6.2 WebSocket-FLV

`GET /ws/live/{name}.flv` with a WebSocket upgrade (`ws://` or `wss://`).

- Same byte stream as HTTP-FLV, as binary messages: the first message holds the FLV header plus the
  catch-up tags, then one message per tag. The server answers pings, ignores any other client message,
  and sends a Close frame when the stream ends.
- The checks of 6.1 (404 / 403) happen *before* the upgrade and return the JSON errors above. A request
  without the WebSocket upgrade headers is rejected by the upgrade extractor (4xx, plain text).
- Viewer counted as `ws_flv`.
- Playable with mpegts.js: `mpegts.createPlayer({type: 'flv', isLive: true, url: 'ws://host/ws/live/evento-2026.flv'})`.

### 6.3 LL-HLS

All paths are `GET /hls/{name}/{file}`. URIs inside the playlists are relative.

| File | Content-Type | Cache-Control | Notes |
|---|---|---|---|
| `master.m3u8` (alias `index.m3u8`) | `application/vnd.apple.mpegurl` | `no-cache` | one variant that points to `stream.m3u8` |
| `stream.m3u8` | `application/vnd.apple.mpegurl` | `no-cache` | media playlist, blocking reload |
| `init.mp4` | `video/mp4` | `no-cache` | `ftyp`+`moov` (video + audio tracks) |
| `seg-{N}.m4s` | `video/iso.segment` | `max-age=3600` (complete) | a whole segment; see below |
| `part-{N}-{P}.m4s` | `video/iso.segment` | `max-age=3600` | one CMAF chunk (part `P` of segment `N`) |

Any other file name: `404 {"error":"not found"}`.

**Resolution and errors** (all file types, in this order): `404 broadcast not found` (unknown name);
`404 HLS is disabled on this server` (`hls.enabled = false`); `403 HLS playback is disabled for this
broadcast`; `404 stream offline` (nothing live, and no stream lingering after its end); `415` with the
packager's reason when the codecs are not H.264 + AAC (e.g. `video codec hevc is not supported for
LL-HLS/LL-DASH (H.264 only)`). After the publisher leaves, the last window stays available for
`hls.linger_secs` (default 30 s) with `#EXT-X-ENDLIST`; a new publish replaces it immediately.

**Startup wait.** `master.m3u8`/`stream.m3u8` wait up to 8 s for the first complete segment (about one
GOP after the stream starts). If none appears: `503 stream is starting, retry shortly`. If the stream
ended before producing one: `404 stream offline`. `init.mp4` waits up to 8 s for the init segment (same 503).

**`stream.m3u8` query parameters (blocking playlist reload)**

| Query | Type | Meaning |
|---|---|---|
| `_HLS_msn` | integer | block until media sequence number `N` is complete (when `_HLS_part` is absent) |
| `_HLS_part` | integer | with `_HLS_msn`: block until part `P` of that segment exists. `_HLS_part` alone is `400 _HLS_part requires _HLS_msn` |
| `_HLS_skip` | - | ignored (no delta updates) |

- If the requested segment/part is already available (or already left the window) the playlist is
  returned immediately; after the stream ended every request is answered immediately.
- `400 _HLS_msn is more than one segment beyond the live edge` when `_HLS_msn` is greater than the newest
  complete segment number + 2 (i.e. more than one segment beyond the in-progress one). A non-numeric
  value is also `400`.
- `503 playlist update timed out` after 3 x `EXT-X-TARGETDURATION` seconds (6 s with 2 s segments).

**`init.mp4?g=N`**: `g` selects the init segment of a codec generation (the playlist uses it after the
encoder changed its sequence headers: `#EXT-X-DISCONTINUITY` + `#EXT-X-MAP:URI="init.mp4?g=1"`); without
it, generation 0. Unknown generation: `404 init segment not found`.

**`seg-{N}.m4s`**: complete segment: the concatenated parts, with `max-age=3600`. The segment being
produced: `200`, chunked, one complete part at a time until the segment completes (`no-cache`,
`X-Accel-Buffering: no`). The segment right after the in-progress one: waits up to 3 x target duration
for it to open (`503 segment not available yet` on timeout). Evicted or unknown: `404 segment not found`.

**`part-{N}-{P}.m4s`**: returns as soon as the part exists (preload hints); waits up to 3 x target
duration for a part that is about to be produced (`503 part not available yet`); `404 part not found`
when it will never exist (segment complete without that part, segment evicted, stream ended).

**Segment numbering.** Media time is the Unix epoch, so the first segment number of a publishing session
is the Unix second of its first keyframe (for example `1791468302`) and the following ones count up by
one. Numbers therefore never repeat across sessions of the same broadcast, and segment/part URLs are
safe to cache.

**Playlist content.** `EXT-X-VERSION:9`; `EXT-X-TARGETDURATION` (longest segment rounded, never
decreasing); `EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=YES,PART-HOLD-BACK=<3 x part>` (no
`CAN-SKIP-UNTIL`); `EXT-X-PART-INF:PART-TARGET=<hls.part_duration>`; `EXT-X-MEDIA-SEQUENCE`;
`EXT-X-DISCONTINUITY-SEQUENCE` when needed; `EXT-X-MAP`; `EXT-X-PROGRAM-DATE-TIME` on the first segment
and after each discontinuity; `EXT-X-PART` lines for the in-progress segment and the newest complete
ones (at least 3 segments and 3 target durations); a trailing `EXT-X-PRELOAD-HINT` for the next part;
`EXT-X-ENDLIST` after the stream ended. One segment is one GOP (a GOP longer than
`hls.max_segment_duration` is split at a part boundary), and the window keeps `hls.window_segments`
complete segments (minimum 3).

```
#EXTM3U
#EXT-X-VERSION:9
#EXT-X-TARGETDURATION:2
#EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=YES,PART-HOLD-BACK=1.500
#EXT-X-PART-INF:PART-TARGET=0.500
#EXT-X-MEDIA-SEQUENCE:1791468302
#EXT-X-MAP:URI="init.mp4"
#EXT-X-PROGRAM-DATE-TIME:2026-10-08T14:05:02.000Z
#EXTINF:2.000,
seg-1791468302.m4s
#EXT-X-PART:DURATION=0.500,URI="part-1791468303-0.m4s",INDEPENDENT=YES
#EXT-X-PART:DURATION=0.500,URI="part-1791468303-1.m4s"
#EXT-X-PART:DURATION=0.500,URI="part-1791468303-2.m4s"
#EXT-X-PART:DURATION=0.500,URI="part-1791468303-3.m4s"
#EXTINF:2.000,
seg-1791468303.m4s
#EXT-X-PART:DURATION=0.500,URI="part-1791468304-0.m4s",INDEPENDENT=YES
#EXT-X-PRELOAD-HINT:TYPE=PART,URI="part-1791468304-1.m4s"
```

Master playlist: `#EXT-X-STREAM-INF:BANDWIDTH=<peak>,AVERAGE-BANDWIDTH=<avg>,CODECS="avc1.64001f,mp4a.40.2",RESOLUTION=1280x720,FRAME-RATE=30.000`
followed by `stream.m3u8` (bandwidth is measured from the segments, else from the encoder's declared
bitrates, else 3 Mbit/s).

Viewer tracking: every request except `init.mp4` counts as activity of an `hls` viewer (keyed by IP +
User-Agent), expiring after `metrics.session_timeout_secs` without requests.

Client example: `hls.js` with `{lowLatencyMode: true}` on `.../hls/evento-2026/master.m3u8`; Safari and
iOS play it natively.

### 6.4 LL-DASH

| Path | Content-Type | Cache-Control | Notes |
|---|---|---|---|
| `GET /dash/{name}/manifest.mpd` | `application/dash+xml` | `no-cache` | dynamic MPD |
| `GET /dash/{name}/init-video.m4s[?g=N]` | `video/mp4` | `no-cache` | video init segment |
| `GET /dash/{name}/init-audio.m4s[?g=N]` | `audio/mp4` | `no-cache` | audio init segment |
| `GET /dash/{name}/chunk-video-{time}.m4s` | `video/mp4` | `max-age=3600` (complete) | video segment starting at `{time}` |
| `GET /dash/{name}/chunk-audio-{time}.m4s` | `audio/mp4` | `max-age=3600` (complete) | audio segment starting at `{time}` |
| `GET /dash/time` | `text/plain; charset=utf-8` | `no-cache` | server UTC time |

Any other file name: `404 {"error":"not found"}`. Resolution and errors are those of LL-HLS
(`404 DASH is disabled on this server`, `403 DASH playback is disabled for this broadcast`, `404 stream
offline`, `415`, ...).

**`GET /dash/time`**: public, needs no broadcast; the current UTC time in ISO 8601 with milliseconds,
e.g. `2026-10-08T14:00:00.000Z`. It is the `UTCTiming` source (`urn:mpeg:dash:utc:http-xsdate:2014`).

**Manifest.** Waits up to 8 s for the first complete segment (`503 stream is starting, retry shortly`,
or `404 stream offline` if the stream ended first). Right after a stream started it may hold the
response for up to 3 s so that dash.js does not start before the first segment. Content:
`type="dynamic"`, `availabilityStartTime="1970-01-01T00:00:00Z"` (media time is the Unix epoch),
`publishTime`, `minimumUpdatePeriod="PT1S"` (removed, and `mediaPresentationDuration` added, after the
stream ended), `minBufferTime="PT1S"`, `timeShiftBufferDepth` = the window,
`ServiceDescription` (`Latency` target 2000 / min 1500 / max 6000 ms, `PlaybackRate` 0.96-1.04),
`UTCTiming` pointing to the absolute URL `<base>/dash/time`, one `Period` per codec generation, and per
track (`video`, and `audio` if present) a `SegmentTemplate` with `SegmentTimeline` and `$Time$`
addressing (`media="chunk-$RepresentationID$-$Time$.m4s"`, explicit `t` on every `S`),
`availabilityTimeOffset` = nominal segment minus part duration and `availabilityTimeComplete="false"`.
The video timescale is 90000; the audio timescale is the AAC sample rate (e.g. 48000). The in-progress
segment is listed as the last `S` once its first chunk exists, with its expected duration.

```xml
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" profiles="urn:mpeg:dash:profile:isoff-live:2011" type="dynamic"
     availabilityStartTime="1970-01-01T00:00:00Z" publishTime="2026-10-08T14:05:10.500Z"
     minimumUpdatePeriod="PT1S" minBufferTime="PT1S" timeShiftBufferDepth="PT12S"
     maxSegmentDuration="PT2S" suggestedPresentationDelay="PT2S">
  <ServiceDescription id="0"> ... </ServiceDescription>
  <Period id="p0" start="PT0S">
    <AdaptationSet id="0" contentType="video" mimeType="video/mp4" segmentAlignment="true" startWithSAP="1">
      <Representation id="video" codecs="avc1.64001f" bandwidth="2400000" width="1280" height="720" frameRate="30">
        <SegmentTemplate timescale="90000" initialization="init-$RepresentationID$.m4s"
                         media="chunk-$RepresentationID$-$Time$.m4s"
                         availabilityTimeOffset="1.500" availabilityTimeComplete="false">
          <SegmentTimeline>
            <S t="161232147900000" d="180000"/>
            <S t="161232148080000" d="180000"/>
          </SegmentTimeline>
        </SegmentTemplate>
      </Representation>
    </AdaptationSet>
    <!-- audio AdaptationSet: timescale = sample rate, same layout -->
  </Period>
  <UTCTiming schemeIdUri="urn:mpeg:dash:utc:http-xsdate:2014" value="http://127.0.0.1:8080/dash/time"/>
</MPD>
```

**Init segments.** `init-video.m4s` / `init-audio.m4s`, optional `?g=N` (the MPD adds it for codec
generations after the first). Waits up to 8 s for the init (`503`); `404 init segment not found` for an
unknown generation or an audio init when the stream has no audio.

**Chunks.** `{time}` is the `t` of an `S` element, in the representation's timescale. A complete
segment is returned whole (`max-age=3600`). The segment being produced is returned with chunked
transfer, each CMAF chunk sent as soon as it exists (`no-cache`, `X-Accel-Buffering: no`). A time just
after the newest segment waits (up to 2 nominal segment durations, at least 1 s) for that segment to
start. Anything else: `404 segment not found` (or `segment not available` after the wait).

Viewer tracking: manifest and chunk requests count as activity of a `dash` viewer (init segments and
`/dash/time` do not).

Client example: `dashjs.MediaPlayer().create()` with
`updateSettings({streaming: {delay: {liveDelay: 2}, liveCatchup: {enabled: true}}})` (dash.js 5 reads
the low-latency parameters from the MPD; `streaming.lowLatencyEnabled` no longer exists).

### 6.5 WebRTC (WHEP)

WebRTC egress following the WHEP draft (`draft-ietf-wish-whep`): the client `POST`s an SDP offer and
gets the SDP answer; media flows over **one UDP port** shared by every session (`webrtc.udp_port`,
default 8443; it is not proxied by nginx/Caddy and must be reachable by viewers). The server is ICE-lite
(`webrtc.ice_lite`) and its answer already contains every candidate (host candidates for
`webrtc.public_ip`, else for the IPv4 addresses of the local interfaces, plus `a=end-of-candidates`), so
no trickle ICE is needed and the client may `POST` without waiting for its own candidates.

Media: H.264 passed through as received (decode order; encoders should not use B-frames, e.g. x264
`tune=zerolatency` or OBS `bframes=0`) with SPS/PPS sent before every IDR; AAC transcoded to Opus
(48 kHz stereo, 20 ms, `webrtc.audio_bitrate_kbps`) by ffmpeg. Without ffmpeg the answer rejects the
audio section (video-only). A new viewer starts at the next keyframe (at most one keyframe interval).

| Method & path | Request | Response |
|---|---|---|
| `POST /whep/{name}` | `Content-Type: application/sdp`, body = SDP offer (≤ 64 KiB) with a video section offering H.264 (`packetization-mode=1`); audio optional | `201 Created`, `Content-Type: application/sdp`, body = SDP answer, `Location: /whep/{name}/{session}`, `ETag: "<session>"`, `Link` (ICE servers, below) |
| `DELETE /whep/{name}/{session}` | | `200` (session closed; also for a session the server closed in the last 2 minutes), `404 session not found` |
| `PATCH /whep/{name}/{session}` | | `405` with `Allow: DELETE, OPTIONS` (trickle ICE / ICE restart not supported) |
| `OPTIONS /whep/{name}` | | `204`, `Allow: POST, OPTIONS`, CORS preflight headers, `Link` |
| `OPTIONS /whep/{name}/{session}` | | `204`, `Allow: DELETE, OPTIONS` |

`POST` errors (JSON `{"error": ...}`), in the order they are checked:

| Status | When |
|---|---|
| `403` | `WebRTC is disabled on this server` (`webrtc.enabled = false`) |
| `404` | `broadcast not found` |
| `403` | `WebRTC playback is disabled for this broadcast` (`enable_webrtc = false`) |
| `415` | `expected Content-Type: application/sdp` |
| `400` | `invalid SDP offer: ...` (empty, not UTF-8, not `v=0`, too large, no media section, no ICE credentials or DTLS fingerprint, rejected by the WebRTC stack) |
| `415` | the offer has no video section or no H.264 (`the offer has no H.264 video codec`; also when only `packetization-mode=0` is offered) |
| `404` | `stream offline` |
| `503` | `WebRTC is not available on this server` (UDP port not bound), `too many WebRTC sessions` (`webrtc.max_sessions`, default 100), `stream is starting (no H.264 sequence header yet)` (after waiting up to 5 s) |
| `415` | the stream's video cannot be sent over WebRTC (HEVC, AV1...: only H.264) or the stream is audio-only |

Every response has `Access-Control-Allow-Origin: *` and `Access-Control-Expose-Headers: Location, Link,
ETag`. When `webrtc.stun_server` is set, `POST` and `OPTIONS` responses carry
`Link: <stun:host:port>; rel="ice-server"` (a bare `host:port` gets the `stun:` prefix).

Session lifetime: the server closes a session on `DELETE`, when the peer connection fails or closes,
after 10 s in the `disconnected` state (consent freshness lost: tab closed without `DELETE`, network
gone), when it does not connect within 30 s, when the stream ends, when `enable_webrtc` is set to false
or the broadcast is deleted, and on shutdown. The viewer is counted as `webrtc` while connected.
The per-broadcast WebRTC publisher (packetizer and Opus encoder) starts with the first session and stops
when the stream ends or after `webrtc.idle_stop_secs` (default 30) without sessions.

Minimal client (the same as `snippets.whep`):

```js
const pc = new RTCPeerConnection();
pc.addTransceiver('video', { direction: 'recvonly' });
pc.addTransceiver('audio', { direction: 'recvonly' });
const stream = new MediaStream();
pc.ontrack = (e) => { stream.addTrack(e.track); video.srcObject = stream; };
await pc.setLocalDescription(await pc.createOffer());
const res = await fetch('http://127.0.0.1:8080/whep/evento-2026', {
  method: 'POST', headers: { 'Content-Type': 'application/sdp' }, body: pc.localDescription.sdp });
const session = new URL(res.headers.get('Location'), res.url).href;   // DELETE it when done
await pc.setRemoteDescription({ type: 'answer', sdp: await res.text() });
```

---

## 7. Pages

| Route | Description |
|---|---|
| `GET /` | the dashboard SPA (`text/html`, `Cache-Control: no-cache`, Italian or English UI per `ui_language`, which also sets `<html lang>`; hash routes `#/`, `#/b/{name}`, `#/settings`). Shows the login view when the API answers 401 |
| `GET /embed/{name}` | player page for iframes |
| `GET /player/{name}` | the same player as a full page, with the title above it |
| `GET /static/{file}` | assets embedded in the binary |
| `GET /waiting/{path}` | files of `data/waiting/` (waiting videos and posters) |
| `GET /favicon.ico` | `204 No Content` (the icon is `/static/favicon.svg`) |

### `/embed/{name}` and `/player/{name}`

Both serve the same HTML (`embed.html`, with `data-mode="embed"` or `"player"`; HTML `no-cache`). The
name is only validated as a slug (`[a-z0-9_-]{1,64}`, else a plain-text `404 broadcast not found`); an
unknown but well-formed name still returns the page, which then displays "Diretta non trovata" ("Live
stream not found" in English). The page
loads [`/api/v1/public/broadcasts/{name}`](#5-public-player-api) and its SSE stream (falling back to
polling every 5 s), and plays the waiting video / waiting text while the status is `offline` or `paused`.

Query parameters (all optional):

| Parameter | Values | Default | Effect |
|---|---|---|---|
| `autoplay` | `1` / `0` | `1` | start playing as soon as the stream is live; with `0` the viewer starts it with the controls (or the play button when `controls=0`) |
| `muted` | `1` / `0` | `1` | start muted. With `muted=0` browsers often block autoplay with sound: the player then retries muted and shows an "Attiva audio" button |
| `controls` | `1` / `0` | `1` | show the native video controls |
| `src` | `auto`, `webrtc` (alias `whep`), `flv`, `hls`, `dash` | `auto` | source to try first (any other value is `auto`); see below |
| `latency` | `1` / `0` | `0` on `/embed`, `1` on `/player` | show the estimated latency in the corner |
| `lang` | `it` / `en` (case-insensitive) | the `ui_language` setting | language of the player texts (waiting/paused badge, built-in waiting text, buttons, "not found"); also sets `<html lang>`. Other values are ignored |

A flag is false when present with the value `0`, `false`, `no` or `off` (case-insensitive) and true for
any other value, including an empty one. The dashboard preview uses `?muted=1&autoplay=1&latency=1`.
Without `lang` the player starts with the server's `<html lang>` and then follows `ui_language` from the
public info (re-read every 60 s while waiting). A custom waiting text is shown as-is in either language;
the built-in one is shown in the player's language.

Source selection: `auto` tries `webrtc` -> `flv` -> `hls` -> `dash` (on iOS: `webrtc` -> `hls` ->
`dash`). `webrtc` is a built-in WHEP client (no library; see [6.5](#65-webrtc-whep)); `flv` plays
HTTP-FLV with mpegts.js (needs Media Source Extensions); `hls` uses native HLS on iOS/Safari and hls.js
elsewhere; `dash` uses dash.js. An explicit `src=hls` starts at HLS and may still fall back to `dash`;
`src=dash` only tries DASH. WebRTC fails over to the next source when the WHEP `POST` is refused (403,
404, 415, 503...), ICE does not connect within 6 s, the connection fails or closes, no video frame
arrives within 8 s, or no video packet arrives for 4 s; leaving it (another source, `offline`/`paused`
status, `pagehide`) closes the peer connection and `DELETE`s the session. Only sources enabled for the broadcast (non-null in `sources`) and supported
by the browser are used. When a source fails fatally, makes no progress for 12 s, or does not start
within 15 s, the player checks the status and tries the next source; when all fail it returns to the
waiting screen and retries after 5 s. The libraries are loaded on demand from jsDelivr: mpegts.js 1.8.2,
hls.js 1.7.3 (`lowLatencyMode`, `liveSyncDuration: 1.5`, `liveMaxLatencyDuration: 4`), dash.js 5.2.1.
`window.rmsPlayer` exposes `status`, `source`, `engine` (`webrtc`, `mpegts.js`, `hls.js`, `native`,
`dash.js`), `latency` (for WebRTC: jitter-buffer delay + half the RTT from `getStats()`),
`peerConnection` (WebRTC only), `setSource(src)` and `reload()` for debugging.

### `/waiting/{path}`

Static file server over `data/waiting/` (with `Range` support, MIME type from the extension): waiting
videos `<name>/<uuid>.mp4`, the global default `_default/<uuid>.mp4`, and posters `<same stem>.jpg`.
No directory listing; unknown files are an empty-bodied `404`. The URLs are in `waiting_video_url`,
`poster_url`, `default_waiting_video_url`, `default_poster_url`.

### `/static/{file}`

Only the top-level files below exist (no sub-paths); unknown names return a plain-text `404`. Responses
carry `Cache-Control: no-cache` and a content-hash `ETag`; `If-None-Match` is honoured (`304`).

`index.html`, `app.js`, `core.js`, `detail.js`, `metrics.js`, `mock.js`, `style.css` (dashboard);
`embed.html`, `player.js` (ES module), `player.css` (player); `i18n.js` (Italian/English texts and
formatting, shared by the dashboard and the player); `favicon.svg`.

---

## 8. curl cookbook

```bash
B=http://127.0.0.1:8080
TOKEN=$(cat ./data/admin_token)            # or the value of RMS_ADMIN_TOKEN / server.admin_token
H="Authorization: Bearer $TOKEN"
J='Content-Type: application/json'

# --- login with a cookie instead of the header
curl -s -c jar.txt -H "$J" -d "{\"token\":\"$TOKEN\"}" $B/api/v1/auth/login -o /dev/null -w '%{http_code}\n'   # 204
curl -s -b jar.txt $B/api/v1/auth/check -o /dev/null -w '%{http_code}\n'                                         # 204
curl -s -b jar.txt -X POST $B/api/v1/auth/logout -o /dev/null -w '%{http_code}\n'                               # 204

# --- broadcasts
curl -s -H "$H" -H "$J" -d '{"title":"Evento 2026","record":true}' $B/api/v1/broadcasts      # 201, name "evento-2026"
curl -s -H "$H" $B/api/v1/broadcasts | jq '.[] | {name, status, viewers: .viewers.total}'
KEY=$(curl -s -H "$H" $B/api/v1/broadcasts/evento-2026 | jq -r .stream_key)
curl -s -H "$H" -H "$J" -X PATCH -d '{"enable_dash":false,"waiting_text":"Torniamo tra poco"}' $B/api/v1/broadcasts/evento-2026
curl -s -H "$H" -X POST $B/api/v1/broadcasts/evento-2026/pause
curl -s -H "$H" -X POST $B/api/v1/broadcasts/evento-2026/resume
curl -s -H "$H" -X POST $B/api/v1/broadcasts/evento-2026/regenerate-key
curl -s -H "$H" -X POST $B/api/v1/broadcasts/evento-2026/disconnect -w '%{http_code}\n'      # 204, or 409 when offline
curl -s -H "$H" $B/api/v1/broadcasts/evento-2026/embed | jq -r .iframe
curl -s -H "$H" -X DELETE $B/api/v1/broadcasts/evento-2026 -w '%{http_code}\n'               # 204

# --- publish a test stream (see scripts/test-publish.sh)
scripts/test-publish.sh "$KEY" 127.0.0.1:1935

# --- recording
curl -s -H "$H" -X POST $B/api/v1/broadcasts/evento-2026/record/start | jq '{id, status, path}'
curl -s -H "$H" -X POST $B/api/v1/broadcasts/evento-2026/record/stop  | jq '{id, status, format}'  # processing -> ready
curl -s -H "$H" "$B/api/v1/recordings?broadcast=evento-2026" | jq '.[0]'
REC=$(curl -s -H "$H" $B/api/v1/broadcasts/evento-2026/recordings | jq -r '.[0].id')
curl -s -H "$H" -OJ "$B/api/v1/recordings/$REC/download?download=1"                           # save with the server's file name
curl -s -H "$H" -H 'Range: bytes=0-1023' "$B/api/v1/recordings/$REC/download" -o /dev/null -w '%{http_code}\n'  # 206
curl -s -H "$H" -X DELETE $B/api/v1/recordings/$REC -w '%{http_code}\n'

# --- metrics
curl -s -H "$H" "$B/api/v1/broadcasts/evento-2026/metrics?step=60" | jq '.points | length'
curl -s -H "$H" "$B/api/v1/broadcasts/evento-2026/metrics?from=2026-10-08T00:00:00Z&to=2026-10-08T12:00:00Z" | jq '{step, n: (.points|length)}'
curl -s -H "$H" "$B/api/v1/broadcasts/evento-2026/metrics/summary?from=2026-10-01T00:00:00Z" | jq
curl -s -H "$H" $B/api/v1/broadcasts/evento-2026/sessions | jq
curl -s -H "$H" $B/api/v1/metrics/overview | jq

# --- waiting video and settings
curl -s -H "$H" -F file=@attesa.mp4 $B/api/v1/broadcasts/evento-2026/waiting-video | jq .waiting_video_url
curl -s -H "$H" -X DELETE $B/api/v1/broadcasts/evento-2026/waiting-video | jq .waiting_video_url
curl -s -H "$H" -F file=@attesa.mp4 $B/api/v1/settings/waiting-video | jq .default_waiting_video
curl -s -H "$H" -H "$J" -X PATCH -d '{"default_waiting_text":"Siamo quasi pronti"}' $B/api/v1/settings
curl -s -H "$H" -H "$J" -X PATCH -d '{"ui_language":"en"}' $B/api/v1/settings | jq .ui_language   # "en"
curl -s $B/api/v1/public/ui                                     # {"language":"en"}, no token
curl -s -H "$H" $B/api/v1/system | jq

# --- events (SSE); -N disables curl's output buffering
curl -N -H "$H" $B/api/v1/events
curl -N $B/api/v1/public/broadcasts/evento-2026/events         # public status stream, no token

# --- public player API
curl -s $B/api/v1/public/broadcasts/evento-2026 | jq '{status, sources}'

# --- playback
ffplay "$B/live/evento-2026.flv"                                # HTTP-FLV
curl -s -i -X OPTIONS $B/whep/evento-2026                       # 204 + CORS (+ Link with a STUN server)
curl -s -i -H 'Content-Type: application/sdp' --data-binary @offer.sdp $B/whep/evento-2026   # 201 + answer
curl -s -X DELETE $B/whep/evento-2026/<session> -w '%{http_code}\n'                          # 200
curl -s "$B/live/evento-2026.flv" | ffprobe -hide_banner -         # inspect the live FLV
ffplay "$B/hls/evento-2026/master.m3u8"                         # LL-HLS
curl -s "$B/hls/evento-2026/master.m3u8"
curl -s "$B/hls/evento-2026/stream.m3u8"
# blocking reload: wait for part 2 of segment N (take N from #EXT-X-PRELOAD-HINT)
curl -s "$B/hls/evento-2026/stream.m3u8?_HLS_msn=1791468304&_HLS_part=2"
curl -s "$B/hls/evento-2026/init.mp4" -o init.mp4
curl -s "$B/dash/evento-2026/manifest.mpd"                      # LL-DASH
curl -s "$B/dash/time"                                          # 2026-10-08T14:00:00.000Z
ffplay "$B/dash/evento-2026/manifest.mpd"

# --- pages
curl -s -o /dev/null -w '%{http_code}\n' "$B/embed/evento-2026?autoplay=0&muted=1&controls=1&src=hls&latency=1"
curl -s "$B/embed/evento-2026?lang=en" | grep -o '<html lang="[a-z]*">'   # <html lang="en">
```
