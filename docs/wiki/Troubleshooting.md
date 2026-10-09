# Troubleshooting

Start with the log (`journalctl -u rms`, `docker logs rms`; more detail with `RUST_LOG=debug` or
`log.level = "debug"`), `GET /healthz` and *Settings → System* in the dashboard (or `GET /api/v1/system`),
which shows ffmpeg, the listeners, the public URL, the WebRTC port and advertised ICE addresses and the VOD
queue.

## Ports

| Port | Used by | If it is taken |
|---|---|---|
| `8080/tcp` (`server.http_bind`) | dashboard, API, players, FLV/HLS/DASH, WHEP signalling | rms exits: `cannot bind HTTP listener on …` |
| `1935/tcp` (`rtmp.bind`) | RTMP from OBS | rms exits at startup |
| `8443/udp` (`webrtc.udp_port`) | WebRTC media, one port for every viewer | WHEP is disabled, the reason is in `/api/v1/system` → `webrtc.error`; players use FLV/HLS |
| `8081/tcp` (`cluster.internal_bind`) | cluster only, internal | |

Change them with `--http-bind`, `--rtmp-bind`, `RMS_WEBRTC_UDP_PORT` or the configuration. `ss -ltnup | grep -E
':(8080|1935|8443)'` shows who holds a port. Two instances on one host need different ports *and* data
directories.

## Firewall

```bash
sudo firewall-cmd --permanent --add-port=8080/tcp --add-port=1935/tcp --add-port=8443/udp && sudo firewall-cmd --reload
# ufw: sudo ufw allow 8080/tcp && sudo ufw allow 1935/tcp && sudo ufw allow 8443/udp
```

Cloud providers also have their own security groups: open `8443/udp` there too.

## WebRTC does not connect

Symptom: the player starts with a few seconds of delay and the *Status* panel shows the viewer on HTTP-FLV
or LL-HLS instead of WebRTC (`?src=webrtc` forces it for a test; `chrome://webrtc-internals` shows the ICE
state).

1. **UDP not reachable**: open `8443/udp` in the host firewall and the cloud security group; WebRTC media never
   goes through nginx/NPM/Caddy.
2. **Behind NAT** (cloud VM with an elastic IP, home router): set `webrtc.public_ip = ["<public IP>"]`
   (`RMS_WEBRTC_PUBLIC_IP`) and forward `8443/udp`. If the NAT maps a different public port, set
   `webrtc.public_port`. Check `candidates` in `GET /api/v1/system`.
3. **Viewer networks blocking UDP** (some corporate networks): there is no TURN server, these viewers fall back
   to FLV/HLS automatically.
4. **Stuttering or frozen picture on WebRTC only**: the encoder sends B-frames. Set x264 *zerolatency* or
   *B-frames = 0* in OBS ([OBS setup](OBS-Setup.md)).
5. **No audio on WebRTC**: ffmpeg is missing (AAC → Opus needs it).
6. **HEVC/AV1 streams**: WebRTC answers `415`; only H.264 is sent.

## OBS

| Problem | Check |
|---|---|
| "Failed to connect to server" | host and port (`rtmp://<host>:1935/live`), firewall, `rtmp.bind`; behind NPM the *Stream* entry |
| Connects then disconnects at once | wrong stream key (it was regenerated?), or the live stream was deleted |
| Dashboard warns about the keyframe interval | OBS's default is "0" (auto): set **2 s** under *Output → Streaming* (Advanced mode) |
| LL-HLS/LL-DASH high latency or slow start | long GOP (see above); `hls.part_duration`; proxy buffering |
| HLS/DASH answer `415` | the stream is HEVC/AV1 or not AAC: use H.264 + AAC (FLV paths still work) |
| HLS/DASH answer `503 stream is starting` | no complete segment yet: wait for the first keyframe (≤ one GOP) |
| Two encoders fight over one key | the newest publisher replaces an idle (> 5 s) one; disconnect the other encoder |

Measure the GOP: `ffprobe -v error -select_streams v:0 -read_intervals "%+10" -show_entries packet=pts_time,flags
-of csv=p=0 http://<host>:8080/live/<name>.flv | grep K`.

## ffmpeg missing

rms works without ffmpeg but with reduced features:

| Feature | Without ffmpeg |
|---|---|
| Recordings | stay FLV (no MP4 remux) |
| Waiting videos | only MP4 uploads are accepted, no poster (`415 ffmpeg not available: upload an MP4 (H.264/AAC)`) |
| WebRTC | video only (no Opus audio) |
| VOD and bucket encodings | unavailable (`503`) — ffmpeg **and** ffprobe are required |

Install a static build in `PATH` or set `ffmpeg.path` (ffprobe is looked up next to it), then restart; the
dashboard's *Settings → System* shows the version found. VOD failures show the last ffmpeg error line on the
item; inbox failures are in `inbox/_failed/<name>.error.txt`.

## Behind a reverse proxy

| Symptom | Fix |
|---|---|
| FLV or the dashboard's real-time updates arrive in bursts, high latency | turn buffering off (`proxy_buffering off;` nginx/NPM, `flush_interval -1` Caddy) |
| WS-FLV fails (`400`/`502`) | forward WebSocket upgrades (NPM: *Websockets Support*) |
| Streams cut after 60–90 s | raise read/send timeouts (`proxy_read_timeout 3600s`) |
| Upload fails with an HTML `413` | raise `client_max_body_size` above `server.max_upload_mb` (and `storage.max_object_mb` for the storage API) |
| Links and embed codes show `http://127.0.0.1:8080` | set `server.public_url` |
| Every viewer has the proxy's IP, HLS viewers undercounted | `server.trust_proxy = true` |
| Old dashboard after an upgrade | disable proxy caching of assets (NPM *Cache Assets*) |
| LL-HLS not really low latency | enable HTTP/2 on the proxy |

Templates: [`deploy/nginx.conf.example`](../../deploy/nginx.conf.example),
[`deploy/Caddyfile.example`](../../deploy/Caddyfile.example), [Nginx Proxy Manager](Nginx-Proxy-Manager.md).

## SQLite and network file systems

The single-node database is `data/rms.sqlite` (WAL mode). Keep the data directory on a **local disk**: SQLite
must not live on NFS/SMB, and it must not be shared by several instances. To run several instances use
[cluster mode](Cluster-Mode.md) with PostgreSQL; there the shared `data/` volume (NFS is fine) holds only files.
Back up with `sqlite3 data/rms.sqlite ".backup backup.sqlite"` while running, or copy the file with rms stopped.

## Disk space

What grows, all under the data directory unless configured otherwise:

| Path | Content | Size hint |
|---|---|---|
| `recordings/<name>/` | recordings (MP4, or FLV without ffmpeg) | ≈ 1.1 GB per hour at 2.5 Mbit/s |
| `vod/<id>/` | VOD source (`vod.keep_source`), renditions, poster | renditions of the `standard` ladder ≈ 1.3 MB per second of 1080p video |
| `buckets/` (`storage.root`) | users' original files (encodings are under `vod/`) | quotas per user |
| `waiting/` | waiting videos and posters | small |
| `upload-tmp/` | uploads in progress (removed when they finish or fail) | up to the upload size |
| `rms.sqlite*` | metrics samples and sessions (`metrics.retention_days`, 90) | small |

Delete old recordings and videos from the dashboard (or the API), set `vod.keep_source = false` if you never
re-encode, lower `metrics.retention_days`, and monitor free space: a full disk makes recordings and encodings
fail (bucket uploads answer `507` only for quotas, not for a full disk).

## Login problems

- **Lost admin token**: read `data/admin_token`, or set `RMS_ADMIN_TOKEN` and restart (it takes precedence).
- **A user cannot sign in**: the key needs the *account* scope (storage-only keys are refused), must not be
  expired or revoked, and the user must be active (suspended users get `403`).
- The browser console shows one `401` on the login page: that is the initial session check, not an error.
