# API reference

The complete, field-level reference is [docs/API.md](../API.md). This page is a map of the endpoint groups.

## Authentication

| Who | How |
|---|---|
| Administrator | `Authorization: Bearer <admin token>` or the `rms_token` cookie set by `POST /api/v1/auth/login {"token": "…"}` |
| User (API key) | `AccessKey: <key>`, `Authorization: Bearer <key>` or the `rms_key` cookie (dashboard login with an `account` key) |
| Viewers | nothing: the public player API, playback endpoints and pages are open (private VOD needs a signed token) |

Errors are JSON `{"error": "…"}` (validation errors of the account API add `"field"`). Details:
[§1 Authentication](../API.md#1-authentication), [§2 Errors](../API.md#2-errors), [§3 Data types](../API.md#3-data-types).

## Endpoint groups

| Group | Endpoints | Docs |
|---|---|---|
| Auth | `POST /api/v1/auth/login`, `POST /api/v1/auth/logout`, `GET /api/v1/auth/check` | [§1](../API.md#1-authentication) |
| Live streams | `GET/POST /api/v1/broadcasts`, `GET/PATCH/DELETE /api/v1/broadcasts/{name}`, `…/regenerate-key`, `…/pause`, `…/resume`, `…/disconnect`, `…/embed` | [§4.1](../API.md#41-broadcasts) |
| Recording | `POST …/record/start`, `POST …/record/stop`, `GET …/recordings`, `GET /api/v1/recordings`, `GET /api/v1/recordings/{id}/download`, `DELETE /api/v1/recordings/{id}` | [§4.2](../API.md#42-recording) |
| Metrics | `GET /api/v1/metrics/overview`, `GET …/sessions`, `GET …/metrics`, `GET …/metrics/summary` | [§4.3](../API.md#43-metrics) |
| Waiting video and settings | `POST/DELETE …/waiting-video`, `GET/PATCH /api/v1/settings`, `POST/DELETE /api/v1/settings/waiting-video` | [§4.4](../API.md#44-waiting-video-and-settings) |
| System | `GET /api/v1/system`, `GET /healthz` | [§4.5](../API.md#45-system) |
| Events | `GET /api/v1/events` (SSE: live status, viewers, settings, VOD progress) | [§4.6](../API.md#46-events-sse) |
| Video on demand | `GET/POST /api/v1/vod`, `GET /api/v1/vod/ladders`, `GET/PATCH/DELETE /api/v1/vod/{id}`, `…/retranscode`, `…/embed`, `…/source` | [§4.7](../API.md#47-video-on-demand) |
| Cluster | `X-Rms-Node`, internal routes on the internal listener (`/internal/*`) | [§4.8](../API.md#48-cluster-mode) |
| Public player API | `GET /api/v1/public/ui`, `GET /api/v1/public/broadcasts/{name}` (+ `/events` SSE), `GET /api/v1/public/vod`, `GET /api/v1/public/vod/{id}` | [§5](../API.md#5-public-player-api) |
| Playback | `/live/{name}.flv`, `/ws/live/{name}.flv`, `/hls/{name}/…`, `/dash/{name}/…`, `/whep/{name}`, `/vod/{id}/…` | [§6](../API.md#6-playback-endpoints) |
| Pages | `/`, `/embed/{name}`, `/player/{name}`, `/embed/vod/{id}`, `/watch/{id}`, `/vod`, `/waiting/…`, `/static/…` | [§7](../API.md#7-pages) |
| Users (admin) | `/api/v1/users`, `/api/v1/users/{id}`, `…/keys`, `…/buckets`, `…/usage`, `…/encodings`, `…/recount` | [§9.3](../API.md#93-users-admin) |
| Account (user) | `/api/v1/account`, `…/keys`, `…/buckets`, `…/buckets/{id}/rotate-signing-key`, `…/buckets/{id}/sign`, `…/encodings`, `…/events` | [§9.4](../API.md#94-account) |
| Storage API | `GET/HEAD/PUT/DELETE /storage/{bucket}/{path}`, `?info=1`, `?action=encode`, `?action=sign` | [§9.2](../API.md#92-storage-api) |
| Signed URLs | `?token=…&expires=…` on `/vod/{id}/…`, `/embed/vod/{id}`, `/watch/{id}` | [§9.5](../API.md#95-signed-playback-urls) |

## Quick examples

```bash
TOKEN=...; H="Authorization: Bearer $TOKEN"; B=http://127.0.0.1:8080/api/v1
curl -s -H "$H" $B/broadcasts                                   # live streams with status and viewers
curl -s -H "$H" -H 'Content-Type: application/json' -d '{"title":"Town Hall","record":true}' $B/broadcasts
curl -s -H "$H" -X POST $B/broadcasts/town-hall/pause           # viewers see the waiting video
curl -s -H "$H" -X POST $B/broadcasts/town-hall/record/start    # manual recording
curl -s -H "$H" "$B/broadcasts/town-hall/metrics?step=60"       # viewers over the last hour
curl -s -H "$H" $B/broadcasts/town-hall/embed                   # iframe, URLs, OBS settings, snippets
curl -N -H "$H" $B/events                                       # SSE
curl -s -H "$H" -H 'Content-Type: application/json' -X PATCH -d '{"ui_language":"en","ui_theme":"dark"}' $B/settings
curl -s http://127.0.0.1:8080/api/v1/public/broadcasts/town-hall      # what the player sees (no auth)
```

More in the [curl cookbook](../API.md#8-curl-cookbook) and the [accounts examples](../API.md#97-curl-examples).
