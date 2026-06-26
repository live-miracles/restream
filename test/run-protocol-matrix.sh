#!/usr/bin/env bash
# Thin compatibility wrapper for the Rust protocol matrix orchestrator.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

DEFAULT_MODES=(ramp mixed-scale bonding burst-verify hls-put bframe-rtmp)

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

case "${1:-}" in
  -h|--help)
    usage
    exit 0
    ;;
  --list-modes)
    printf '%s\n' "${DEFAULT_MODES[@]}"
    exit 0
    ;;
esac

exec "$ROOT/scripts/resource-limit" cargo run --quiet --bin protocol_matrix -- "$@"
