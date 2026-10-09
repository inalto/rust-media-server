# syntax=docker/dockerfile:1
# rust-media-server (rms) — multi-stage image: Rust builder + static ffmpeg + slim runtime.
# Build:  docker build -t rms:latest .
#         podman build --format docker -t rms:latest .   (OCI format drops HEALTHCHECK; compose has its own)
# Run:    docker run -d --name rms -p 8080:8080 -p 1935:1935 -p 8443:8443/udp -v rms-data:/data rms:latest

FROM docker.io/library/rust:1-bookworm AS builder
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
# Strip debug info for a small image (the repo's release profile keeps line tables for local builds).
ENV CARGO_PROFILE_RELEASE_DEBUG=0 CARGO_PROFILE_RELEASE_STRIP=true
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked && install -m755 target/release/rms /usr/local/bin/rms

FROM docker.io/library/debian:bookworm-slim AS ffmpeg
ARG FFMPEG_URL=https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-master-latest-linux64-gpl.tar.xz
RUN apt-get update && apt-get install -y --no-install-recommends curl xz-utils ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && curl -sSL "$FFMPEG_URL" -o /tmp/ffmpeg.tar.xz \
 && mkdir -p /tmp/ff && tar -xJf /tmp/ffmpeg.tar.xz -C /tmp/ff --strip-components=1 \
 && install -m755 /tmp/ff/bin/ffmpeg /tmp/ff/bin/ffprobe /usr/local/bin/

FROM docker.io/library/debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl tini \
 && rm -rf /var/lib/apt/lists/* \
 && useradd -r -u 10001 -d /data -s /usr/sbin/nologin rms \
 && mkdir -p /data && chown rms:rms /data
COPY --from=builder /usr/local/bin/rms /usr/local/bin/rms
COPY --from=ffmpeg /usr/local/bin/ffmpeg /usr/local/bin/ffprobe /usr/local/bin/
USER rms
WORKDIR /data
VOLUME ["/data"]
EXPOSE 8080/tcp 1935/tcp 8443/udp
# Every config key can be set as RMS_<SECTION>_<KEY>; see config.example.toml.
ENV RMS_SERVER_DATA_DIR=/data \
    RMS_SERVER_HTTP_BIND=0.0.0.0:8080 \
    RMS_RTMP_BIND=0.0.0.0:1935 \
    RMS_WEBRTC_UDP_PORT=8443 \
    RUST_LOG=info
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
  CMD curl -fsS http://127.0.0.1:8080/healthz >/dev/null || exit 1
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/rms"]
