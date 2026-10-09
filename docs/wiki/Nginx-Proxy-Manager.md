# Nginx Proxy Manager

[Nginx Proxy Manager](https://nginxproxymanager.com/) (NPM) can publish rms with Let's Encrypt TLS, HTTP/2 and
WebSockets. This page summarizes the full, verified step-by-step guide (NPM 2.16.0) in
[deploy/nginx-proxy-manager.md](../../deploy/nginx-proxy-manager.md) (Italian), which also explains every
choice with sources.

```
Internet ──80/443──▶ NPM ──http──▶ rms:8080     dashboard, API, players, FLV, WS-FLV, HLS, DASH, WHEP signalling
OBS ───────1935/tcp─▶ NPM Stream ─▶ rms:1935    (or directly to rms)
Browser ───8443/udp──────────────▶ rms:8443     WebRTC media: DIRECT, never through the HTTP proxy host
```

## 1. rms

```toml
[server]
public_url = "https://stream.example.com"   # links and embeds in the dashboard
trust_proxy = true                           # viewer IP from X-Real-IP (or the last X-Forwarded-For)
[webrtc]
public_ip = ["203.0.113.10"]                 # public IP of the host receiving 8443/udp
```

Environment equivalents: `RMS_SERVER_PUBLIC_URL`, `RMS_SERVER_TRUST_PROXY=true`, `RMS_WEBRTC_PUBLIC_IP`.

## 2. Same compose network

Run NPM and rms on one Docker/Podman network and point the proxy host at the service name `rms`. Publish only
`8443/udp` from rms (and `1935` if OBS connects directly); expose `8080`/`1935` to NPM only. Keep the NPM admin
UI (`81`) on `127.0.0.1`.

## 3. Proxy host settings

| Tab | Setting | Value |
|---|---|---|
| Details | Domain Names | `stream.example.com` |
| Details | Scheme / Forward Hostname / Port | `http` / `rms` / `8080` |
| Details | **Cache Assets** | **off** (it would cache the dashboard JS/CSS and posters for 30 minutes) |
| Details | Block Common Exploits | on (turn it off if a legitimate client gets `403`) |
| Details | **Websockets Support** | **on** (needed by WS-FLV) |
| Details | Access List | *Publicly Accessible* (Basic auth would block embeds and strip the API `Bearer`) |
| SSL | Certificate | Request a new Let's Encrypt certificate |
| SSL | Force SSL, **HTTP/2 Support** | on (Low-Latency HLS expects HTTP/2) |
| SSL | HSTS | only when the whole domain is HTTPS |

**Advanced → Custom Nginx Configuration:**

```nginx
# rms: streaming media (HTTP-FLV, SSE, LL-HLS parts, LL-DASH chunks) and large uploads
proxy_buffering off;
proxy_request_buffering off;
proxy_cache off;
proxy_read_timeout 3600s;
proxy_send_timeout 3600s;
client_max_body_size 1100m;
gzip off;
```

- `proxy_buffering off` — otherwise FLV, SSE, LL-HLS parts and LL-DASH chunks arrive in bursts.
- `proxy_request_buffering off` — uploads go to rms as they arrive instead of being spooled by nginx.
- `proxy_read_timeout`/`proxy_send_timeout` — NPM's default 90 s would cut idle long-lived connections.
- `client_max_body_size` ≥ `server.max_upload_mb` (1024 MiB); above it nginx answers `413` itself. For the
  storage API (`storage.max_object_mb`, 20 GiB by default) raise it accordingly.
- Do **not** add `proxy_set_header Connection ""`: it has no effect there and would break WS-FLV if it had.

## 4. RTMP and WebRTC

- **RTMP**: either publish `1935:1935` directly on rms (simplest) or add an NPM *Stream* (incoming `1935` →
  `rms:1935`, TCP); with the stream rms sees NPM's address as the publisher IP. OBS server:
  `rtmp://stream.example.com:1935/live`.
- **WebRTC**: do not proxy it. Publish `8443:8443/udp` on rms, open the firewall
  (`firewall-cmd --permanent --add-port=8443/udp`) and set `webrtc.public_ip`. The WHEP signalling
  (`POST /whep/<name>`) goes through the proxy host like any request. Without UDP the player falls back to
  HTTP-FLV/LL-HLS.

## 5. Quick checks

```bash
H=https://stream.example.com
curl -sI --http2 $H/healthz | head -1                                  # HTTP/2 200
curl -sN -m 5 -o /dev/null -w 'ttfb=%{time_starttransfer}s\n' $H/live/<name>.flv   # < 1 s, not buffered
curl -si --http1.1 -m 3 -H 'Connection: Upgrade' -H 'Upgrade: websocket' -H 'Sec-WebSocket-Version: 13' \
     -H 'Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==' $H/ws/live/<name>.flv | head -1      # HTTP/1.1 101
curl -si -X OPTIONS $H/whep/<name> | head -1                           # 204
```

## 6. Typical problems

| Symptom | Likely cause |
|---|---|
| `502` at once | `rms` not reachable (name/network, rms stopped) |
| FLV/SSE in bursts, high latency | `proxy_buffering off` missing (Advanced tab not saved) |
| WS-FLV `400`/`502` | *Websockets Support* off |
| Stream cut after 90 s of silence | `proxy_read_timeout` not applied |
| Upload `413` HTML page | `client_max_body_size` too small (a JSON `413` comes from rms) |
| Old dashboard CSS/JS, stale posters | *Cache Assets* on |
| Viewer IPs = NPM's IP | `server.trust_proxy = false` |
| WebRTC never connects, FLV works | `8443/udp` not reachable or wrong `webrtc.public_ip` (see `GET /api/v1/system` → candidates) |

## Cluster

NPM cannot balance between several rms nodes (one forward host per proxy host, no health checks, no
affinity). Use **NPM → HAProxy (`lb:8080`) → nodes** with the same proxy host options; see
[Cluster mode](Cluster-Mode.md).
