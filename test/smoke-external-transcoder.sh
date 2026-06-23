#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

command -v ffmpeg >/dev/null 2>&1 || { echo "ffmpeg not found" >&2; exit 1; }
command -v mediamtx >/dev/null 2>&1 || { echo "mediamtx not found" >&2; exit 1; }

WORK_DIR="${WORK_DIR:-/tmp/restream-external-smoke}"
mkdir -p "$WORK_DIR"
rm -f "$WORK_DIR"/restream.log "$WORK_DIR"/mediamtx.yml

cat > "$WORK_DIR/mediamtx.yml" <<'EOF'
logLevel: warn
api: false
metrics: false
pprof: false
rtsp: false
hls: false
webrtc: false
srt: false
rtmp: true
rtmpAddress: :11937
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

mediamtx "$WORK_DIR/mediamtx.yml" >/dev/null 2>&1 &
MEDIAMTX_PID=$!
trap 'kill "$MEDIAMTX_PID" 2>/dev/null || true' EXIT

sleep 2

cargo build --release >/dev/null
cp target/release/restream "$WORK_DIR/restream"

RESTREAM_HTTP_PORT=13031 \
RESTREAM_RTMP_PORT=11935 \
RESTREAM_SRT_PORT=11081 \
"$WORK_DIR/restream" > "$WORK_DIR/restream.log" 2>&1 &
RESTREAM_PID=$!
trap 'kill "$RESTREAM_PID" "$MEDIAMTX_PID" 2>/dev/null || true' EXIT

for i in $(seq 1 30); do
  if curl -sf http://127.0.0.1:13031/healthz >/dev/null 2>&1; then break; fi
  sleep 1
done

curl -sf -X POST http://127.0.0.1:13031/api/auth/login -d '{"password":"admin"}' >/dev/null
PIPE_ID=$(curl -sf -X POST http://127.0.0.1:13031/pipelines \
  -H 'Content-Type: application/json' \
  -d '{"name":"external-smoke","streamKey":"external-smoke"}' | jq -r '.pipeline.id')

ffmpeg -nostdin -hide_banner -loglevel error -re -f lavfi -i 'testsrc2=size=1280x720:rate=30' -f flv -c:v libx264 -c:a aac -b:a 128k -y 'rtmp://127.0.0.1:11935/live/external-smoke' >/dev/null 2>&1 &
FFMPEG_PID=$!
trap 'kill "$FFMPEG_PID" "$RESTREAM_PID" "$MEDIAMTX_PID" 2>/dev/null || true' EXIT

sleep 5

OUTPUT_ID=$(curl -sf -X POST "http://127.0.0.1:13031/pipelines/${PIPE_ID}/outputs" \
  -H 'Content-Type: application/json' \
  -d '{"name":"smoke-pass","url":"rtmp://127.0.0.1:11937/live/smoke-pass","encoding":"source"}' | jq -r '.output.id')
curl -sf -X POST "http://127.0.0.1:13031/pipelines/${PIPE_ID}/outputs/${OUTPUT_ID}/start" >/dev/null

sleep 3

if grep -q "external-transcoder" "$WORK_DIR/restream.log" 2>/dev/null; then
  echo "unexpected external transcoder usage for passthrough output" >&2
  exit 1
fi

echo "passthrough smoke ok"
