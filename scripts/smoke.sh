#!/usr/bin/env bash
# curl-based smoke test of the rms HTTP surface (no publisher needed).
#
# Usage: scripts/smoke.sh [base_url] [admin_token]
#   base_url defaults to http://127.0.0.1:8080
#   admin_token defaults to $RMS_ADMIN_TOKEN, then to the contents of ${DATA_DIR:-./data}/admin_token
# Creates a temporary broadcast "smoke-<random>", exercises the API and deletes it.
set -euo pipefail

BASE="${1:-http://127.0.0.1:8080}"
TOKEN="${2:-${RMS_ADMIN_TOKEN:-}}"
if [[ -z "$TOKEN" ]]; then
  TOKEN_FILE="${DATA_DIR:-./data}/admin_token"
  [[ -r "$TOKEN_FILE" ]] || { echo "no admin token: pass it as 2nd argument or set RMS_ADMIN_TOKEN / DATA_DIR" >&2; exit 2; }
  TOKEN="$(tr -d '[:space:]' < "$TOKEN_FILE")"
fi
command -v curl >/dev/null || { echo "curl is required" >&2; exit 2; }

PASS=0; FAIL=0
AUTH=(-H "Authorization: Bearer $TOKEN")
NAME="smoke-$RANDOM$RANDOM"
TMP="$(mktemp)"; trap 'rm -f "$TMP"' EXIT

# expect <description> <expected_status> <curl args...>; body saved in $TMP
expect() {
  local desc="$1" want="$2"; shift 2
  local got
  got="$(curl -s -o "$TMP" -w '%{http_code}' --max-time 10 "$@" || true)"
  if [[ "$got" == "$want" ]]; then
    PASS=$((PASS + 1)); printf '  ok   %-58s %s\n' "$desc" "$got"
  else
    FAIL=$((FAIL + 1)); printf '  FAIL %-58s got %s want %s\n' "$desc" "$got" "$want"; head -c 300 "$TMP"; echo
  fi
}
# contains <description> <substring>  (checks the last response body)
contains() {
  if grep -q -- "$2" "$TMP"; then PASS=$((PASS + 1)); printf '  ok   %s\n' "$1"
  else FAIL=$((FAIL + 1)); printf '  FAIL %s (missing %s)\n' "$1" "$2"; head -c 300 "$TMP"; echo; fi
}
not_contains() {
  if grep -q -- "$2" "$TMP"; then FAIL=$((FAIL + 1)); printf '  FAIL %s (found %s)\n' "$1" "$2"
  else PASS=$((PASS + 1)); printf '  ok   %s\n' "$1"; fi
}

echo "rms smoke test against $BASE (broadcast $NAME)"
expect "auth check without token -> 401" 401 "$BASE/api/v1/auth/check"
contains "401 body is JSON error" '"error"'
expect "auth check with bearer -> 204" 204 "${AUTH[@]}" "$BASE/api/v1/auth/check"
expect "login with wrong token -> 401" 401 -H 'Content-Type: application/json' -d '{"token":"nope"}' "$BASE/api/v1/auth/login"
expect "login with token -> 204 (cookie)" 204 -c "$TMP.cookies" -H 'Content-Type: application/json' -d "{\"token\":\"$TOKEN\"}" "$BASE/api/v1/auth/login"
expect "cookie auth -> 204" 204 -b "$TMP.cookies" "$BASE/api/v1/auth/check"
rm -f "$TMP.cookies"
expect "admin API without token -> 401" 401 "$BASE/api/v1/broadcasts"
expect "GET /system" 200 "${AUTH[@]}" "$BASE/api/v1/system"
contains "system has version" '"version"'
expect "GET /broadcasts" 200 "${AUTH[@]}" "$BASE/api/v1/broadcasts"
expect "POST /broadcasts invalid name -> 400" 400 "${AUTH[@]}" -H 'Content-Type: application/json' -d '{"title":"x","name":"Bad Name"}' "$BASE/api/v1/broadcasts"
expect "POST /broadcasts missing title -> 400" 400 "${AUTH[@]}" -H 'Content-Type: application/json' -d '{"name":"x"}' "$BASE/api/v1/broadcasts"
expect "POST /broadcasts -> 201" 201 "${AUTH[@]}" -H 'Content-Type: application/json' -d "{\"title\":\"Smoke test\",\"name\":\"$NAME\"}" "$BASE/api/v1/broadcasts"
contains "view has status offline" '"status":"offline"'
contains "view has stream_key" '"stream_key"'
contains "view has OBS server" '"server":"rtmp://'
contains "view has flv url" "/live/$NAME.flv"
expect "POST duplicate -> 409" 409 "${AUTH[@]}" -H 'Content-Type: application/json' -d "{\"title\":\"Smoke test\",\"name\":\"$NAME\"}" "$BASE/api/v1/broadcasts"
expect "GET /broadcasts/{name}" 200 "${AUTH[@]}" "$BASE/api/v1/broadcasts/$NAME"
expect "GET unknown broadcast -> 404" 404 "${AUTH[@]}" "$BASE/api/v1/broadcasts/does-not-exist-$RANDOM"
expect "PATCH title" 200 "${AUTH[@]}" -X PATCH -H 'Content-Type: application/json' -d '{"title":"Smoke 2","enable_dash":false}' "$BASE/api/v1/broadcasts/$NAME"
contains "PATCH applied" '"title":"Smoke 2"'
expect "PATCH name -> 400" 400 "${AUTH[@]}" -X PATCH -H 'Content-Type: application/json' -d '{"name":"other"}' "$BASE/api/v1/broadcasts/$NAME"
expect "pause" 200 "${AUTH[@]}" -X POST "$BASE/api/v1/broadcasts/$NAME/pause"
contains "status paused" '"status":"paused"'
expect "public info while paused" 200 "$BASE/api/v1/public/broadcasts/$NAME"
contains "public status paused" '"status":"paused"'
not_contains "public info hides stream key" 'stream_key'
contains "public dash source null when disabled" '"dash":null'
expect "resume" 200 "${AUTH[@]}" -X POST "$BASE/api/v1/broadcasts/$NAME/resume"
contains "status offline" '"status":"offline"'
expect "regenerate key" 200 "${AUTH[@]}" -X POST "$BASE/api/v1/broadcasts/$NAME/regenerate-key"
expect "disconnect when offline -> 409" 409 "${AUTH[@]}" -X POST "$BASE/api/v1/broadcasts/$NAME/disconnect"
expect "embed info" 200 "${AUTH[@]}" "$BASE/api/v1/broadcasts/$NAME/embed"
contains "embed has iframe" '<iframe'
contains "embed has mpegts snippet" 'mpegts'
expect "sessions (empty)" 200 "${AUTH[@]}" "$BASE/api/v1/broadcasts/$NAME/sessions"
expect "metrics overview" 200 "${AUTH[@]}" "$BASE/api/v1/metrics/overview"
expect "metrics time series" 200 "${AUTH[@]}" "$BASE/api/v1/broadcasts/$NAME/metrics"
expect "recordings list" 200 "${AUTH[@]}" "$BASE/api/v1/recordings"
expect "record/start when offline -> 409" 409 "${AUTH[@]}" -X POST "$BASE/api/v1/broadcasts/$NAME/record/start"
expect "GET /settings" 200 "${AUTH[@]}" "$BASE/api/v1/settings"
contains "settings has default waiting text" '"default_waiting_text"'
contains "settings has upload limit" '"max_upload_mb"'
expect "waiting video upload without file -> 400" 400 "${AUTH[@]}" -F "note=x" "$BASE/api/v1/broadcasts/$NAME/waiting-video"
expect "waiting video upload of a non-video -> 415" 415 "${AUTH[@]}" -F "file=@$0;type=text/x-shellscript" "$BASE/api/v1/broadcasts/$NAME/waiting-video"
expect "DELETE waiting video (none set)" 200 "${AUTH[@]}" -X DELETE "$BASE/api/v1/broadcasts/$NAME/waiting-video"
expect "HTTP-FLV offline -> 404" 404 "$BASE/live/$NAME.flv"
contains "offline error message" 'stream offline'
expect "HTTP-FLV unknown -> 404" 404 "$BASE/live/does-not-exist-$RANDOM.flv"
expect "dashboard page" 200 "$BASE/"
expect "embed page" 200 "$BASE/embed/$NAME"
contains "embed page has name" "data-name=\"$NAME\""
expect "player page" 200 "$BASE/player/$NAME"
expect "static asset" 200 "$BASE/static/player.js"
expect "public SSE first event" 200 --max-time 2 "$BASE/api/v1/public/broadcasts/$NAME/events" || true
expect "DELETE broadcast -> 204" 204 "${AUTH[@]}" -X DELETE "$BASE/api/v1/broadcasts/$NAME"
expect "GET deleted -> 404" 404 "${AUTH[@]}" "$BASE/api/v1/broadcasts/$NAME"

echo "passed: $PASS  failed: $FAIL"
[[ "$FAIL" -eq 0 ]]
