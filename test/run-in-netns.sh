#!/usr/bin/env bash
# run-in-netns.sh — run the mixed scale test inside a private network namespace
#
# Uses "unshare --net --user --map-root-user" (no sudo required) to get a fresh
# loopback-only network namespace so ports 1935, 10080, 1936, 8891, 9997, 3030
# never conflict with anything running on the host.
#
# ISOLATE is forced to 0: a single restream+mediamtx pair starts at the top
# and serves all five ingest configurations.  The netns itself provides full
# port isolation from the host, so per-config restarts are unnecessary.
#
# Usage:
#   test/run-in-netns.sh [any extra env vars passed to run-mixed-scale-test.sh]
#
# Examples:
#   N_PER_GROUP=2  test/run-in-netns.sh
#   N_PER_GROUP=25 RESTREAM_USE_INTERNAL_TRANSCODER=1 test/run-in-netns.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"

export ISOLATE=0
export RESTREAM_USE_INTERNAL_TRANSCODER="${RESTREAM_USE_INTERNAL_TRANSCODER:-1}"

exec unshare --net --user --map-root-user \
  bash -c '
    set -euo pipefail
    # Bring loopback up — it starts DOWN in every new network namespace
    ip link set lo up
    echo "[netns] loopback up: $(ip addr show lo | grep "inet " | awk "{print \$2}")"
    exec "$@"
  ' -- \
  bash "$ROOT/test/run-mixed-scale-test.sh" "$@"
