#!/usr/bin/env bash
# run-2x3-matrix.sh — 2-ingest × 3-protocol × 2-encoding end-to-end matrix test
#
# Matrix (10 outputs across 2 pipelines):
#   2 ingest protocols : RTMP, SRT
#   3 egress protocols : RTMP → MediaMTX, SRT → MediaMTX, HLS → restream preview
#   2 encodings        : source passthrough, 720p transcode (HLS is source-only)
#
# Verification: ffprobe pulls RTMP/SRT/HLS from MediaMTX and restream to confirm
# correct resolution (1920×1080 passthrough vs 1280×720 transcode).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

API_URL="${API_URL:-http://127.0.0.1:3030}"
WORK_DIR="${WORK_DIR:-test/artifacts/2x3-matrix}"
RESTREAM_BIN="${RESTREAM_BIN:-$ROOT/target/release/restream}"
RESTREAM_LOG="${WORK_DIR}/restream.log"
MTX_PID=""
RESTREAM_PID=""
RTMP_PUBLISH_PID=""
SRT_PUBLISH_PID=""
COOKIE_JAR=""

mkdir -p "$WORK_DIR"

# ── Ports ──────────────────────────────────────────────────────────────────
RESTREAM_HTTP=3030
RESTREAM_RTMP=1935
RESTREAM_SRT=10080
MTX_RTMP=1936      # MediaMTX RTMP ingest + pull
MTX_SRT=8891       # MediaMTX SRT  ingest + pull
MTX_HLS=8890       # MediaMTX HLS  pull
MTX_API=9997

# ── Expected video dimensions ──────────────────────────────────────────────
# Publishers send 1920×1080 so passthrough ≠ transcode.
PASS_DIMS="1920x1080"   # source/passthrough output
TC_DIMS="1280x720"      # 720p transcode output

fail()   { echo "FAIL: $*" >&2; exit 1; }
log_ok() { echo "ok: $*" | tee -a "$WORK_DIR/summary.txt"; }

cleanup() {
  [[ -n "${RTMP_PUBLISH_PID:-}" ]] && kill "$RTMP_PUBLISH_PID" 2>/dev/null || true
  [[ -n "${SRT_PUBLISH_PID:-}" ]]  && kill "$SRT_PUBLISH_PID"  2>/dev/null || true
  [[ -n "${MTX_PID:-}" ]]          && kill "$MTX_PID"          2>/dev/null || true
  [[ -n "${RESTREAM_PID:-}" ]]     && kill "$RESTREAM_PID"     2>/dev/null || true
  [[ -n "${COOKIE_JAR:-}" ]]       && rm -f "$COOKIE_JAR"
}
trap cleanup EXIT

command -v ffmpeg   >/dev/null 2>&1 || { echo "ffmpeg not found"   >&2; exit 1; }
command -v ffprobe  >/dev/null 2>&1 || { echo "ffprobe not found"  >&2; exit 1; }
command -v curl     >/dev/null 2>&1 || { echo "curl not found"     >&2; exit 1; }
command -v jq       >/dev/null 2>&1 || { echo "jq not found"       >&2; exit 1; }
command -v mediamtx >/dev/null 2>&1 || { echo "mediamtx not found" >&2; exit 1; }

# ── Start restream ─────────────────────────────────────────────────────────
cleanup_existing_service() {
  local pids
  pids=$(ps -eo pid=,args= | grep '[t]arget/release/restream' | awk '{print $1}' || true)
  if [[ -n "$pids" ]]; then
    kill $pids 2>/dev/null || true
    sleep 2
  fi
}

cleanup_db() {
  rm -f "$ROOT"/data.db{,-shm,-wal} "$ROOT"/restream.db{,-shm,-wal}
}

start_restream() {
  [[ -x "$RESTREAM_BIN" ]] || fail "restream binary not found at $RESTREAM_BIN"
  cleanup_existing_service
  cleanup_db
  : > "$RESTREAM_LOG"
  "$RESTREAM_BIN" >"$RESTREAM_LOG" 2>&1 &
  RESTREAM_PID=$!
  for i in $(seq 1 30); do
    curl -sf "$API_URL/healthz" >/dev/null 2>&1 && return 0
    sleep 1
  done
  tail -50 "$RESTREAM_LOG" >&2 || true
  fail "restream did not become ready"
}

# ── Start MediaMTX ─────────────────────────────────────────────────────────
# MediaMTX is the external sink: accepts RTMP + SRT pushes and exposes them
# for ffprobe verification via RTMP, SRT, and HLS pull.
start_mediamtx() {
  pkill -f 'mediamtx ' 2>/dev/null || true; true
  sleep 1

  cat > "$WORK_DIR/mediamtx.yml" <<EOF
logLevel: warn
rtmp: yes
rtmpAddress: :${MTX_RTMP}
rtmpEncryption: "no"
rtsp: no
srt: yes
srtAddress: :${MTX_SRT}
hls: yes
hlsAddress: :${MTX_HLS}
hlsPartDuration: 200ms
hlsSegmentDuration: 2s
webrtc: no
api: yes
apiAddress: :${MTX_API}
metrics: no
paths:
  all:
EOF

  mediamtx "$WORK_DIR/mediamtx.yml" >"$WORK_DIR/mediamtx.log" 2>&1 &
  MTX_PID=$!

  for i in $(seq 1 30); do
    curl -sf "http://127.0.0.1:${MTX_API}/v3/paths/list" >/dev/null 2>&1 && return 0
    sleep 1
  done
  tail -30 "$WORK_DIR/mediamtx.log" >&2 || true
  fail "mediamtx did not become ready"
}

start_restream
start_mediamtx

COOKIE_JAR=$(mktemp)

api() {
  local method="$1" path="$2"
  shift 2
  curl -sf -X "$method" "$API_URL$path" \
    -H 'Content-Type: application/json' \
    -b "$COOKIE_JAR" -c "$COOKIE_JAR" "$@"
}

api POST /api/auth/login -d '{"password":"admin"}' >/dev/null

# ── Sanity: no leftover state ───────────────────────────────────────────────
assert_empty_state() {
  local json count
  json=$(api GET /health)
  count=$(jq '(.pipelines // {}) | length' <<<"$json")
  [[ "$count" == "0" ]] || fail "expected 0 pipelines before test, found $count"
}
assert_empty_state

# ── Create ingest pipelines ─────────────────────────────────────────────────
ensure_pipeline() {
  local name="$1" stream_key="$2"
  local existing
  existing=$(api GET /pipelines | \
    jq -r --arg k "$stream_key" '.[] | select(.streamKey==$k) | .id' | head -n1)
  [[ -n "$existing" ]] && { echo "$existing"; return; }
  api POST /pipelines \
    -d "{\"name\":\"$name\",\"streamKey\":\"$stream_key\"}" | jq -r '.pipeline.id'
}

PIPE_ID_RTMP=$(ensure_pipeline "matrix-rtmp" "mat-rtmp")
PIPE_ID_SRT=$(ensure_pipeline  "matrix-srt"  "mat-srt")

echo "pipelines: rtmp=$PIPE_ID_RTMP srt=$PIPE_ID_SRT"

# ── Create outputs ─────────────────────────────────────────────────────────
# 5 outputs per pipeline = 10 total
#
#  rtmp-pass / rtmp-720p → MediaMTX RTMP push (verified via RTMP + HLS pulls)
#  srt-pass  / srt-720p  → MediaMTX SRT  push (verified via SRT pull)
#  hls-preview           → restream internal HLS segmenter, source-only
#                          (verified via HTTP pull from restream)
create_output() {
  local pipe_id="$1" name="$2" url="$3" encoding="$4"
  api POST "/pipelines/$pipe_id/outputs" \
    -d "{\"name\":\"$name\",\"url\":\"$url\",\"encoding\":\"$encoding\"}" \
    | jq -r '.output.id'
}

start_output() {
  local pipe_id="$1" out_id="$2"
  api POST "/pipelines/$pipe_id/outputs/$out_id/start" >/dev/null
  echo "  started $out_id"
}

echo "=== creating RTMP-pipeline outputs ==="
RP_RTMP_PASS=$(create_output "$PIPE_ID_RTMP" "rtmp-pass"   "rtmp://127.0.0.1:${MTX_RTMP}/live/rp-rtmp-pass"                         "source")
RP_RTMP_720P=$(create_output "$PIPE_ID_RTMP" "rtmp-720p"   "rtmp://127.0.0.1:${MTX_RTMP}/live/rp-rtmp-720p"                         "720p")
RP_SRT_PASS=$( create_output "$PIPE_ID_RTMP" "srt-pass"    "srt://127.0.0.1:${MTX_SRT}?streamid=publish:live/rp-srt-pass"           "source")
RP_SRT_720P=$( create_output "$PIPE_ID_RTMP" "srt-720p"    "srt://127.0.0.1:${MTX_SRT}?streamid=publish:live/rp-srt-720p"           "720p")
RP_HLS=$(       create_output "$PIPE_ID_RTMP" "hls-preview" "hls://rp-hls-preview"                                                  "source")

echo "=== creating SRT-pipeline outputs ==="
SP_RTMP_PASS=$(create_output "$PIPE_ID_SRT"  "rtmp-pass"   "rtmp://127.0.0.1:${MTX_RTMP}/live/sp-rtmp-pass"                         "source")
SP_RTMP_720P=$(create_output "$PIPE_ID_SRT"  "rtmp-720p"   "rtmp://127.0.0.1:${MTX_RTMP}/live/sp-rtmp-720p"                         "720p")
SP_SRT_PASS=$( create_output "$PIPE_ID_SRT"  "srt-pass"    "srt://127.0.0.1:${MTX_SRT}?streamid=publish:live/sp-srt-pass"           "source")
SP_SRT_720P=$( create_output "$PIPE_ID_SRT"  "srt-720p"    "srt://127.0.0.1:${MTX_SRT}?streamid=publish:live/sp-srt-720p"           "720p")
SP_HLS=$(       create_output "$PIPE_ID_SRT"  "hls-preview" "hls://sp-hls-preview"                                                  "source")

echo "=== starting all outputs ==="
start_output "$PIPE_ID_RTMP" "$RP_RTMP_PASS"
start_output "$PIPE_ID_RTMP" "$RP_RTMP_720P"
start_output "$PIPE_ID_RTMP" "$RP_SRT_PASS"
start_output "$PIPE_ID_RTMP" "$RP_SRT_720P"
start_output "$PIPE_ID_RTMP" "$RP_HLS"

start_output "$PIPE_ID_SRT"  "$SP_RTMP_PASS"
start_output "$PIPE_ID_SRT"  "$SP_RTMP_720P"
start_output "$PIPE_ID_SRT"  "$SP_SRT_PASS"
start_output "$PIPE_ID_SRT"  "$SP_SRT_720P"
start_output "$PIPE_ID_SRT"  "$SP_HLS"

# ── Start publishers: 1080p so passthrough ≠ 720p transcode ───────────────
echo "=== starting publishers (1920x1080) ==="

ffmpeg -nostdin -hide_banner -loglevel error \
  -re \
  -f lavfi -i 'testsrc2=size=1920x1080:rate=30' \
  -f lavfi -i 'anullsrc=r=48000:cl=stereo' \
  -c:v libx264 -preset ultrafast -tune zerolatency -b:v 4M \
  -c:a aac -b:a 64k \
  -f flv "rtmp://127.0.0.1:${RESTREAM_RTMP}/live/mat-rtmp" \
  >/dev/null 2>&1 &
RTMP_PUBLISH_PID=$!

ffmpeg -nostdin -hide_banner -loglevel error \
  -re \
  -f lavfi -i 'testsrc2=size=1920x1080:rate=30' \
  -f lavfi -i 'anullsrc=r=48000:cl=stereo' \
  -c:v libx264 -preset ultrafast -tune zerolatency -b:v 4M \
  -c:a aac -b:a 64k \
  -f mpegts "srt://127.0.0.1:${RESTREAM_SRT}?streamid=publish:live/mat-srt&latency=200000" \
  >/dev/null 2>&1 &
SRT_PUBLISH_PID=$!

# ── Wait for live input on both pipelines ──────────────────────────────────
wait_for_input_live() {
  local pipeline_id="$1" label="$2"
  local json
  for i in $(seq 1 45); do
    json=$(api GET /health)
    if jq -e --arg pid "$pipeline_id" \
      '.pipelines[$pid].input.status == "on" and (.pipelines[$pid].input.bytesReceived // 0) > 0' \
      <<<"$json" >/dev/null 2>&1; then
      log_ok "input-live: $label"
      return 0
    fi
    sleep 1
  done
  api GET /health | jq --arg pid "$pipeline_id" '.pipelines[$pid]' >&2 || true
  fail "$label: ingest did not go live within 45s"
}

wait_for_input_live "$PIPE_ID_RTMP" "matrix-rtmp"
wait_for_input_live "$PIPE_ID_SRT"  "matrix-srt"

# Give egress clients and transcoders time to connect and buffer
sleep 8

# ── ffprobe helpers ────────────────────────────────────────────────────────
# Returns "WxH" for the first video stream, or empty string on error.
probe_dims() {
  local url="$1"
  ffprobe -v error \
    -probesize 10000000 -analyzeduration 10000000 \
    -select_streams v:0 \
    -show_entries stream=width,height \
    -of csv=p=0 \
    "$url" 2>/dev/null | tr ',' 'x' | head -n1 | tr -d '[:space:]'
}

# verify_stream LABEL URL EXPECTED_DIMS
# Retries every 2s for up to 60s.
verify_stream() {
  local label="$1" url="$2" expected="$3"
  local dims=""
  echo "  probing: $label"
  for attempt in $(seq 1 30); do
    dims=$(probe_dims "$url" || true)
    if [[ "$dims" == "$expected" ]]; then
      log_ok "ffprobe: $label → $dims"
      return 0
    fi
    [[ -n "$dims" ]] && echo "    attempt $attempt: got '$dims', want '$expected'" >&2
    sleep 2
  done
  fail "ffprobe: $label — expected $expected, got '${dims:-<no output>}' from $url"
}

# ── Verify RTMP-ingest pipeline ────────────────────────────────────────────
echo ""
echo "=== ffprobe: RTMP-pipeline outputs ==="

# RTMP pull from MediaMTX
verify_stream "rp/rtmp-pass [rtmp]"  "rtmp://127.0.0.1:${MTX_RTMP}/live/rp-rtmp-pass"  "$PASS_DIMS"
verify_stream "rp/rtmp-720p [rtmp]"  "rtmp://127.0.0.1:${MTX_RTMP}/live/rp-rtmp-720p"  "$TC_DIMS"

# SRT pull from MediaMTX (streamid format: read:live/path)
verify_stream "rp/srt-pass [srt]"    "srt://127.0.0.1:${MTX_SRT}?streamid=read:live/rp-srt-pass&timeout=15000000"   "$PASS_DIMS"
verify_stream "rp/srt-720p [srt]"    "srt://127.0.0.1:${MTX_SRT}?streamid=read:live/rp-srt-720p&timeout=15000000"   "$TC_DIMS"

# HLS pull from MediaMTX (generated from the RTMP pushes above)
verify_stream "rp/rtmp-pass [hls/mtx]"  "http://127.0.0.1:${MTX_HLS}/live/rp-rtmp-pass/index.m3u8"  "$PASS_DIMS"
verify_stream "rp/rtmp-720p [hls/mtx]"  "http://127.0.0.1:${MTX_HLS}/live/rp-rtmp-720p/index.m3u8"  "$TC_DIMS"

# HLS preview from restream's internal segmenter (source-only, one per pipeline)
verify_stream "rp/hls-preview [hls/restream]" \
  "http://127.0.0.1:${RESTREAM_HTTP}/hls/${PIPE_ID_RTMP}/index.m3u8" "$PASS_DIMS"

# ── Verify SRT-ingest pipeline ─────────────────────────────────────────────
echo ""
echo "=== ffprobe: SRT-pipeline outputs ==="

verify_stream "sp/rtmp-pass [rtmp]"  "rtmp://127.0.0.1:${MTX_RTMP}/live/sp-rtmp-pass"  "$PASS_DIMS"
verify_stream "sp/rtmp-720p [rtmp]"  "rtmp://127.0.0.1:${MTX_RTMP}/live/sp-rtmp-720p"  "$TC_DIMS"

verify_stream "sp/srt-pass [srt]"    "srt://127.0.0.1:${MTX_SRT}?streamid=read:live/sp-srt-pass&timeout=15000000"   "$PASS_DIMS"
verify_stream "sp/srt-720p [srt]"    "srt://127.0.0.1:${MTX_SRT}?streamid=read:live/sp-srt-720p&timeout=15000000"   "$TC_DIMS"

verify_stream "sp/rtmp-pass [hls/mtx]"  "http://127.0.0.1:${MTX_HLS}/live/sp-rtmp-pass/index.m3u8"  "$PASS_DIMS"
verify_stream "sp/rtmp-720p [hls/mtx]"  "http://127.0.0.1:${MTX_HLS}/live/sp-rtmp-720p/index.m3u8"  "$TC_DIMS"

verify_stream "sp/hls-preview [hls/restream]" \
  "http://127.0.0.1:${RESTREAM_HTTP}/hls/${PIPE_ID_SRT}/index.m3u8" "$PASS_DIMS"

# ── Structural counts ──────────────────────────────────────────────────────
echo ""
echo "=== asserting final counts ==="
json=$(api GET /health)
pipe_count=$(jq '(.pipelines // {}) | length' <<<"$json")
out_count=$(jq  '[.pipelines[]?.outputs // {} | .[]] | length' <<<"$json")
[[ "$pipe_count" == "2" ]]  || fail "expected 2 pipelines, found $pipe_count"
[[ "$out_count"  == "10" ]] || fail "expected 10 outputs, found $out_count"
log_ok "final-counts: pipelines=$pipe_count outputs=$out_count"

# ── External transcoder evidence ───────────────────────────────────────────
ext_launches=$(grep -c '\[external-transcoder\] Launching ffmpeg' "$RESTREAM_LOG" 2>/dev/null || true)
log_ok "external-transcoder launches: $ext_launches"

echo ""
echo "=== PASS: 2×3 matrix test complete ==="
cat "$WORK_DIR/summary.txt"
