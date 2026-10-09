# Docker

The [`Dockerfile`](../../Dockerfile) builds a small runtime image in three stages: the Rust build, a static
ffmpeg/ffprobe (BtbN `linux64-gpl` build) and `debian:bookworm-slim` with `tini` and `curl`, running as the
non-root user `rms` (UID 10001) with the data volume on `/data`.

```bash
docker build -t rms:latest .
# Podman: podman build --format docker -t rms:latest .   (the OCI format drops HEALTHCHECK)
docker run -d --name rms -p 8080:8080 -p 1935:1935 -p 8443:8443/udp -v rms-data:/data \
           -e RMS_ADMIN_TOKEN=change-me rms:latest
docker logs rms | grep 'admin token'     # when RMS_ADMIN_TOKEN is not set
```

## Single-node compose

[`docker-compose.yml`](../../docker-compose.yml):

```yaml
services:
  rms:
    image: rms:latest
    build: .
    ports:
      - "8080:8080"        # HTTP: dashboard, API, player, HLS/DASH/FLV (proxy this one)
      - "1935:1935"        # RTMP ingest from OBS
      - "8443:8443/udp"    # WebRTC (WHEP) — must reach the container directly, not via the proxy
    volumes:
      - rms-data:/data
    environment:
      RMS_ADMIN_TOKEN: "change-me-to-a-long-random-string"
      RMS_SERVER_PUBLIC_URL: "https://stream.example.com"
      RMS_SERVER_TRUST_PROXY: "true"
      RMS_WEBRTC_PUBLIC_IP: "203.0.113.10"
```

```bash
docker compose up -d
docker compose logs -f rms
curl -fsS http://127.0.0.1:8080/healthz
```

- Set `RMS_ADMIN_TOKEN` to a long random string (`openssl rand -hex 24`).
- `RMS_SERVER_PUBLIC_URL` is the address viewers use (behind the proxy); `RMS_SERVER_TRUST_PROXY=true` only
  when a reverse proxy is in front.
- `RMS_WEBRTC_PUBLIC_IP` must be the public IP of the host: the container's own address is not reachable by
  browsers. If `8443/udp` cannot be exposed, set `RMS_WEBRTC_ENABLED=false` (players use FLV/HLS).
- Any configuration key can be set as `RMS_<SECTION>_<KEY>`; or mount a `config.toml` and pass
  `--config /etc/rms/config.toml` as the command.

## Volume

`/data` holds the SQLite database, recordings, waiting videos, VOD (sources and renditions), bucket files and
temporary uploads. Back it up with the container stopped (or copy `rms.sqlite` with `sqlite3 .backup`). VOD and
recordings can grow quickly: watch the free space (see [Troubleshooting](Troubleshooting.md#disk-space)).

## Health check

The image declares `HEALTHCHECK` on `GET /healthz` (`200` in service, `503` while shutting down or draining);
the compose files declare their own `healthcheck:` too, which Podman needs because its OCI images drop
`HEALTHCHECK`.

## Behind a reverse proxy and in a cluster

- [Nginx Proxy Manager](Nginx-Proxy-Manager.md) on the same compose network (proxy `rms:8080`).
- [Cluster mode](Cluster-Mode.md): `docker-compose.cluster.yml` with PostgreSQL, two nodes and HAProxy.

## Notes

- Rootless Podman: ports below 1024 need `sysctl net.ipv4.ip_unprivileged_port_start=80`; the default
  `rootlessport` forwarder hides the client IP (`pasta` keeps it), which matters for viewer IPs and WebRTC.
- The image bundles a GPL build of ffmpeg: check the licence obligations before publishing the image.
- For NVENC encoding inside the container the NVIDIA container runtime is required.
