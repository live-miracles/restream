#!/usr/bin/env bash
# Thin compatibility wrapper for the Rust protocol matrix orchestrator.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

DEFAULT_MODES=(
  ramp
  mixed-scale
  bonding
  burst-verify
  hls-put
  bframe-rtmp
  correctness-srt-rtmp
  correctness-hevc-rtmp
  correctness-hevc-srt
)

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
  --preflight-only        Run aggregate preflight for all selected modes only
  --restream-bin <path>   RESTREAM_BIN for non-bonding modes
  --list-modes            Print default mode names without building
  -h, --help              Show this help without building

Default modes: ${DEFAULT_MODES[*]}
USAGE
}

mode_exists() {
  local wanted="$1" mode
  for mode in "${DEFAULT_MODES[@]}"; do
    [[ "$mode" == "$wanted" ]] && return 0
  done
  return 1
}

validate_mode_list() {
  local list="$1" mode count=0
  IFS=',' read -ra _matrix_modes <<< "$list"
  for mode in "${_matrix_modes[@]}"; do
    mode="${mode//[[:space:]]/}"
    if [[ -z "$mode" ]]; then
      continue
    fi
    count=$((count + 1))
    if ! mode_exists "$mode"; then
      echo "Unknown protocol matrix mode: $mode" >&2
      echo "Known modes: ${DEFAULT_MODES[*]}" >&2
      exit 2
    fi
  done
  unset _matrix_modes
  if [[ "$count" -eq 0 ]]; then
    echo "--only-modes did not include any modes" >&2
    exit 2
  fi
}

args=("$@")
i=0
while [[ "$i" -lt "${#args[@]}" ]]; do
  case "${args[$i]}" in
    -h|--help)
      usage
      exit 0
      ;;
    --list-modes)
      printf '%s\n' "${DEFAULT_MODES[@]}"
      exit 0
      ;;
    --only-modes)
      if [[ $((i + 1)) -ge "${#args[@]}" ]]; then
        echo "--only-modes requires a comma-separated mode list" >&2
        exit 2
      fi
      validate_mode_list "${args[$((i + 1))]}"
      i=$((i + 2))
      ;;
    --)
      break
      ;;
    *)
      i=$((i + 1))
      ;;
  esac
done

exec env RESTREAM_PROTOCOL_MATRIX_ONLY=1 \
  "$ROOT/scripts/resource-limit" cargo run --quiet --bin protocol_matrix -- "$@"
