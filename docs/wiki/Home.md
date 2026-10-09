# rms — rust-media-server wiki

**rms** is a single-binary, low-latency live media server written in Rust. OBS publishes over RTMP; rms
delivers the stream to browsers over WebRTC (WHEP), HTTP-FLV / WebSocket-FLV, LL-HLS and LL-DASH, records it,
counts the viewers, loops a waiting video while the stream is offline and serves video on demand with
multi-bitrate ladders. A built-in dashboard manages everything; users, API keys and buckets with a
bunny.net-style storage API let other applications upload and publish videos.

![Live streams dashboard](../screenshots/dashboard-broadcasts.png)

## Getting started

1. [Installation](Installation.md): build or run the Docker image, first login with the admin token.
2. [OBS setup](OBS-Setup.md): server, stream key and low-latency encoder settings.
3. [Playback and embedding](Playback-and-Embedding.md): protocols, URLs, the embed player and its parameters.

## Guides

| Page | What it covers |
|---|---|
| [Installation](Installation.md) | requirements, build, first start, systemd, ports |
| [Configuration](Configuration.md) | `config.toml`, environment variables, every section explained |
| [OBS setup](OBS-Setup.md) | OBS Studio settings for x264 and NVENC, checking the keyframe interval |
| [Playback and embedding](Playback-and-Embedding.md) | WebRTC, HTTP-FLV, WS-FLV, LL-HLS, LL-DASH, embed parameters, JavaScript snippets |
| [Recording and waiting video](Recording-and-Waiting-Video.md) | automatic/manual recording, MP4 remux, waiting videos and texts, pause |
| [Metrics](Metrics.md) | viewers per protocol, charts, sessions, metrics API |
| [VOD and multi-bitrate](VOD-and-Multi-bitrate.md) | uploads, inbox folder, ladders, hardware encoders, VOD player and catalogue |
| [Users, API keys and buckets](Users-API-Keys-and-Buckets.md) | users, portal, storage API with curl, permissions, signed URLs |
| [Cluster mode](Cluster-Mode.md) | PostgreSQL, relays, HAProxy, compose cluster |
| [Docker](Docker.md) | image, single-node compose, volumes and environment |
| [Nginx Proxy Manager](Nginx-Proxy-Manager.md) | TLS reverse proxy settings for rms |
| [API reference](API-Reference.md) | endpoint groups and where to find the details |
| [Troubleshooting](Troubleshooting.md) | ports, firewall, OBS, ffmpeg, proxies, databases, disk space |

## Latency at a glance

| Protocol | Measured latency (localhost, built-in player) | When to use it |
|---|---|---|
| WebRTC (WHEP) | ≈ 40 ms glass-to-glass | interactivity (auctions, Q&A); needs `8443/udp` |
| HTTP-FLV / WS-FLV | ≈ 0.3–1.4 s | desktop and Android without UDP |
| LL-HLS | ≈ 1.8 s (hls.js) | iOS/Safari, CDNs, large audiences |
| LL-DASH | ≈ 2.1 s (dash.js) | DASH ecosystems |

The embed player chooses automatically (WebRTC → HTTP-FLV → LL-HLS → LL-DASH) and falls back by itself.

## Reference documents in the repository

- [README](../../README.md) ([Italian](../../README.it.md))
- [docs/API.md](../API.md) — HTTP API reference
- [docs/ARCHITECTURE.md](../ARCHITECTURE.md) — architecture, as-built notes and measurements
- [docs/ACCOUNTS.md](../ACCOUNTS.md) — users, keys, buckets, signed URLs
- [docs/PROTOCOLS.md](../PROTOCOLS.md) — low-latency protocols and OBS (Italian)
- [docs/CLUSTER.md](../CLUSTER.md) — cluster study and design (Italian)
- [deploy/nginx-proxy-manager.md](../../deploy/nginx-proxy-manager.md) — NPM guide (Italian)
