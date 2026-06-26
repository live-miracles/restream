#!/usr/bin/env bash
# run-integration.sh — unified integration test runner
#
# Usage:
#   scripts/resource-limit ./test/run-integration.sh [--host] [--preflight] [--fast] [--json path] [--only checks] <mode>
#
# By default every mode that manages its own server processes runs inside a
# private loopback network namespace (unshare --net) so ports never conflict
# with anything on the host.  Pass --host to skip the namespace wrapper.
#
# Modes:
#   ramp        8 ingest×egress×encoding configs, outputs added one-by-one, per-step RSS/CPU snapshots
#   mixed-scale 5 configs (h264-rtmp; h264-srt anchor: HLS+smoke+lifecycle; h265-srt: TC_SPAWNS; multi-audio ×2)
#   bonding     SRT broadcast+backup bonding (requires static build)
#   burst-verify closed-GOP RTMP/SRT matrix that verifies graph burst reader stats
#   hls-put     SRT ingest to YouTube/path-style HTTP HLS PUT upload sinks
#   bframe-rtmp RTMP B-frame ingest to RTMP egress timestamp round-trip
#   correctness-srt-rtmp   SRT H.264/AAC ingest to native RTMP egress
#   correctness-hevc-rtmp  SRT H.265 ingest to H.264 RTMP egress
#   correctness-hevc-srt   SRT H.265 ingest to native SRT egress
#
# Common env overrides (all modes):
#   RESTREAM_BIN   path to restream binary (default: target/release/restream)
#   WORK_DIR       artifact directory      (default: test/artifacts/<mode>)
#   RESTREAM_DB_PATH SQLite file path       (default: data.db)
#   RESTREAM_HTTP/RTMP/SRT  port overrides
#   MTX_RTMP/SRT/HLS/API    mediamtx port overrides
#   HLS_PUT_PORT            dummy HLS PUT sink port (default: 8990)
#   RESTREAM_ARTIFACT_MIN_FREE_MB fail before live runs when free space is below this (default: 2048)
#   ALLOW_GLOBAL_PROCESS_CLEANUP=1 opt into legacy host-wide restream/mediamtx cleanup
#
# Each mode writes WORK_DIR/manifest.json with RUNNING → PASS/FAIL status.
#
# Mode-specific env overrides are documented inside each run_* function.
set -euo pipefail

# ── Argument parsing ───────────────────────────────────────────────────────────
HOST_NETWORK=0
FAST_MODE=0
PREFLIGHT=0
SKIP_LOAD=0
ASSERTION_LOG=""
ONLY_CHECKS=""
BASELINE_PATH=""
SAVE_BASELINE_PATH=""
RESUME_FROM=""
POSITIONAL_ARGS=()
REEXEC_ARGS=()

while [[ $# -gt 0 ]]; do
  case "$1" in
    --host)
      HOST_NETWORK=1
      shift
      ;;
    --fast)
      FAST_MODE=1
      REEXEC_ARGS+=("--fast")
      shift
      ;;
    --preflight)
      PREFLIGHT=1
      REEXEC_ARGS+=("--preflight")
      shift
      ;;
    --skip-load)
      SKIP_LOAD=1
      REEXEC_ARGS+=("--skip-load")
      shift
      ;;
    --json)
      [[ $# -ge 2 ]] || { echo "--json requires a path" >&2; exit 2; }
      ASSERTION_LOG="$2"
      REEXEC_ARGS+=("--json" "$2")
      shift 2
      ;;
    --only)
      [[ $# -ge 2 ]] || { echo "--only requires a comma-separated list" >&2; exit 2; }
      ONLY_CHECKS="$2"
      REEXEC_ARGS+=("--only" "$2")
      shift 2
      ;;
    --baseline)
      [[ $# -ge 2 ]] || { echo "--baseline requires a path" >&2; exit 2; }
      BASELINE_PATH="$2"
      REEXEC_ARGS+=("--baseline" "$2")
      shift 2
      ;;
    --save-baseline)
      [[ $# -ge 2 ]] || { echo "--save-baseline requires a path" >&2; exit 2; }
      SAVE_BASELINE_PATH="$2"
      REEXEC_ARGS+=("--save-baseline" "$2")
      shift 2
      ;;
    --resume-from)
      [[ $# -ge 2 ]] || { echo "--resume-from requires an assertion id" >&2; exit 2; }
      RESUME_FROM="$2"
      REEXEC_ARGS+=("--resume-from" "$2")
      shift 2
      ;;
    --)
      shift
      while [[ $# -gt 0 ]]; do POSITIONAL_ARGS+=("$1"); REEXEC_ARGS+=("$1"); shift; done
      ;;
    -*)
      echo "Unknown option: $1" >&2
      exit 2
      ;;
    *)
      POSITIONAL_ARGS+=("$1")
      REEXEC_ARGS+=("$1")
      shift
      ;;
  esac
done

MODE="${POSITIONAL_ARGS[0]:-}"
if [[ -z "$MODE" ]]; then
  grep '^#   [a-z]' "$0" | sed 's/^#   /  /' >&2
  echo "Usage: $0 [--host] [--preflight] [--fast] [--json path] [--only checks] <mode>" >&2
  exit 1
fi

if [[ "$FAST_MODE" == "1" ]]; then
  export N_PER_GROUP="${N_PER_GROUP:-1}"
  export N_OUTPUTS="${N_OUTPUTS:-1}"
  export SNAP_EVERY="${SNAP_EVERY:-999}"
  export SNAPSHOT_SLEEP_SECS="${SNAPSHOT_SLEEP_SECS:-0}"
fi

# ── Roots ──────────────────────────────────────────────────────────────────────
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SCRIPT_PATH="${ROOT}/test/run-integration.sh"
cd "$ROOT"

RESTREAM_BIN="${RESTREAM_BIN:-$ROOT/target/release/restream}"
RESTREAM_DB_PATH="${RESTREAM_DB_PATH:-data.db}"
ARTIFACT_ROOT_ABS="${ROOT}/test/artifacts"
ARTIFACT_KEEP_RUNS=3
RESTREAM_ARTIFACT_MIN_FREE_MB="${RESTREAM_ARTIFACT_MIN_FREE_MB:-2048}"
TEST_HARNESS_RTMP=11935
TEST_HARNESS_SRT=11080

# ── Port defaults (each mode may override before calling start_*) ──────────────
RESTREAM_HTTP="${RESTREAM_HTTP:-3030}"
RESTREAM_RTMP="${RESTREAM_RTMP:-1935}"
RESTREAM_SRT="${RESTREAM_SRT:-10080}"
MTX_RTMP="${MTX_RTMP:-1936}"
MTX_SRT="${MTX_SRT:-8891}"
MTX_HLS="${MTX_HLS:-8890}"
MTX_API="${MTX_API:-9997}"
HLS_PUT_PORT="${HLS_PUT_PORT:-8990}"

API_URL="http://127.0.0.1:${RESTREAM_HTTP}"

# ── Network namespace self-reexec ──────────────────────────────────────────────
# bonding uses its own static binaries with random ports and needs the host
# network.  All other modes start their own servers and benefit from a private
# namespace.  Skip if --host was given or we are already inside netns.
if [[ "$PREFLIGHT" != "1" && "$HOST_NETWORK" == "0" && "${_IN_NETNS:-0}" != "1" ]]; then
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
        bash "$SCRIPT_PATH" "${REEXEC_ARGS[@]}"
      ;;
  esac
fi

# ── Global process/file handles ────────────────────────────────────────────────
RESTREAM_PID=""
MTX_PID=""
PUB_PID=""            # single publisher (scale, mixed-scale, hevc-load, smoke)
PUB_PIDS=()           # multiple publishers (matrix)
HLS_PUT_PID=""
COOKIE_JAR=""
WORK_DIR="${WORK_DIR:-test/artifacts/${MODE}}"
RESTREAM_LOG="${WORK_DIR}/restream.log"
HLS_PUT_DIR="${WORK_DIR}/hls-put-sink"
SCALE_LOG="/dev/null"
SUMMARY_LOG="/dev/null"
SNAPSHOTS="/dev/null"
MANIFEST=""
RUN_STARTED_AT="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

# ── Shared helpers ─────────────────────────────────────────────────────────────

fail()   { echo "FAIL: $*" >&2; exit 1; }
log_ok() { echo "ok: $*" | tee -a "${WORK_DIR}/summary.txt"; }

is_uint() {
  [[ "$1" =~ ^[0-9]+$ ]]
}

require_uint_env() {
  local name="$1" value="$2"
  if ! is_uint "$value"; then
    fail "${name} must be a non-negative integer"
  fi
}

cleanup() {
  [[ ${#PUB_PIDS[@]} -gt 0 ]] && { for p in "${PUB_PIDS[@]}"; do kill "$p" 2>/dev/null || true; done; }
  [[ -n "$PUB_PID" ]]      && kill "$PUB_PID"      2>/dev/null || true
  [[ -n "$HLS_PUT_PID" ]]  && kill "$HLS_PUT_PID"  2>/dev/null || true
  [[ -n "$MTX_PID" ]]      && kill "$MTX_PID"      2>/dev/null || true
  [[ -n "$RESTREAM_PID" ]] && kill "$RESTREAM_PID" 2>/dev/null || true
  [[ -n "$COOKIE_JAR" ]]   && rm -f "$COOKIE_JAR"  || true
}

json_escape() {
  local value="$1"
  value=${value//\\/\\\\}
  value=${value//\"/\\\"}
  value=${value//$'\n'/\\n}
  printf '%s' "$value"
}

now_ms() {
  date +%s%3N
}

abs_path() {
  local path="$1" dir base
  if [[ "$path" == /* ]]; then
    printf '%s\n' "$path"
    return
  fi
  dir="$(dirname "$path")"
  base="$(basename "$path")"
  mkdir -p "$dir"
  printf '%s/%s\n' "$(cd "$dir" && pwd)" "$base"
}

available_mb_for_path() {
  local path="$1" probe
  probe="$path"
  while [[ ! -e "$probe" && "$probe" != "/" ]]; do
    probe="$(dirname "$probe")"
  done
  df -Pm "$probe" | awk 'NR==2 {print $4}'
}

ensure_artifact_free_space() {
  require_uint_env RESTREAM_ARTIFACT_MIN_FREE_MB "$RESTREAM_ARTIFACT_MIN_FREE_MB"
  [[ "$RESTREAM_ARTIFACT_MIN_FREE_MB" -eq 0 ]] && return 0

  local free_mb
  free_mb="$(available_mb_for_path "$ARTIFACT_ROOT_ABS")"
  if [[ -z "$free_mb" || ! "$free_mb" =~ ^[0-9]+$ ]]; then
    fail "could not determine free space for artifact root ${ARTIFACT_ROOT_ABS}"
  fi
  if (( free_mb < RESTREAM_ARTIFACT_MIN_FREE_MB )); then
    fail "artifact filesystem has ${free_mb}MB free, below RESTREAM_ARTIFACT_MIN_FREE_MB=${RESTREAM_ARTIFACT_MIN_FREE_MB}; prune test/artifacts or lower the guard intentionally"
  fi
}

prune_old_artifacts() {
  [[ "${KEEP_ARTIFACTS:-0}" != "1" ]] || return 0
  [[ -d "$ARTIFACT_ROOT_ABS" ]] || return 0

  local work_abs protected_top keep_remaining entry path
  work_abs="$(abs_path "$WORK_DIR")"
  protected_top=""
  if [[ "$work_abs" == "$ARTIFACT_ROOT_ABS"/* ]]; then
    protected_top="${work_abs#"$ARTIFACT_ROOT_ABS"/}"
    protected_top="${ARTIFACT_ROOT_ABS}/${protected_top%%/*}"
  fi
  keep_remaining="$ARTIFACT_KEEP_RUNS"
  [[ -z "$protected_top" ]] || keep_remaining=$((keep_remaining - 1))
  (( keep_remaining >= 0 )) || keep_remaining=0

  while IFS= read -r entry; do
    path="${entry#* }"
    if [[ -n "$protected_top" && "$path" == "$protected_top" ]]; then
      continue
    fi
    if (( keep_remaining > 0 )); then
      keep_remaining=$(( keep_remaining - 1 ))
      continue
    fi
    rm -rf -- "$path"
  done < <(find "$ARTIFACT_ROOT_ABS" -mindepth 1 -maxdepth 1 -type d -printf '%T@ %p\n' | sort -rn)
}

init_assertion_log() {
  [[ -n "$ASSERTION_LOG" ]] || return 0
  mkdir -p "$(dirname "$ASSERTION_LOG")"
  : > "$ASSERTION_LOG"
}

json_tail_array() {
  local file="$1" lines="${2:-30}" pattern="${3:-}" sep="" line
  printf '['
  if [[ -f "$file" ]]; then
    while IFS= read -r line; do
      printf '%s"%s"' "$sep" "$(json_escape "$line")"
      sep=','
    done < <(
      if [[ -n "$pattern" ]]; then
        grep -F "$pattern" "$file" 2>/dev/null | tail -n "$lines" || true
      else
        tail -n "$lines" "$file" 2>/dev/null || true
      fi
    )
  fi
  printf ']'
}

emit_result() {
  local id="$1" status="$2" ms="${3:-0}" extra="${4:-}"
  [[ -n "$ASSERTION_LOG" ]] || return 0
  printf '{"id":"%s","mode":"%s","config":"%s","status":"%s","ms":%s%s}\n' \
    "$(json_escape "$id")" \
    "$(json_escape "$MODE")" \
    "$(json_escape "${CURRENT_CONFIG:-}")" \
    "$(json_escape "$status")" \
    "${ms:-0}" \
    "$extra" >> "$ASSERTION_LOG"
}

emit_preflight() {
  local json="$1"
  printf '%s\n' "$json"
  [[ -n "$ASSERTION_LOG" ]] && printf '%s\n' "$json" >> "$ASSERTION_LOG"
  return 0
}

fail_assertion() {
  local id="$1" message="$2" ms="${3:-0}" extra="${4:-}"
  emit_result "$id" "fail" "$ms" ",\"message\":\"$(json_escape "$message")\"${extra}"
  fail "$message"
}

only_has() {
  local wanted="$1" item
  [[ -z "$ONLY_CHECKS" ]] && return 0
  IFS=',' read -ra _only_items <<< "$ONLY_CHECKS"
  for item in "${_only_items[@]}"; do
    item="${item//[[:space:]]/}"
    item="${item//_/-}"
    [[ "$item" == "$wanted" ]] && return 0
  done
  return 1
}

check_selected() {
  local check="$1"
  [[ -z "$ONLY_CHECKS" ]] && return 0
  only_has "$check"
}

resume_allows() {
  local id="$1"
  [[ -z "$RESUME_FROM" || "${_RESUME_ACTIVE:-0}" == "1" ]] && return 0
  if [[ "$id" == "$RESUME_FROM" ]]; then
    _RESUME_ACTIVE=1
    return 0
  fi
  return 1
}

harness_only_mode() {
  case "$MODE" in
    burst-verify|hls-put|bframe-rtmp|correctness-srt-rtmp|correctness-hevc-rtmp|correctness-hevc-srt)
      return 0
      ;;
    *)
      return 1
      ;;
  esac
}

mode_deps() {
  case "$MODE" in
    bonding)      printf '%s\n' bash timeout ;;
    ramp)         printf '%s\n' cargo ffmpeg ffprobe curl jq mediamtx ;;
    mixed-scale)  printf '%s\n' cargo ffmpeg ffprobe curl jq mediamtx ;;
    burst-verify) printf '%s\n' cargo ffmpeg jq ;;
    hls-put)      printf '%s\n' cargo ffmpeg ffprobe jq ;;
    bframe-rtmp)  printf '%s\n' cargo ffmpeg ffprobe jq ;;
    correctness-srt-rtmp|correctness-hevc-rtmp|correctness-hevc-srt)
                  printf '%s\n' cargo ffmpeg ffprobe jq ;;
    *)            printf '%s\n' ffmpeg ffprobe curl jq mediamtx ;;
  esac
}

mode_ports() {
  case "$MODE" in
    bonding) ;;
    burst-verify) printf '%s\n' "$TEST_HARNESS_RTMP" "$TEST_HARNESS_SRT" ;;
    hls-put)      printf '%s\n' "$TEST_HARNESS_SRT" "$HLS_PUT_PORT" ;;
    bframe-rtmp)  printf '%s\n' "$TEST_HARNESS_RTMP" ;;
    correctness-srt-rtmp|correctness-hevc-rtmp)
                  printf '%s\n' "$TEST_HARNESS_RTMP" "$TEST_HARNESS_SRT" ;;
    correctness-hevc-srt)
                  printf '%s\n' "$TEST_HARNESS_SRT" ;;
    *)            printf '%s\n' "$RESTREAM_HTTP" "$RESTREAM_RTMP" "$RESTREAM_SRT" "$MTX_RTMP" "$MTX_SRT" "$MTX_HLS" "$MTX_API" ;;
  esac
}

port_is_busy() {
  local port="$1"
  if command -v ss >/dev/null 2>&1; then
    ss -H -ltnu 2>/dev/null | awk '{print $5}' | grep -Eq "(^|:)${port}$"
  elif command -v lsof >/dev/null 2>&1; then
    lsof -iTCP:"$port" -iUDP:"$port" -sTCP:LISTEN >/dev/null 2>&1
  else
    return 1
  fi
}

run_preflight() {
  init_assertion_log
  local fail_count=0 status missing=() cmd busy=() ports=()

  if [[ "$MODE" == "bonding" ]]; then
    emit_preflight "{\"check\":\"binary\",\"path\":\"$(json_escape "$RESTREAM_BIN")\",\"status\":\"skip\",\"hint\":\"bonding mode builds and runs static SRT helper binaries\"}"
  elif harness_only_mode; then
    emit_preflight "{\"check\":\"binary\",\"path\":\"$(json_escape "$RESTREAM_BIN")\",\"status\":\"skip\",\"hint\":\"mode is managed by the Rust test_harness in-process\"}"
  elif [[ -x "$RESTREAM_BIN" ]]; then
    local mtime
    mtime="$(date -u -r "$RESTREAM_BIN" +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || echo unknown)"
    emit_preflight "{\"check\":\"binary\",\"path\":\"$(json_escape "$RESTREAM_BIN")\",\"status\":\"ok\",\"mtime\":\"$(json_escape "$mtime")\"}"
  else
    emit_preflight "{\"check\":\"binary\",\"path\":\"$(json_escape "$RESTREAM_BIN")\",\"status\":\"fail\",\"hint\":\"run scripts/resource-limit cargo build --release\"}"
    fail_count=$(( fail_count + 1 ))
  fi

  while IFS= read -r cmd; do
    [[ -z "$cmd" ]] && continue
    command -v "$cmd" >/dev/null 2>&1 || missing+=("$cmd")
  done < <(mode_deps)
  if [[ "${#missing[@]}" -eq 0 ]]; then status=ok; else status=fail; fail_count=$(( fail_count + 1 )); fi
  local deps_json="[" sep=""
  for cmd in "${missing[@]}"; do deps_json+="${sep}\"$(json_escape "$cmd")\""; sep=','; done
  deps_json+="]"
  emit_preflight "{\"check\":\"deps\",\"missing\":${deps_json},\"status\":\"${status}\"}"

  if command -v unshare >/dev/null 2>&1 && command -v ip >/dev/null 2>&1; then
    emit_preflight "{\"check\":\"netns\",\"unshare_available\":true,\"ip_available\":true,\"status\":\"ok\"}"
  elif [[ "$HOST_NETWORK" == "0" && "$MODE" != "bonding" ]]; then
    emit_preflight "{\"check\":\"netns\",\"unshare_available\":false,\"status\":\"fail\",\"hint\":\"install util-linux and iproute2, or pass --host\"}"
    fail_count=$(( fail_count + 1 ))
  else
    emit_preflight "{\"check\":\"netns\",\"unshare_available\":false,\"status\":\"skip\",\"hint\":\"host networking requested or mode does not use netns\"}"
  fi

  local free_mb
  if ! is_uint "$RESTREAM_ARTIFACT_MIN_FREE_MB"; then
    emit_preflight "{\"check\":\"artifact-disk\",\"status\":\"fail\",\"hint\":\"RESTREAM_ARTIFACT_MIN_FREE_MB must be a non-negative integer\"}"
    fail_count=$(( fail_count + 1 ))
  else
    mkdir -p "$ARTIFACT_ROOT_ABS"
    free_mb="$(available_mb_for_path "$ARTIFACT_ROOT_ABS")"
    if [[ -z "$free_mb" || ! "$free_mb" =~ ^[0-9]+$ ]]; then
      emit_preflight "{\"check\":\"artifact-disk\",\"root\":\"$(json_escape "$ARTIFACT_ROOT_ABS")\",\"status\":\"fail\",\"hint\":\"could not determine free space\"}"
      fail_count=$(( fail_count + 1 ))
    elif (( RESTREAM_ARTIFACT_MIN_FREE_MB > 0 && free_mb < RESTREAM_ARTIFACT_MIN_FREE_MB )); then
      emit_preflight "{\"check\":\"artifact-disk\",\"root\":\"$(json_escape "$ARTIFACT_ROOT_ABS")\",\"freeMb\":${free_mb},\"minFreeMb\":${RESTREAM_ARTIFACT_MIN_FREE_MB},\"status\":\"fail\",\"hint\":\"prune test artifacts or lower RESTREAM_ARTIFACT_MIN_FREE_MB intentionally\"}"
      fail_count=$(( fail_count + 1 ))
    else
      emit_preflight "{\"check\":\"artifact-disk\",\"root\":\"$(json_escape "$ARTIFACT_ROOT_ABS")\",\"freeMb\":${free_mb},\"minFreeMb\":${RESTREAM_ARTIFACT_MIN_FREE_MB},\"status\":\"ok\"}"
    fi
  fi

  mapfile -t ports < <(mode_ports)
  if [[ "$HOST_NETWORK" == "1" || "$MODE" == "bonding" ]]; then
    local port
    for port in "${ports[@]}"; do
      port_is_busy "$port" && busy+=("$port")
    done
  fi
  local ports_json="[" sep2=""
  for port in "${ports[@]}"; do ports_json+="${sep2}${port}"; sep2=','; done
  ports_json+="]"

  local busy_json="[" sep3=""
  for port in "${busy[@]}"; do busy_json+="${sep3}${port}"; sep3=','; done
  busy_json+="]"
  if [[ "${#busy[@]}" -eq 0 ]]; then
    emit_preflight "{\"check\":\"ports\",\"ports\":${ports_json},\"busy\":${busy_json},\"status\":\"ok\",\"hostNetwork\":${HOST_NETWORK}}"
  else
    emit_preflight "{\"check\":\"ports\",\"ports\":${ports_json},\"busy\":${busy_json},\"status\":\"fail\",\"hostNetwork\":${HOST_NETWORK},\"hint\":\"free the listed ports or omit --host\"}"
    fail_count=$(( fail_count + 1 ))
  fi

  [[ "$fail_count" -eq 0 ]]
}

write_manifest() {
  local status="$1" finished_at="${2:-}"
  [[ -n "$MANIFEST" ]] || return 0
  local git_head finished_json
  git_head="$(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
  if [[ -n "$finished_at" ]]; then
    finished_json="\"$(json_escape "$finished_at")\""
  else
    finished_json="null"
  fi
  cat > "$MANIFEST" <<JSON
{
  "mode": "$(json_escape "$MODE")",
  "status": "$(json_escape "$status")",
  "startedAt": "$(json_escape "$RUN_STARTED_AT")",
  "finishedAt": ${finished_json},
  "workDir": "$(json_escape "$WORK_DIR")",
  "gitHead": "$(json_escape "$git_head")",
  "hostNetwork": ${HOST_NETWORK},
  "networkNamespace": "$([[ "${_IN_NETNS:-0}" == "1" ]] && echo "private" || echo "host")",
  "artifacts": {
    "restreamLog": "$(json_escape "$RESTREAM_LOG")",
    "summary": "$(json_escape "$SUMMARY_LOG")",
    "scaleCsv": "$(json_escape "$SCALE_LOG")"${EXTRA_ARTIFACTS_JSON:-}
  }
}
JSON
}

init_run_artifacts() {
  mkdir -p "$WORK_DIR"
  prune_old_artifacts
  ensure_artifact_free_space
  if [[ "${KEEP_ARTIFACTS:-0}" != "1" && -d "$WORK_DIR" ]]; then
    rm -rf "$WORK_DIR"
  fi
  mkdir -p "$WORK_DIR"
  MANIFEST="${WORK_DIR}/manifest.json"
  init_assertion_log
  write_manifest "RUNNING"
}

on_exit() {
  local status=$?
  if [[ -n "$MANIFEST" ]]; then
    if [[ "$status" -eq 0 ]]; then
      write_manifest "PASS" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" || true
    else
      write_manifest "FAIL" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" || true
    fi
  fi
  cleanup
  return "$status"
}
trap on_exit EXIT

check_deps() {
  for cmd in "$@"; do
    command -v "$cmd" >/dev/null 2>&1 || { echo "${cmd} not found" >&2; exit 1; }
  done
}

check_embedded_ffmpeg_fresh() {
  [[ -n "${FFMPEG_BIN_PATH:-}" ]] && return 0
  [[ -x "$RESTREAM_BIN" ]] || return 0
  local embedded_ffmpeg="$ROOT/public/bin/ffmpeg"
  if [[ -x "$embedded_ffmpeg" && "$embedded_ffmpeg" -nt "$RESTREAM_BIN" ]]; then
    fail "RESTREAM_BIN is older than public/bin/ffmpeg; rebuild $RESTREAM_BIN for embedded FFmpeg tests or set FFMPEG_BIN_PATH explicitly for system-FFmpeg diagnosis"
  fi
}

cleanup_restream_procs() {
  [[ "${ALLOW_GLOBAL_PROCESS_CLEANUP:-0}" == "1" ]] || return 0
  local pids
  pids=$(ps -eo pid=,comm= | awk '$2 == "restream" {print $1}' || true)
  [[ -n "$pids" ]] && { kill -9 $pids 2>/dev/null || true; sleep 3; }
  return 0
}

cleanup_db() {
  local db_url="$RESTREAM_DB_PATH"
  local db_path="${db_url#sqlite:}"
  db_path="${db_path%%\?*}"
  [[ -n "$db_path" ]] || db_path="data.db"
  if [[ "$db_path" != /* ]]; then
    db_path="$ROOT/$db_path"
  fi
  rm -f "$db_path" "${db_path}-shm" "${db_path}-wal"
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
  RESTREAM_DB_PATH="$RESTREAM_DB_PATH" \
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
  if [[ "${ALLOW_GLOBAL_PROCESS_CLEANUP:-0}" == "1" ]]; then
    pkill -f 'mediamtx ' 2>/dev/null || true
    sleep 1
  fi
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

probe_dims_capture() {
  local url="$1" stderr_path="$2"
  local -a ffprobe_headers=()
  if [[ -n "${COOKIE_JAR:-}" && -f "$COOKIE_JAR" ]]; then
    case "$url" in
      "${API_URL}/hls/"*|"${API_URL}/preview/hls/"*)
        local cookie
        cookie="$(awk '$6 == "session" { print $6 "=" $7 }' "$COOKIE_JAR" | tail -n1)"
        [[ -n "$cookie" ]] && ffprobe_headers=(-headers $'Cookie: '"$cookie"$'\r\n')
        ;;
    esac
  fi
  ffprobe -v error \
    -probesize 10000000 -analyzeduration 10000000 \
    -select_streams v:0 -show_entries stream=width,height \
    -of csv=p=0 "${ffprobe_headers[@]}" "$url" 2>"$stderr_path" | tr ',' 'x' | head -n1 | tr -d '[:space:]'
}

probe_dims() {
  probe_dims_capture "$1" /dev/null
}

# verify_stream: fatal on timeout; 30 × 2 s = 60 s max
verify_stream() {
  local label="$1" url="$2" expected="$3"
  local assertion_id="${4:-ffprobe:${label}}"
  resume_allows "$assertion_id" || return 0
  local started_ms; started_ms="$(now_ms)"
  local dims=""
  local safe_id stderr_path raw_stderr
  safe_id="$(printf '%s' "$assertion_id" | tr -c 'A-Za-z0-9_.-' '_')"
  stderr_path="${WORK_DIR}/ffprobe-${safe_id}.stderr"
  echo "  probing: $label"
  for attempt in $(seq 1 30); do
    dims=$(probe_dims_capture "$url" "$stderr_path" || true)
    if [[ "$dims" == "$expected" ]]; then
      local elapsed=$(( $(now_ms) - started_ms ))
      emit_result "$assertion_id" "pass" "$elapsed" \
        ",\"label\":\"$(json_escape "$label")\",\"expected\":\"$(json_escape "$expected")\",\"got\":\"$(json_escape "$dims")\",\"url\":\"$(json_escape "$url")\""
      log_ok "ffprobe: $label → $dims"
      return 0
    fi
    [[ -n "$dims" ]] && echo "    attempt $attempt: got '$dims', want '$expected'" >&2
    sleep 2
  done
  raw_stderr="$(tail -20 "$stderr_path" 2>/dev/null || true)"
  local elapsed=$(( $(now_ms) - started_ms ))
  local context
  context=",\"label\":\"$(json_escape "$label")\",\"expected\":\"$(json_escape "$expected")\",\"got\":\"$(json_escape "${dims:-}")\",\"url\":\"$(json_escape "$url")\""
  context+=",\"ffprobe_command\":\"$(json_escape "ffprobe -v error -probesize 10000000 -analyzeduration 10000000 -select_streams v:0 -show_entries stream=width,height -of csv=p=0 $url")\""
  context+=",\"ffprobe_stderr\":\"$(json_escape "$raw_stderr")\""
  context+=",\"restream_log_tail\":$(json_tail_array "$RESTREAM_LOG" 30 "${CURRENT_CONFIG:-}")"
  context+=",\"mediamtx_log_tail\":$(json_tail_array "${WORK_DIR}/mediamtx.log" 10)"
  fail_assertion "$assertion_id" "ffprobe: $label — expected $expected, got '${dims:-<no output>}' from $url" "$elapsed" "$context"
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
  local sleep_secs="${SNAPSHOT_SLEEP_SECS:-3}"
  [[ "$sleep_secs" == "0" ]] || sleep "$sleep_secs"
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
  local sleep_secs="${SNAPSHOT_SLEEP_SECS:-3}"
  [[ "$sleep_secs" == "0" ]] || sleep "$sleep_secs"
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

rss_summary_get() {
  local file="$1" cfg="$2" key="$3"
  grep -E "^${cfg}," "$file" 2>/dev/null | tail -n1 | grep -o "${key}=[^,]*" | cut -d= -f2
}

write_rss_baseline() {
  local summary_file="$1" out_file="$2"
  mkdir -p "$(dirname "$out_file")"
  {
    echo "config,rss_delta_kb,per_output_kb,ext_ffmpeg_n,ext_ffmpeg_rss_kb"
    while IFS=',' read -r cfg _rest; do
      [[ -n "$cfg" ]] || continue
      printf "%s,%s,%s,%s,%s\n" \
        "$cfg" \
        "$(rss_summary_get "$summary_file" "$cfg" rss_delta_kb)" \
        "$(rss_summary_get "$summary_file" "$cfg" per_output_kb)" \
        "$(rss_summary_get "$summary_file" "$cfg" ext_ffmpeg_n)" \
        "$(rss_summary_get "$summary_file" "$cfg" ext_ffmpeg_rss_kb)"
    done < "$summary_file"
  } > "$out_file"
  echo "BASELINE: wrote $out_file"
}

compare_rss_baseline() {
  local summary_file="$1" baseline_file="$2"
  [[ -f "$baseline_file" ]] || fail "baseline file not found: $baseline_file"
  local threshold="${RSS_BASELINE_THRESHOLD_PCT:-5}"
  local regressions=0 cfg baseline _per _n _r current delta pct status
  while IFS=',' read -r cfg baseline _per _n _r; do
    [[ "$cfg" == "config" || -z "$cfg" ]] && continue
    current="$(rss_summary_get "$summary_file" "$cfg" rss_delta_kb)"
    [[ -n "$current" ]] || continue
    delta=$(( current - baseline ))
    if [[ "$baseline" -le 0 ]]; then
      if [[ "$current" -le 0 ]]; then pct=0; else pct=999; fi
    else
      pct=$(( delta * 100 / baseline ))
    fi
    if [[ "$delta" -gt 0 && "$pct" -gt "$threshold" ]]; then
      status="fail"
      regressions=$(( regressions + 1 ))
      printf "  REGRESSION %-22s baseline=%s current=%s delta=%+d (%+d%% > %s%%)\n" \
        "$cfg" "$baseline" "$current" "$delta" "$pct" "$threshold"
    else
      status="pass"
      printf "  baseline  %-22s baseline=%s current=%s delta=%+d (%+d%%)\n" \
        "$cfg" "$baseline" "$current" "$delta" "$pct"
    fi
    CURRENT_CONFIG="$cfg" emit_result "RSS-baseline-${cfg}" "$status" 0 \
      ",\"baseline_kb\":${baseline},\"current_kb\":${current},\"delta_kb\":${delta},\"threshold_pct\":${threshold},\"pct\":${pct}"
  done < "$baseline_file"
  [[ "$regressions" -eq 0 ]] || fail "RSS baseline regression(s): $regressions"
}


# ── Mode: ramp ─────────────────────────────────────────────────────────────────
# 8 configs (2 ingests × 2 egress protocols × 2 encodings), outputs added one-by-one.
# Per-step RSS+CPU+ffmpeg snapshots; ffprobe spot-checks first and last output.
# Env: N_OUTPUTS (default 10), ISOLATE=1 (restart per config), SNAP_EVERY (default 1)
run_ramp() {
  local N_OUTPUTS="${N_OUTPUTS:-10}"
  local ISOLATE="${ISOLATE:-0}"
  local SNAP_EVERY="${SNAP_EVERY:-1}"

  ulimit -n 65536 2>/dev/null || true

  WORK_DIR="${WORK_DIR:-test/artifacts/scale}"
  RESTREAM_LOG="${WORK_DIR}/restream.log"
  SCALE_LOG="${WORK_DIR}/scale.csv"
  SUMMARY_LOG="${WORK_DIR}/summary.txt"
  init_run_artifacts
  check_deps cargo ffmpeg ffprobe curl jq mediamtx

  printf "config,step,label,cpu_pct,rss_kb,ffmpeg_n,ffmpeg_rss_kb,total_rss_kb\n" > "$SCALE_LOG"
  : > "$SUMMARY_LOG"

  local RAMP_FAMILY_DEFAULTS="rtmp-rtmp-src rtmp-rtmp-720p rtmp-srt-src rtmp-srt-720p srt-rtmp-src srt-rtmp-720p srt-srt-src srt-srt-720p"
  local RAMP_FAMILY_SELECTED="${RAMP_FAMILY_CONFIGS:-$RAMP_FAMILY_DEFAULTS}"
  ramp_rust_owns_config() {
    local cfg="$1" item
    [[ "${RAMP_RUST_FAMILY:-rtmp-ingest}" != "0" ]] || return 1
    for item in $RAMP_FAMILY_SELECTED; do
      [[ "$item" == "$cfg" ]] && return 0
    done
    return 1
  }
  local RAMP_NEEDS_LEGACY=0
  for cfg in rtmp-rtmp-src rtmp-rtmp-720p rtmp-srt-src rtmp-srt-720p srt-rtmp-src srt-rtmp-720p srt-srt-src srt-srt-720p; do
    if ! ramp_rust_owns_config "$cfg"; then
      RAMP_NEEDS_LEGACY=1
      break
    fi
  done

  if [[ "${RAMP_RUST_FAMILY:-rtmp-rtmp}" != "0" ]]; then
    echo "  [rust] ramp-family: RTMP/SRT ingest → RTMP/SRT outputs"
    TEST_HARNESS_ARTIFACT_DIR="$WORK_DIR" \
    SCALE_LOG="$SCALE_LOG" \
    SUMMARY_LOG="$SUMMARY_LOG" \
    RAMP_FAMILY_CONFIGS="$RAMP_FAMILY_SELECTED" \
    cargo run --quiet --bin test_harness -- ramp-family \
      >"${WORK_DIR}/ramp-family.log" 2>&1 \
      || { tail -120 "${WORK_DIR}/ramp-family.log" >&2 || true; fail "Rust ramp-family harness failed"; }
  fi

  if [[ "$RAMP_NEEDS_LEGACY" == "1" ]]; then
    start_restream
    start_mediamtx
    COOKIE_JAR=$(mktemp)
    api POST /api/auth/login -d '{"password":"admin"}' >/dev/null
  fi

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

  ramp_rust_owns_config "rtmp-rtmp-src"  || run_scale_config "rtmp-rtmp-src"  rtmp rtmp source
  ramp_rust_owns_config "rtmp-rtmp-720p" || run_scale_config "rtmp-rtmp-720p" rtmp rtmp 720p
  ramp_rust_owns_config "rtmp-srt-src"   || run_scale_config "rtmp-srt-src"   rtmp srt  source
  ramp_rust_owns_config "rtmp-srt-720p"  || run_scale_config "rtmp-srt-720p"  rtmp srt  720p
  ramp_rust_owns_config "srt-rtmp-src"   || run_scale_config "srt-rtmp-src"   srt  rtmp source
  ramp_rust_owns_config "srt-rtmp-720p"  || run_scale_config "srt-rtmp-720p"  srt  rtmp 720p
  ramp_rust_owns_config "srt-srt-src"    || run_scale_config "srt-srt-src"    srt  srt  source
  ramp_rust_owns_config "srt-srt-720p"   || run_scale_config "srt-srt-720p"   srt  srt  720p

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
# 5 configs × N outputs per group (RTMP-src + RTMP-720p + SRT-src + SRT-720p).
# h264-rtmp: RTMP/FLV H.264 ingest baseline.
# h264-srt (anchor): HLS output + smoke check (no ext transcoder before 720p) +
#   fatal verify_stream across all protocol×encoding combos + stop lifecycle.
# h265-srt: asserts bounded shared internal h264-tc transcoders (TC_SPAWNS).
# h264-srt-multi / h265-srt-multi: multi-audio track routing.
# Env: N_PER_GROUP (default 25), ISOLATE=1 (default)
run_mixed_scale() {
  local N_PER_GROUP="${N_PER_GROUP:-25}"
  local ISOLATE="${ISOLATE:-1}"

  ulimit -n 65536 2>/dev/null || true

  WORK_DIR="${WORK_DIR:-test/artifacts/mixed-scale}"
  RESTREAM_LOG="${WORK_DIR}/restream.log"
  SCALE_LOG="${WORK_DIR}/scale.csv"
  SUMMARY_LOG="${WORK_DIR}/summary.txt"
  MIXED_LOG_INDEX="${WORK_DIR}/mixed-scale-logs.json"
  EXTRA_ARTIFACTS_JSON=", \"mixedScaleLogs\": \"$(json_escape "$MIXED_LOG_INDEX")\""
  init_run_artifacts
  check_deps cargo ffmpeg ffprobe curl jq mediamtx
  check_embedded_ffmpeg_fresh

  # RSS_SUMMARY is separate from SUMMARY_LOG so log_ok() "ok: ..." lines don't
  # pollute the CSV that the final summary table reads back.
  local RSS_SUMMARY="${WORK_DIR}/rss-summary.csv"
  printf "config,label,cpu_pct,rss_kb,ext_ffmpeg_n,ext_ffmpeg_rss_kb\n" > "$SCALE_LOG"
  : > "$RSS_SUMMARY"
  : > "$SUMMARY_LOG"
  cat >"$MIXED_LOG_INDEX" <<JSON
{
  "mixed-h264-rtmp": {
    "harnessLog": "$(json_escape "${WORK_DIR}/mixed-h264-rtmp.log")",
    "restreamLog": "$(json_escape "${WORK_DIR}/mixed-h264-rtmp-restream.log")"
  },
  "mixed-anchor": {
    "harnessLog": "$(json_escape "${WORK_DIR}/mixed-anchor.log")",
    "restreamLog": "$(json_escape "${WORK_DIR}/mixed-anchor-restream.log")"
  },
  "mixed-h265-srt": {
    "harnessLog": "$(json_escape "${WORK_DIR}/mixed-h265-srt.log")",
    "restreamLog": "$(json_escape "${WORK_DIR}/mixed-h265-srt-restream.log")"
  },
  "mixed-h264-srt-multi": {
    "harnessLog": "$(json_escape "${WORK_DIR}/mixed-h264-srt-multi.log")",
    "restreamLog": "$(json_escape "${WORK_DIR}/mixed-h264-srt-multi-restream.log")"
  },
  "mixed-h265-srt-multi": {
    "harnessLog": "$(json_escape "${WORK_DIR}/mixed-h265-srt-multi.log")",
    "restreamLog": "$(json_escape "${WORK_DIR}/mixed-h265-srt-multi-restream.log")"
  }
}
JSON

  run_mixed_rust_slice() {
    local command="$1" label="$2"
    echo "  [rust] ${command}: ${label}"
    TEST_HARNESS_ARTIFACT_DIR="$WORK_DIR" \
    WORK_DIR="$WORK_DIR" \
    SCALE_LOG="$SCALE_LOG" \
    RSS_SUMMARY="$RSS_SUMMARY" \
    SUMMARY_LOG="$SUMMARY_LOG" \
    ASSERTION_LOG="$ASSERTION_LOG" \
    ONLY_CHECKS="$ONLY_CHECKS" \
    RESUME_FROM="$RESUME_FROM" \
    SKIP_LOAD="$SKIP_LOAD" \
    N_PER_GROUP="$N_PER_GROUP" \
    SNAPSHOT_SLEEP_SECS="${SNAPSHOT_SLEEP_SECS:-3}" \
    cargo run --quiet --bin test_harness -- "$command" \
      >"${WORK_DIR}/${command}.log" 2>&1 \
      || { tail -160 "${WORK_DIR}/${command}.log" >&2 || true; fail "Rust ${command} harness failed"; }
  }

  run_mixed_rust_slice mixed-h264-rtmp "h264-rtmp RTMP ingest baseline"
  run_mixed_rust_slice mixed-anchor "h264-srt HLS/smoke/lifecycle"
  run_mixed_rust_slice mixed-h265-srt "h265-srt TC_SPAWNS"
  run_mixed_rust_slice mixed-h264-srt-multi "h264-srt-multi multi-audio"
  run_mixed_rust_slice mixed-h265-srt-multi "h265-srt-multi multi-audio"

  echo "  [rust] all mixed-scale configs delegated"

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
  if [[ -n "$BASELINE_PATH" ]]; then
    compare_rss_baseline "$RSS_SUMMARY" "$BASELINE_PATH"
  fi
  if [[ -n "$SAVE_BASELINE_PATH" ]]; then
    write_rss_baseline "$RSS_SUMMARY" "$SAVE_BASELINE_PATH"
  fi
}


# ── Mode: bonding ──────────────────────────────────────────────────────────────
# Verifies SRT socket-level bonding: broadcast group (2 members, failover=0, 1 message)
# and backup group (2 members, failover=1, 2 messages). Requires static build.
run_bonding() {
  init_run_artifacts

  local BUILD_ROOT="${RESTREAM_BUILD_ROOT:-$ROOT/.build/static}"

  if [[ ! -f "$BUILD_ROOT/env.sh" ]]; then
    "$ROOT/scripts/resource-limit" "$ROOT/scripts/setup-static-build.sh"
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

# ── burst-verify mode ─────────────────────────────────────────────────────────
# Streams a matrix of RTMP/SRT × h264/h265 × 4K/1080p × fps × single/dual audio
# with proper closed GOPs, then queries the pipeline graph API to verify that
# pull_burst instrumentation reports avgBurstSize > 0 and burstCount > 0 for
# every active ring buffer reader.
#
# Mode env overrides:
#   BURST_SETTLE_SECS   seconds to stream before sampling graph (default: 8)
#   BURST_CONFIGS       space-separated config names to run (default: all)
run_burst_verify() {
  WORK_DIR="${WORK_DIR:-test/artifacts/burst-verify}"
  RESTREAM_LOG="${WORK_DIR}/restream.log"
  SUMMARY_LOG="${WORK_DIR}/summary.txt"
  init_run_artifacts
  check_deps cargo ffmpeg jq
  : > "$SUMMARY_LOG"

  TEST_HARNESS_ARTIFACT_DIR="$WORK_DIR" \
    cargo run --quiet --bin test_harness -- burst-verify \
    >"${WORK_DIR}/test-harness.log" 2>&1 \
    || { tail -120 "${WORK_DIR}/test-harness.log" >&2 || true; fail "Rust burst-verify harness failed"; }

  local result_json="${WORK_DIR}/burst-verify.json"
  jq -r '.cases[] | "burst-verify: \(.config) - \(.burstOk) reader(s) with live burst stats"' "$result_json" \
    | while IFS= read -r line; do
        log_ok "$line"
      done
}

# ── hls-put mode ──────────────────────────────────────────────────────────────
# Publishes one SRT H.264/AAC input, starts HTTP HLS PUT outputs that target
# local YouTube-style ?file= and path-style sinks, and verifies playlist plus
# segment delivery for both shapes.
#
# Mode env overrides:
#   HLS_PUT_SETTLE_SECS seconds to wait for HLS upload (default: 8)
#   HLS_PUT_RESTART_SECS seconds to wait after sink restart (default: 12)
run_hls_put() {
  WORK_DIR="${WORK_DIR:-test/artifacts/hls-put}"
  RESTREAM_LOG="${WORK_DIR}/restream.log"
  SUMMARY_LOG="${WORK_DIR}/summary.txt"
  HLS_PUT_DIR="${WORK_DIR}/hls-put-sink"
  init_run_artifacts
  check_deps cargo ffmpeg ffprobe jq
  : > "$SUMMARY_LOG"

  HLS_PUT_DIR="$HLS_PUT_DIR" \
  HLS_PUT_PORT="$HLS_PUT_PORT" \
  TEST_HARNESS_ARTIFACT_DIR="$WORK_DIR" \
    cargo run --quiet --bin test_harness -- hls-put \
    >"${WORK_DIR}/test-harness.log" 2>&1 \
    || { tail -80 "${WORK_DIR}/test-harness.log" >&2 || true; fail "Rust hls-put harness failed"; }

  local result_json="${WORK_DIR}/hls-put.json"
  local youtube_dims akamai_dims
  youtube_dims="$(jq -r '.youtube.dimensions' "$result_json")"
  akamai_dims="$(jq -r '.akamai.dimensions' "$result_json")"
  log_ok "hls-put: YouTube-style playlist and segment uploaded via PUT with ffprobe dimensions ${youtube_dims}"
  log_ok "hls-put: path-style playlist and segment uploaded via PUT with signed query and ffprobe dimensions ${akamai_dims}"
  log_ok "hls-put: YouTube-style and path-style uploads recovered after dummy sink restart"
}

# ── bframe-rtmp mode ─────────────────────────────────────────────────────────
# Publishes one RTMP H.264/AAC input with B-frames, egresses it over RTMP, and
# verifies the egress packet stream preserves composition offsets (PTS > DTS)
# while keeping DTS monotone.
run_bframe_rtmp() {
  WORK_DIR="${WORK_DIR:-test/artifacts/bframe-rtmp}"
  RESTREAM_LOG="${WORK_DIR}/restream.log"
  SUMMARY_LOG="${WORK_DIR}/summary.txt"
  init_run_artifacts
  check_deps cargo ffmpeg ffprobe jq
  : > "$SUMMARY_LOG"

  TEST_HARNESS_ARTIFACT_DIR="$WORK_DIR" \
    cargo run --quiet --bin test_harness -- bframe-rtmp \
    >"${WORK_DIR}/test-harness.log" 2>&1 \
    || { tail -80 "${WORK_DIR}/test-harness.log" >&2 || true; fail "Rust bframe-rtmp harness failed"; }
  printf 'publisher managed by Rust test_harness bframe-rtmp\n' >"${WORK_DIR}/publisher.log"

  local result_json="${WORK_DIR}/bframe-rtmp.json"
  local packet_count bframe_count dts_monotone
  packet_count="$(jq -r '.packetCount' "$result_json")"
  bframe_count="$(jq -r '.bframeCount' "$result_json")"
  dts_monotone="$(jq -r '.dtsMonotone' "$result_json")"
  [[ "$packet_count" -ge 30 ]] || fail "expected at least 30 video packets, got ${packet_count}"
  [[ "$bframe_count" -gt 0 ]] || fail "RTMP egress did not expose any packets with PTS > DTS"
  [[ "$dts_monotone" == "true" ]] || fail "RTMP egress DTS values are not monotone"

  log_ok "bframe-rtmp: ${bframe_count}/${packet_count} packets had PTS>DTS and DTS stayed monotone"
}

run_harness_correctness() {
  local command="$1" label="$2"
  WORK_DIR="${WORK_DIR:-test/artifacts/${command}}"
  RESTREAM_LOG="${WORK_DIR}/restream.log"
  SUMMARY_LOG="${WORK_DIR}/summary.txt"
  init_run_artifacts
  check_deps cargo ffmpeg ffprobe jq
  : > "$SUMMARY_LOG"

  TEST_HARNESS_ARTIFACT_DIR="$WORK_DIR" \
    cargo run --quiet --bin test_harness -- "$command" \
    >"${WORK_DIR}/test-harness.log" 2>&1 \
    || { tail -100 "${WORK_DIR}/test-harness.log" >&2 || true; fail "Rust ${command} harness failed"; }
  printf 'scenario managed by Rust test_harness %s\n' "$command" >"${WORK_DIR}/restream.log"

  local result_json="${WORK_DIR}/${command}.json"
  local video_codec audio_codec
  jq -e '.passed == true' "$result_json" >/dev/null \
    || fail "${command} did not report passed=true"
  video_codec="$(jq -r '.videoCodec // "unknown"' "$result_json")"
  audio_codec="$(jq -r '.audioCodec // "unknown"' "$result_json")"
  log_ok "${command}: ${label} verified video=${video_codec} audio=${audio_codec}"
}

# ── Dispatch ───────────────────────────────────────────────────────────────────
mkdir -p "$WORK_DIR"

if [[ "$PREFLIGHT" == "1" ]]; then
  run_preflight
  exit $?
fi

case "$MODE" in
  ramp)         run_ramp         ;;
  mixed-scale)  run_mixed_scale  ;;
  bonding)      run_bonding      ;;
  burst-verify) run_burst_verify ;;
  hls-put)      run_hls_put      ;;
  bframe-rtmp)  run_bframe_rtmp  ;;
  correctness-srt-rtmp)
    run_harness_correctness "correctness-srt-rtmp" "SRT H.264/AAC to RTMP egress"
    ;;
  correctness-hevc-rtmp)
    run_harness_correctness "correctness-hevc-rtmp" "SRT H.265 to H.264 RTMP egress"
    ;;
  correctness-hevc-srt)
    run_harness_correctness "correctness-hevc-srt" "SRT H.265 passthrough"
    ;;
  *)
    echo "Unknown mode: $MODE" >&2
    echo "Valid modes: ramp mixed-scale bonding burst-verify hls-put bframe-rtmp correctness-srt-rtmp correctness-hevc-rtmp correctness-hevc-srt" >&2
    exit 1
    ;;
esac
