#!/usr/bin/env bash
# run-protocol-matrix.sh - aggregate runner for checked-in live integration modes.
#
# Each mode still owns its per-mode manifest through run-integration.sh. This
# wrapper adds a run-level artifact directory and manifest so release evidence
# can be published as one auditable matrix.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
RUNNER="${ROOT}/test/run-integration.sh"

DEFAULT_MODES=(ramp mixed-scale bonding burst-verify hls-put bframe-rtmp)
MODES=("${DEFAULT_MODES[@]}")
RUN_ID="${RUN_ID:-$(date -u +%Y%m%dT%H%M%SZ)}"
WORK_ROOT="${WORK_ROOT:-}"
WORK_ROOT_EXPLICIT=0
RESTREAM_BIN_ARG=""
CONTINUE_ON_FAIL=0
STARTED_AT="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
RUN_FLAGS=()
EXTRA_ARGS=()

usage() {
  cat <<USAGE
Usage: $0 [options] [-- extra run-integration args]

Options:
  --run-id <id>           Artifact run id (default: UTC timestamp)
  --work-root <path>      Aggregate artifact root (default: test/artifacts/<run-id>)
  --only-modes <list>     Comma-separated mode list
  --host                  Pass --host to run-integration.sh
  --fast                  Pass --fast to run-integration.sh
  --skip-load             Pass --skip-load to run-integration.sh
  --continue-on-fail      Run remaining modes after a failure
  --restream-bin <path>   RESTREAM_BIN for non-bonding modes
  -h, --help              Show this help

Default modes: ${DEFAULT_MODES[*]}
USAGE
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --run-id)
      [[ $# -ge 2 ]] || { echo "--run-id requires a value" >&2; exit 2; }
      RUN_ID="$2"
      if [[ "$WORK_ROOT_EXPLICIT" == "0" ]]; then
        WORK_ROOT=""
      fi
      shift 2
      ;;
    --work-root)
      [[ $# -ge 2 ]] || { echo "--work-root requires a path" >&2; exit 2; }
      WORK_ROOT="$2"
      WORK_ROOT_EXPLICIT=1
      shift 2
      ;;
    --only-modes)
      [[ $# -ge 2 ]] || { echo "--only-modes requires a comma-separated list" >&2; exit 2; }
      IFS=',' read -ra MODES <<< "$2"
      shift 2
      ;;
    --host|--fast|--skip-load)
      RUN_FLAGS+=("$1")
      shift
      ;;
    --continue-on-fail)
      CONTINUE_ON_FAIL=1
      shift
      ;;
    --restream-bin)
      [[ $# -ge 2 ]] || { echo "--restream-bin requires a path" >&2; exit 2; }
      RESTREAM_BIN_ARG="$2"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    --)
      shift
      while [[ $# -gt 0 ]]; do EXTRA_ARGS+=("$1"); shift; done
      ;;
    *)
      echo "Unknown option: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

WORK_ROOT="${WORK_ROOT:-${ROOT}/test/artifacts/${RUN_ID}}"
case "$WORK_ROOT" in
  /*) ;;
  *) WORK_ROOT="${ROOT}/${WORK_ROOT}" ;;
esac

RESULTS_JSONL="${WORK_ROOT}/results.jsonl"
MATRIX_MANIFEST="${WORK_ROOT}/manifest.json"

json_escape() {
  local value="$1"
  value=${value//\\/\\\\}
  value=${value//\"/\\\"}
  value=${value//$'\n'/\\n}
  printf '%s' "$value"
}

json_string_array() {
  local sep="" item
  printf '['
  for item in "$@"; do
    printf '%s"%s"' "$sep" "$(json_escape "$item")"
    sep=','
  done
  printf ']'
}

write_matrix_manifest() {
  local status="$1" finished_at="${2:-}" git_head finished_json
  git_head="$(git -C "$ROOT" rev-parse --short HEAD 2>/dev/null || echo unknown)"
  if [[ -n "$finished_at" ]]; then
    finished_json="\"$(json_escape "$finished_at")\""
  else
    finished_json="null"
  fi
  cat > "$MATRIX_MANIFEST" <<JSON
{
  "kind": "protocol-matrix",
  "status": "$(json_escape "$status")",
  "runId": "$(json_escape "$RUN_ID")",
  "startedAt": "$(json_escape "$STARTED_AT")",
  "finishedAt": ${finished_json},
  "gitHead": "$(json_escape "$git_head")",
  "workRoot": "$(json_escape "$WORK_ROOT")",
  "modes": $(json_string_array "${MODES[@]}"),
  "resultsJsonl": "$(json_escape "$RESULTS_JSONL")"
}
JSON
}

append_result() {
  local mode="$1" status="$2" preflight_status="$3" exit_code="$4" started_at="$5" finished_at="$6" mode_dir="$7"
  printf '{"mode":"%s","status":"%s","preflightStatus":"%s","exitCode":%s,"startedAt":"%s","finishedAt":"%s","workDir":"%s","manifest":"%s","assertions":"%s","preflight":"%s","log":"%s"}\n' \
    "$(json_escape "$mode")" \
    "$(json_escape "$status")" \
    "$(json_escape "$preflight_status")" \
    "$exit_code" \
    "$(json_escape "$started_at")" \
    "$(json_escape "$finished_at")" \
    "$(json_escape "$mode_dir")" \
    "$(json_escape "${mode_dir}/manifest.json")" \
    "$(json_escape "${mode_dir}/assertions.jsonl")" \
    "$(json_escape "${mode_dir}/preflight.jsonl")" \
    "$(json_escape "${mode_dir}/run.log")" >> "$RESULTS_JSONL"
}

run_mode() {
  local mode="$1" mode_dir="${WORK_ROOT}/${mode}" started finished exit_code=0 preflight_status=PASS status=PASS
  started="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  mkdir -p "$mode_dir"

  echo "[matrix] preflight ${mode}"
  if ! WORK_DIR="$mode_dir" RESTREAM_BIN="${RESTREAM_BIN_ARG:-${RESTREAM_BIN:-}}" \
    "$RUNNER" "${RUN_FLAGS[@]}" --preflight --json "${mode_dir}/preflight.jsonl" "${EXTRA_ARGS[@]}" "$mode" \
    >"${mode_dir}/preflight.log" 2>&1; then
    preflight_status=FAIL
    status=FAIL
    exit_code=1
  fi

  if [[ "$status" == "PASS" ]]; then
    echo "[matrix] run ${mode}"
    if ! WORK_DIR="$mode_dir" RESTREAM_BIN="${RESTREAM_BIN_ARG:-${RESTREAM_BIN:-}}" \
      "$RUNNER" "${RUN_FLAGS[@]}" --json "${mode_dir}/assertions.jsonl" "${EXTRA_ARGS[@]}" "$mode" \
      >"${mode_dir}/run.log" 2>&1; then
      status=FAIL
      exit_code=1
    fi
  fi

  finished="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  append_result "$mode" "$status" "$preflight_status" "$exit_code" "$started" "$finished" "$mode_dir"
  echo "[matrix] ${mode}: ${status}"
  [[ "$status" == "PASS" ]]
}

mkdir -p "$WORK_ROOT"
: > "$RESULTS_JSONL"
write_matrix_manifest "RUNNING"

overall=0
for mode in "${MODES[@]}"; do
  mode="${mode//[[:space:]]/}"
  [[ -n "$mode" ]] || continue
  if ! run_mode "$mode"; then
    overall=1
    if [[ "$CONTINUE_ON_FAIL" != "1" ]]; then
      break
    fi
  fi
done

if [[ "$overall" -eq 0 ]]; then
  write_matrix_manifest "PASS" "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
else
  write_matrix_manifest "FAIL" "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
fi

echo "[matrix] manifest=${MATRIX_MANIFEST}"
exit "$overall"
