#!/usr/bin/env bash
# run-mixed-scale-test.sh — mixed protocol/encoding scale test across 5 ingest types
#
# 5 ingest combinations × 100 mixed outputs (25 RTMP-src + 25 RTMP-720p +
#                                             25 SRT-src  + 25 SRT-720p):
#
#   h264-rtmp       H.264 RTMP ingest, single audio
#   h264-srt        H.264 SRT  ingest, single audio
#   h265-srt        H.265 SRT  ingest, single audio
#   h264-srt-multi  H.264 SRT  ingest, 2 audio tracks (720p+atrack:0 / 720p+atrack:0,1)
#   h265-srt-multi  H.265 SRT  ingest, 2 audio tracks
#
# With ISOLATE=1 both restream and mediamtx restart before each ingest type.
# Expected shared-stage counts per pipeline:
#   single-audio h264:  1 ext FFmpeg (video:720p), shared by all 50 transcoded outputs
#   single-audio h265:  1 ext FFmpeg (video:720p) + 1 int hevc_to_h264 thread
#   multi-audio  h264:  1 ext FFmpeg (video:720p) + 2 int audio-routing threads
#   multi-audio  h265:  1 ext FFmpeg (video:720p) + 1 int hevc_to_h264 + 2 int audio threads
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

N_PER_GROUP="${N_PER_GROUP:-25}"   # outputs per protocol×encoding group (×4 = total)
ISOLATE="${ISOLATE:-1}"
API_URL="${API_URL:-http://127.0.0.1:3030}"
WORK_DIR="${WORK_DIR:-test/artifacts/mixed-scale-test}"
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

ulimit -n 65536 2>/dev/null || true

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

snapshot() {
  local cfg="$1" label="$2"
  sleep 3
  local cpu_r rss_r
  cpu_r=$(ps -p "$RESTREAM_PID" -o %cpu= 2>/dev/null | tr -d ' \n') || cpu_r=0
  rss_r=$(ps -p "$RESTREAM_PID" -o rss=  2>/dev/null | tr -d ' \n') || rss_r=0
  cpu_r=${cpu_r:-0}; rss_r=${rss_r:-0}
  local ffmpeg_ext ffmpeg_ext_rss
  ffmpeg_ext=$(ps aux | awk '/[f]fmpeg.*pipe:1/{n++} END{print n+0}')
  ffmpeg_ext_rss=$(ps aux | awk '/[f]fmpeg.*pipe:1/{sum+=$6} END{print sum+0}')
  printf "  %-45s cpu=%-5s rss=%-8s ext_ffmpeg#=%-3s ext_ffmpeg_rss=%s KB\n" \
    "$label" "${cpu_r}%" "${rss_r} KB" "$ffmpeg_ext" "$ffmpeg_ext_rss"
  echo "${cfg},\"${label}\",${cpu_r},${rss_r},${ffmpeg_ext},${ffmpeg_ext_rss}" >> "$SCALE_LOG"
}

probe_dims() {
  ffprobe -v error -probesize 5000000 -analyzeduration 5000000 \
    -select_streams v:0 -show_entries stream=width,height \
    -of csv=p=0 "$1" 2>/dev/null | tr ',' 'x' | head -n1 | tr -d '[:space:]'
}

check() {
  local label="$1" url="$2" expected="$3"
  local dims=""
  for i in $(seq 1 15); do
    dims=$(probe_dims "$url" || true)
    if [[ "$dims" == "$expected" ]]; then
      printf "  ok   %-45s → %s\n" "$label" "$dims"; return 0
    fi
    sleep 2
  done
  printf "  FAIL %-45s expected=%s got=%s\n" "$label" "$expected" "${dims:-none}"
}

# ── Run one ingest configuration ────────────────────────────────────────────
run_config() {
  local cfg="$1"          # label
  local ingest_proto="$2" # rtmp | srt
  local ingest_codec="$3" # h264 | h265
  local multi_audio="$4"  # 0 | 1
  local stream_key="sk-${cfg}"
  local N="$N_PER_GROUP"
  local TOTAL=$(( N * 4 ))

  echo ""
  echo "══════════════════════════════════════════════════════════════════════════"
  printf "  %-22s  %s %s ingest%s → %s RTMP-src + %s RTMP-720p + %s SRT-src + %s SRT-720p\n" \
    "$cfg" "$ingest_proto" "$ingest_codec" \
    "$([[ $multi_audio == 1 ]] && echo ' 2-audio' || echo '')" \
    "$N" "$N" "$N" "$N"
  echo "══════════════════════════════════════════════════════════════════════════"

  if [[ "${ISOLATE:-1}" == "1" ]]; then
    echo "  [isolate] restarting restream + mediamtx..."
    kill "$RESTREAM_PID" 2>/dev/null || true
    kill "$MTX_PID"      2>/dev/null || true
    sleep 3
    start_mediamtx
    start_restream
    rm -f "$COOKIE_JAR" 2>/dev/null || true
    COOKIE_JAR=$(mktemp)
    api POST /api/auth/login -d '{"password":"admin"}' >/dev/null
  fi

  local pipe_id
  pipe_id=$(api POST /pipelines \
    -d "{\"name\":\"${cfg}\",\"streamKey\":\"${stream_key}\"}" | jq -r '.pipeline.id')

  # ── Start publisher ────────────────────────────────────────────────────────
  local pub_url
  if [[ "$ingest_proto" == "rtmp" ]]; then
    pub_url="rtmp://127.0.0.1:${RESTREAM_RTMP}/live/${stream_key}"
  else
    pub_url="srt://127.0.0.1:${RESTREAM_SRT}?streamid=publish:live/${stream_key}&latency=200000"
  fi

  local codec_args=()
  if [[ "$ingest_codec" == "h265" ]]; then
    codec_args=( -c:v libx265 -preset ultrafast -x265-params "log-level=none" )
  else
    codec_args=( -c:v libx264 -preset ultrafast -tune zerolatency )
  fi

  local map_args=()
  local audio_inputs=()
  if [[ "$multi_audio" == "1" ]]; then
    audio_inputs=( -f lavfi -i 'anullsrc=r=48000:cl=stereo' -f lavfi -i 'anullsrc=r=44100:cl=mono' )
    map_args=( -map 0:v -map 1:a -map 2:a )
  else
    audio_inputs=( -f lavfi -i 'anullsrc=r=48000:cl=stereo' )
    map_args=( -map 0:v -map 1:a )
  fi

  local fmt_args=()
  if [[ "$ingest_proto" == "rtmp" ]]; then
    fmt_args=( -f flv "$pub_url" )
  else
    fmt_args=( -f mpegts "$pub_url" )
  fi

  ffmpeg -nostdin -hide_banner -loglevel error \
    -re \
    -f lavfi -i 'testsrc2=size=1920x1080:rate=30' \
    "${audio_inputs[@]}" \
    "${codec_args[@]}" "${map_args[@]}" \
    -b:v 4M -c:a aac -b:a 64k \
    "${fmt_args[@]}" >/dev/null 2>&1 &
  PUB_PID=$!

  # Wait for live input
  local live=0
  for i in $(seq 1 45); do
    local json; json=$(api GET /health 2>/dev/null || echo '{}')
    if jq -e --arg pid "$pipe_id" \
      '.pipelines[$pid].input.status == "on" and (.pipelines[$pid].input.bytesReceived // 0) > 0' \
      <<<"$json" >/dev/null 2>&1; then live=1; break; fi
    sleep 1
  done
  [[ "$live" == "1" ]] || fail "$cfg: ingest did not go live"

  # ── Determine encodings ────────────────────────────────────────────────────
  local enc_rtmp_720p enc_srt_720p
  if [[ "$multi_audio" == "1" ]]; then
    enc_rtmp_720p="720p+atrack:0"
    enc_srt_720p="720p+atrack:0,1"
  else
    enc_rtmp_720p="720p"
    enc_srt_720p="720p"
  fi

  # ── Baseline ───────────────────────────────────────────────────────────────
  local rss_baseline
  rss_baseline=$(ps -p "$RESTREAM_PID" -o rss= 2>/dev/null | tr -d ' \n')
  snapshot "$cfg" "baseline (input live, 0 outputs)"

  # ── Create + start all outputs ─────────────────────────────────────────────
  local out_ids=()

  echo "  adding ${N} RTMP source outputs..."
  for n in $(seq 1 "$N"); do
    local oid
    oid=$(api POST "/pipelines/${pipe_id}/outputs" \
      -d "{\"name\":\"rtmp-src-${n}\",\"url\":\"rtmp://127.0.0.1:${MTX_RTMP}/live/${cfg}-rtmp-src-${n}\",\"encoding\":\"source\"}" \
      | jq -r '.output.id')
    api POST "/pipelines/${pipe_id}/outputs/${oid}/start" >/dev/null
    out_ids+=("$oid")
  done
  snapshot "$cfg" "after ${N} RTMP source"

  echo "  adding ${N} RTMP 720p outputs (enc=${enc_rtmp_720p})..."
  for n in $(seq 1 "$N"); do
    local oid
    oid=$(api POST "/pipelines/${pipe_id}/outputs" \
      -d "{\"name\":\"rtmp-720p-${n}\",\"url\":\"rtmp://127.0.0.1:${MTX_RTMP}/live/${cfg}-rtmp-720p-${n}\",\"encoding\":\"${enc_rtmp_720p}\"}" \
      | jq -r '.output.id')
    api POST "/pipelines/${pipe_id}/outputs/${oid}/start" >/dev/null
    out_ids+=("$oid")
  done
  snapshot "$cfg" "after ${N} RTMP 720p"

  echo "  adding ${N} SRT source outputs..."
  for n in $(seq 1 "$N"); do
    local oid
    oid=$(api POST "/pipelines/${pipe_id}/outputs" \
      -d "{\"name\":\"srt-src-${n}\",\"url\":\"srt://127.0.0.1:${MTX_SRT}?streamid=publish:live/${cfg}-srt-src-${n}\",\"encoding\":\"source\"}" \
      | jq -r '.output.id')
    api POST "/pipelines/${pipe_id}/outputs/${oid}/start" >/dev/null
    out_ids+=("$oid")
  done
  snapshot "$cfg" "after ${N} SRT source"

  echo "  adding ${N} SRT 720p outputs (enc=${enc_srt_720p})..."
  for n in $(seq 1 "$N"); do
    local oid
    oid=$(api POST "/pipelines/${pipe_id}/outputs" \
      -d "{\"name\":\"srt-720p-${n}\",\"url\":\"srt://127.0.0.1:${MTX_SRT}?streamid=publish:live/${cfg}-srt-720p-${n}\",\"encoding\":\"${enc_srt_720p}\"}" \
      | jq -r '.output.id')
    api POST "/pipelines/${pipe_id}/outputs/${oid}/start" >/dev/null
    out_ids+=("$oid")
  done
  snapshot "$cfg" "after all ${TOTAL} outputs"

  # ── RSS summary ────────────────────────────────────────────────────────────
  local rss_final ffmpeg_ext_n ffmpeg_ext_rss
  rss_final=$(ps -p "$RESTREAM_PID" -o rss= 2>/dev/null | tr -d ' \n')
  ffmpeg_ext_n=$(ps aux | awk '/[f]fmpeg.*pipe:1/{n++} END{print n+0}')
  ffmpeg_ext_rss=$(ps aux | awk '/[f]fmpeg.*pipe:1/{sum+=$6} END{print sum+0}')
  local rss_delta=$(( rss_final - rss_baseline ))
  local per_output=$(( rss_delta / TOTAL ))

  printf "  RESULT %-22s  restream_delta=+%-8s  per_output=~%-8s  ext_ffmpeg#=%-3s  ext_ffmpeg_rss=%s KB\n" \
    "$cfg" "${rss_delta} KB" "${per_output} KB" "$ffmpeg_ext_n" "$ffmpeg_ext_rss"
  printf "%s,rss_delta_kb=%s,per_output_kb=%s,ext_ffmpeg_n=%s,ext_ffmpeg_rss_kb=%s\n" \
    "$cfg" "$rss_delta" "$per_output" "$ffmpeg_ext_n" "$ffmpeg_ext_rss" >> "$SUMMARY_LOG"

  # ── ffprobe spot-checks ────────────────────────────────────────────────────
  echo "  spot-checks:"
  local srt_tout="&timeout=30000000"
  check "RTMP-src  out${N}"   "rtmp://127.0.0.1:${MTX_RTMP}/live/${cfg}-rtmp-src-${N}"    "1920x1080"
  check "RTMP-720p out${N}"   "rtmp://127.0.0.1:${MTX_RTMP}/live/${cfg}-rtmp-720p-${N}"   "1280x720"
  check "SRT-src   out${N}"   "srt://127.0.0.1:${MTX_SRT}?streamid=read:live/${cfg}-srt-src-${N}${srt_tout}"   "1920x1080"
  check "SRT-720p  out${N}"   "srt://127.0.0.1:${MTX_SRT}?streamid=read:live/${cfg}-srt-720p-${N}${srt_tout}"  "1280x720"

  # ── Teardown ───────────────────────────────────────────────────────────────
  kill "$PUB_PID" 2>/dev/null || true; PUB_PID=""
  for oid in "${out_ids[@]}"; do
    api POST "/pipelines/${pipe_id}/outputs/${oid}/stop" >/dev/null 2>/dev/null || true
  done
  sleep 8
}

# ── Main ───────────────────────────────────────────────────────────────────
printf "config,label,cpu_pct,rss_kb,ext_ffmpeg_n,ext_ffmpeg_rss_kb\n" > "$SCALE_LOG"
: > "$SUMMARY_LOG"

start_restream
start_mediamtx
COOKIE_JAR=$(mktemp)
api POST /api/auth/login -d '{"password":"admin"}' >/dev/null

run_config "h264-rtmp"       rtmp h264 0
run_config "h264-srt"        srt  h264 0
run_config "h265-srt"        srt  h265 0
run_config "h264-srt-multi"  srt  h264 1
run_config "h265-srt-multi"  srt  h265 1

# ── Final summary ──────────────────────────────────────────────────────────
echo ""
echo "══════════════════════════════════════════════════════════════════════════"
printf "  Summary — %s outputs per group (%s total per ingest)\n" "$N_PER_GROUP" "$(( N_PER_GROUP * 4 ))"
echo "══════════════════════════════════════════════════════════════════════════"
printf "%-24s  %-16s  %-14s  %-12s  %s\n" \
  "config" "restream_delta" "per_output KB" "ext_ffmpeg#" "ext_ffmpeg_rss KB"
printf "%-24s  %-16s  %-14s  %-12s  %s\n" \
  "------------------------" "----------------" "--------------" "------------" "-----------------"
while IFS=',' read -r cfg rest; do
  d=$(echo "$rest"   | grep -o 'rss_delta_kb=[^,]*'   | cut -d= -f2)
  p=$(echo "$rest"   | grep -o 'per_output_kb=[^,]*'  | cut -d= -f2)
  n=$(echo "$rest"   | grep -o 'ext_ffmpeg_n=[^,]*'   | cut -d= -f2)
  r=$(echo "$rest"   | grep -o 'ext_ffmpeg_rss_kb=[^,]*' | cut -d= -f2)
  printf "%-24s  +%-15s  %-14s  %-12s  %s\n" "$cfg" "${d} KB" "${p} KB" "$n" "$r"
done < "$SUMMARY_LOG"

echo ""
echo "CSV:  $SCALE_LOG"
echo "SUMM: $SUMMARY_LOG"
