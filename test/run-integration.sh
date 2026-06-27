#!/usr/bin/env bash
# Thin wrapper: re-exec test_harness inside a private loopback network
# namespace so ports never conflict with the host.  Pass --host to skip.
set -euo pipefail
BIN="${RESTREAM_TEST_HARNESS:-./target/release/test_harness}"
[[ "${1:-}" == "--host" ]] && shift && exec "$BIN" "$@"
exec unshare --net --user --map-root-user "$0" --host "$@"
