# rms HTTP API reference

Reference for every HTTP surface of rust-media-server (`rms`): the admin REST API, the Server-Sent
Events streams, the public player API, the playback endpoints (HTTP-FLV, WS-FLV, LL-HLS, LL-DASH,
WebRTC via WHEP), the HTML pages and the users' accounts, API keys and storage API (§9). It is derived
from the code (`src/api/*.rs`, `src/storage/*.rs`, `src/egress/*.rs`, `src/packager/*.rs`,
`src/webrtc/*.rs`, `src/web/mod.rs`, `src/main.rs`); where it and [ARCHITECTURE.md](ARCHITECTURE.md) §8
differ, this file describes the behaviour of the code (see also ARCHITECTURE.md §13, §14, §16, §17, §18,
§19 and §22, and [ACCOUNTS.md](ACCOUNTS.md)).

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
   [Waiting video and settings](#44-waiting-video-and-settings) - [System](#45-system) - [Events (SSE)](#46-events-sse) -
   [Video on demand](#47-video-on-demand) - [Cluster mode](#48-cluster-mode)
5. [Public player API](#5-public-player-api)
6. [Playback endpoints](#6-playback-endpoints)
7. [Pages](#7-pages)
8. [curl cookbook](#8-curl-cookbook)
9. [Accounts, API keys and the storage API](#9-accounts-api-keys-and-the-storage-api): [Principals](#91-principals-and-api-keys) -
   [Storage API](#92-storage-api) - [Users (admin)](#93-users-admin) - [Account](#94-account) -
   [Signed playback URLs](#95-signed-playback-urls) - [Data types](#96-data-types) - [curl](#97-curl-examples)

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

In cluster mode (`cluster.enabled = true`) only step 1 applies: the token must be configured and
identical on every node (startup fails otherwise; the per-node file is neither read nor generated).

The admin token is the superuser (no expiry). It is compared in constant time. Users (phase 8A)
authenticate with **API keys** (`rms_` + 40 hex characters, [§9.1](#91-principals-and-api-keys)): they
can use the account and storage APIs, never the admin API.

### Bearer header, AccessKey header or cookie

Every authenticated request carries one credential, read in this order:

1. `Authorization: Bearer <admin token | API key>` (the scheme is matched as `Bearer ` or `bearer `);
2. `AccessKey: <API key>` (bunny.net-style header);
3. the cookie `rms_token=<admin token>` set by `POST /api/v1/auth/login`, else the cookie
   `rms_key=<API key>` set by a login with a key.

If a `Bearer` header is present it is the only credential considered: a wrong bearer token is a 401
even when a valid cookie is also sent. The cookie *is* the token or key (it is not a separate session
id). Browsers cannot set headers on `EventSource`, so the SSE streams are used with the cookie. Admin
endpoints need the admin token: a valid user key gets `403 this endpoint needs the admin token`.

### `POST /api/v1/auth/login`

Public. Sets the login cookie.

| | |
|---|---|
| Body | `{"token": "<admin token or API key>"}` (surrounding whitespace is trimmed) |
| Success (admin token) | `200 {"role": "admin"}` with `Set-Cookie: rms_token=<token>; Path=/; HttpOnly; SameSite=Lax; Max-Age=2592000` (30 days) and `rms_key=; …; Max-Age=0`; `; Secure` is added when the request is HTTPS (`server.public_url` starts with `https://`, or `X-Forwarded-Proto: https`) |
| Success (API key) | `200 {"role": "user", "user": {"id", "username", "display_name"}, "scopes": [...], "key_id"}` with `Set-Cookie: rms_key=<key>; …` (until the key expires, at most 30 days) and `rms_token=; …; Max-Age=0`; the user's `last_login_at` is updated |
| `401` | wrong token, unknown/revoked/expired key (answered after a 400 ms delay to slow down brute force; logged as a warning) |
| `403` | a valid key of a suspended user |
| `400` | the token is correct but contains characters that cannot go in a cookie (non-printable, `;`, `,`): `token contains characters unsuitable for a cookie; use the Authorization header`; or the body is not valid JSON / lacks `token` |

Until phase 8A the success status was `204 No Content` (no body); clients that only test `res.ok`
are unaffected.

### `POST /api/v1/auth/logout`

Public, no body. Always `204 No Content` with `Set-Cookie: rms_token=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0`
and the same for `rms_key`.

### `GET /api/v1/auth/check`

Public in the sense that it never needs a valid token to be called: `200` with the same body as the
login (`{"role": "admin"}` or `{"role": "user", "user", "scopes", "key_id"}`) when the request carries a
valid credential (header or cookie), `401` otherwise, `403` for a suspended user. The dashboard uses it
to decide between the login view, the admin panel and the user portal.

### Which routes need the token

| Route | Auth |
|---|---|
| `/api/v1/auth/login`, `/api/v1/auth/logout`, `/api/v1/auth/check` | none (see above) |
| `/api/v1/public/**` | none |
| `/live/{name}.flv`, `/ws/live/{name}.flv`, `/hls/**`, `/dash/**`, `/whep/**` | none |
| `/`, `/embed/{name}`, `/player/{name}`, `/static/*`, `/waiting/*`, `/favicon.ico` | none |
| `/healthz` | none ([4.5](#45-system)) |
| `/internal/**` (cluster mode, separate listener `cluster.internal_bind`) | `Authorization: Bearer <cluster.internal_secret>` |
| `/api/v1/account/**` | a user key with the `account` scope, or the admin token with `?user=<id or username>` ([9.4](#94-account)) |
| `/storage/**` | an API key (permissions per path) or the admin token ([9.2](#92-storage-api)) |
| `/vod/{id}/**` of a **private** video | a signed token (`?token=&expires=` or the `rms_play_<id>` cookie, [9.5](#95-signed-playback-urls)) |
| everything else under `/api/v1` (broadcasts, recordings incl. downloads, metrics, settings, system, events, vod, users) | admin token, else `401` (`403` for a user key) |

### CORS

`Access-Control-Allow-Origin: *` (permissive CORS, including preflight) is sent on the public player API,
all playback endpoints (FLV, WS-FLV, HLS, DASH, WHEP, VOD) and the pages (`/`, `/embed`, `/player`, `/static`,
`/waiting`), and on the storage API `/storage/**` (keys travel in headers, never in cookies across
sites). WHEP responses also carry `Access-Control-Expose-Headers: Location, Link, ETag` (see
[6.5](#65-webrtc-whep)). The admin API, the account API and the auth endpoints send no CORS headers.

---

## 2. Errors

Every error produced by the API handlers is JSON:

```json
{"error": "broadcast not found"}
```

with `Cache-Control: no-store` (and `WWW-Authenticate: Bearer` on the 401 of a missing/wrong
credential). Validation errors of the account, user and storage APIs also name the offending field:

```json
{"error": "the username must be 2-32 characters among a-z, 0-9, - and _", "field": "username"}
```

Internal errors (500) are logged in detail and returned as the generic `{"error":"internal server error"}`.

| Status | Meaning in this API |
|---|---|
| `400` | invalid input: malformed JSON or wrong `Content-Type`, invalid field/query value, malformed multipart, `name cannot be changed`, blocking-reload parameters out of range |
| `401` | missing or wrong admin token / API key (unknown, revoked, expired); a private video without a valid signed token |
| `403` | playback of a protocol is disabled for that broadcast (`enable_flv` / `enable_hls` / `enable_dash` / `enable_webrtc` = false), or WebRTC is disabled on the server; a user key on the admin API, a key without the `account` scope on the account API, a storage path outside the key's permissions, any key of a suspended user |
| `404` | unknown or malformed broadcast name, unknown recording id or WHEP session, stream offline, unknown path (`{"error":"not found"}` from the global fallback), protocol disabled on the server |
| `405` | `PATCH` of a WHEP session (trickle ICE is not supported; JSON body and `Allow` header) |
| `409` | conflict: duplicate broadcast name, `broadcast is not live`, `broadcast is already being recorded`, `broadcast is not being recorded`, deleting a recording that is still recording/processing |
| `413` | upload larger than `server.max_upload_mb`; storage API file larger than `storage.max_object_mb` |
| `415` | unsupported media: waiting-video upload that is not an accepted video; HLS/DASH requested for a stream whose codecs cannot be packaged; WHEP offer without `Content-Type: application/sdp` or without H.264, or a stream whose video is not H.264 |
| `422` | waiting-video upload that ffmpeg cannot decode/convert |
| `500` | internal error |
| `503` | not ready yet: playlist/MPD/part not available in time, the waiting-video conversion timed out; WHEP: WebRTC unavailable, too many sessions, stream starting |
| `507` | storage API upload over the user's `quota_bytes` (files + encodings) |

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

Present in `BroadcastView.live` while a publisher is connected (in cluster mode: to any node; for a
stream published on another node the fields come from that node's `/internal/status`, cached 3 s, or
only `started_at` + `node_id` when it does not answer).

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
| `node_id` | string | cluster mode only (absent on a single node): the node the publisher is connected to |

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
| `ui_theme` | `"system"` \| `"light"` \| `"dark"` | default colour theme of the dashboard and of the player (setting key `ui_theme`, default `"system"` = follow the browser's `prefers-color-scheme`). Each dashboard browser can override it with the top-bar toggle (stored in that browser's `localStorage`, key `rms.theme`); a player page with `?theme=` ([7](#7-pages)) |

```json
{
  "default_waiting_video": "_default/3c2d1e0f9a8b4c7d8e6f5a4b3c2d1e0f.mp4",
  "default_waiting_video_url": "http://127.0.0.1:8080/waiting/_default/3c2d1e0f9a8b4c7d8e6f5a4b3c2d1e0f.mp4",
  "default_poster_url": "http://127.0.0.1:8080/waiting/_default/3c2d1e0f9a8b4c7d8e6f5a4b3c2d1e0f.jpg",
  "default_waiting_text": "La diretta inizierà a breve",
  "max_upload_mb": 1024,
  "ffmpeg_available": true,
  "ui_language": "it",
  "ui_theme": "system"
}
```

### VodView

A video on demand item (`GET /api/v1/vod`, SSE `vod`). `id` is a UUID (lowercase, hyphenated; the API
also accepts it in upper case).

| Field | Type | Notes |
|---|---|---|
| `id` | string | UUID |
| `title`, `description` | string | title ≤ 200 characters (default: the file name without extension, `_` → space), description ≤ 5000 |
| `source_path` | string | original file relative to `data/vod/` (`<id>/source.<ext>`) |
| `source_name` | string | original file name (download name) |
| `source_size` | integer | bytes |
| `source_duration_secs` | number \| null | from ffprobe |
| `source_width`, `source_height` | integer \| null | *display* size (sample aspect ratio applied, rotation metadata honoured) |
| `source_codec` | string \| null | e.g. `h264`, `hevc`, `prores` |
| `origin` | `"upload"` \| `"inbox"` | dashboard/API upload or inbox folder |
| `status` | `"queued"` \| `"processing"` \| `"ready"` \| `"failed"` | |
| `progress` | integer | 0-99 while `processing`, 100 when `ready` |
| `ladder` | string | ladder used (or to be used) for the transcode |
| `renditions` | array | renditions actually produced (empty until `ready`): `{name, width, height, video_kbps, audio_kbps, bandwidth}`; `video_kbps` 0 = constant quality (CRF 23); `bandwidth` = measured peak bit rate (video + audio, bit/s), the HLS `BANDWIDTH` |
| `error` | string \| null | ffmpeg's error (tail of stderr) when `failed` |
| `created_at`, `updated_at`, `ready_at` | timestamp | `ready_at` is null until ready |
| `plays` | integer | playback sessions started (see [6.6](#66-video-on-demand)) |
| `urls` | object | `hls` (`/vod/{id}/hls/master.m3u8`), `dash` (`/vod/{id}/dash/manifest.mpd`), `poster` (`/vod/{id}/poster.jpg`), `embed` (`/embed/vod/{id}`), `watch` (`/watch/{id}`), absolute |
| `poster_url` | string \| null | `urls.poster` when the poster exists |
| `source_available` | boolean | the original is still stored (download, re-transcode); false after a transcode with `vod.keep_source = false` |
| `viewers_now` | integer | current HLS + DASH sessions of this video |
| `user_id`, `bucket_id`, `object_id` | string \| null | owner, bucket and bucket object of an encoding of a bucket file ([§9](#9-accounts-api-keys-and-the-storage-api)); `null` for the admin library |
| `object_path` | string \| null | path of the original in its bucket (`/videos/clip.mp4`; then `source_path` is the same and `origin` is `"storage"`) |
| `visibility` | `"public"` \| `"private"` | private videos need a signed URL ([9.5](#95-signed-playback-urls)); only encodings of bucket files can be private |
| `output_bytes` | integer | size of the encoding output (renditions + poster), counted in the owner's quota |

```json
{
  "id": "4fb48570-aaa4-45ed-8bd6-46f2e62f794d",
  "title": "Test 1080p",
  "description": "Mira di prova 30 s",
  "source_path": "4fb48570-aaa4-45ed-8bd6-46f2e62f794d/source.mp4",
  "source_name": "src1080.mp4",
  "source_size": 20729831,
  "source_duration_secs": 30.0,
  "source_width": 1920,
  "source_height": 1080,
  "source_codec": "h264",
  "origin": "upload",
  "status": "ready",
  "progress": 100,
  "ladder": "standard",
  "renditions": [
    {"name": "1080p", "width": 1920, "height": 1080, "video_kbps": 5000, "audio_kbps": 160, "bandwidth": 5252266},
    {"name": "720p", "width": 1280, "height": 720, "video_kbps": 2800, "audio_kbps": 128, "bandwidth": 3009751},
    {"name": "480p", "width": 854, "height": 480, "video_kbps": 1400, "audio_kbps": 96, "bandwidth": 1565627},
    {"name": "360p", "width": 640, "height": 360, "video_kbps": 800, "audio_kbps": 96, "bandwidth": 939985}
  ],
  "error": null,
  "created_at": "2026-10-09T16:59:54.023Z",
  "updated_at": "2026-10-09T17:01:39.219Z",
  "ready_at": "2026-10-09T17:01:39.219Z",
  "plays": 4,
  "urls": {
    "hls": "http://127.0.0.1:8080/vod/4fb48570-aaa4-45ed-8bd6-46f2e62f794d/hls/master.m3u8",
    "dash": "http://127.0.0.1:8080/vod/4fb48570-aaa4-45ed-8bd6-46f2e62f794d/dash/manifest.mpd",
    "poster": "http://127.0.0.1:8080/vod/4fb48570-aaa4-45ed-8bd6-46f2e62f794d/poster.jpg",
    "embed": "http://127.0.0.1:8080/embed/vod/4fb48570-aaa4-45ed-8bd6-46f2e62f794d",
    "watch": "http://127.0.0.1:8080/watch/4fb48570-aaa4-45ed-8bd6-46f2e62f794d"
  },
  "poster_url": "http://127.0.0.1:8080/vod/4fb48570-aaa4-45ed-8bd6-46f2e62f794d/poster.jpg",
  "source_available": true,
  "viewers_now": 0,
  "user_id": null,
  "bucket_id": null,
  "object_id": null,
  "visibility": "public",
  "output_bytes": 38012345,
  "object_path": null
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
honours the proxy headers only when `server.trust_proxy = true`: `X-Real-IP` first, else the **last**
`X-Forwarded-For` element (the one added by the trusted proxy; the earlier ones come from the client),
else the socket address. `user_agent` is cut at 512 characters.

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
the samples of its bucket (in cluster mode: the maximum per node, then the sum over the nodes). The
series is *dense*: buckets without samples (stream offline) are
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
  In cluster mode the samples of the nodes are added up per sampling slot (`metrics.sample_interval_secs`)
  before taking the maximum (peak of the sum, not sum of the peaks).
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
| `ui_theme` | string | `"system"`, `"light"` or `"dark"` (exact, lowercase); omitted = unchanged |

- Response `200`: [Settings](#settings). `400` if the text is too long, `ui_language` is anything else
  (`ui_language must be "it" or "en"`) or `ui_theme` is anything else
  (`ui_theme must be "system", "light" or "dark"`); nothing is written on a `400`.
- A request that sets any field sends a `settings` event on the admin SSE stream
  ([4.6](#46-events-sse)), so open dashboards switch language and default theme without a reload
  (a browser whose toggle overrides the theme keeps its own choice).

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
  "hub": {"live_streams": 1, "relays": 0},
  "webrtc": {
    "enabled": true,
    "available": true,
    "udp_port": 8443,
    "public_port": 8443,
    "public_ip": [],
    "candidates": ["192.168.1.20:8443"],
    "ice_lite": true,
    "sessions": 2,
    "publishers": 1,
    "audio": true,
    "error": null
  },
  "vod": {
    "enabled": true,
    "available": true,
    "inbox_dir": "./data/vod/inbox",
    "jobs_running": 1,
    "queued": 2,
    "max_parallel_jobs": 1,
    "encoder": "libx264"
  },
  "cluster": {
    "enabled": true,
    "node_id": "rms1",
    "backend": "postgres",
    "nodes": [
      {"node_id": "rms1", "advertise_url": "http://rms1:8081", "whep_url": "https://stream.example.com/whep",
       "version": "0.1.0", "started_at": "2026-10-09T18:00:01.204Z", "heartbeat": "2026-10-09T18:12:31.007Z",
       "live_streams": 1, "viewers": 12, "draining": false, "alive": true},
      {"node_id": "rms2", "advertise_url": "http://rms2:8081", "whep_url": null,
       "version": "0.1.0", "started_at": "2026-10-09T18:00:03.950Z", "heartbeat": "2026-10-09T18:12:30.411Z",
       "live_streams": 0, "viewers": 3, "draining": false, "alive": true}
    ],
    "live_streams": [
      {"name": "evento-2026", "node_id": "rms1", "session_id": "5d0c…", "started_at": "2026-10-09T18:05:12.391Z",
       "heartbeat": "2026-10-09T18:12:31.012Z"}
    ],
    "relays": []
  }
}
```

`hub.live_streams` counts the streams published on this node, `hub.relays` the streams of other nodes
relayed here (cluster mode).

`ffmpeg.path` / `ffmpeg.version` are `null` when ffmpeg is unavailable (`version` is the first line of
`ffmpeg -version`). `public_url` is `server.public_url` as configured (empty = derived from `Host`).

`webrtc`: `enabled` is `webrtc.enabled`; `available` is false when disabled or when the UDP port could
not be bound (`error` says why); `udp_port` is the bound port and `public_port` the port advertised to
viewers (`webrtc.public_port`, or `udp_port` when that is 0: set it when a NAT maps another public port
to the node, e.g. several nodes behind one IP); `candidates` are the ICE host candidates given to viewers
(`webrtc.public_ip`, else the IPv4 addresses of the local interfaces, with `public_port`); `sessions` counts
WHEP sessions (connecting or connected); `publishers` the broadcasts with a running WebRTC publisher;
`audio` is false without ffmpeg (sessions are then video-only).

`vod`: `enabled` is `vod.enabled`; `available` is false when disabled or when ffmpeg/ffprobe are missing
(no uploads, no inbox, no transcoding; existing videos stay playable); `inbox_dir` the watched folder;
`jobs_running` the transcodes running in this process; `queued` the items waiting (database).

`cluster` (preview, [CLUSTER.md](CLUSTER.md)): `enabled` is `cluster.enabled`; `node_id` this node
(`cluster.node_id`, the host name by default); `backend` `"sqlite"` (single node) or `"postgres"`
(cluster mode); `nodes` the rows of the node registry, sorted by `node_id` (always `[]` on a single
node). Each node refreshes `heartbeat` (database clock), `live_streams`, `viewers` (live broadcasts,
all protocols) and `draining` every `cluster.heartbeat_secs`; `alive` is false once the heartbeat is
older than `cluster.node_ttl_secs`, and every node deletes such rows shortly after. `whep_url` is
`cluster.whep_url` (else `<server.public_url>/whep`) when WebRTC is available, else `null`.
`live_streams` are the stream owners (`live_streams` rows with a fresh heartbeat: `session_id` is the
claim of the current publish); `relays` the relays running on this node: `{name, origin_node,
started_at, state: "connecting"|"ready"|"failed"|"stopped", attempts, bytes_in, viewers}` (see
[4.8](#48-cluster-mode)). Both are `[]` on a single node.

#### `GET /healthz`

Public, no token, no CORS. Liveness probe for load balancers and container health checks
(`Cache-Control: no-store`; `HEAD` works too).

```json
{"node_id": "rms1", "cluster": true, "db": true, "live_streams": 1, "version": "0.1.0", "draining": false}
```

| Status | When |
|---|---|
| `200` | serving, **also when the database is unreachable** (`"db": false`): the data plane keeps working and a database outage must not take every node out of the pool |
| `503` | draining or shutting down (`"draining": true`): stop sending new requests here |

`db` is the result of a `SELECT 1` bounded to 2 s; `live_streams` counts the streams published on
this node (relays excluded). A node drains on `SIGTERM` and on `POST /internal/drain`
([4.8](#48-cluster-mode)).

### 4.6 Events (SSE)

#### `GET /api/v1/events`

Admin Server-Sent Events stream (`Content-Type: text/event-stream`, `Cache-Control: no-cache`,
`X-Accel-Buffering: no`). Authenticate with the cookie (browser `EventSource`) or a Bearer header
(`curl -N`). A `heartbeat` is sent immediately, then every 15 s. The stream ends when the server shuts down.

| Event | Data (JSON) | When |
|---|---|---|
| `heartbeat` | `{"ts": "2026-10-08T14:00:00.000Z"}` | on connect and every 15 s |
| `broadcast` | a full [BroadcastView](#broadcastview) | any change of a broadcast: create, patch, pause/resume, regenerate-key, waiting video, recording start/stop/finalize, stream start/end, stream metadata (codec info) update |
| `stream` | `{"name": "evento-2026", "status": "live"}` | stream start/end; `status` is the new `offline`/`live`/`paused`; sent right before the matching `broadcast` event. Cluster mode: also when a stream of another node starts, ends or changes owner, and when a local relay starts/stops (`status` is the effective one: live anywhere in the cluster) |
| `viewers` | `{"name": "evento-2026", "counts": ViewerCounts}` | the viewer counts of a broadcast changed (cluster mode: counts of every node, this node's live counts plus the other nodes' latest samples; a change on another node shows up within ~2 s plus one sampling interval) |
| `broadcast_deleted` | `{"name": "evento-2026"}` | a broadcast was deleted |
| `settings` | `{"ui_language": "en", "ui_theme": "system", "default_waiting_text": "The live stream will start shortly"}` | after a `PATCH /api/v1/settings` that set `ui_language`, `ui_theme` and/or `default_waiting_text` (effective values) |
| `vod` | a full [VodView](#vodview) | a video on demand item was created (upload, inbox), changed status, progressed (≥ 1 %, at most every 0.5 s), got its poster, was edited or re-queued, or counted a play |
| `vod_deleted` | `{"id": "4fb48570-…"}` | a video on demand item was deleted |

```
event: viewers
data: {"name":"evento-2026","counts":{"flv":3,"ws_flv":0,"hls":1,"dash":0,"webrtc":0,"total":4}}

event: stream
data: {"name":"evento-2026","status":"live"}
```

Events are not replayed: after a reconnect, re-read `GET /api/v1/broadcasts` (and `GET /api/v1/vod`).
In cluster mode every node's stream shows changes made on any node (broadcasts, settings, videos,
streams): they travel between nodes as PostgreSQL notifications ([4.8](#48-cluster-mode)).
The viewer counts of a video on demand also arrive as `viewers` events with `name` = `vod:<id>`.

### 4.7 Video on demand

Uploaded (or inbox) videos are transcoded by ffmpeg in **one pass** into every rendition of a *ladder*
(`[vod]` in the configuration, ARCHITECTURE.md §19) and packaged as CMAF (fMP4) segments served both as
HLS and as DASH. Needs ffmpeg and ffprobe; with `vod.enabled = false` or without them, uploads and
re-transcodes answer `503` (listing, editing, deleting and playback still work).

#### `GET /api/v1/vod`

- Query: `status` = `queued` | `processing` | `ready` | `failed` (optional; anything else → `400`).
- Response `200`: `[VodView]`, newest first.

#### `POST /api/v1/vod`

Multipart form (streamed to `data/upload-tmp/`, never buffered in memory):

| Field | Required | Notes |
|---|---|---|
| `file` | yes | the video. Accepted extensions: `mp4`, `mov`, `m4v`, `mkv`, `webm`, `avi`, `mts`, `m2ts`, `ts`, `mxf`, `flv`, `mpg`, `mpeg`, `wmv` (else the MIME type: `video/mp4`, `video/quicktime`, `video/x-matroska`, `video/webm`, `video/x-msvideo`, `video/mp2t`, `application/mxf`, `video/x-flv`, `video/mpeg`, `video/x-ms-wmv`, ...). Only the first `file` field is used |
| `title` | no | default: the file name without extension, `_` → space |
| `description` | no | ≤ 5000 characters |
| `ladder` | no | a configured ladder (default `vod.default_ladder`); sent *before* `file`, an unknown ladder is rejected before the upload is received |

The file is probed with ffprobe, moved to `data/vod/<id>/source.<ext>` and queued.

- Response `201`: the [VodView](#vodview) (`status: "queued"`).
- Errors: `400` missing `file`, empty file, unknown ladder, title/description too long or a text field
  over 64 KiB; `413` larger than `server.max_upload_mb` MiB; `415` unsupported type; `422` ffprobe cannot
  read the file or it has no video stream (`could not process the video: ...`); `503` VOD disabled or
  ffmpeg/ffprobe missing.

#### `GET /api/v1/vod/ladders`

The ladders of the configuration (read-only) and the VOD limits:

```json
{
  "enabled": true,
  "available": true,
  "default_ladder": "standard",
  "ladders": [
    {"name": "standard", "description": "1080p → 360p, per desktop e mobile", "renditions": [
      {"name": "1080p", "height": 1080, "video_kbps": 5000, "audio_kbps": 160},
      {"name": "720p", "height": 720, "video_kbps": 2800, "audio_kbps": 128},
      {"name": "480p", "height": 480, "video_kbps": 1400, "audio_kbps": 96},
      {"name": "360p", "height": 360, "video_kbps": 800, "audio_kbps": 96}]},
    {"name": "mobile", "description": "720p e 360p, leggero per le reti mobili", "renditions": ["..."]},
    {"name": "single", "description": "Una sola qualità, alla risoluzione originale (CRF 23)", "renditions": [
      {"name": "source", "height": 0, "video_kbps": 0, "audio_kbps": 128}]}
  ],
  "inbox_dir": "./data/vod/inbox",
  "segment_duration": 4.0,
  "encoder": "libx264",
  "keep_source": true,
  "max_upload_mb": 1024,
  "extensions": ["mp4", "mov", "m4v", "mkv", "webm", "avi", "mts", "m2ts", "ts", "mxf", "flv", "mpg", "mpeg", "wmv"]
}
```

`height` is the short side of the output (the height of a landscape video, the width of a portrait one);
`0` = the source size; `video_kbps` `0` = constant quality (CRF 23).

#### `GET /api/v1/vod/{id}`

- Response `200`: [VodView](#vodview); `404 video not found` for an unknown or malformed id.

#### `PATCH /api/v1/vod/{id}`

- Body: `{"title"?: string, "description"?: string}` (trimmed; empty title → `400`).
- Response `200`: the updated [VodView](#vodview).

#### `DELETE /api/v1/vod/{id}`

- Query: `force=1` (also `true`, `yes`).
- Response `204`: row, source, poster and renditions removed (`data/vod/<id>/`). `409` while the video
  is `processing` unless `force` is given, which cancels the job (ffmpeg is killed) first.

#### `POST /api/v1/vod/{id}/retranscode`

- Body (optional, JSON): `{"ladder"?: string}` (default: the item's ladder).
- Removes the current output (renditions and poster) and re-queues the item. Also the way to retry a
  `failed` item.
- Response `200`: the [VodView](#vodview) (`status: "queued"`). `400` unknown ladder; `409` already
  `processing`, or the original was deleted (`vod.keep_source = false`); `503` VOD unavailable.

#### `GET /api/v1/vod/{id}/embed`

- Response `200`: `{iframe, urls, snippets: {hlsjs, dashjs}}`: the `<iframe>` of `/embed/vod/{id}`,
  the [VodView](#vodview) `urls`, and ready-to-paste hls.js (`startLevel: -1`,
  `capLevelToPlayerSize: true`, native HLS fallback) and dash.js examples.

#### `GET /api/v1/vod/{id}/source`

- The original file (`Content-Disposition: attachment`, `Range` supported). `404` when it was removed.

### 4.8 Cluster mode

Several rms nodes share one PostgreSQL database ([CLUSTER.md](CLUSTER.md)); every public route works on
every node. What changes for clients:

- **`X-Rms-Node: <node_id>`** on every HTTP response of a node in cluster mode (absent on a single
  node): which node answered behind the load balancer.
- **Status and views** are cluster-wide: a broadcast is `live` when any node publishes it (the
  `live_streams` table, refreshed every 2 s and by notifications); `BroadcastView.live` describes the
  owner's stream ([LiveInfo](#liveinfo)); `viewers`, `/metrics/overview`, `metrics/summary.current`
  add up the nodes (this node's live counts + the other nodes' latest samples); `GET
  /broadcasts/{name}/sessions` lists this node's sessions only.
- **Playback anywhere**: FLV, WS-FLV, HLS, DASH and WHEP requests for a stream published on another
  node start a relay from the owner (one per stream and node; the first request waits up to 6 s for
  a keyframe). HLS/DASH timelines are per node: keep one viewer on one node (hash of `X-Real-IP`, see
  `deploy/haproxy.cfg`).
- **Publishing**: any node accepts any key. When another node publishes the key, its publisher is
  disconnected (internal kick) and the new one takes over within ~0.5 s; a draining node rejects
  publishes. `POST /broadcasts/{name}/disconnect` reaches the owner node (`503` when it cannot be
  reached). Recordings are made by the owner node only: `record/start` on another node answers `409
  the stream is published on node <id>`.
- **Downloads**: `GET /recordings/{id}/download` serves the file from the shared volume; when the file
  is missing locally it is fetched (Range forwarded) from the node that wrote it.
- **WHEP**: session ids are `<node_id>-<uuid>`; `DELETE /whep/{name}/{session}` of another node's
  session is forwarded to it; a node without usable WebRTC answers `307` ([6.5](#65-webrtc-whep)).

#### Internal routes

Served only on the internal listener (`cluster.internal_bind`, default `0.0.0.0:8081`; never publish it
through the proxy), every route requires `Authorization: Bearer <cluster.internal_secret>` (`401`
otherwise; cookies are ignored). Used by the nodes themselves; listed here for operators.

| Method & path | Response |
|---|---|
| `GET /internal/ping` | `{"node_id", "version", "draining"}` |
| `GET /internal/relay/{name}.flv` | the FLV byte stream of a stream **published on this node** (header, metadata, sequence headers, GOP, then live tags; no viewer session); `404` otherwise (a relay is never relayed again) |
| `POST /internal/kick/{name}` | `204` the local publisher is disconnected (another node takes the stream over); `404` none |
| `GET /internal/status/{name}` | `200` `{name, node_id, started_at, publisher_ip, video_codec, audio_codec, width, height, fps, bytes_in, recording, packager}` of a stream published here; `404` otherwise |
| `POST /internal/drain` | `{"node_id", "draining": true}`: `/healthz` answers `503` (the load balancer stops sending new requests), no new publishes or relays on this node; current publishers, viewers and relays keep running |
| `DELETE /internal/drain` | `{"node_id", "draining": false}`: back in the pool |
| `GET /internal/files/{kind}/{path}` | a file of `recordings`, `waiting` or `vod` (relative `path`, plain components only), `Range` supported; `404` unknown kind or file |
| `POST /internal/whep/delete/{session}?name={name}` | `200` the WHEP session of this node is closed; `404` unknown |

```bash
curl -s -H "Authorization: Bearer $RMS_INTERNAL_SECRET" http://rms2:8081/internal/ping
curl -s -X POST -H "Authorization: Bearer $RMS_INTERNAL_SECRET" http://rms2:8081/internal/drain   # before maintenance
```

---

## 5. Public player API

No authentication, permissive CORS. These endpoints never expose the stream key; they are what the
embed player and the dashboard login page use.

#### `GET /api/v1/public/ui`

UI settings needed before login (the dashboard reads the language and the default theme here before
showing the login view).

- Response `200`: `{"language": "it", "theme": "system"}` (`language`: `"it"` or `"en"`, the
  `ui_language` setting; `theme`: `"system"`, `"light"` or `"dark"`, the `ui_theme` setting).

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
  "ui_language": "it",
  "ui_theme": "system"
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
| `ui_theme` | `"system"` \| `"light"` \| `"dark"` | the default theme setting: the player's colours follow it unless the page has `?theme=` |

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

#### `GET /api/v1/public/vod`

The ready **public** videos on demand (`visibility = public`, admin library and users' public encodings),
most recently published first (used by the `/vod` catalogue page):

```json
[{"id": "4fb48570-aaa4-45ed-8bd6-46f2e62f794d", "title": "Test 1080p", "description": "Mira di prova 30 s",
  "duration_secs": 30.0, "poster_url": "http://127.0.0.1:8080/vod/4fb48570-…/poster.jpg",
  "watch_url": "http://127.0.0.1:8080/watch/4fb48570-…", "ready_at": "2026-10-09T17:01:39.219Z"}]
```

#### `GET /api/v1/public/vod/{id}`

Player info of one video, in **any** status (the player shows "processing" until it is ready); `404
video not found` for unknown ids.

```json
{
  "id": "4fb48570-aaa4-45ed-8bd6-46f2e62f794d",
  "title": "Test 1080p",
  "description": "Mira di prova 30 s",
  "status": "ready",
  "progress": 100,
  "duration_secs": 30.0,
  "poster_url": "http://127.0.0.1:8080/vod/4fb48570-…/poster.jpg",
  "sources": {
    "hls": "http://127.0.0.1:8080/vod/4fb48570-…/hls/master.m3u8",
    "dash": "http://127.0.0.1:8080/vod/4fb48570-…/dash/manifest.mpd"
  },
  "renditions": [{"name": "1080p", "width": 1920, "height": 1080, "bandwidth": 5252266}, "..."],
  "ui_language": "it",
  "ui_theme": "system"
}
```

`sources.*` are `null` until `status` is `ready`; `error` details are never exposed.

A **private** video answers `401 {"error": "this video is private: a valid signed link (token, expires) is
required"}` unless the request carries a valid `?token=<t>&expires=<unix>` ([9.5](#95-signed-playback-urls));
with one, `sources.hls`, `sources.dash` and `poster_url` carry the same `token`/`expires` query.

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
| `POST /whep/{name}` | `Content-Type: application/sdp`, body = SDP offer (≤ 64 KiB) with a video section offering H.264 (`packetization-mode=1`); audio optional | `201 Created`, `Content-Type: application/sdp`, body = SDP answer, `Location: /whep/{name}/{session}`, `ETag: "<session>"`, `Link` (ICE servers, below). Cluster mode: `307 Temporary Redirect` (below) |
| `DELETE /whep/{name}/{session}` | | `200` (session closed; also for a session the server closed in the last 2 minutes), `404 session not found`; cluster mode: a session of another node is closed there (`502` when that node cannot be reached) |
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

**Cluster mode.** Session ids are `<node_id>-<uuid>` (a plain 32-hex-digit UUID on a single node), so
any node can route the `DELETE`. A node whose WebRTC is unusable (`webrtc.enabled = false` or UDP
port not bound) answers `POST` with **`307 Temporary Redirect`**, `Location: <whep_url>/{name}` of the
least loaded alive node whose `whep_url` differs from its own (`cluster.whep_url` per node, e.g.
`https://rms2.example.com/whep`; nodes sharing one hostname never redirect to each other), when
`cluster.whep_redirect = true` (default); the public player info then offers `sources.whep` too. The
browser re-sends the `POST` with its body to the new URL (Fetch, `307`); cross-origin targets get a
CORS preflight, which rms answers (verified with Chrome 150, page on one node, session on the other).
Resolve the `Location` of the `201` against `res.url` (the final URL), not the first endpoint.

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

### 6.6 Video on demand

| Route | Content |
|---|---|
| `GET /vod/{id}/hls/master.m3u8` | HLS master playlist (`application/vnd.apple.mpegurl`, `Cache-Control: public, max-age=60`) |
| `GET /vod/{id}/hls/media_<n>.m3u8` | HLS media playlists (same headers) |
| `GET /vod/{id}/dash/manifest.mpd` | static DASH MPD (`application/dash+xml`, `public, max-age=60`) |
| `GET /vod/{id}/{hls,dash}/init-<n>.m4s` | CMAF init segment of representation `n` (`video/mp4`) |
| `GET /vod/{id}/{hls,dash}/chunk-<n>-<k>.m4s` | media segment `k` (`video/iso.segment`) |
| `GET /vod/{id}/poster.jpg` | poster (frame at 10 % of the duration, ≤ 1280 px wide; `public, max-age=60`) |

All public, CORS `*`, `Range` supported (HEAD too). Both prefixes serve the same files
(`data/vod/<id>/abr/`), so the relative URLs inside the manifests resolve under the prefix of the
manifest. Segments and init segments are referenced with a `?v=<revision>` query (new at every
transcode) and served with `Cache-Control: public, max-age=31536000, immutable`. The master and the MPD
answer `404 video not ready` / `404 video not found` unless the item is `ready`; other unknown files
are `404`.

- **HLS**: version 7, `#EXT-X-INDEPENDENT-SEGMENTS`, one `#EXT-X-MEDIA` audio group per audio bitrate
  (`aud160`, `aud128`, ...), one `#EXT-X-STREAM-INF` per rendition with `BANDWIDTH` (measured peak segment
  bit rate, video + its audio), `AVERAGE-BANDWIDTH`, `RESOLUTION`, `FRAME-RATE`, `CODECS`
  (`avc1.640029,mp4a.40.2`: High profile from 720p, Main below; level 4.1 or higher when needed) and
  `AUDIO`; the largest rendition up to 720p is listed first (native players start with it). Media
  playlists: `#EXT-X-PLAYLIST-TYPE:VOD`, `#EXT-X-MAP`, `EXTINF` = `vod.segment_duration` (default 4 s;
  every segment starts with a keyframe), `#EXT-X-ENDLIST`.
- **DASH**: `type="static"`, one video adaptation set (all renditions) and one audio adaptation set
  (one representation per distinct audio bitrate), `SegmentTemplate` with `$Number%05d$`.

Every request touches the viewer tracker under the name `vod:<id>` (protocol `hls` / `dash`; sessions
expire after `metrics.session_timeout_secs`): that is `viewers_now`. A master/MPD request that opens a
new session increments `plays`. VOD sessions are not written to the live metrics tables.

**Private videos** ([9.5](#95-signed-playback-urls)): every route above needs a valid signed token, as
the query `?token=<t>&expires=<unix>` or the cookie `rms_play_<id>=<t>:<expires>` (`HttpOnly`,
`SameSite=Lax`, `Path=/vod/<id>/`, `Max-Age` until the expiry) that the first request accepted with
the query sets; otherwise `401` (JSON, CORS). Their master/media playlists and MPD are rewritten so that
every URI carries `&token=…&expires=…` (native HLS players and `ffprobe` follow the signed master
without help) and every response is private: playlists/MPD `Cache-Control: private, no-store`, segments
`private, max-age=31536000, immutable`, poster `private, max-age=60`. Public videos ignore tokens.

```text
#EXTM3U
#EXT-X-VERSION:7
#EXT-X-INDEPENDENT-SEGMENTS
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="aud160",NAME="AAC 160 kbit/s",DEFAULT=YES,AUTOSELECT=YES,CHANNELS="2",URI="media_4.m3u8?v=5f4db239"
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="aud128",NAME="AAC 128 kbit/s",DEFAULT=YES,AUTOSELECT=YES,CHANNELS="2",URI="media_5.m3u8?v=5f4db239"
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="aud96",NAME="AAC 96 kbit/s",DEFAULT=YES,AUTOSELECT=YES,CHANNELS="2",URI="media_6.m3u8?v=5f4db239"
#EXT-X-STREAM-INF:BANDWIDTH=3009751,AVERAGE-BANDWIDTH=2928487,RESOLUTION=1280x720,FRAME-RATE=30.000,CODECS="avc1.640029,mp4a.40.2",AUDIO="aud128"
media_1.m3u8?v=5f4db239
#EXT-X-STREAM-INF:BANDWIDTH=5252266,AVERAGE-BANDWIDTH=5162686,RESOLUTION=1920x1080,FRAME-RATE=30.000,CODECS="avc1.640029,mp4a.40.2",AUDIO="aud160"
media_0.m3u8?v=5f4db239
#EXT-X-STREAM-INF:BANDWIDTH=1565627,AVERAGE-BANDWIDTH=1497718,RESOLUTION=854x480,FRAME-RATE=30.000,CODECS="avc1.4d4029,mp4a.40.2",AUDIO="aud96"
media_2.m3u8?v=5f4db239
#EXT-X-STREAM-INF:BANDWIDTH=939985,AVERAGE-BANDWIDTH=896886,RESOLUTION=640x360,FRAME-RATE=30.000,CODECS="avc1.4d4029,mp4a.40.2",AUDIO="aud96"
media_3.m3u8?v=5f4db239
```

---

## 7. Pages

| Route | Description |
|---|---|
| `GET /` | the dashboard SPA (`text/html`, `Cache-Control: no-cache`, Italian or English UI per `ui_language`, which also sets `<html lang>`; `<html data-theme>` from `ui_theme`, overridden before the first paint by this browser's toggle choice (`localStorage` `rms.theme`); hash routes `#/`, `#/b/{name}`, `#/vod`, `#/settings`). Shows the login view when the API answers 401 |
| `GET /embed/{name}` | player page for iframes |
| `GET /player/{name}` | the same player as a full page, with the title above it |
| `GET /embed/vod/{id}` | video on demand player for iframes |
| `GET /watch/{id}` | the same VOD player as a full page, with title, description and a link to `/vod` |
| `GET /vod` | public catalogue of the ready videos on demand (links to `/watch/{id}`; `?lang=`, `?theme=` as the player pages) |
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
| `theme` | `system` / `light` / `dark` (case-insensitive) | the `ui_theme` setting | colours of the player page (waiting screen, title, badges and buttons over the video; the video area stays black). `system` follows the viewer's `prefers-color-scheme`, live. Sets `<html data-theme>` (server-side, and by an inline script before the first paint). Other values are ignored |

A flag is false when present with the value `0`, `false`, `no` or `off` (case-insensitive) and true for
any other value, including an empty one. The dashboard preview uses `?muted=1&autoplay=1&latency=1`.
Without `lang` the player starts with the server's `<html lang>` and then follows `ui_language` from the
public info (re-read every 60 s while waiting). A custom waiting text is shown as-is in either language;
the built-in one is shown in the player's language. Likewise, without `theme` the player starts with the
server's `<html data-theme>` and then follows `ui_theme` from the public info: the theme resolves as
`?theme=` -> `ui_theme` -> `system`. The dashboard preview adds `&theme=` with the dashboard's own theme
and keeps it in sync (same-origin `postMessage` `{type: "rms-theme", theme}`, no reload).

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
`peerConnection` (WebRTC only), `theme` (resolved: `light` / `dark`), `setSource(src)` and `reload()`
for debugging.

### `/embed/vod/{id}` and `/watch/{id}`

Both serve `vod.html` (`data-kind="vod"`, `data-mode="embed"` or `"watch"`); `player.js` then runs its
VOD mode (`vod-player.js`). A malformed id is a plain-text `404`; an unknown well-formed id gets the page,
which shows "Video non trovato" ("Video not found"). The page reads
[`/api/v1/public/vod/{id}`](#get-apiv1publicvodid): while the video is `queued`/`processing` it shows a
"processing" card with the progress (polled every 5 s) and starts by itself when it is ready.

| Parameter | Values | Default | Effect |
|---|---|---|---|
| `autoplay` | `1` / `0` | `0` | start playing at once (otherwise the poster is shown until the viewer presses play) |
| `muted` | `1` / `0` | = `autoplay` | start muted; with `autoplay=1&muted=0` a blocked autoplay is retried muted with an "Attiva audio" button |
| `controls` | `1` / `0` | `1` | native video controls (without them a play button is shown) |
| `t` | seconds | `0` | start position |
| `src` | `auto`, `hls`, `dash` | `auto` | `auto`/`hls`: native HLS on iOS/Safari, else hls.js, falling back to dash.js; `dash`: dash.js only |
| `lang` | `it` / `en` | `ui_language` | as the live player |
| `theme` | `system` / `light` / `dark` | `ui_theme` | as the live player |
| `token`, `expires` | signed token | none | private videos ([9.5](#95-signed-playback-urls)): passed to the info request and appended by the player to the master/MPD, poster, playlist and segment requests (hls.js `xhrSetup` re-opens each XHR with the query, dash.js gets a request interceptor; native HLS relies on the rewritten playlists). Without a valid token the page shows "Video privato" / "Private video" |

hls.js 1.7.3 runs with `startLevel: -1` and `capLevelToPlayerSize: true` (adaptive bitrate); dash.js
5.2.1 with its default ABR. A quality button over the video lists *Auto* and every rendition
(`hls.levels` / dash.js video representations, labelled by the short side, e.g. `720p`, with the bit
rate); choosing one pins it (`hls.currentLevel` / `setRepresentationForTypeById` with automatic switching
off), *Auto* restores ABR. Native HLS has no quality API: the button is hidden. `window.rmsPlayer`
exposes `kind: "vod"`, `status`, `source`, `engine` (`hls.js`, `native`, `dash.js`), `hls`, `dash`
(the library instances), `levels`, `currentLevel`, `autoLevel`, `setLevel(i)` (`-1` = Auto), `info`,
`language`, `theme`, `options`, `signed` (the page has `token`/`expires`), `reload()`; `status` is
`private` when the info answered 401.

### `/waiting/{path}`

Static file server over `data/waiting/` (with `Range` support, MIME type from the extension): waiting
videos `<name>/<uuid>.mp4`, the global default `_default/<uuid>.mp4`, and posters `<same stem>.jpg`.
No directory listing; unknown files are an empty-bodied `404`. The URLs are in `waiting_video_url`,
`poster_url`, `default_waiting_video_url`, `default_poster_url`.

### `/static/{file}`

Only the top-level files below exist (no sub-paths); unknown names return a plain-text `404`. Responses
carry `Cache-Control: no-cache` and a content-hash `ETag`; `If-None-Match` is honoured (`304`).

`index.html`, `app.js`, `core.js`, `detail.js`, `metrics.js`, `mock.js`, `style.css` (dashboard);
`embed.html`, `player.js` (ES module), `player.css` (player); `vod.js` (dashboard VOD section),
`vod.html`, `vod-player.js` (VOD player), `catalog.html`, `catalog.js` (public catalogue); `i18n.js` (Italian/English texts and
formatting, shared by the dashboard and the player); `theme.js` (dark/light theme resolution, shared);
`favicon.svg`.

---

## 8. curl cookbook

```bash
B=http://127.0.0.1:8080
TOKEN=$(cat ./data/admin_token)            # or the value of RMS_ADMIN_TOKEN / server.admin_token
H="Authorization: Bearer $TOKEN"
J='Content-Type: application/json'

# --- login with a cookie instead of the header
curl -s -c jar.txt -H "$J" -d "{\"token\":\"$TOKEN\"}" $B/api/v1/auth/login                                     # {"role":"admin"}
curl -s -b jar.txt $B/api/v1/auth/check                                                                          # {"role":"admin"}
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
curl -s $B/api/v1/public/ui                                     # {"language":"en","theme":"system"}, no token
curl -s -H "$H" -H "$J" -X PATCH -d '{"ui_theme":"light"}' $B/api/v1/settings | jq .ui_theme   # "light"
curl -s -o /dev/null -w '%{http_code}\n' -H "$H" -H "$J" -X PATCH -d '{"ui_theme":"blue"}' $B/api/v1/settings   # 400
curl -s -H "$H" $B/api/v1/system | jq
curl -s -H "$H" $B/api/v1/system | jq .cluster                 # {"enabled":false,"node_id":"…","backend":"sqlite","nodes":[]}
curl -s $B/healthz                                              # liveness, no token: 200 (503 while draining)
curl -s -H "Authorization: Bearer $RMS_CLUSTER_INTERNAL_SECRET" http://rms1:8081/internal/ping   # cluster: internal listener

# --- video on demand
curl -s -H "$H" $B/api/v1/vod/ladders | jq '.ladders[].name'
curl -s -H "$H" -F file=@evento.mp4 -F ladder=standard -F "title=Evento 2026" $B/api/v1/vod | jq '{id, status}'   # 201
VOD=$(curl -s -H "$H" "$B/api/v1/vod?status=queued" | jq -r '.[0].id')
curl -s -H "$H" $B/api/v1/vod/$VOD | jq '{status, progress, renditions}'
curl -s -H "$H" -H "$J" -X PATCH -d '{"description":"Replica"}' $B/api/v1/vod/$VOD | jq .description
curl -s -H "$H" -H "$J" -X POST -d '{"ladder":"mobile"}' $B/api/v1/vod/$VOD/retranscode | jq .status   # "queued"
curl -s -H "$H" $B/api/v1/vod/$VOD/embed | jq -r .iframe
curl -s -H "$H" -OJ $B/api/v1/vod/$VOD/source                # original file
cp evento.mp4 ./data/vod/inbox/mobile/                       # inbox: imported with the "mobile" ladder
curl -s $B/api/v1/public/vod | jq '.[].title'                 # ready videos, no token
curl -s "$B/vod/$VOD/hls/master.m3u8"
ffprobe "$B/vod/$VOD/dash/manifest.mpd"
curl -s -H "$H" -X DELETE "$B/api/v1/vod/$VOD?force=1" -w '%{http_code}\n'   # 204

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
curl -s "$B/embed/evento-2026?lang=en" | grep -o '<html lang="[a-z]*"'   # <html lang="en"
curl -s "$B/embed/evento-2026?theme=light" | grep -o 'data-theme="[a-z]*"'   # data-theme="light"
```

---

## 9. Accounts, API keys and the storage API

Phase 8A ([ACCOUNTS.md](ACCOUNTS.md), ARCHITECTURE.md §22), modelled on bunny.net Storage Zones + Stream:
every **user** owns **buckets** (directory trees of files kept as originals) and **API keys** with
path-scoped permissions; files are uploaded with a plain HTTP storage API and video files are **encoded**
by the VOD pipeline ([4.7](#47-video-on-demand)) into multi-bitrate HLS/DASH while the originals stay in the
bucket; private encodings are played with **signed URLs**. The admin token stays the superuser: it manages
users ([9.3](#93-users-admin)) and may act as any user on the account API (`?user=`).

Files live under `<storage.root>/<bucket id>/<path>` (default `data/buckets`; never under the bucket
name, so renames are free). Configuration: `storage.max_object_mb` (default 20480) and `storage.root`.

### 9.1 Principals and API keys

A key is `rms_` + 40 lowercase hex characters (160 random bits). It is returned **once**, by the request
that creates it; the server stores its SHA-256 and the first 12 characters (`key_prefix`, e.g.
`rms_96202e94`) for lists. Send it as `Authorization: Bearer <key>`, `AccessKey: <key>` or (dashboard)
the `rms_key` cookie set by [`POST /api/v1/auth/login`](#post-apiv1authlogin).

| Scope | Grants |
|---|---|
| `account` | the account API ([9.4](#94-account)): keys, buckets, encodings, signing; **`admin` access to every bucket of the user** on the storage API |
| `storage` | the storage API ([9.2](#92-storage-api)) within the key's `permissions` |

`permissions` is a list of `{"bucket": "<bucket id> | *", "prefix": "/videos/", "access": "read | write |
admin"}` (`*` = every bucket of the user; `prefix` defaults to `/` = the whole bucket; on input a bucket
**name** is accepted and stored as its id; at most 100 entries). Access levels are ordered
`read < write < admin`:

| Level | Allows |
|---|---|
| `read` | list directories, download files, `HEAD`, `?info=1`, `?action=sign` |
| `write` | + upload/overwrite files, create directories, delete files, `?action=encode` |
| `admin` | + delete directories recursively |

An entry applies when its bucket is `*` or the bucket's id and the **access path** starts with its
prefix (plain string prefix): the access path is the file path (`/videos/a.mp4`) or, for a directory,
the path with its trailing slash (`/videos/`; the root is `/`). The effective level is the highest of the
matching entries. So `/videos/` covers `GET /storage/b/videos/` and everything below, but not the root
listing nor `/videos-old.txt`; `/vid` would cover both `/videos/…` and `/vid.mp4`.

Failures: no credential, unknown, **revoked** or **expired** key → `401`; any key of a **suspended**
user → `403 the user is suspended`; a user key on the admin API → `403`; a key without the `account`
scope on the account API → `403`; a path outside the permissions → `403 access denied: this key needs
write access to /other/x.txt`. `last_used_at` is updated at most once a minute.

### 9.2 Storage API

`/storage/{bucket}/{path}` with a key or the admin token; CORS permissive (keys travel in headers);
`Cache-Control: no-store` on JSON responses, `private, max-age=0` on file bodies.

| Method & path | Needs | Response |
|---|---|---|
| `GET /storage/{b}/` · `GET /storage/{b}/{dir}/` | read on the directory | `200 [ObjectView]`: directories first, then by name; a missing directory lists `[]`. `GET /storage/{b}/{dir}` (no slash) of an existing directory lists it too |
| `GET /storage/{b}/{file}` | read | the original: `Range` (206), `ETag: "<sha256>"` + `If-None-Match` (304), `Last-Modified`, `Content-Type` of the object; `?download=1` adds `Content-Disposition: attachment` (RFC 5987 UTF-8 name) |
| `GET /storage/{b}/{file}?info=1[&expires_secs=3600]` | read | `200 ObjectInfo`: the object, its storage URLs and, when the encoding is private, signed playback URLs |
| `HEAD /storage/{b}/{file}` | read | the headers of `GET` without the body |
| `PUT /storage/{b}/{path}/{file}` | write | the raw body is the file. `201 ObjectView` (with `encoding` when queued) |
| `PUT /storage/{b}/{dir}/` | write | creates the directory and its parents (idempotent): `201 ObjectView` |
| `DELETE /storage/{b}/{file}` | write | `204`: the file, its index row and its encoding (job cancelled, renditions removed) |
| `DELETE /storage/{b}/{dir}/` | **admin** | `204`: the directory, everything below it and their encodings. The root cannot be deleted (`400`: delete the bucket) |
| `POST /storage/{b}/{file}?action=encode` body `{"ladder"?, "visibility"?}` (optional) | write | (re)queues the encoding: `202 ObjectView` with `encoding` (`status: "queued"`); a running job is cancelled, the old output removed |
| `POST /storage/{b}/{file}?action=sign` body `{"expires_secs"?}` (optional, default 3600, max 1 year) | read | `200 SignedUrls`; `409` when the file has no encoding |

**Upload** (`PUT` of a file): the body is streamed to `data/upload-tmp/` (never held in memory) while its
SHA-256 is computed, then renamed into place (parent directories created; overwrite allowed, the object
keeps its id). Headers:

| Header | Effect |
|---|---|
| `Content-Length` | when larger than `storage.max_object_mb` MiB → `413` before the body is read; over the quota → `507` (without it the limits apply while streaming) |
| `Checksum: <sha256 hex>` | verified after the upload: mismatch → `400 {"error": "checksum mismatch: the SHA-256 of the body is …", "field": "Checksum"}`, nothing stored; not 64 hex characters → `400` |
| `X-Rms-Encode: <ladder>` | encode with that ladder (unknown → `400`, field `X-Rms-Encode`; `503` when `vod.enabled = false`) |
| `X-Rms-Encode: none` | no encoding; an existing encoding of the overwritten file is deleted |
| *(no `X-Rms-Encode`)* | encode when the bucket has `auto_encode` and the name has a video extension (`mp4`, `mov`, `m4v`, `mkv`, `webm`, `avi`, `mts`, `m2ts`, `ts`, `mxf`, `flv`, `mpg`, `mpeg`, `wmv`); an already encoded file is always re-encoded when overwritten |
| `X-Rms-Visibility: public \| private` | visibility of the encoding (default: the existing encoding's, else the bucket's `public_playback`) |

The ladder of a new encoding is the header's, else the bucket's `default_ladder`, else the user's, else
`vod.default_ladder`. The encoding references the original **in place** (`source_path` = object path, no
copy; `vod.keep_source` does not apply), its title is the file name. `Content-Type` of the stored object
comes from the file name (`mime_guess`), `application/octet-stream` when unknown.

**Quota**: `users.quota_bytes` (0 = unlimited) limits originals + encoding outputs (`usage.bytes_used`);
an upload is refused with `507 storage quota exceeded` when `bytes_used − size of the overwritten file +
body size > quota_bytes`. Encoding outputs are counted when they are produced (an encoding may take a
user over the quota; the next upload is then refused).

**Paths**: each segment is percent-decoded (strictly: `%ZZ` → `400`; `+` stays `+`), must be UTF-8 and is
normalized to NFC (`e%CC%81.mp4` and `%C3%A9.mp4` are the same file). Rejected with `400 {"field":
"path"}`: empty segments (`a//b`), `.`/`..` (also encoded), control characters, `\` and an encoded `/`,
names starting with `.rms` (reserved), segments over 255 bytes, paths over 1024 bytes. A trailing `/`
names a directory. `PUT` of a file below a path that is a file, or of a file where a directory exists (and
vice versa) → `409`.

Errors: `404 bucket not found` (unknown or malformed name), `404 file not found`, `403` (permissions),
`409` (file/directory conflicts, `?action=sign` without an encoding), `413`, `507`.

### 9.3 Users (admin)

Admin token only. `{id}` is a user id or username.

| Route | Body | Response |
|---|---|---|
| `GET /api/v1/users` | | `200 [UserView]` by username |
| `POST /api/v1/users` | `{username, display_name?, email?, status?, quota_bytes?, max_buckets?, default_ladder?}` | `201 UserView`; `400 {field}` (username slug `[a-z0-9_-]{2,32}`, email, status `active`/`suspended`, max_buckets ≤ 10000, ladder), `409` username taken |
| `GET /api/v1/users/{id}` | | `200 UserView` |
| `PATCH /api/v1/users/{id}` | any field of `POST` (`email`/`default_ladder`: `null` clears) | `200 UserView` |
| `DELETE /api/v1/users/{id}[?force=1]` | | `204`; `409` when the user owns buckets unless `force`, which deletes buckets, files, encodings and keys |
| `GET /api/v1/users/{id}/keys` | | `200 [KeyView]` newest first (revoked ones included) |
| `POST /api/v1/users/{id}/keys` | as [`POST /account/keys`](#94-account) | `201 CreatedKey` |
| `PATCH /api/v1/users/{id}/keys/{kid}` | `{name?, scopes?, permissions?, expires_at?}` | `200 KeyView` |
| `DELETE /api/v1/users/{id}/keys/{kid}` | | `204` (revoked) |
| `GET /api/v1/users/{id}/buckets` | | `200 [BucketView]` |
| `POST /api/v1/users/{id}/buckets` | as [`POST /account/buckets`](#94-account) | `201 BucketView` |
| `GET /api/v1/users/{id}/usage` | | `200 {user_id, quota_bytes, max_buckets, usage: Usage, buckets: [{id, name, bytes_used, objects_count}]}` |
| `GET /api/v1/users/{id}/encodings` | | `200 [VodView]` |
| `POST /api/v1/users/{id}/recount` | | `200 {report: {buckets, files_checked, sizes_fixed, missing_files, encodings_measured}, usage: Usage}`: re-measures the files (fixing the indexed sizes) and the encoding outputs on disk, then recomputes the counters |

The admin may also call every `/api/v1/account/...` route with `?user=<id or username>`; without it the
account routes answer `400 {"field": "user"}`.

### 9.4 Account

A key with the `account` scope (or the admin with `?user=`). `{id}` of a bucket is its id or name.

| Route | Body | Response |
|---|---|---|
| `GET /api/v1/account` | | `200 AccountView` |
| `GET /api/v1/account/keys` | | `200 [KeyView]` |
| `POST /api/v1/account/keys` | `{name, scopes, permissions?, expires_at?}` (`scopes` ⊆ `account`, `storage`; a storage-only key needs ≥ 1 permission; `expires_at` RFC 3339 in the future) | `201 CreatedKey` (the `key` is shown only here) |
| `PATCH /api/v1/account/keys/{kid}` | `{name?, scopes?, permissions?, expires_at?}` (`expires_at: null` = never) | `200 KeyView`; `409` revoked |
| `DELETE /api/v1/account/keys/{kid}` | | `204` revoked (a key may revoke itself) |
| `GET /api/v1/account/buckets` | | `200 [BucketView]` |
| `POST /api/v1/account/buckets` | `{name, description?, default_ladder?, auto_encode?, public_playback?}` (`name`: global slug `[a-z0-9_-]{2,63}`) | `201 BucketView`; `409` name taken or `max_buckets` reached |
| `GET /api/v1/account/buckets/{id}` | | `200 BucketView` |
| `PATCH /api/v1/account/buckets/{id}` | `{name?, description?, default_ladder?, auto_encode?, public_playback?}` | `200 BucketView` (a rename changes the storage URLs only) |
| `DELETE /api/v1/account/buckets/{id}[?force=1]` | | `204`; `409` when not empty unless `force` (files and encodings deleted) |
| `POST /api/v1/account/buckets/{id}/rotate-signing-key` | | `200 BucketView` with the new `signing_key`: every signed URL of the bucket stops working |
| `POST /api/v1/account/buckets/{id}/sign` | `{path, expires_secs?}` | `200 SignedUrls`; `404` file, `409` no encoding |
| `GET /api/v1/account/encodings` | | `200 [VodView]` of the user, newest first |
| `GET /api/v1/account/encodings/{vod_id}` | | `200 VodView` |
| `PATCH /api/v1/account/encodings/{vod_id}` | `{title?, description?, visibility?}` | `200 VodView` (visibility changes need no re-encode) |
| `GET /api/v1/account/events` | | SSE: `vod` (a [VodView](#vodview)) when one of the user's encodings is created, progresses or changes, `vod_deleted` `{id}`, `heartbeat` `{ts}` on connect and every 15 s |

### 9.5 Signed playback URLs

Encodings with `visibility = private` are played only with a token signed by the bucket's `signing_key`:

```
token = base64url_nopad( HMAC-SHA256( key = signing_key (its 64 hex characters, as text),
                                      message = "<vod_id>|<expires>" ) )
```

`expires` is a Unix time in seconds. The URLs carry `?token=<token>&expires=<expires>`; `?action=sign`,
`?info=1` and `POST /account/buckets/{id}/sign` build them for you. A client can sign itself:

```bash
EXP=$(( $(date +%s) + 3600 ))
TOKEN=$(printf '%s|%s' "$VOD_ID" "$EXP" | openssl dgst -sha256 -hmac "$SIGNING_KEY" -binary | base64 | tr '+/' '-_' | tr -d '=')
echo "$B/embed/vod/$VOD_ID?token=$TOKEN&expires=$EXP"
```

Accepted on every `/vod/{id}/*` request (master, playlists, MPD, init and media segments, poster) and on
`GET /api/v1/public/vod/{id}`; the first playback request accepted with the query sets the cookie
`rms_play_<id>` (`Path=/vod/<id>/`) so later same-origin requests work without it; playlists and MPD are
rewritten with the token ([6.6](#66-video-on-demand)); the players of `/embed/vod/{id}` and `/watch/{id}`
forward `token`/`expires` of the page to every request ([7](#embedvodid-and-watchid)). Wrong, tampered or
expired tokens → `401`. Rotating the signing key invalidates every token at once on the node that rotated
it, within 5 s on the other nodes of a cluster. Public encodings ignore tokens. The admin library cannot be
private (`PATCH /vod/{id} {"visibility":"private"}` → `400`): signing keys belong to buckets.

### 9.6 Data types

**UserView**: the user + `usage`.

| Field | Type | Notes |
|---|---|---|
| `id`, `username`, `display_name` | string | `display_name` defaults to the username |
| `email` | string \| null | |
| `status` | `"active"` \| `"suspended"` | |
| `quota_bytes` | integer | 0 = unlimited (files + encodings) |
| `max_buckets` | integer | default 10 |
| `default_ladder` | string \| null | |
| `created_at`, `updated_at`, `last_login_at` | timestamp | `last_login_at`: last dashboard login with a key |
| `usage` | Usage | `{bytes_used, originals_bytes, encodings_bytes, buckets, objects, encodings, keys}` (`objects` = files; `keys` = not revoked) |

**KeyView** (`CreatedKey` = the same + `key`): `{id, user_id, name, key_prefix, scopes, permissions,
expires_at, last_used_at, revoked_at, created_at, state}` with `state` = `active` | `expired` | `revoked`.

**BucketView**: `{id, user_id, name, description, default_ladder, auto_encode, public_playback,
signing_key, bytes_used, objects_count, created_at, updated_at, storage_url}` (`bytes_used` = files +
encoding outputs, `objects_count` = files; `storage_url` = `<base>/storage/<name>/`).

**ObjectView**: `{id, name, path, is_dir, size, content_type, sha256, created_at, updated_at, encoding?}`;
`encoding` (only when the file has one) = `{vod_id, status, progress, ladder, visibility, renditions,
error, duration_secs, output_bytes, hls_url, dash_url, poster_url, embed_url, watch_url, created_at,
ready_at}` (unsigned URLs; `poster_url` null until the poster exists).

**ObjectInfo** (`?info=1`): ObjectView + `bucket: {id, name}`, `url`, `download_url`, `signed`
(SignedUrls when the encoding is private, else null).

**SignedUrls**: `{hls, dash, poster, embed, watch, token, expires, expires_at}`.

**AccountView**: `{user, usage: Usage + quota_bytes, limits: {quota_bytes, max_buckets, max_object_mb,
ladders, default_ladder, vod_available}, key: {id, scopes} | null}` (`key` = the key of the request).

```json
{
  "id": "ae6da904-d3e1-4b97-a4d3-e4a93d140cbf", "name": "clip.mp4", "path": "/videos/clip.mp4",
  "is_dir": false, "size": 4358120, "content_type": "video/mp4",
  "sha256": "07e6e65232db95c8618cdb8cbc46ae317d13c9e5e2107500b12d81eb787a5adc",
  "created_at": "2026-10-09T21:07:42.315Z", "updated_at": "2026-10-09T21:07:42.315Z",
  "encoding": {
    "vod_id": "4b298743-1303-4fca-b518-ba82d17863de", "status": "ready", "progress": 100,
    "ladder": "standard", "visibility": "private",
    "renditions": [{"name": "720p", "width": 1280, "height": 720, "video_kbps": 2800, "audio_kbps": 128, "bandwidth": 2989925}, "..."],
    "error": null, "duration_secs": 12.0, "output_bytes": 7860009,
    "hls_url": "http://127.0.0.1:8080/vod/4b298743-…/hls/master.m3u8",
    "dash_url": "http://127.0.0.1:8080/vod/4b298743-…/dash/manifest.mpd",
    "poster_url": "http://127.0.0.1:8080/vod/4b298743-…/poster.jpg",
    "embed_url": "http://127.0.0.1:8080/embed/vod/4b298743-…",
    "watch_url": "http://127.0.0.1:8080/watch/4b298743-…",
    "created_at": "2026-10-09T21:07:42.316Z", "ready_at": "2026-10-09T21:07:45.172Z"
  }
}
```

```json
{"hls": "http://127.0.0.1:8080/vod/4b298743-…/hls/master.m3u8?token=d9paAvKBadcpU4RYvZCfUyZAidWy0r-ogn3Mb_qoPYY&expires=1791583687",
 "dash": "…/dash/manifest.mpd?token=…&expires=1791583687", "poster": "…/poster.jpg?token=…&expires=1791583687",
 "embed": "…/embed/vod/4b298743-…?token=…&expires=1791583687", "watch": "…/watch/4b298743-…?token=…&expires=1791583687",
 "token": "d9paAvKBadcpU4RYvZCfUyZAidWy0r-ogn3Mb_qoPYY", "expires": 1791583687, "expires_at": "2026-10-09T22:08:07Z"}
```

### 9.7 curl examples

```bash
B=http://127.0.0.1:8080; H="Authorization: Bearer $TOKEN"; J='Content-Type: application/json'

# --- admin: a user and its first key (with the account scope)
curl -s -H "$H" -H "$J" -d '{"username":"alice","display_name":"Alice Rossi","quota_bytes":10737418240}' $B/api/v1/users
KEY=$(curl -s -H "$H" -H "$J" -d '{"name":"dashboard","scopes":["account"]}' $B/api/v1/users/alice/keys | jq -r .key)
curl -s -H "AccessKey: $KEY" $B/api/v1/auth/check                       # {"role":"user","user":{…},"scopes":["account"],…}

# --- the user: a bucket and an upload-only key for /videos/
curl -s -H "AccessKey: $KEY" -H "$J" -d '{"name":"alice-media","public_playback":false}' $B/api/v1/account/buckets
UP=$(curl -s -H "AccessKey: $KEY" -H "$J" -d '{"name":"uploader","scopes":["storage"],
  "permissions":[{"bucket":"alice-media","prefix":"/videos/","access":"write"}]}' $B/api/v1/account/keys | jq -r .key)

# --- storage API
curl -s -H "AccessKey: $UP" -H "Checksum: $(sha256sum clip.mp4 | cut -d' ' -f1)" \
     -X PUT --data-binary @clip.mp4 $B/storage/alice-media/videos/clip.mp4 | jq .encoding.status   # "queued"
curl -s -H "AccessKey: $UP" -T big.mov $B/storage/alice-media/videos/big.mov          # -T streams large files
curl -s -H "AccessKey: $UP" -X PUT -H 'X-Rms-Encode: none' --data-binary @notes.txt $B/storage/alice-media/videos/notes.txt
curl -s -H "AccessKey: $UP" $B/storage/alice-media/videos/ | jq '.[] | {name, size, status: .encoding.status}'
curl -s -H "AccessKey: $UP" -o clip.mp4 $B/storage/alice-media/videos/clip.mp4       # the original (Range ok)
curl -s -H "AccessKey: $UP" "$B/storage/alice-media/videos/clip.mp4?info=1" | jq .signed.embed
curl -s -H "AccessKey: $UP" -X POST -H "$J" -d '{"ladder":"mobile","visibility":"public"}' \
     "$B/storage/alice-media/videos/clip.mp4?action=encode"                            # 202, re-encoded
curl -s -H "AccessKey: $UP" -X POST -H "$J" -d '{"expires_secs":86400}' \
     "$B/storage/alice-media/videos/clip.mp4?action=sign" | jq -r .embed                 # signed player link
curl -s -H "AccessKey: $UP" -X DELETE $B/storage/alice-media/videos/clip.mp4 -w '%{http_code}\n'   # 204 (+ encoding)
curl -s -H "AccessKey: $KEY" -X DELETE $B/storage/alice-media/videos/ -w '%{http_code}\n'           # 204 (needs admin)

# --- account
curl -s -H "AccessKey: $KEY" $B/api/v1/account | jq '.usage'
curl -s -H "AccessKey: $KEY" -X POST $B/api/v1/account/buckets/alice-media/rotate-signing-key | jq -r .signing_key
curl -N -H "AccessKey: $KEY" $B/api/v1/account/events                                # vod / vod_deleted / heartbeat
curl -s -H "$H" "$B/api/v1/account/buckets?user=alice"                              # the admin, as alice
curl -s -H "$H" -X POST $B/api/v1/users/alice/recount | jq .report
```
