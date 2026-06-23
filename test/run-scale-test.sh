#!/usr/bin/env bash
# run-scale-test.sh — structured scale test: 8 ingest×output×encoding configs
#
# Each config creates one pipeline, publishes to it, and adds N_OUTPUTS outputs
# one by one, measuring restream RSS + ffmpeg stage processes at every step.
#
# Configs tested sequentially (default N_OUTPUTS=10):
#   RTMP-in → RTMP source ×N   RTMP-in → RTMP 720p ×N
#   RTMP-in → SRT  source ×N   RTMP-in → SRT  720p ×N
#   SRT-in  → RTMP source ×N   SRT-in  → RTMP 720p ×N
#   SRT-in  → SRT  source ×N   SRT-in  → SRT  720p ×N
#
# Key observations expected:
#   source  configs: ffmpeg#=0 throughout; ~2 MB/output (RTMP) or ~7 MB/output (SRT)
#   transcode configs: ffmpeg#=1 after out1, stays at 1; ffmpeg_rss flat
#
# Usage: N_OUTPUTS=10 ./test/run-scale-test.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

N_OUTPUTS="${N_OUTPUTS:-10}"
ISOLATE="${ISOLATE:-0}"      # set ISOLATE=1 to restart restream before each config
SNAP_EVERY="${SNAP_EVERY:-1}" # snapshot interval; SNAP_EVERY=10 records only every 10th output

# Raise fd limit — 100 RTMP egresses + ingest + HTTP + SQLite
ulimit -n 65536 2>/dev/null || true
API_URL="${API_URL:-http://127.0.0.1:3030}"
WORK_DIR="${WORK_DIR:-test/artifacts/scale-test}"
RESTREAM_BIN="${RESTREAM_BIN:-$ROOT/target/release/restream}"
RESTREAM_LOG="${WORK_DIR}/restream.log"
SCALE_LOG="${WORK_DIR}/scale.csv"
SUMMARY_LOG="${WORK_DIR}/summary.txt"
RESTREAM_PID=""
MTX_PID=""
PUB_PID=""
COOKIE_JAR=""

mkdir -p "$WORK_DIR"

RESTREAM_RTMP=1935
RESTREAM_SRT=10080
MTX_RTMP=1936
MTX_SRT=8891
MTX_API=9997

fail() { echo "FAIL: $*" >&2; exit 1; }

cleanup() {
  [[ -n "${PUB_PID:-}" ]]      && kill "$PUB_PID"      2>/dev/null || true
  [[ -n "${MTX_PID:-}" ]]      && kill "$MTX_PID"      2>/dev/null || true
  [[ -n "${RESTREAM_PID:-}" ]] && kill "$RESTREAM_PID" 2>/dev/null || true
  [[ -n "${COOKIE_JAR:-}" ]]   && rm -f "$COOKIE_JAR"
}
trap cleanup EXIT

command -v ffmpeg   >/dev/null 2>&1 || { echo "ffmpeg not found"   >&2; exit 1; }
command -v ffprobe  >/dev/null 2>&1 || { echo "ffprobe not found"  >&2; exit 1; }
command -v mediamtx >/dev/null 2>&1 || { echo "mediamtx not found" >&2; exit 1; }
command -v jq       >/dev/null 2>&1 || { echo "jq not found"       >&2; exit 1; }

# ── Start restream ─────────────────────────────────────────────────────────
start_restream() {
  local pids
  pids=$(ps -eo pid=,args= | awk '/[t]arget\/release\/restream/{print $1}' || true)
  if [[ -n "$pids" ]]; then kill $pids 2>/dev/null || true; sleep 2; fi
  rm -f "$ROOT"/data.db{,-shm,-wal} "$ROOT"/restream.db{,-shm,-wal}
  : > "$RESTREAM_LOG"
  "$RESTREAM_BIN" >"$RESTREAM_LOG" 2>&1 &
  RESTREAM_PID=$!
  for i in $(seq 1 30); do
    curl -sf "$API_URL/healthz" >/dev/null 2>&1 && return 0; sleep 1
  done
  fail "restream did not start"
}

start_mediamtx() {
  pkill -f 'mediamtx ' 2>/dev/null || true; sleep 1
  cat > "$WORK_DIR/mediamtx.yml" <<YML
logLevel: warn
rtmp: yes
rtmpAddress: :${MTX_RTMP}
srt: yes
srtAddress: :${MTX_SRT}
hls: no
webrtc: no
api: yes
apiAddress: :${MTX_API}
paths:
  all:
YML
  mediamtx "$WORK_DIR/mediamtx.yml" >"$WORK_DIR/mediamtx.log" 2>&1 &
  MTX_PID=$!
  for i in $(seq 1 20); do
    curl -sf "http://127.0.0.1:${MTX_API}/v3/paths/list" >/dev/null 2>&1 && return 0; sleep 1
  done
  fail "mediamtx did not start"
}

api() {
  local method="$1" path="$2"; shift 2
  curl -sf -X "$method" "$API_URL$path" \
    -H 'Content-Type: application/json' \
    -b "$COOKIE_JAR" -c "$COOKIE_JAR" "$@"
}

# ── Snapshot: RSS + CPU at current point ────────────────────────────────────
snapshot() {
  local cfg="$1" step="$2" label="$3"
  sleep 3
  local cpu_r rss_r
  cpu_r=$(ps -p "$RESTREAM_PID" -o %cpu= 2>/dev/null | tr -d ' \n') || cpu_r=0
  rss_r=$(ps -p "$RESTREAM_PID" -o rss=  2>/dev/null | tr -d ' \n') || rss_r=0
  cpu_r=${cpu_r:-0}; rss_r=${rss_r:-0}
  local ffmpeg_n ffmpeg_rss
  ffmpeg_n=$(ps aux | awk '/[f]fmpeg.*pipe:1/{n++} END{print n+0}')
  ffmpeg_rss=$(ps aux | awk '/[f]fmpeg.*pipe:1/{sum+=$6} END{print sum+0}')
  local total_rss=$(( ${rss_r} + ${ffmpeg_rss} ))
  printf "  %-4s %-20s cpu=%-5s rss=%-8s ffmpeg#=%-2s ffmpeg_rss=%-9s total=%s KB\n" \
    "${step}." "$label" "${cpu_r}%" "${rss_r} KB" \
    "$ffmpeg_n" "${ffmpeg_rss} KB" "$total_rss"
  echo "${cfg},${step},\"${label}\",${cpu_r},${rss_r},${ffmpeg_n},${ffmpeg_rss},${total_rss}" \
    >> "$SCALE_LOG"
}

# ── ffprobe spot-check ─────────────────────────────────────────────────────
probe_dims() {
  ffprobe -v error -probesize 5000000 -analyzeduration 5000000 \
    -select_streams v:0 -show_entries stream=width,height \
    -of csv=p=0 "$1" 2>/dev/null | tr ',' 'x' | head -n1 | tr -d '[:space:]'
}

check() {
  local label="$1" url="$2" expected="$3"
  local dims=""
  for i in $(seq 1 10); do
    dims=$(probe_dims "$url" || true)
    if [[ "$dims" == "$expected" ]]; then
      printf "    ok   %-35s → %s\n" "$label" "$dims"; return 0
    fi
    sleep 2
  done
  printf "    FAIL %-35s expected=%s got=%s\n" "$label" "$expected" "${dims:-none}"
}

# ── Run one config ──────────────────────────────────────────────────────────
run_config() {
  local cfg="$1" ingest_proto="$2" out_proto="$3" encoding="$4"
  local stream_key="sk-${cfg}"

  echo ""
  echo "══════════════════════════════════════════════════════════════════"
  printf "  %-18s  %s-ingest → %s %s ×%s outputs\n" \
    "$cfg" "$ingest_proto" "$out_proto" "$encoding" "$N_OUTPUTS"
  echo "══════════════════════════════════════════════════════════════════"

  # ISOLATE=1: restart both restream and MediaMTX so previous config's RSS and
  # accumulated SRT/RTMP path state are fully released before the next baseline.
  if [[ "${ISOLATE:-0}" == "1" ]]; then
    echo "  [isolate] restarting restream + mediamtx for clean baseline..."
    kill "$RESTREAM_PID" 2>/dev/null || true
    kill "$MTX_PID"      2>/dev/null || true
    sleep 3
    start_mediamtx
    start_restream
    rm -f "$COOKIE_JAR" 2>/dev/null || true
    COOKIE_JAR=$(mktemp)
    api POST /api/auth/login -d '{"password":"admin"}' >/dev/null
  fi

  # Create pipeline
  local pipe_id
  pipe_id=$(api POST /pipelines \
    -d "{\"name\":\"${cfg}\",\"streamKey\":\"${stream_key}\"}" | jq -r '.pipeline.id')

  # Start publisher
  if [[ "$ingest_proto" == "rtmp" ]]; then
    ffmpeg -nostdin -hide_banner -loglevel error \
      -re -f lavfi -i 'testsrc2=size=1920x1080:rate=30' \
      -f lavfi -i 'anullsrc=r=48000:cl=stereo' \
      -c:v libx264 -preset ultrafast -tune zerolatency -b:v 4M \
      -c:a aac -b:a 64k \
      -f flv "rtmp://127.0.0.1:${RESTREAM_RTMP}/live/${stream_key}" \
      >/dev/null 2>&1 &
  else
    ffmpeg -nostdin -hide_banner -loglevel error \
      -re -f lavfi -i 'testsrc2=size=1920x1080:rate=30' \
      -f lavfi -i 'anullsrc=r=48000:cl=stereo' \
      -c:v libx264 -preset ultrafast -tune zerolatency -b:v 4M \
      -c:a aac -b:a 64k \
      -f mpegts \
      "srt://127.0.0.1:${RESTREAM_SRT}?streamid=publish:live/${stream_key}&latency=200000" \
      >/dev/null 2>&1 &
  fi
  PUB_PID=$!

  # Wait for live input
  local waited=0
  for i in $(seq 1 45); do
    local json
    json=$(api GET /health 2>/dev/null || echo '{}')
    if jq -e --arg pid "$pipe_id" \
      '.pipelines[$pid].input.status == "on" and (.pipelines[$pid].input.bytesReceived // 0) > 0' \
      <<<"$json" >/dev/null 2>&1; then waited=1; break; fi
    sleep 1
  done
  [[ "$waited" == "1" ]] || fail "$cfg: ingest did not go live"

  # Baseline RSS before any outputs
  snapshot "$cfg" 0 "baseline"
  local rss_baseline
  rss_baseline=$(ps -p "$RESTREAM_PID" -o rss= 2>/dev/null | tr -d ' \n')

  # Add N_OUTPUTS outputs one by one
  local out_ids=()
  for n in $(seq 1 "$N_OUTPUTS"); do
    local url
    if [[ "$out_proto" == "rtmp" ]]; then
      url="rtmp://127.0.0.1:${MTX_RTMP}/live/${cfg}-${n}"
    else
      url="srt://127.0.0.1:${MTX_SRT}?streamid=publish:live/${cfg}-${n}"
    fi
    local out_id
    out_id=$(api POST "/pipelines/${pipe_id}/outputs" \
      -d "{\"name\":\"out${n}\",\"url\":\"${url}\",\"encoding\":\"${encoding}\"}" \
      | jq -r '.output.id')
    api POST "/pipelines/${pipe_id}/outputs/${out_id}/start" >/dev/null
    out_ids+=("$out_id")

    # Snapshot at milestone: always record out1 and every SNAP_EVERY-th output
    if (( n == 1 )) || (( n % SNAP_EVERY == 0 )); then
      snapshot "$cfg" "$n" "out${n}"
    fi
  done

  # Compute per-output RSS delta (restream process only, excluding shared ffmpeg)
  local rss_final ffmpeg_n_final ffmpeg_rss_final
  rss_final=$(ps -p "$RESTREAM_PID" -o rss= 2>/dev/null | tr -d ' \n')
  ffmpeg_n_final=$(ps aux | awk '/[f]fmpeg.*pipe:1/{n++} END{print n+0}')
  ffmpeg_rss_final=$(ps aux | awk '/[f]fmpeg.*pipe:1/{sum+=$6} END{print sum+0}')
  local rss_delta=$(( rss_final - rss_baseline ))
  local per_output=$(( rss_delta / N_OUTPUTS ))

  printf "  RESULT %-18s  restream_delta=+%-8s  per_output=~%-7s  ffmpeg#=%-2s  ffmpeg_rss=%s KB\n" \
    "$cfg" "${rss_delta} KB" "${per_output} KB" "$ffmpeg_n_final" "$ffmpeg_rss_final"
  printf "%s,rss_delta_kb=%s,per_output_kb=%s,ffmpeg_n=%s,ffmpeg_rss_kb=%s\n" \
    "$cfg" "$rss_delta" "$per_output" "$ffmpeg_n_final" "$ffmpeg_rss_final" >> "$SUMMARY_LOG"

  # ffprobe: first and last output
  echo "  spot-checks:"
  local first_url last_url expected
  if [[ "$out_proto" == "rtmp" ]]; then
    first_url="rtmp://127.0.0.1:${MTX_RTMP}/live/${cfg}-1"
    last_url="rtmp://127.0.0.1:${MTX_RTMP}/live/${cfg}-${N_OUTPUTS}"
  else
    first_url="srt://127.0.0.1:${MTX_SRT}?streamid=read:live/${cfg}-1&timeout=30000000"
    last_url="srt://127.0.0.1:${MTX_SRT}?streamid=read:live/${cfg}-${N_OUTPUTS}&timeout=30000000"
  fi
  [[ "$encoding" == "source" ]] && expected="1920x1080" || expected="1280x720"
  check "out1"            "$first_url" "$expected"
  check "out${N_OUTPUTS}" "$last_url"  "$expected"

  # Teardown: stop publisher and all outputs
  kill "$PUB_PID" 2>/dev/null || true; PUB_PID=""
  for oid in "${out_ids[@]}"; do
    api POST "/pipelines/${pipe_id}/outputs/${oid}/stop" >/dev/null 2>/dev/null || true
  done
  sleep 8  # allow 100 SRT/RTMP sockets to fully close before next config
}

# ── Main ───────────────────────────────────────────────────────────────────
printf "config,step,label,cpu_pct,rss_kb,ffmpeg_n,ffmpeg_rss_kb,total_rss_kb\n" > "$SCALE_LOG"
: > "$SUMMARY_LOG"

start_restream
start_mediamtx
COOKIE_JAR=$(mktemp)
api POST /api/auth/login -d '{"password":"admin"}' >/dev/null

run_config "rtmp-rtmp-src"  rtmp rtmp source
run_config "rtmp-rtmp-720p" rtmp rtmp 720p
run_config "rtmp-srt-src"   rtmp srt  source
run_config "rtmp-srt-720p"  rtmp srt  720p
run_config "srt-rtmp-src"   srt  rtmp source
run_config "srt-rtmp-720p"  srt  rtmp 720p
run_config "srt-srt-src"    srt  srt  source
run_config "srt-srt-720p"   srt  srt  720p

# ── Final summary table ────────────────────────────────────────────────────
echo ""
echo "══════════════════════════════════════════════════════════════════"
printf "  Summary — %s outputs per config\n" "$N_OUTPUTS"
echo "══════════════════════════════════════════════════════════════════"
printf "%-22s  %-16s  %-14s  %-9s  %s\n" \
  "config" "restream_delta" "per_output KB" "ffmpeg#" "ffmpeg_rss KB"
printf "%-22s  %-16s  %-14s  %-9s  %s\n" \
  "----------------------" "----------------" "--------------" "---------" "-------------"
while IFS=',' read -r cfg rest; do
  rss_delta=$(echo "$rest" | grep -o 'rss_delta_kb=[^,]*'  | cut -d= -f2)
  per_out=$(echo "$rest"   | grep -o 'per_output_kb=[^,]*' | cut -d= -f2)
  fn=$(echo "$rest"        | grep -o 'ffmpeg_n=[^,]*'      | cut -d= -f2)
  frss=$(echo "$rest"      | grep -o 'ffmpeg_rss_kb=[^,]*' | cut -d= -f2)
  printf "%-22s  +%-15s  %-14s  %-9s  %s\n" \
    "$cfg" "${rss_delta} KB" "${per_out} KB" "$fn" "$frss"
done < "$SUMMARY_LOG"

echo ""
echo "CSV:  $SCALE_LOG"
echo "SUMM: $SUMMARY_LOG"
