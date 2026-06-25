#!/usr/bin/env bash
# run-integration.sh — unified integration test runner
#
# Usage:
#   ./test/run-integration.sh [--host] <mode>
#
# By default every mode that manages its own server processes runs inside a
# private loopback network namespace (unshare --net) so ports never conflict
# with anything on the host.  Pass --host to skip the namespace wrapper.
#
# Modes:
#   ramp        8 ingest×egress×encoding configs, outputs added one-by-one, per-step RSS/CPU snapshots
#   mixed-scale 4 configs (h264-srt anchor: HLS+smoke+lifecycle; h265-srt: TC_SPAWNS; multi-audio ×2)
#   bonding     SRT broadcast+backup bonding (requires static build)
#
# Common env overrides (all modes):
#   RESTREAM_BIN   path to restream binary (default: target/release/restream)
#   WORK_DIR       artifact directory      (default: test/artifacts/<mode>)
#   RESTREAM_HTTP/RTMP/SRT  port overrides
#   MTX_RTMP/SRT/HLS/API    mediamtx port overrides
#
# Mode-specific env overrides are documented inside each run_* function.
set -euo pipefail

# ── Argument parsing ───────────────────────────────────────────────────────────
HOST_NETWORK=0
FILTERED_ARGS=()
for _arg in "$@"; do
  [[ "$_arg" == "--host" ]] && HOST_NETWORK=1 || FILTERED_ARGS+=("$_arg")
done
MODE="${FILTERED_ARGS[0]:-}"
if [[ -z "$MODE" ]]; then
  grep '^#   [a-z]' "$0" | sed 's/^#   /  /' >&2
  echo "Usage: $0 [--host] <mode>" >&2
  exit 1
fi

# ── Network namespace self-reexec ──────────────────────────────────────────────
# bonding uses its own static binaries with random ports and needs the host
# network.  All other modes start their own servers and benefit from a private
# namespace.  Skip if --host was given or we are already inside netns.
if [[ "$HOST_NETWORK" == "0" && "${_IN_NETNS:-0}" != "1" ]]; then
  case "$MODE" in
    bonding) ;;
    *)
      export _IN_NETNS=1
      exec unshare --net --user --map-root-user \
        bash -c '
          set -euo pipefail
          ip link set lo up
          echo "[netns] loopback: $(ip addr show lo | awk "/inet /{print \$2}")"
          exec "$@"
        ' -- \
        bash "$0" "$MODE"
      ;;
  esac
fi

# ── Roots ──────────────────────────────────────────────────────────────────────
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

RESTREAM_BIN="${RESTREAM_BIN:-$ROOT/target/release/restream}"

# ── Port defaults (each mode may override before calling start_*) ──────────────
RESTREAM_HTTP="${RESTREAM_HTTP:-3030}"
RESTREAM_RTMP="${RESTREAM_RTMP:-1935}"
RESTREAM_SRT="${RESTREAM_SRT:-10080}"
MTX_RTMP="${MTX_RTMP:-1936}"
MTX_SRT="${MTX_SRT:-8891}"
MTX_HLS="${MTX_HLS:-8890}"
MTX_API="${MTX_API:-9997}"

API_URL="http://127.0.0.1:${RESTREAM_HTTP}"

# ── Global process/file handles ────────────────────────────────────────────────
RESTREAM_PID=""
MTX_PID=""
PUB_PID=""            # single publisher (scale, mixed-scale, hevc-load, smoke)
PUB_PIDS=()           # multiple publishers (matrix)
COOKIE_JAR=""
WORK_DIR="${WORK_DIR:-test/artifacts/${MODE}}"
RESTREAM_LOG="${WORK_DIR}/restream.log"
SCALE_LOG="/dev/null"
SUMMARY_LOG="/dev/null"
SNAPSHOTS="/dev/null"

# ── Shared helpers ─────────────────────────────────────────────────────────────

fail()   { echo "FAIL: $*" >&2; exit 1; }
log_ok() { echo "ok: $*" | tee -a "${WORK_DIR}/summary.txt"; }

cleanup() {
  [[ ${#PUB_PIDS[@]} -gt 0 ]] && { for p in "${PUB_PIDS[@]}"; do kill "$p" 2>/dev/null || true; done; }
  [[ -n "$PUB_PID" ]]      && kill "$PUB_PID"      2>/dev/null || true
  [[ -n "$MTX_PID" ]]      && kill "$MTX_PID"      2>/dev/null || true
  [[ -n "$RESTREAM_PID" ]] && kill "$RESTREAM_PID" 2>/dev/null || true
  [[ -n "$COOKIE_JAR" ]]   && rm -f "$COOKIE_JAR"  || true
}
trap cleanup EXIT

check_deps() {
  for cmd in "$@"; do
    command -v "$cmd" >/dev/null 2>&1 || { echo "${cmd} not found" >&2; exit 1; }
  done
}

cleanup_restream_procs() {
  local pids
  pids=$(ps -eo pid=,comm= | awk '$2 == "restream" {print $1}' || true)
  [[ -n "$pids" ]] && { kill -9 $pids 2>/dev/null || true; sleep 3; }
}

cleanup_db() {
  rm -f "$ROOT"/data.db{,-shm,-wal} "$ROOT"/restream.db{,-shm,-wal}
}

start_restream() {
  [[ -x "$RESTREAM_BIN" ]] || fail "restream binary not found at $RESTREAM_BIN"
  cleanup_restream_procs
  cleanup_db
  : > "$RESTREAM_LOG"
  API_URL="http://127.0.0.1:${RESTREAM_HTTP}"
  RESTREAM_HTTP_PORT="$RESTREAM_HTTP" \
  RESTREAM_RTMP_PORT="$RESTREAM_RTMP" \
  RESTREAM_SRT_PORT="$RESTREAM_SRT"  \
  "$RESTREAM_BIN" >"$RESTREAM_LOG" 2>&1 &
  RESTREAM_PID=$!
  for i in $(seq 1 30); do
    curl -sf "$API_URL/healthz" >/dev/null 2>&1 && return 0; sleep 1
  done
  tail -50 "$RESTREAM_LOG" >&2 || true
  fail "restream did not become ready"
}

# Full mediamtx: RTMP + SRT + optional HLS + API (ramp, mixed-scale)
# Env: MTX_HLS_ENABLED=yes|no (default no), MTX_LOG_LEVEL=warn|info (default warn)
start_mediamtx() {
  local hls_enabled="${MTX_HLS_ENABLED:-no}"
  local log_level="${MTX_LOG_LEVEL:-warn}"
  pkill -f 'mediamtx ' 2>/dev/null || true; sleep 1
  {
    cat <<YML
logLevel: ${log_level}
rtmp: yes
rtmpAddress: :${MTX_RTMP}
rtmpEncryption: "no"
rtsp: no
srt: yes
srtAddress: :${MTX_SRT}
hls: ${hls_enabled}
YML
    if [[ "$hls_enabled" == "yes" ]]; then
      printf "hlsAddress: :%s\nhlsPartDuration: 200ms\nhlsSegmentDuration: 2s\n" "$MTX_HLS"
    fi
    cat <<YML
webrtc: no
api: yes
apiAddress: :${MTX_API}
metrics: no
paths:
  all:
YML
  } > "${WORK_DIR}/mediamtx.yml"
  mediamtx "${WORK_DIR}/mediamtx.yml" >"${WORK_DIR}/mediamtx.log" 2>&1 &
  MTX_PID=$!
  for i in $(seq 1 30); do
    curl -sf "http://127.0.0.1:${MTX_API}/v3/paths/list" >/dev/null 2>&1 && return 0; sleep 1
  done
  tail -30 "${WORK_DIR}/mediamtx.log" >&2 || true
  fail "mediamtx did not become ready"
}


api() {
  local method="$1" path="$2"; shift 2
  curl -sf -X "$method" "$API_URL$path" \
    -H 'Content-Type: application/json' \
    -b "$COOKIE_JAR" -c "$COOKIE_JAR" "$@"
}

probe_dims() {
  ffprobe -v error \
    -probesize 10000000 -analyzeduration 10000000 \
    -select_streams v:0 -show_entries stream=width,height \
    -of csv=p=0 "$1" 2>/dev/null | tr ',' 'x' | head -n1 | tr -d '[:space:]'
}

# verify_stream: fatal on timeout; 30 × 2 s = 60 s max
verify_stream() {
  local label="$1" url="$2" expected="$3"
  local dims=""
  echo "  probing: $label"
  for attempt in $(seq 1 30); do
    dims=$(probe_dims "$url" || true)
    if [[ "$dims" == "$expected" ]]; then log_ok "ffprobe: $label → $dims"; return 0; fi
    [[ -n "$dims" ]] && echo "    attempt $attempt: got '$dims', want '$expected'" >&2
    sleep 2
  done
  fail "ffprobe: $label — expected $expected, got '${dims:-<no output>}' from $url"
}

# check_stream: non-fatal (prints FAIL); retries × 2 s
check_stream() {
  local label="$1" url="$2" expected="$3" retries="${4:-15}"
  local dims=""
  for i in $(seq 1 "$retries"); do
    dims=$(probe_dims "$url" || true)
    if [[ "$dims" == "$expected" ]]; then
      printf "  ok   %-45s → %s\n" "$label" "$dims"; return 0
    fi
    sleep 2
  done
  printf "  FAIL %-45s expected=%s got=%s\n" "$label" "$expected" "${dims:-none}"
}

wait_for_input_live() {
  local pipeline_id="$1" label="$2"
  for i in $(seq 1 45); do
    local json; json=$(api GET /health 2>/dev/null || echo '{}')
    if jq -e --arg pid "$pipeline_id" \
      '.pipelines[$pid].input.status == "on" and (.pipelines[$pid].input.bytesReceived // 0) > 0' \
      <<<"$json" >/dev/null 2>&1; then
      echo "  input live: $label"; return 0
    fi
    sleep 1
  done
  api GET /health | jq --arg pid "$pipeline_id" '.pipelines[$pid]' >&2 || true
  fail "$label: ingest did not go live within 45 s"
}

wait_srt_ready() {
  for i in $(seq 1 10); do
    if timeout 1 bash -c "echo '' | nc -u -q1 127.0.0.1 ${RESTREAM_SRT}" 2>/dev/null \
       || nc -z 127.0.0.1 "${RESTREAM_SRT}" 2>/dev/null; then
      return 0
    fi
    sleep 1
  done
  return 0  # best-effort; SRT UDP may not respond to nc
}

# snapshot_scale: CSV row + human line for scale mode
# Args: cfg step label
snapshot_scale() {
  local cfg="$1" step="$2" label="$3"
  sleep 3
  local cpu rss
  cpu=$(ps -p "$RESTREAM_PID" -o %cpu= 2>/dev/null | tr -d ' \n') || cpu=0
  rss=$(ps -p "$RESTREAM_PID" -o rss=  2>/dev/null | tr -d ' \n') || rss=0
  cpu=${cpu:-0}; rss=${rss:-0}
  local ffmpeg_n ffmpeg_rss
  ffmpeg_n=$(ps aux | awk '/[f]fmpeg.*pipe:1/{n++} END{print n+0}')
  ffmpeg_rss=$(ps aux | awk '/[f]fmpeg.*pipe:1/{sum+=$6} END{print sum+0}')
  local total_rss=$(( rss + ffmpeg_rss ))
  printf "  %-4s %-20s cpu=%-5s rss=%-8s ffmpeg#=%-2s ffmpeg_rss=%-9s total=%s KB\n" \
    "${step}." "$label" "${cpu}%" "${rss} KB" "$ffmpeg_n" "${ffmpeg_rss} KB" "$total_rss"
  echo "${cfg},${step},\"${label}\",${cpu},${rss},${ffmpeg_n},${ffmpeg_rss},${total_rss}" \
    >> "$SCALE_LOG"
}

# snapshot_mixed: CSV row + human line for mixed-scale mode
# Args: cfg label
snapshot_mixed() {
  local cfg="$1" label="$2"
  sleep 3
  local cpu rss
  cpu=$(ps -p "$RESTREAM_PID" -o %cpu= 2>/dev/null | tr -d ' \n') || cpu=0
  rss=$(ps -p "$RESTREAM_PID" -o rss=  2>/dev/null | tr -d ' \n') || rss=0
  cpu=${cpu:-0}; rss=${rss:-0}
  local ffmpeg_ext ffmpeg_ext_rss
  ffmpeg_ext=$(ps aux | awk '/[f]fmpeg.*pipe:1/{n++} END{print n+0}')
  ffmpeg_ext_rss=$(ps aux | awk '/[f]fmpeg.*pipe:1/{sum+=$6} END{print sum+0}')
  printf "  %-45s cpu=%-5s rss=%-8s ext_ffmpeg#=%-3s ext_ffmpeg_rss=%s KB\n" \
    "$label" "${cpu}%" "${rss} KB" "$ffmpeg_ext" "$ffmpeg_ext_rss"
  echo "${cfg},\"${label}\",${cpu},${rss},${ffmpeg_ext},${ffmpeg_ext_rss}" >> "$SCALE_LOG"
}

# snapshot_proc: CSV row + human line for hevc-load mode (reads /proc)
# Args: phase egress_count
snapshot_proc() {
  local phase="$1" egress_count="$2"
  local rss_kb threads
  rss_kb=$(awk '/^VmRSS:/{print $2}' /proc/"$RESTREAM_PID"/status)
  threads=$(awk '/^Threads:/{print $2}' /proc/"$RESTREAM_PID"/status)
  echo "${phase},${egress_count},${rss_kb},${threads}" >> "$SNAPSHOTS"
  printf "  %-22s egress=%-3d  rss=%8s kB  threads=%s\n" "$phase" "$egress_count" "$rss_kb" "$threads"
}


# ── Mode: ramp ─────────────────────────────────────────────────────────────────
# 8 configs (2 ingests × 2 egress protocols × 2 encodings), outputs added one-by-one.
# Per-step RSS+CPU+ffmpeg snapshots; ffprobe spot-checks first and last output.
# Env: N_OUTPUTS (default 10), ISOLATE=1 (restart per config), SNAP_EVERY (default 1)
run_ramp() {
  check_deps ffmpeg ffprobe curl jq mediamtx

  local N_OUTPUTS="${N_OUTPUTS:-10}"
  local ISOLATE="${ISOLATE:-0}"
  local SNAP_EVERY="${SNAP_EVERY:-1}"

  ulimit -n 65536 2>/dev/null || true

  WORK_DIR="${WORK_DIR:-test/artifacts/scale}"
  RESTREAM_LOG="${WORK_DIR}/restream.log"
  SCALE_LOG="${WORK_DIR}/scale.csv"
  SUMMARY_LOG="${WORK_DIR}/summary.txt"
  mkdir -p "$WORK_DIR"

  printf "config,step,label,cpu_pct,rss_kb,ffmpeg_n,ffmpeg_rss_kb,total_rss_kb\n" > "$SCALE_LOG"
  : > "$SUMMARY_LOG"

  start_restream
  start_mediamtx
  COOKIE_JAR=$(mktemp)
  api POST /api/auth/login -d '{"password":"admin"}' >/dev/null

  run_scale_config() {
    local cfg="$1" ingest_proto="$2" out_proto="$3" encoding="$4"
    local stream_key="sk-${cfg}"

    echo ""
    echo "══════════════════════════════════════════════════════════════════"
    printf "  %-18s  %s-ingest → %s %s ×%s outputs\n" \
      "$cfg" "$ingest_proto" "$out_proto" "$encoding" "$N_OUTPUTS"
    echo "══════════════════════════════════════════════════════════════════"

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

    local pipe_id
    pipe_id=$(api POST /pipelines \
      -d "{\"name\":\"${cfg}\",\"streamKey\":\"${stream_key}\"}" | jq -r '.pipeline.id')

    if [[ "$ingest_proto" == "rtmp" ]]; then
      ffmpeg -nostdin -hide_banner -loglevel error -re \
        -f lavfi -i 'testsrc2=size=1920x1080:rate=30' \
        -f lavfi -i 'anullsrc=r=48000:cl=stereo' \
        -c:v libx264 -preset ultrafast -tune zerolatency -b:v 4M -c:a aac -b:a 64k \
        -f flv "rtmp://127.0.0.1:${RESTREAM_RTMP}/live/${stream_key}" >/dev/null 2>&1 &
    else
      ffmpeg -nostdin -hide_banner -loglevel error -re \
        -f lavfi -i 'testsrc2=size=1920x1080:rate=30' \
        -f lavfi -i 'anullsrc=r=48000:cl=stereo' \
        -c:v libx264 -preset ultrafast -tune zerolatency -b:v 4M -c:a aac -b:a 64k \
        -f mpegts "srt://127.0.0.1:${RESTREAM_SRT}?streamid=publish:live/${stream_key}&latency=200000" \
        >/dev/null 2>&1 &
    fi
    PUB_PID=$!

    wait_for_input_live "$pipe_id" "$cfg"
    snapshot_scale "$cfg" 0 "baseline"
    local rss_baseline; rss_baseline=$(ps -p "$RESTREAM_PID" -o rss= 2>/dev/null | tr -d ' \n')

    local out_ids=()
    for n in $(seq 1 "$N_OUTPUTS"); do
      local url out_id
      if [[ "$out_proto" == "rtmp" ]]; then
        url="rtmp://127.0.0.1:${MTX_RTMP}/live/${cfg}-${n}"
      else
        url="srt://127.0.0.1:${MTX_SRT}?streamid=publish:live/${cfg}-${n}"
      fi
      out_id=$(api POST "/pipelines/${pipe_id}/outputs" \
        -d "{\"name\":\"out${n}\",\"url\":\"${url}\",\"encoding\":\"${encoding}\"}" \
        | jq -r '.output.id')
      api POST "/pipelines/${pipe_id}/outputs/${out_id}/start" >/dev/null
      out_ids+=("$out_id")
      if (( n == 1 )) || (( n % SNAP_EVERY == 0 )); then
        snapshot_scale "$cfg" "$n" "out${n}"
      fi
    done

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
    check_stream "out1"            "$first_url" "$expected" 10
    check_stream "out${N_OUTPUTS}" "$last_url"  "$expected" 10

    kill "$PUB_PID" 2>/dev/null || true; PUB_PID=""
    for oid in "${out_ids[@]}"; do
      api POST "/pipelines/${pipe_id}/outputs/${oid}/stop" >/dev/null 2>/dev/null || true
    done
    sleep 8
  }

  run_scale_config "rtmp-rtmp-src"  rtmp rtmp source
  run_scale_config "rtmp-rtmp-720p" rtmp rtmp 720p
  run_scale_config "rtmp-srt-src"   rtmp srt  source
  run_scale_config "rtmp-srt-720p"  rtmp srt  720p
  run_scale_config "srt-rtmp-src"   srt  rtmp source
  run_scale_config "srt-rtmp-720p"  srt  rtmp 720p
  run_scale_config "srt-srt-src"    srt  srt  source
  run_scale_config "srt-srt-720p"   srt  srt  720p

  echo ""
  echo "══════════════════════════════════════════════════════════════════"
  printf "  Summary — %s outputs per config\n" "$N_OUTPUTS"
  echo "══════════════════════════════════════════════════════════════════"
  printf "%-22s  %-16s  %-14s  %-9s  %s\n" config restream_delta "per_output KB" "ffmpeg#" "ffmpeg_rss KB"
  printf "%-22s  %-16s  %-14s  %-9s  %s\n" "----------------------" "----------------" "--------------" "---------" "-------------"
  while IFS=',' read -r cfg rest; do
    local d p n r
    d=$(echo "$rest" | grep -o 'rss_delta_kb=[^,]*'  | cut -d= -f2)
    p=$(echo "$rest" | grep -o 'per_output_kb=[^,]*' | cut -d= -f2)
    n=$(echo "$rest" | grep -o 'ffmpeg_n=[^,]*'      | cut -d= -f2)
    r=$(echo "$rest" | grep -o 'ffmpeg_rss_kb=[^,]*' | cut -d= -f2)
    printf "%-22s  +%-15s  %-14s  %-9s  %s\n" "$cfg" "${d} KB" "${p} KB" "$n" "$r"
  done < "$SUMMARY_LOG"

  echo ""; echo "CSV:  $SCALE_LOG"; echo "SUMM: $SUMMARY_LOG"
}

# ── Mode: mixed-scale ──────────────────────────────────────────────────────────
# 4 configs × N outputs per group (RTMP-src + RTMP-720p + SRT-src + SRT-720p).
# h264-srt (anchor): HLS output + smoke check (no ext transcoder before 720p) +
#   fatal verify_stream across all protocol×encoding combos + stop lifecycle.
# h265-srt: asserts exactly 1 shared internal h264-tc transcoder (TC_SPAWNS=1).
# h264-srt-multi / h265-srt-multi: multi-audio track routing.
# Env: N_PER_GROUP (default 25), ISOLATE=1 (default)
run_mixed_scale() {
  check_deps ffmpeg ffprobe curl jq mediamtx

  local N_PER_GROUP="${N_PER_GROUP:-25}"
  local ISOLATE="${ISOLATE:-1}"

  ulimit -n 65536 2>/dev/null || true

  WORK_DIR="${WORK_DIR:-test/artifacts/mixed-scale}"
  RESTREAM_LOG="${WORK_DIR}/restream.log"
  SCALE_LOG="${WORK_DIR}/scale.csv"
  SUMMARY_LOG="${WORK_DIR}/summary.txt"
  mkdir -p "$WORK_DIR"

  # RSS_SUMMARY is separate from SUMMARY_LOG so log_ok() "ok: ..." lines don't
  # pollute the CSV that the final summary table reads back.
  local RSS_SUMMARY="${WORK_DIR}/rss-summary.csv"
  printf "config,label,cpu_pct,rss_kb,ext_ffmpeg_n,ext_ffmpeg_rss_kb\n" > "$SCALE_LOG"
  : > "$RSS_SUMMARY"
  : > "$SUMMARY_LOG"

  MTX_HLS_ENABLED=yes  # anchor config (h264-srt) probes both HLS endpoints
  start_restream
  start_mediamtx
  COOKIE_JAR=$(mktemp)
  api POST /api/auth/login -d '{"password":"admin"}' >/dev/null

  # run_mixed_config cfg ingest_proto ingest_codec multi_audio [do_anchor] [do_tc_spawns]
  #   do_anchor=1  : HLS output + smoke check + fatal verify_stream + stop lifecycle
  #   do_tc_spawns=1: assert exactly 1 shared internal h264-tc was spawned
  run_mixed_config() {
    local cfg="$1" ingest_proto="$2" ingest_codec="$3" multi_audio="$4"
    local do_anchor="${5:-0}"
    local do_tc_spawns="${6:-0}"
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
      MTX_HLS_ENABLED=yes
      start_mediamtx
      start_restream
      wait_srt_ready 2>/dev/null || true
      rm -f "$COOKIE_JAR" 2>/dev/null || true
      COOKIE_JAR=$(mktemp)
      api POST /api/auth/login -d '{"password":"admin"}' >/dev/null
    fi

    local pipe_id
    pipe_id=$(api POST /pipelines \
      -d "{\"name\":\"${cfg}\",\"streamKey\":\"${stream_key}\"}" | jq -r '.pipeline.id')

    local pub_url codec_args=() map_args=() audio_inputs=() fmt_args=()
    if [[ "$ingest_proto" == "rtmp" ]]; then
      pub_url="rtmp://127.0.0.1:${RESTREAM_RTMP}/live/${stream_key}"
      fmt_args=( -f flv "$pub_url" )
    else
      pub_url="srt://127.0.0.1:${RESTREAM_SRT}?streamid=publish:live/${stream_key}&latency=200000"
      fmt_args=( -f mpegts "$pub_url" )
    fi
    if [[ "$ingest_codec" == "h265" ]]; then
      codec_args=( -c:v libx265 -preset ultrafast -tune zerolatency -x265-params "log-level=none" )
    else
      codec_args=( -c:v libx264 -preset ultrafast -tune zerolatency )
    fi
    if [[ "$multi_audio" == "1" ]]; then
      audio_inputs=( -f lavfi -i 'anullsrc=r=48000:cl=stereo' -f lavfi -i 'anullsrc=r=44100:cl=mono' )
      map_args=( -map 0:v -map 1:a -map 2:a )
    else
      audio_inputs=( -f lavfi -i 'anullsrc=r=48000:cl=stereo' )
      map_args=( -map 0:v -map 1:a )
    fi

    ffmpeg -nostdin -hide_banner -loglevel error -re \
      -f lavfi -i 'testsrc2=size=1920x1080:rate=30' \
      "${audio_inputs[@]}" \
      "${codec_args[@]}" "${map_args[@]}" \
      -b:v 1.5M -c:a aac -b:a 64k \
      "${fmt_args[@]}" >"${WORK_DIR}/publisher.log" 2>&1 &
    PUB_PID=$!

    wait_for_input_live "$pipe_id" "$cfg"

    local enc_rtmp_720p enc_srt_720p
    if [[ "$multi_audio" == "1" ]]; then
      enc_rtmp_720p="720p+atrack:0"; enc_srt_720p="720p+atrack:0,1"
    else
      enc_rtmp_720p="720p"; enc_srt_720p="720p"
    fi

    local rss_baseline; rss_baseline=$(ps -p "$RESTREAM_PID" -o rss= 2>/dev/null | tr -d ' \n')
    snapshot_mixed "$cfg" "baseline (input live, 0 outputs)"

    local out_ids=()

    # anchor: add HLS output alongside RTMP source outputs so restream internal
    # HLS segmenter starts immediately; probed after all groups are up
    if [[ "$do_anchor" == "1" ]]; then
      local hls_oid
      hls_oid=$(api POST "/pipelines/${pipe_id}/outputs" \
        -d "{\"name\":\"hls-preview\",\"url\":\"hls://${cfg}-preview\",\"encoding\":\"source\"}" \
        | jq -r '.output.id')
      api POST "/pipelines/${pipe_id}/outputs/${hls_oid}/start" >/dev/null
      out_ids+=("$hls_oid")
    fi

    echo "  adding ${N} RTMP source outputs..."
    for n in $(seq 1 "$N"); do
      local oid
      oid=$(api POST "/pipelines/${pipe_id}/outputs" \
        -d "{\"name\":\"rtmp-src-${n}\",\"url\":\"rtmp://127.0.0.1:${MTX_RTMP}/live/${cfg}-rtmp-src-${n}\",\"encoding\":\"source\"}" \
        | jq -r '.output.id')
      api POST "/pipelines/${pipe_id}/outputs/${oid}/start" >/dev/null
      out_ids+=("$oid")
    done
    snapshot_mixed "$cfg" "after ${N} RTMP source"

    # anchor smoke: snapshot_mixed already slept 3 s; assert no external
    # transcoder has fired yet (source outputs must not trigger the transcoder)
    if [[ "$do_anchor" == "1" ]]; then
      local ext_before
      ext_before=$(grep -c '\[external-transcoder\] Launching ffmpeg' "$RESTREAM_LOG" 2>/dev/null || true)
      [[ "$ext_before" == "0" ]] || \
        fail "smoke: external transcoder fired before 720p outputs ($ext_before launches)"
      log_ok "smoke: no external transcoder for source outputs"
    fi

    echo "  adding ${N} RTMP 720p outputs (enc=${enc_rtmp_720p})..."
    for n in $(seq 1 "$N"); do
      local oid
      oid=$(api POST "/pipelines/${pipe_id}/outputs" \
        -d "{\"name\":\"rtmp-720p-${n}\",\"url\":\"rtmp://127.0.0.1:${MTX_RTMP}/live/${cfg}-rtmp-720p-${n}\",\"encoding\":\"${enc_rtmp_720p}\"}" \
        | jq -r '.output.id')
      api POST "/pipelines/${pipe_id}/outputs/${oid}/start" >/dev/null
      out_ids+=("$oid")
    done
    snapshot_mixed "$cfg" "after ${N} RTMP 720p"

    echo "  adding ${N} SRT source outputs..."
    for n in $(seq 1 "$N"); do
      local oid
      oid=$(api POST "/pipelines/${pipe_id}/outputs" \
        -d "{\"name\":\"srt-src-${n}\",\"url\":\"srt://127.0.0.1:${MTX_SRT}?streamid=publish:live/${cfg}-srt-src-${n}\",\"encoding\":\"source\"}" \
        | jq -r '.output.id')
      api POST "/pipelines/${pipe_id}/outputs/${oid}/start" >/dev/null
      out_ids+=("$oid")
    done
    snapshot_mixed "$cfg" "after ${N} SRT source"

    echo "  adding ${N} SRT 720p outputs (enc=${enc_srt_720p})..."
    for n in $(seq 1 "$N"); do
      local oid
      oid=$(api POST "/pipelines/${pipe_id}/outputs" \
        -d "{\"name\":\"srt-720p-${n}\",\"url\":\"srt://127.0.0.1:${MTX_SRT}?streamid=publish:live/${cfg}-srt-720p-${n}\",\"encoding\":\"${enc_srt_720p}\"}" \
        | jq -r '.output.id')
      api POST "/pipelines/${pipe_id}/outputs/${oid}/start" >/dev/null
      out_ids+=("$oid")
    done
    snapshot_mixed "$cfg" "after all ${TOTAL} outputs"

    local rss_final ffmpeg_ext_n ffmpeg_ext_rss
    rss_final=$(ps -p "$RESTREAM_PID" -o rss= 2>/dev/null | tr -d ' \n')
    ffmpeg_ext_n=$(ps aux | awk '/[f]fmpeg.*pipe:1/{n++} END{print n+0}')
    ffmpeg_ext_rss=$(ps aux | awk '/[f]fmpeg.*pipe:1/{sum+=$6} END{print sum+0}')
    local rss_delta=$(( rss_final - rss_baseline ))
    local per_output=$(( rss_delta / TOTAL ))
    printf "  RESULT %-22s  restream_delta=+%-8s  per_output=~%-8s  ext_ffmpeg#=%-3s  ext_ffmpeg_rss=%s KB\n" \
      "$cfg" "${rss_delta} KB" "${per_output} KB" "$ffmpeg_ext_n" "$ffmpeg_ext_rss"
    printf "%s,rss_delta_kb=%s,per_output_kb=%s,ext_ffmpeg_n=%s,ext_ffmpeg_rss_kb=%s\n" \
      "$cfg" "$rss_delta" "$per_output" "$ffmpeg_ext_n" "$ffmpeg_ext_rss" >> "$RSS_SUMMARY"

    local srt_tout="&timeout=30000000"
    echo "  spot-checks:"
    if [[ "$do_anchor" == "1" ]]; then
      # anchor: fatal verify_stream — correctness gate for all protocol×encoding + both HLS endpoints
      verify_stream "RTMP-src  out${N}"  "rtmp://127.0.0.1:${MTX_RTMP}/live/${cfg}-rtmp-src-${N}"                          "1920x1080"
      verify_stream "RTMP-720p out${N}"  "rtmp://127.0.0.1:${MTX_RTMP}/live/${cfg}-rtmp-720p-${N}"                         "1280x720"
      verify_stream "SRT-src   out${N}"  "srt://127.0.0.1:${MTX_SRT}?streamid=read:live/${cfg}-srt-src-${N}${srt_tout}"   "1920x1080"
      verify_stream "SRT-720p  out${N}"  "srt://127.0.0.1:${MTX_SRT}?streamid=read:live/${cfg}-srt-720p-${N}${srt_tout}"  "1280x720"
      verify_stream "HLS/mtx"            "http://127.0.0.1:${MTX_HLS}/live/${cfg}-rtmp-src-${N}/index.m3u8"                "1920x1080"
      verify_stream "HLS/restream"       "http://127.0.0.1:${RESTREAM_HTTP}/hls/${pipe_id}/index.m3u8"                     "1920x1080"
    else
      check_stream "RTMP-src  out${N}"  "rtmp://127.0.0.1:${MTX_RTMP}/live/${cfg}-rtmp-src-${N}"                           "1920x1080"
      check_stream "RTMP-720p out${N}"  "rtmp://127.0.0.1:${MTX_RTMP}/live/${cfg}-rtmp-720p-${N}"                          "1280x720"
      check_stream "SRT-src   out${N}"  "srt://127.0.0.1:${MTX_SRT}?streamid=read:live/${cfg}-srt-src-${N}${srt_tout}"    "1920x1080"
      check_stream "SRT-720p  out${N}"  "srt://127.0.0.1:${MTX_SRT}?streamid=read:live/${cfg}-srt-720p-${N}${srt_tout}"   "1280x720"
    fi

    # h265-srt: stage sharing — each unique HEVC consumer path spawns exactly
    # one h264-tc (source→RTMP needs one; 720p external ffmpeg feed needs one).
    # With both source and 720p outputs, the bound is ext_ffmpeg_n + 1.
    # Failure means N outputs each spawned their own transcoder (sharing broke).
    if [[ "$do_tc_spawns" == "1" ]]; then
      local tc_spawns tc_max
      tc_spawns=$(grep -c '\[h264-tc\] Spawning' "$RESTREAM_LOG" 2>/dev/null || true)
      tc_spawns=${tc_spawns:-0}
      tc_max=$(( ffmpeg_ext_n + 1 ))
      [[ "$tc_spawns" -ge 1 && "$tc_spawns" -le "$tc_max" ]] || \
        fail "${cfg}: expected 1..${tc_max} h264-tc spawns (got ${tc_spawns}; N=${N} outputs — sharing broken if >${tc_max})"
      log_ok "${cfg}: TC_SPAWNS=${tc_spawns} ≤ $((ffmpeg_ext_n + 1)) (stage sharing confirmed for ${TOTAL} outputs)"
    fi

    kill "$PUB_PID" 2>/dev/null || true; PUB_PID=""

    if [[ "$do_anchor" == "1" ]]; then
      # stop lifecycle: call /stop on every output and verify all reach "stopped"
      for oid in "${out_ids[@]}"; do
        api POST "/pipelines/${pipe_id}/outputs/${oid}/stop" >/dev/null 2>/dev/null || true
      done
      echo "  lifecycle: stop requested for ${#out_ids[@]} outputs"
      local stop_deadline=$(( SECONDS + 60 ))
      while [[ $SECONDS -lt $stop_deadline ]]; do
        local all_stopped=true config_now
        config_now=$(api GET /config)
        for oid in "${out_ids[@]}"; do
          local job_status
          job_status=$(echo "$config_now" | jq -r \
            --arg pid "$pipe_id" --arg oid "$oid" \
            '.jobs[] | select(.pipelineId==$pid and .outputId==$oid) | .status // empty')
          if [[ -n "$job_status" && "$job_status" != "stopped" ]]; then all_stopped=false; fi
        done
        $all_stopped && break
        sleep 1
      done
      log_ok "lifecycle: all outputs stopped"
      sleep 3
    else
      for oid in "${out_ids[@]}"; do
        api POST "/pipelines/${pipe_id}/outputs/${oid}/stop" >/dev/null 2>/dev/null || true
      done
      sleep 8
    fi
  }

  #              cfg                proto  codec  multi  anchor  tc_spawns
  run_mixed_config "h264-srt"       srt    h264   0      1       0
  run_mixed_config "h265-srt"       srt    h265   0      0       1
  run_mixed_config "h264-srt-multi" srt    h264   1      0       0
  run_mixed_config "h265-srt-multi" srt    h265   1      0       0

  echo ""
  echo "══════════════════════════════════════════════════════════════════════════"
  printf "  Summary — %s outputs per group (%s total per ingest)\n" "$N_PER_GROUP" "$(( N_PER_GROUP * 4 ))"
  echo "══════════════════════════════════════════════════════════════════════════"
  printf "%-24s  %-16s  %-14s  %-12s  %s\n" config restream_delta "per_output KB" "ext_ffmpeg#" "ext_ffmpeg_rss KB"
  printf "%-24s  %-16s  %-14s  %-12s  %s\n" "------------------------" "----------------" "--------------" "------------" "-----------------"
  while IFS=',' read -r cfg rest; do
    local d p n r
    d=$(echo "$rest" | grep -o 'rss_delta_kb=[^,]*'      | cut -d= -f2)
    p=$(echo "$rest" | grep -o 'per_output_kb=[^,]*'     | cut -d= -f2)
    n=$(echo "$rest" | grep -o 'ext_ffmpeg_n=[^,]*'      | cut -d= -f2)
    r=$(echo "$rest" | grep -o 'ext_ffmpeg_rss_kb=[^,]*' | cut -d= -f2)
    printf "%-24s  +%-15s  %-14s  %-12s  %s\n" "$cfg" "${d} KB" "${p} KB" "$n" "$r"
  done < "$RSS_SUMMARY"

  echo ""; echo "CSV:  $SCALE_LOG"; echo "SUMM: $RSS_SUMMARY"
}


# ── Mode: bonding ──────────────────────────────────────────────────────────────
# Verifies SRT socket-level bonding: broadcast group (2 members, failover=0, 1 message)
# and backup group (2 members, failover=1, 2 messages). Requires static build.
run_bonding() {
  local BUILD_ROOT="${RESTREAM_BUILD_ROOT:-$ROOT/.build/static}"

  if [[ ! -f "$BUILD_ROOT/env.sh" ]]; then
    "$ROOT/scripts/setup-static-build.sh"
  fi
  # shellcheck source=/dev/null
  source "$BUILD_ROOT/env.sh"

  local SERVER="$BUILD_ROOT/prefix/bin/restream-srt-bond-server"
  local CLIENT="$BUILD_ROOT/prefix/bin/restream-srt-bond-client"

  run_bond_mode() {
    local mode="$1"
    local server_log="$BUILD_ROOT/${mode}-server.log"
    local client_log="$BUILD_ROOT/${mode}-client.log"

    local server_pid="" port=""
    for _ in {1..20}; do
      port=$(( 20000 + RANDOM % 40000 ))
      : > "$server_log"
      timeout 15s "$SERVER" "$mode" "$port" >"$server_log" 2>&1 &
      server_pid=$!
      trap 'kill "$server_pid" 2>/dev/null || true' RETURN

      for _ in {1..25}; do
        grep -q "^ready port=$port$" "$server_log" && break
        kill -0 "$server_pid" 2>/dev/null || break
        sleep 0.02
      done
      grep -q "^ready port=$port$" "$server_log" && break
      wait "$server_pid" 2>/dev/null || true
      server_pid=""
    done
    if [[ -z "$server_pid" ]]; then
      cat "$server_log" >&2; return 1
    fi

    if ! timeout 15s "$CLIENT" "$mode" "$port" >"$client_log" 2>&1; then
      cat "$client_log" >&2; cat "$server_log" >&2; return 1
    fi
    if ! wait "$server_pid"; then
      cat "$client_log" >&2; cat "$server_log" >&2; return 1
    fi
    trap - RETURN

    local expected_failover=0 expected_messages=1
    if [[ "$mode" == "backup" ]]; then expected_failover=1; expected_messages=2; fi

    if ! grep -q "connected_group type=$mode members=2 failover=$expected_failover" "$client_log" ||
       ! grep -q "accepted_group members=2 messages=$expected_messages" "$server_log"; then
      cat "$client_log" >&2; cat "$server_log" >&2; return 1
    fi
    echo "SRT $mode bonding: PASS"
  }

  run_bond_mode broadcast
  run_bond_mode backup
}

# ── Dispatch ───────────────────────────────────────────────────────────────────
mkdir -p "$WORK_DIR"

case "$MODE" in
  ramp)        run_ramp        ;;
  mixed-scale) run_mixed_scale ;;
  bonding)     run_bonding     ;;
  *)
    echo "Unknown mode: $MODE" >&2
    echo "Valid modes: ramp mixed-scale bonding" >&2
    exit 1
    ;;
esac
