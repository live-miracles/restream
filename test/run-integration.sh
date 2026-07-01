#!/usr/bin/env bash
# Deprecated compatibility wrapper for direct test_harness usage.
# Prefer: scripts/resource-limit target/debug/test_harness <mode>
# Host-network opt-out is now: --no-netns
set -euo pipefail

BIN="${RESTREAM_TEST_HARNESS:-./target/debug/test_harness}"

if [[ "${1:-}" == "--host" ]]; then
	shift
	set -- --no-netns "$@"
fi

if [[ -z "${RESTREAM_SUPPRESS_WRAPPER_WARN:-}" ]]; then
	echo "[deprecated] test/run-integration.sh is a compatibility wrapper; use '$BIN' directly" >&2
fi

exec "$BIN" "$@"
