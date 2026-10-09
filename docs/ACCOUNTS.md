# Accounts, API keys and buckets (storage API) — design contract

Multi-tenant content storage for rms, modelled on bunny.net Storage Zones + Stream: every **user** owns
**buckets** (directory trees of files kept as originals), manages **API keys** with path-scoped
permissions, uploads videos through a simple HTTP storage API and gets them **encoded** into
multi-bitrate HLS/DASH (reusing the VOD pipeline, §19) while the originals stay in the bucket.
Playback of private content uses **signed URLs**. The existing admin token remains the superuser.

Language conventions as in ARCHITECTURE.md: code/API/docs in English, dashboard it/en via `i18n.js`.

## 1. Entities (schema v5 on SQLite, Postgres migration 3)

```sql
CREATE TABLE users (
  id TEXT PRIMARY KEY, username TEXT UNIQUE NOT NULL,          -- slug [a-z0-9-_]{2,32}
  display_name TEXT NOT NULL, email TEXT, status TEXT NOT NULL DEFAULT 'active',   -- active|suspended
  quota_bytes INTEGER NOT NULL DEFAULT 0,                      -- 0 = unlimited (originals + encodings)
  max_buckets INTEGER NOT NULL DEFAULT 10, default_ladder TEXT,
  created_at TEXT NOT NULL, updated_at TEXT NOT NULL, last_login_at TEXT);
CREATE TABLE api_keys (
  id TEXT PRIMARY KEY, user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  name TEXT NOT NULL, key_prefix TEXT NOT NULL,                -- first 12 chars, for display ("rms_ab12cd34")
  key_hash TEXT UNIQUE NOT NULL,                               -- sha256 hex of the full key
  scopes TEXT NOT NULL DEFAULT '[]',                           -- JSON: ["account"] and/or ["storage"]
  permissions TEXT NOT NULL DEFAULT '[]',                      -- JSON: [{"bucket":"<id>|*","prefix":"/videos/","access":"read|write|admin"}]
  expires_at TEXT, last_used_at TEXT, revoked_at TEXT, created_at TEXT NOT NULL);
CREATE TABLE buckets (
  id TEXT PRIMARY KEY, user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  name TEXT UNIQUE NOT NULL,                                   -- global slug, used in URLs (/storage/{name}/...)
  description TEXT NOT NULL DEFAULT '', default_ladder TEXT, auto_encode INTEGER NOT NULL DEFAULT 1,
  public_playback INTEGER NOT NULL DEFAULT 1,                  -- 0 = encodings need a signed URL
  signing_key TEXT NOT NULL,                                   -- 32 random bytes hex, rotatable
  bytes_used INTEGER NOT NULL DEFAULT 0, objects_count INTEGER NOT NULL DEFAULT 0,
  created_at TEXT NOT NULL, updated_at TEXT NOT NULL);
CREATE TABLE bucket_objects (                                  -- index of the files on disk (writes go through the API only)
  id TEXT PRIMARY KEY, bucket_id TEXT NOT NULL REFERENCES buckets(id) ON DELETE CASCADE,
  path TEXT NOT NULL,                                          -- normalized, leading '/', no trailing '/', NFC, no '..'
  is_dir INTEGER NOT NULL DEFAULT 0, size INTEGER NOT NULL DEFAULT 0, content_type TEXT, sha256 TEXT,
  created_at TEXT NOT NULL, updated_at TEXT NOT NULL, UNIQUE (bucket_id, path));
ALTER TABLE vod_items ADD COLUMN user_id TEXT; ADD COLUMN bucket_id TEXT; ADD COLUMN object_id TEXT;
ALTER TABLE vod_items ADD COLUMN visibility TEXT NOT NULL DEFAULT 'public';   -- public|private
ALTER TABLE vod_items ADD COLUMN output_bytes INTEGER NOT NULL DEFAULT 0;     -- (8A addition) encoding size, counted in quotas
CREATE INDEX idx_objects_bucket_path ON bucket_objects(bucket_id, path);
```
Files live under `data/buckets/<bucket_id>/<path>` (never under the bucket *name*, so renames are free).
Encodings keep living under `data/vod/<id>/abr/` + `poster.jpg`; `vod_items.source_path` points **into the
bucket** (no copy). The library items created before this phase have `user_id = NULL` (admin library).

API key format: `rms_` + 40 hex chars, shown **once** at creation; stored as sha256; `key_prefix` for lists.
Access levels: `read < write < admin`. `write` = upload/overwrite/delete files/encode; `admin` = also delete
directories recursively and change bucket settings through the storage API `?action=` endpoints.
Permission match: entry applies when `entry.bucket ∈ {"*", bucket.id}` and `path` (or the directory being
listed, with trailing `/`) starts with `entry.prefix` (`/` = whole bucket). The `account` scope implicitly
grants `admin` on all of the user's buckets and allows managing keys/buckets. Suspended users: every key 403.

## 2. Authentication & principals

`Principal = Admin | User { user_id, key_id, scopes, permissions }`, resolved once per request by a
middleware on `/api/v1/*` and `/storage/*`, from (in order): `Authorization: Bearer <admin token | api key>`,
`AccessKey: <api key>` (bunny-style), cookies `rms_token` (admin) / `rms_key` (user key, HttpOnly, SameSite=Lax).
`POST /api/v1/auth/login {token}` accepts both; response `200 {role:"admin"}` or `200 {role:"user", user:{id,
username, display_name}, scopes, key_id}` and sets the matching cookie (clearing the other one).
`GET /api/v1/auth/check` → `200 {role, user?, scopes?, key_id?}` (both answered `204` before 8A).
`last_used_at` updated at most once a minute. Constant-time comparison for the admin token; keys are
looked up by hash. Rate-limit failed logins (existing 400 ms delay).

## 3. Storage API (bunny-like) — `/storage/{bucket}/{path...}`

| Method & path | Needs | Behaviour |
|---|---|---|
| `GET /storage/{b}/` and `GET /storage/{b}/{dir}/` | read on dir | JSON listing `[{id, name, path, is_dir, size, content_type, sha256, created_at, updated_at, encoding?}]` (`encoding` = `{vod_id, status, progress, renditions, hls_url, dash_url, poster_url, visibility}` when the file has an encoding) |
| `GET /storage/{b}/{file}` | read | the original file (Range, ETag, `Content-Type`; `?download=1` → attachment); `?info=1` → object JSON incl. encoding + playback URLs (+ `signed` URLs when private, computed with the bucket signing key, `?expires_secs=` default 3600) |
| `HEAD /storage/{b}/{file}` | read | metadata headers only |
| `PUT /storage/{b}/{path}/{file}` | write | raw body streamed to `data/upload-tmp` then renamed into place (creates parent dirs); overwrite allowed; optional `Checksum: <sha256 hex>` verified (400 on mismatch); size limit `storage.max_object_mb` (default 20480); quota check (507 Insufficient Storage); `X-Rms-Encode: <ladder>` forces encoding, `X-Rms-Encode: none` disables it, otherwise bucket `auto_encode` + video extension decides; `X-Rms-Visibility: public|private` (default from bucket `public_playback`); overwrite of an encoded video re-encodes. Response `201 {object}` |
| `PUT /storage/{b}/{dir}/` | write | create an (empty) directory → 201 |
| `DELETE /storage/{b}/{file}` | write | deletes the file and its encoding → 204 |
| `DELETE /storage/{b}/{dir}/` | admin | recursive delete → 204 |
| `POST /storage/{b}/{file}?action=encode` body `{ladder?, visibility?}` | write | (re)queues the encoding → 202 object view with `encoding` |
| `POST /storage/{b}/{file}?action=sign` body `{expires_secs?}` | read | signed playback URLs `{hls, dash, poster, embed, expires}` |

Rules: path normalization (percent-decoding, NFC, reject `..`, control chars, empty segments, `.rms` prefix);
listings hide nothing (encodings live elsewhere). Content types from `mime_guess`. Every response carries
`Cache-Control: no-store` except file bodies (`private, max-age=0`). CORS permissive (keys are sent as headers).
As built (8A): a missing directory lists `[]`; `DELETE` of the bucket root → 400 (delete the bucket);
file/directory clashes → 409; `X-Rms-Encode: none` on an overwrite deletes the stale encoding.

## 4. Management API

Admin (`Principal::Admin`): `GET/POST /api/v1/users`, `GET/PATCH/DELETE /api/v1/users/{id}` (delete needs
`?force=1` when the user owns buckets; cascades), `GET/POST /api/v1/users/{id}/keys`, `PATCH/DELETE
/api/v1/users/{id}/keys/{kid}`, `GET/POST /api/v1/users/{id}/buckets`, `GET /api/v1/users/{id}/usage`,
`GET /api/v1/users/{id}/encodings`. Admin may also call every `/api/v1/account/...` route with `?user=<id>`.

Account (`Principal::User` with scope `account`): `GET /api/v1/account` → `{user, usage:{bytes_used, quota_bytes,
buckets, objects, encodings}, limits}`; `GET/POST /api/v1/account/keys` (POST body `{name, scopes, permissions,
expires_at?}` → `201 {key: "rms_…", id, …}` once), `PATCH /api/v1/account/keys/{kid}` (name/permissions/expires),
`DELETE /api/v1/account/keys/{kid}` (revoke); `GET/POST /api/v1/account/buckets` (`{name, description?,
default_ladder?, auto_encode?, public_playback?}`), `GET/PATCH/DELETE /api/v1/account/buckets/{id}` (delete needs
`?force=1` when not empty), `POST /api/v1/account/buckets/{id}/rotate-signing-key`, `POST
/api/v1/account/buckets/{id}/sign {path, expires_secs?}`, `GET /api/v1/account/encodings` (the user's VOD items),
`GET /api/v1/account/events` (SSE: `vod` events of the user's encodings + heartbeat). Validation: 400 with
field messages; names are slugs; a key's `permissions[].bucket` must belong to the same user.
As built (8A): bucket and user `{id}` accept the name too (`permissions[].bucket` names are stored as ids);
a storage-only key needs ≥ 1 permission; added `GET/PATCH /api/v1/account/encodings/{id}` (title,
description, visibility); `max_buckets` reached → 409.

## 5. Encoding & playback

- Encoding = `vod_items` row linked to `(user_id, bucket_id, object_id)`; `title` defaults to the file name;
  the job runner is unchanged (ladders from config, leases, inbox untouched). The VOD library page (admin)
  shows all items with their owner/bucket; `/api/v1/public/vod` (catalogue) lists only `visibility = public`.
- Deleting the object deletes the encoding; deleting the bucket deletes everything; quotas count original +
  encoding sizes (`bytes_used` recomputed on each change; `POST /api/v1/users/{id}/recount` for repair).
- **Signed URLs** for `visibility = private`: `token = base64url(HMAC-SHA256(signing_key, "<vod_id>|<expires>"))`,
  query `?token=<t>&expires=<unix>` accepted on every `/vod/{id}/*` request; the first accepted request also sets
  cookie `rms_play_<vod_id>=<t>:<expires>` (HttpOnly, `Path=/vod/<id>/`, Max-Age until expiry) so same-origin
  segment requests work without query; the player (embed/watch) propagates the query to all requests anyway
  (hls.js `xhrSetup` re-opening with the query; dash.js request interceptor). 401 without/expired token.
  Rotating the signing key invalidates all tokens. Public items ignore tokens.
  As built (8A): the HMAC key is the `signing_key` text (64 hex chars); playlists and MPD of private items are
  rewritten with the token (native HLS works without player help); the access data is cached 5 s per node, so a
  rotation or visibility change made on another cluster node applies within 5 s; admin-library items stay public.

## 6. Dashboard

- **Admin**: "Utenti" page (`#/users`): table (username, display name, status, buckets, usage, keys, last login),
  create/edit/suspend/delete, user detail (`#/users/<id>`) with usage bars, keys (create → show once dialog,
  revoke), buckets (create, open as the user), encodings list.
- **User portal** (login with an `account` key): nav "I miei bucket" (`#/buckets`), "Chiavi API" (`#/keys`),
  "Account" (`#/account`); bucket browser (`#/buckets/<id>/<path>`): breadcrumb, folders first, upload
  (drag & drop, multi-file, streamed `PUT` with progress, ladder/visibility choice), new folder, delete,
  encode/re-encode, encoding status/progress live (account SSE), play (embed page; signed URL when private),
  object drawer (links: direct, HLS, DASH, poster, embed code, signed links with expiry picker, copy buttons),
  bucket settings (description, auto-encode, default ladder, public playback, rotate signing key). Keys page:
  create dialog with scopes and permission rows (bucket select, prefix, access), expiry; list with prefix,
  scopes, last used, revoke. Account page: usage/quota, limits.
- i18n keys in both dictionaries; colours via tokens; 390 px layouts.
- As built (8B): routes use the bucket **name** (`#/buckets/<name>/<path…>`; admin `#/users/<username>/buckets/<name>/<path…>`);
  the admin opens a user's buckets with the admin principal (account routes with `?user=`, the storage API with the
  admin cookie) instead of "as the user"; a login with a key lacking the `account` scope is refused (and logged out);
  the key edit dialog changes name, permissions and expiry (scopes are chosen at creation); uploads run one at a
  time from a queue that survives folder navigation; the admin "Video on demand" library shows owner/bucket chips
  and signs its preview of private items through `/account/buckets/{id}/sign?user=`. Details: ARCHITECTURE.md §23.

## 7. Phases

- **8A (backend)**: schema + repos (both backends, `dual_db_test!`), principals/auth middleware, storage API,
  management API, encoding link + quotas, signed URLs + player token propagation, `scripts/smoke.sh` additions,
  `docs/API.md` sections, ARCHITECTURE §22.
- **8B (dashboard)**: admin Users pages, user portal, bucket browser, keys, account; screenshots.
- **8C (docs)**: English `README.md` (GitHub) with screenshots in `docs/screenshots/`, Italian `README.it.md`,
  wiki pages in `docs/wiki/`.
