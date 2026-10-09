# Cluster mode

With `[cluster] enabled = true` several rms nodes share a **PostgreSQL** database instead of SQLite and behave
as one server: every node accepts any stream key and serves any live stream. Without `[cluster]` nothing
changes (SQLite, single node). The design study, measurements and limits are in
[docs/CLUSTER.md](../CLUSTER.md) (Italian); internal routes in [docs/API.md](../API.md) §4.8.

```
Internet ──443──▶ Nginx Proxy Manager (TLS, HTTP/2) ──▶ HAProxy :8080 ──▶ rms1:8080 | rms2:8080
OBS ─────1935/tcp──▶ HAProxy :1935 ──▶ any node (it becomes the owner of the stream)
Browser ─8443/udp, 8444/udp──────────▶ rms1, rms2 (WebRTC, directly, one UDP port per node)
rms1 ◀──── internal network :8081 (/internal/*: relay, kick, status, files) ────▶ rms2
rms1, rms2 ──▶ PostgreSQL (state, sessions, metrics, jobs; LISTEN/NOTIFY for events)
```

## How it works

- **The publisher's node owns the stream.** The node receiving the RTMP connection registers the stream (with
  a heartbeat). If the same key arrives on another node (e.g. OBS reconnecting through the load balancer), the
  new node disconnects the old publisher and takes the stream over in about 0.3 s.
- **Relays.** A viewer on another node (FLV, WS-FLV, LL-HLS, LL-DASH or WebRTC) makes that node pull the stream
  from the owner over HTTP-FLV on the internal network — one relay per stream and node, stopped
  `cluster.relay_idle_secs` after the last local viewer. Measured added delay on a LAN: < 1 ms.
- **Shared state and events.** Live status, viewers (summed over the nodes), dashboards (SSE), settings and VOD
  are updated on every node through PostgreSQL `LISTEN/NOTIFY`.
- **Recordings** are written by the owner; downloads work from any node. The **VOD** encoding queue is shared
  (leases per node). Every response carries `X-Rms-Node`.
- **Failures.** When a node dies its streams go offline after `cluster.node_ttl_secs` (15 s) and OBS reconnects
  to another node. `POST /internal/drain` or `SIGTERM` takes a node out of the pool without cutting the
  publishers already connected (`/healthz` answers `503` while draining).

## Requirements

- PostgreSQL (tested with 16).
- On every node: a unique `cluster.node_id`, `cluster.advertise_url` (internal URL reachable by the other
  nodes, e.g. `http://rms1:8081`), the same `cluster.internal_secret` (≥ 32 characters) and the same
  `server.admin_token` (mandatory: `data/admin_token` is not used in cluster mode).
- A shared volume for `data/` (same host, or NFS across hosts) for recordings, waiting videos and VOD; bucket
  files need a shared `storage.root`. Never put SQLite on NFS — in cluster mode the database is PostgreSQL.
- The internal listener (`cluster.internal_bind`, port 8081) must never be exposed to the Internet.

```toml
[server]
admin_token = "same-long-random-token-on-every-node"
public_url = "https://stream.example.com"
trust_proxy = true

[cluster]
enabled = true
node_id = "rms1"
database_url = "postgres://rms:PASSWORD@db:5432/rms"
advertise_url = "http://rms1:8081"
internal_secret = "at-least-32-random-characters-identical-everywhere"

[webrtc]
udp_port = 8443          # 8444 on rms2, ...
public_port = 8443
public_ip = ["203.0.113.10"]
```

## Ready-made compose file

[`docker-compose.cluster.yml`](../../docker-compose.cluster.yml): PostgreSQL 16, two nodes `rms1`/`rms2` from
the same image and HAProxy 2.9 with [`deploy/haproxy.cfg`](../../deploy/haproxy.cfg).

```bash
docker build -t rms:latest .                     # podman build --format docker -t rms:latest .
export RMS_ADMIN_TOKEN="$(openssl rand -hex 24)" PG_PASSWORD="$(openssl rand -hex 16)" \
       RMS_INTERNAL_SECRET="$(openssl rand -hex 32)" PUBLIC_IP=203.0.113.10 PUBLIC_URL=https://stream.example.com
docker compose -f docker-compose.cluster.yml up -d   # podman-compose -f docker-compose.cluster.yml up -d
curl -s -D - http://127.0.0.1:8080/healthz | grep -i x-rms-node    # which node answered
```

Required variables: `RMS_ADMIN_TOKEN`, `PG_PASSWORD`, `RMS_INTERNAL_SECRET`. Optional: `PUBLIC_URL`,
`PUBLIC_IP`, `RMS_IMAGE` and the published ports (`LB_HTTP_PORT`, `LB_RTMP_PORT`, `LB_STATS_PORT`,
`RMS1_WEBRTC_PORT`, `RMS2_WEBRTC_PORT`). On hosts whose routes cover the private ranges (e.g. a `10.0.0.0/8`
LAN) set an explicit subnet with the commented `networks:` block at the end of the file.

| Port | Where | Use |
|---|---|---|
| 8080/tcp | HAProxy (private, for NPM) | dashboard, API, players, FLV, HLS, DASH, WHEP signalling |
| 1935/tcp | HAProxy | RTMP from OBS (to any node) |
| 8404/tcp | HAProxy (private) | statistics (`/stats`) |
| 8443/udp, 8444/udp | rms1, rms2 directly | WebRTC: one UDP port per node, never through a proxy |
| 8081/tcp | internal network | `/internal/*` between nodes, protected by the secret |
| 5432/tcp | internal network | PostgreSQL |

## HAProxy

[`deploy/haproxy.cfg`](../../deploy/haproxy.cfg):

- HTTP on `:8080`, round robin with a `/healthz` health check (a draining node leaves the pool);
- `/hls/` and `/dash/` hashed on `X-Real-IP`: the same viewer stays on the same node, because every node
  packages its own LL-HLS/LL-DASH timeline;
- `/whep/` to the nodes with reachable UDP; `/internal/` blocked;
- RTMP in TCP mode on `:1935` (`balance first`, health-checked through `/healthz`);
- statistics on `:8404/stats`; long timeouts (1 h) for FLV, WS-FLV, SSE and blocking playlist requests.

**Nginx Proxy Manager in front**: NPM does TLS and HTTP/2 and forwards to HAProxy (`lb:8080`); NPM alone cannot
balance between nodes. See [Nginx Proxy Manager](Nginx-Proxy-Manager.md) and
[deploy/nginx-proxy-manager.md §10](../../deploy/nginx-proxy-manager.md).

## WebRTC in a cluster

Each node needs its own reachable UDP port (`webrtc.udp_port`, `webrtc.public_port`, `webrtc.public_ip`). A
node without usable WebRTC (`webrtc.enabled = false`) answers `POST /whep` with `307` to the `cluster.whep_url`
of another node (e.g. `https://rms1.example.com/whep`). With a single host name for all nodes, route `/whep/`
only to the WebRTC nodes (backend `rms_whep`).

## Limits

- LL-HLS/LL-DASH timelines are per node: the load balancer needs per-IP affinity; if a viewer's node dies the
  player reloads from another node (new timeline).
- A stream fails over only after `cluster.node_ttl_secs`.
- Without shared storage only recordings can be read from other nodes (not waiting videos, VOD or bucket files).
- Relayed streams restart (viewers reconnect) when the upstream connection ends.
