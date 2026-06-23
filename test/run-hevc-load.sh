#!/usr/bin/env bash
#
# H.265 load test: 1 SRT ingest (H.265 4K60 + 16 audio) → 100 RTMP outputs
#
# Starts the real restream binary + mediamtx, publishes via ffmpeg,
# adds RTMP outputs through the API one at a time, measuring resource
# impact at each step.
#
# Usage:
#   ./test/run-hevc-load.sh
#   EGRESS_COUNT=200 ./test/run-hevc-load.sh
#
set -euo pipefail

EGRESS_COUNT="${EGRESS_COUNT:-100}"
HTTP_PORT="${HTTP_PORT:-13030}"
RTMP_PORT="${RTMP_PORT:-11935}"
SRT_PORT="${SRT_PORT:-11080}"
MEDIAMTX_RTMP_PORT="${MEDIAMTX_RTMP_PORT:-11936}"
AUDIO_SOURCE="${AUDIO_SOURCE:-media/colorbar-timer.mp4}"
WORK_DIR="${WORK_DIR:-/tmp/restream-hevc-load}"
KEEP_RUNNING="${KEEP_RUNNING:-0}"

cleanup() {
    if [[ "$KEEP_RUNNING" == "1" ]]; then
        echo ""
        echo "== KEEP_RUNNING=1: leaving services alive =="
        echo "  restream:  pid=$RESTREAM_PID  dashboard: http://localhost:${HTTP_PORT}"
        echo "  mediamtx:  pid=$MEDIAMTX_PID  rtmp:     ${MEDIAMTX_RTMP_PORT}"
        echo "  ffmpeg:    pid=$FFMPEG_PID"
        echo "  log:       $WORK_DIR/restream.log"
        echo "  csv:       $WORK_DIR/snapshots.csv"
        return
    fi
    [[ -n "${FFMPEG_PID:-}" ]] && kill "$FFMPEG_PID" 2>/dev/null || true
    [[ -n "${RESTREAM_PID:-}" ]] && kill "$RESTREAM_PID" 2>/dev/null || true
    [[ -n "${MEDIAMTX_PID:-}" ]] && kill "$MEDIAMTX_PID" 2>/dev/null || true
}
trap cleanup EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }

command -v ffmpeg >/dev/null 2>&1 || fail "ffmpeg not found"
command -v curl >/dev/null 2>&1 || fail "curl not found"
command -v jq >/dev/null 2>&1 || fail "jq not found"
command -v mediamtx >/dev/null 2>&1 || fail "mediamtx not found"
[[ -f "$AUDIO_SOURCE" ]] || fail "Audio source not found: $AUDIO_SOURCE"

mkdir -p "$WORK_DIR"
rm -f "$WORK_DIR/data.db" "$WORK_DIR/restream.log" "$WORK_DIR/snapshots.csv"

# ---- mediamtx (RTMP sink) ----
cat > "$WORK_DIR/mediamtx.yml" <<EOF
logLevel: warn
api: false
metrics: false
pprof: false
rtsp: false
hls: false
webrtc: false
srt: false
rtmp: true
rtmpAddress: :${MEDIAMTX_RTMP_PORT}
authInternalUsers:
- user: any
  pass:
  permissions:
  - action: publish
  - action: read
  - action: playback
paths:
  all:
EOF

echo "== Starting mediamtx on RTMP port ${MEDIAMTX_RTMP_PORT} =="
mediamtx "$WORK_DIR/mediamtx.yml" >/dev/null 2>&1 &
MEDIAMTX_PID=$!
sleep 2

# ---- restream binary ----
echo "== Starting restream (HTTP=${HTTP_PORT} RTMP=${RTMP_PORT} SRT=${SRT_PORT}) =="
# Run restream from WORK_DIR so it uses a clean data.db
cp ./target/release/restream "$WORK_DIR/restream" 2>/dev/null || true
cd "$WORK_DIR"
RESTREAM_HTTP_PORT="$HTTP_PORT" \
RESTREAM_RTMP_PORT="$RTMP_PORT" \
RESTREAM_SRT_PORT="$SRT_PORT" \
./restream > restream.log 2>&1 &
RESTREAM_PID=$!
cd - >/dev/null

for i in $(seq 1 30); do
    if curl -sf "http://localhost:${HTTP_PORT}/healthz" >/dev/null 2>&1; then break; fi
    if [[ $i -eq 30 ]]; then fail "restream not reachable"; fi
    sleep 1
done
echo "restream is up (pid=$RESTREAM_PID)"

API_URL="http://localhost:${HTTP_PORT}"
COOKIE_JAR=$(mktemp)
SNAPSHOTS="$WORK_DIR/snapshots.csv"
echo "phase,egress_count,rss_kb,threads" > "$SNAPSHOTS"

api() {
    local method="$1" path="$2"
    shift 2
    curl -sf -X "$method" "$API_URL$path" \
        -H "Content-Type: application/json" \
        -b "$COOKIE_JAR" -c "$COOKIE_JAR" "$@"
}

snapshot() {
    local phase="$1" egress_count="$2"
    local rss_kb threads
    rss_kb=$(awk '/^VmRSS:/{print $2}' /proc/$RESTREAM_PID/status)
    threads=$(awk '/^Threads:/{print $2}' /proc/$RESTREAM_PID/status)
    echo "${phase},${egress_count},${rss_kb},${threads}" >> "$SNAPSHOTS"
    printf "  %-22s egress=%-3d  rss=%8s kB  threads=%s\n" "$phase" "$egress_count" "$rss_kb" "$threads"
}

add_output() {
    local i="$1"
    local url="rtmp://127.0.0.1:${MEDIAMTX_RTMP_PORT}/live/hevc-out-${i}"
    local out_id
    out_id=$(api POST "/pipelines/${PIPE_ID}/outputs" \
        -d "{\"name\":\"load-${i}\",\"url\":\"${url}\",\"encoding\":\"source\"}" \
        | jq -r '.output.id')
    api POST "/pipelines/${PIPE_ID}/outputs/${out_id}/start" >/dev/null
}

# ---- Login ----
api POST /api/auth/login -d '{"password":"admin"}' >/dev/null
echo "Logged in"

# ---- Create pipeline ----
PIPE_ID=$(api POST /pipelines \
    -d '{"name":"H.265 Load Source","streamKey":"hevc-load"}' \
    | jq -r '.pipeline.id')
echo "Created pipeline: $PIPE_ID"

# ---- Baseline: no input, no outputs ----
echo "== Resource tracking =="
snapshot "baseline_no_input" 0

# ---- Start ffmpeg publishing H.265 to SRT ----
echo "== Starting ffmpeg: 4K60 H.265 + 16 audio tracks → SRT =="
FFMPEG_ARGS=(
    -nostdin -hide_banner -loglevel error
    -re
    -f lavfi -i "testsrc2=size=3840x2160:rate=60"
    -stream_loop -1 -i "$AUDIO_SOURCE"
    -map 0:v
)
for i in $(seq 0 15); do
    FFMPEG_ARGS+=(-map "1:a:${i}")
done
FFMPEG_ARGS+=(
    -c:v libx265 -preset fast -g 120 -bf 0
    -x265-params log-level=error
    -c:a copy
    -f mpegts
    "srt://127.0.0.1:${SRT_PORT}?streamid=publish:live/hevc-load&pkt_size=1316"
)
ffmpeg "${FFMPEG_ARGS[@]}" >/dev/null 2>&1 &
FFMPEG_PID=$!

# Wait for ingest
echo "Waiting for SRT ingest..."
for i in $(seq 1 60); do
    if grep -q "Probed video: hevc" "$WORK_DIR/restream.log" 2>/dev/null; then break; fi
    sleep 1
done
echo "SRT ingest established"

# Wait 60s for source ring buffer to fill
echo "Waiting 60s for source ring buffer to fill..."
sleep 60
snapshot "input_60s" 0

# Add 1 egress, wait 60s
echo "== Add 1 egress, wait 60s =="
add_output 0
sleep 60
snapshot "egress_1_60s" 1

# Add 2 egresses, wait 60s
echo "== Add 2 egresses, wait 60s =="
for i in 1 2; do add_output $i; done
sleep 60
snapshot "egress_3_60s" 3

# Add 8 egresses, wait 10s
echo "== Add 8 egresses, wait 10s =="
for i in 3 4 5 6 7 8 9 10; do add_output $i; done
sleep 10
snapshot "egress_11_10s" 11

# Add remaining in batches of 10, wait 10s between
REMAINING=$((EGRESS_COUNT - 11))
echo "== Add ${REMAINING} egresses in batches of 10, 10s between =="
for batch_start in $(seq 11 10 $((EGRESS_COUNT - 1))); do
    batch_end=$((batch_start + 10))
    if [[ $batch_end -gt $EGRESS_COUNT ]]; then batch_end=$EGRESS_COUNT; fi
    for i in $(seq $batch_start $((batch_end - 1))); do
        add_output $i
    done
    sleep 10
    snapshot "batch_${batch_end}" $batch_end
    echo "  ... ${batch_end}/${EGRESS_COUNT} outputs"
done

# ---- Final stabilization ----
sleep 5
snapshot "stable" $EGRESS_COUNT

# ---- Summary ----
echo ""
echo "== Summary =="

BASELINE_RSS=$(awk -F, 'NR==2{print $3}' "$SNAPSHOTS")            # baseline_no_input
INPUT_RSS=$(awk -F, 'NR==3{print $3}' "$SNAPSHOTS")               # input_60s
ONE_EGRESS_RSS=$(awk -F, 'NR==4{print $3}' "$SNAPSHOTS")          # egress_1_60s
THREE_EGRESS_RSS=$(awk -F, 'NR==5{print $3}' "$SNAPSHOTS")        # egress_3_60s
ELEVEN_EGRESS_RSS=$(awk -F, 'NR==6{print $3}' "$SNAPSHOTS")       # egress_11_10s
FINAL_RSS=$(tail -1 "$SNAPSHOTS" | cut -d, -f3)

echo "  baseline (no input):       ${BASELINE_RSS} kB"
echo "  input (60s):               ${INPUT_RSS} kB  (delta: $((INPUT_RSS - BASELINE_RSS)) kB)"
echo "  +1 egress (60s):           ${ONE_EGRESS_RSS} kB  (delta: $((ONE_EGRESS_RSS - INPUT_RSS)) kB)"
echo "  +2 more = 3 (60s):         ${THREE_EGRESS_RSS} kB  (delta: $((THREE_EGRESS_RSS - ONE_EGRESS_RSS)) kB, $(( (THREE_EGRESS_RSS - ONE_EGRESS_RSS) / 2 )) kB/egress)"
echo "  +8 more = 11 (10s):        ${ELEVEN_EGRESS_RSS} kB  (delta: $((ELEVEN_EGRESS_RSS - THREE_EGRESS_RSS)) kB, $(( (ELEVEN_EGRESS_RSS - THREE_EGRESS_RSS) / 8 )) kB/egress)"
echo "  ${EGRESS_COUNT} outputs (final):    ${FINAL_RSS} kB  (delta from 11: $((FINAL_RSS - ELEVEN_EGRESS_RSS)) kB, $(( (FINAL_RSS - ELEVEN_EGRESS_RSS) / (EGRESS_COUNT - 11) )) kB/egress)"

TC_SPAWNS=$(grep -c '\[h264-tc\] Spawning' "$WORK_DIR/restream.log" 2>/dev/null || true)
TC_SPAWNS=${TC_SPAWNS:-0}

echo "  transcoder spawns:         ${TC_SPAWNS}"
echo "  shared:                    $([[ "$TC_SPAWNS" -eq 1 ]] && echo 'YES (1 encoder)' || echo "NO (${TC_SPAWNS} encoders)")"

echo ""
echo "CSV: $SNAPSHOTS"
echo "Log: $WORK_DIR/restream.log"

if [[ "$TC_SPAWNS" -eq 1 ]]; then
    echo ""
    echo "PASS: 1 shared encoder for ${EGRESS_COUNT} outputs"
    exit 0
else
    echo ""
    echo "FAIL: expected 1 transcoder, got ${TC_SPAWNS}"
    exit 1
fi
