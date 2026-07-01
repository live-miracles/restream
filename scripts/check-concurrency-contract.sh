#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"
source "$ROOT_DIR/scripts/concurrency-proof-common.sh"

LOG_DIR="$ROOT_DIR/test/artifacts/concurrency-contract-logs"
mkdir -p "$LOG_DIR"

cleanup_runtime() {
  pkill -x restream >/dev/null 2>&1 || true
  pkill -x mediamtx >/dev/null 2>&1 || true
  pkill -x ffmpeg >/dev/null 2>&1 || true
  pkill -x test_harness >/dev/null 2>&1 || true
}

run_logged() {
  local label="$1"
  shift
  local log_file="$LOG_DIR/${label}.log"

  if ! "$@" >"$log_file" 2>&1; then
    cat "$log_file"
    return 1
  fi
}

run_harness_mode() {
  local mode="$1"
  local work_dir="$2"
  local log_file="$LOG_DIR/${mode}.log"

  cleanup_runtime
  if ! RESTREAM_BIN=target/debug/restream \
    WORK_DIR="$work_dir" \
    target/debug/test_harness "$mode" >"$log_file" 2>&1; then
    cat "$log_file"
    return 1
  fi
  cleanup_runtime
}

trap cleanup_runtime EXIT

run_logged history-grouping bash scripts/check-history-grouping.sh

run_common_concurrency_checks run_logged
run_logged build-harness-bins scripts/resource-limit cargo build --bin restream --bin test_harness

run_harness_mode fault-resilience test/artifacts/concurrency-contract

run_harness_mode fault-egress-retry test/artifacts/concurrency-fault-egress-retry

run_harness_mode fault-output-stall test/artifacts/concurrency-fault-output-stall

run_harness_mode recovery test/artifacts/concurrency-recovery
