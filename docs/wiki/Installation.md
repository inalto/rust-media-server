# Installation

rms is one binary (`rms`) with the dashboard and the players embedded. It stores its state in an embedded
SQLite database under the data directory; `ffmpeg` is optional but recommended.

## Requirements

| What | Notes |
|---|---|
| OS | Linux x86_64 (tested on AlmaLinux 9) |
| Rust | stable, edition 2024, **≥ 1.89** to build (developed with 1.99); not needed at runtime |
| ffmpeg + ffprobe | optional, in `PATH` or `ffmpeg.path`; static builds are fine. Used for: MP4 remux of recordings, waiting-video normalization, AAC → Opus audio for WebRTC; **required for VOD** (with `ffprobe`) |
| Ports | `8080/tcp` HTTP (dashboard, API, players, FLV/HLS/DASH, WHEP signalling) · `1935/tcp` RTMP · `8443/udp` WebRTC media (one port for every viewer) |

Without ffmpeg rms still ingests, delivers and records (FLV), but WebRTC sessions are video-only, uploaded
waiting videos must already be MP4 and video on demand is unavailable. The dashboard (*Settings → System*)
shows whether ffmpeg was found.

## Build from source

```bash
git clone <repository-url> rust-media-server
cd rust-media-server
cargo build --release
./target/release/rms --help
```

```
Usage: rms [OPTIONS]

Options:
      --config <FILE>     Configuration file (default: ./config.toml if present)
      --data-dir <DIR>    Data directory (database, recordings, waiting videos, admin token)
      --http-bind <ADDR>  HTTP listen address, e.g. 0.0.0.0:8080
      --rtmp-bind <ADDR>  RTMP listen address, e.g. 0.0.0.0:1935
  -h, --help              Print help
  -V, --version           Print version
```

## First start and login

```bash
./target/release/rms --data-dir ./data
```

On the first start rms creates `data/` (SQLite database `rms.sqlite`, recordings, waiting videos, VOD,
buckets) and generates an **admin token**: it is printed once in the log
(`generated a new admin token: …`) and saved in `data/admin_token` (mode 0600). To choose it yourself set
`RMS_ADMIN_TOKEN` or `server.admin_token` in the configuration (mandatory and identical on every node in
cluster mode).

Open `http://<host>:8080/`, paste the token and sign in. Users created by the administrator sign in on the
same page with an API key that has the *Account* scope (see [Users, API keys and buckets](Users-API-Keys-and-Buckets.md)).

![Sign-in page](../screenshots/login.png)

Then:

1. *Settings → Language and theme*: English or Italian, dark, light or system theme.
2. *Live streams → New live stream*: a title (the URL name is derived from it) and options.
3. Configure OBS with the values of the *OBS settings* panel: [OBS setup](OBS-Setup.md).
4. Copy the embed code from *Links and embed*: [Playback and embedding](Playback-and-Embedding.md).

## Try it without OBS

`scripts/test-publish.sh` publishes an ffmpeg test pattern (720p30 H.264 + 440 Hz AAC, 2 s GOP, burned-in
clock when `drawtext` is available):

```bash
./target/release/rms --data-dir /var/tmp/rms-data --http-bind 127.0.0.1:8080 --rtmp-bind 127.0.0.1:1935 &
TOKEN=$(cat /var/tmp/rms-data/admin_token)
curl -s -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
     -d '{"title":"Test"}' http://127.0.0.1:8080/api/v1/broadcasts      # note "stream_key"
scripts/test-publish.sh <stream_key> 127.0.0.1:1935                      # DURATION=60 to stop after 60 s
DATA_DIR=/var/tmp/rms-data scripts/smoke.sh http://127.0.0.1:8080        # curl smoke test
```

Open `http://127.0.0.1:8080/player/test`.

## Run as a systemd service

[`deploy/rms.service`](../../deploy/rms.service) runs rms as the `rms` user with its data in `/var/lib/rms`:

```bash
sudo cp target/release/rms /usr/local/bin/rms
sudo useradd -r -s /sbin/nologin -d /var/lib/rms rms && sudo mkdir -p /var/lib/rms /etc/rms
sudo cp config.example.toml /etc/rms/config.toml && sudo chown -R rms:rms /var/lib/rms
sudo cp deploy/rms.service /etc/systemd/system/ && sudo systemctl enable --now rms
journalctl -u rms | grep 'admin token'      # first start: the generated token
```

Without root: copy the unit to `~/.config/systemd/user/`, drop `User`/`Group` and use `systemctl --user`.

## Firewall

```bash
sudo firewall-cmd --permanent --add-port=8080/tcp --add-port=1935/tcp --add-port=8443/udp
sudo firewall-cmd --reload
```

Behind a TLS reverse proxy only the proxy needs `80/443`; keep `1935/tcp` and `8443/udp` open on the rms
host (they are not proxied). Behind NAT set `webrtc.public_ip` to the public address and forward
`8443/udp` (see [Troubleshooting](Troubleshooting.md#webrtc-does-not-connect)).

## Other ways to run it

- [Docker](Docker.md): `docker build -t rms:latest .` and `docker compose up -d`.
- [Cluster mode](Cluster-Mode.md): several nodes, PostgreSQL and HAProxy.
- Behind [Nginx Proxy Manager](Nginx-Proxy-Manager.md), nginx or Caddy for TLS.

## Health check

`GET /healthz` (no authentication) answers `200` with
`{"node_id","cluster","db","live_streams","version","draining"}` while the server is in service (also when the
database is down: `"db": false`) and `503` while it shuts down or drains.
