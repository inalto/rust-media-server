#!/usr/bin/env bash
# Publish an ffmpeg test pattern (720p30 H.264 + 440 Hz AAC, 2 s GOP) to rms.
#
# Usage: scripts/test-publish.sh <stream_key> [host[:port]] [app]
#   host defaults to localhost:1935, app to "live".
# Env: DURATION=<secs> stops after that many seconds (default: run until killed).
set -euo pipefail

if [[ $# -lt 1 || -z "${1:-}" ]]; then
  echo "usage: $0 <stream_key> [host[:port]] [app]" >&2
  exit 2
fi
KEY="$1"
HOST="${2:-localhost:1935}"
APP="${3:-live}"
[[ "$HOST" == *:* ]] || HOST="$HOST:1935"
URL="rtmp://$HOST/$APP/$KEY"

FFMPEG="${FFMPEG:-$(command -v ffmpeg || true)}"
[[ -n "$FFMPEG" ]] || { echo "ffmpeg not found in PATH (set FFMPEG=...)" >&2; exit 1; }

# Burned-in wall clock timecode (useful to eyeball latency) when drawtext is available.
VF=()
if "$FFMPEG" -hide_banner -filters 2>/dev/null | grep -q ' drawtext '; then
  VF=(-vf "drawtext=text='%{localtime\:%H\\\\\:%M\\\\\:%S}':fontsize=64:fontcolor=white:box=1:boxcolor=black@0.6:boxborderw=12:x=(w-tw)/2:y=h-th-40")
fi

LIMIT=()
[[ -n "${DURATION:-}" ]] && LIMIT=(-t "$DURATION")

echo "publishing test pattern to rtmp://$HOST/$APP/${KEY:0:4}…" >&2
exec "$FFMPEG" -hide_banner -loglevel "${LOGLEVEL:-warning}" -re \
  -f lavfi -i testsrc2=size=1280x720:rate=30 \
  -f lavfi -i sine=frequency=440:sample_rate=48000 \
  "${VF[@]}" \
  -c:v libx264 -preset veryfast -tune zerolatency -g 60 -keyint_min 60 -sc_threshold 0 \
  -b:v 2500k -maxrate 2500k -bufsize 5000k -pix_fmt yuv420p \
  -c:a aac -b:a 128k -ar 48000 \
  "${LIMIT[@]}" \
  -f flv "$URL"
