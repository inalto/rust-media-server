# Playback and embedding

Every live stream is available over five protocols at once; the built-in player picks the best one for the
viewer. All playback endpoints are public, CORS-enabled (`Access-Control-Allow-Origin: *`) and counted in
the [metrics](Metrics.md).

![Links and embed panel](../screenshots/broadcast-links-embed.png)

## Protocols and URLs

`<name>` is the URL name of the live stream (e.g. `product-launch`), `<host>` the server (or
`server.public_url`).

| Protocol | URL | Players | Latency (localhost) |
|---|---|---|---|
| WebRTC (WHEP) | `http://<host>:8080/whep/<name>` — `POST` of an SDP offer; media on `8443/udp` | built-in player, WHEP snippet, GStreamer `whepsrc`, any WHEP client | ≈ 40 ms glass-to-glass |
| HTTP-FLV | `http://<host>:8080/live/<name>.flv` | mpegts.js, VLC, ffplay | ≈ 0.3–1.4 s |
| WebSocket-FLV | `ws://<host>:8080/ws/live/<name>.flv` (`wss://` behind TLS) | mpegts.js | as HTTP-FLV |
| LL-HLS | `http://<host>:8080/hls/<name>/master.m3u8` | hls.js, Safari/iOS native, VLC | ≈ 1.8 s (hls.js) |
| LL-DASH | `http://<host>:8080/dash/<name>/manifest.mpd` | dash.js | ≈ 2.1 s (dash.js) |
| Embed player | `http://<host>:8080/embed/<name>` | `<iframe>` | |
| Full-page player | `http://<host>:8080/player/<name>` | browser (title above the video, latency shown) | |

Notes:

- **WebRTC** needs UDP from the viewer to `webrtc.udp_port` (default 8443): the signalling (`POST /whep/…`)
  goes through your reverse proxy like any HTTP request, the media does not. No TURN server is used; viewers on
  networks that block outgoing UDP fall back to the next protocol. H.264 is passed through (no B-frames), AAC
  is transcoded to Opus with ffmpeg (video-only without ffmpeg). A new viewer starts at the next keyframe.
- **HTTP-FLV / WS-FLV** start from the GOP cache (last keyframe) and skip ahead when a client falls behind.
  They also carry HEVC/AV1 from Enhanced RTMP.
- **LL-HLS / LL-DASH** are CMAF (fMP4) with 0.5 s parts and blocking playlist reload (HLS) or chunked transfer
  (DASH); they work well through CDNs and HTTP/2. Behind a reverse proxy, buffering must be off.
- A **paused** live stream keeps all endpoints running: only the public status changes, and the embed player
  shows the waiting video.

## The embed player

```html
<iframe src="https://stream.example.com/embed/product-launch?autoplay=1&muted=1"
        width="960" height="540" frameborder="0"
        allow="autoplay; fullscreen; picture-in-picture" allowfullscreen></iframe>
```

![Live player with the LIVE badge and the latency indicator](../screenshots/player-live.png)

- Source selection with `src=auto` (default): **WebRTC → HTTP-FLV → LL-HLS → LL-DASH** (on iOS: WebRTC →
  LL-HLS → LL-DASH; LL-HLS is native on iOS/Safari, hls.js elsewhere). Only sources enabled for the live
  stream and supported by the browser are tried.
- Failover: WebRTC gives up when the WHEP `POST` is refused, ICE does not connect within 6 s, no frame
  arrives within 8 s or no video packet for 4 s; the other engines when they fail fatally, make no progress
  for 12 s or do not start within 15 s. When everything fails the player returns to the waiting screen and
  retries after 5 s.
- While the stream is offline or paused the player loops the [waiting video](Recording-and-Waiting-Video.md)
  (or shows the waiting text) and switches to the live stream by itself, using the public SSE status stream
  (polling every 5 s as a fallback).
- Browsers block autoplay with sound: keep `muted=1`; the player shows an **Unmute** button. With
  `muted=0` a blocked autoplay is retried muted.
- The *Links and embed* panel of the dashboard builds the iframe with these options and shows ready-to-paste
  examples for hls.js, mpegts.js, dash.js and a library-free WHEP client.

![Player while the live stream is offline](../screenshots/player-waiting.png)

### Live player parameters

All optional, on `/embed/<name>` and `/player/<name>`:

| Parameter | Values | Default | Effect |
|---|---|---|---|
| `autoplay` | `1` / `0` | `1` | start as soon as the stream is live |
| `muted` | `1` / `0` | `1` | start muted |
| `controls` | `1` / `0` | `1` | native video controls |
| `src` | `auto`, `webrtc` (alias `whep`), `flv`, `hls`, `dash` | `auto` | source to try first, then fall through (`dash` tries DASH only) |
| `latency` | `1` / `0` | `0` on `/embed`, `1` on `/player` | show the estimated latency next to the LIVE badge |
| `lang` | `it` / `en` | dashboard setting | language of the player texts |
| `theme` | `system` / `light` / `dark` | dashboard setting | colours of the waiting screen and overlays (the video area stays black) |

A flag is false for `0`, `false`, `no`, `off`. `window.rmsPlayer` exposes `status`, `source`, `engine`,
`latency`, `setSource(src)` and `reload()` for debugging.

### VOD player parameters

On `/embed/vod/<id>` and `/watch/<id>` (see [VOD and multi-bitrate](VOD-and-Multi-bitrate.md)):

| Parameter | Values | Default | Effect |
|---|---|---|---|
| `autoplay` | `1` / `0` | `0` | start at once (otherwise the poster is shown until play) |
| `muted` | `1` / `0` | = `autoplay` | start muted |
| `controls` | `1` / `0` | `1` | native controls (otherwise a play button) |
| `t` | seconds | `0` | start position |
| `src` | `auto`, `hls`, `dash` | `auto` | `auto`/`hls`: native HLS on iOS/Safari, else hls.js, then dash.js |
| `lang`, `theme` | as above | dashboard settings | |
| `token`, `expires` | signed token | — | required for **private** videos ([signed URLs](Users-API-Keys-and-Buckets.md#signed-urls)) |

The VOD player chooses the quality automatically (bandwidth and player size) and has a quality menu to pin one.

## Your own player

The *Links and embed* panel and `GET /api/v1/broadcasts/<name>/embed` return self-contained snippets. In short:

```html
<!-- LL-HLS with hls.js -->
<video id="v" controls muted autoplay playsinline></video>
<script src="https://cdn.jsdelivr.net/npm/hls.js@1.7.3/dist/hls.min.js"></script>
<script>
  const url = 'https://stream.example.com/hls/product-launch/master.m3u8';
  const v = document.getElementById('v');
  if (v.canPlayType('application/vnd.apple.mpegurl')) v.src = url;           // Safari / iOS
  else { const hls = new Hls({ lowLatencyMode: true }); hls.loadSource(url); hls.attachMedia(v); }
</script>
```

```html
<!-- HTTP-FLV with mpegts.js -->
<script src="https://cdn.jsdelivr.net/npm/mpegts.js@1.8.2/dist/mpegts.js"></script>
<script>
  const p = mpegts.createPlayer({ type: 'flv', isLive: true,
                                  url: 'https://stream.example.com/live/product-launch.flv' },
                                { liveBufferLatencyChasing: true });
  p.attachMediaElement(document.getElementById('v')); p.load(); p.play();
</script>
```

```js
// WebRTC (WHEP), no library
const pc = new RTCPeerConnection();
pc.addTransceiver('video', { direction: 'recvonly' });
pc.addTransceiver('audio', { direction: 'recvonly' });
const stream = new MediaStream();
pc.ontrack = (e) => { stream.addTrack(e.track); video.srcObject = stream; };
await pc.setLocalDescription(await pc.createOffer());
const res = await fetch('https://stream.example.com/whep/product-launch', {
  method: 'POST', headers: { 'Content-Type': 'application/sdp' }, body: pc.localDescription.sdp });
const session = new URL(res.headers.get('Location'), res.url).href;   // DELETE it when done
await pc.setRemoteDescription({ type: 'answer', sdp: await res.text() });
```

For dash.js use `dashjs.MediaPlayer().create()` with the MPD URL (the dashboard snippet sets
`streaming.delay.liveDelay: 2` and live catch-up). Pinned library versions: hls.js 1.7.3, mpegts.js 1.8.2,
dash.js 5.2.1.

## Other clients

```bash
ffplay -fflags nobuffer http://<host>:8080/live/<name>.flv
vlc http://<host>:8080/hls/<name>/master.m3u8
# WHEP: any WHEP client (e.g. GStreamer whepsrc) with the endpoint http://<host>:8080/whep/<name>
```

## Per-stream switches

In *Edit* on a live stream you can disable HTTP-FLV (also WS-FLV), LL-HLS, LL-DASH or WebRTC for that stream
(`enable_flv`, `enable_hls`, `enable_dash`, `enable_webrtc`): the endpoint answers `403` and the player skips it.

Field-level details: [docs/API.md](../API.md) §6 (playback endpoints) and §7 (pages).
