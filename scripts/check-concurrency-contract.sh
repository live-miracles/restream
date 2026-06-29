#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

find_test_binary() {
  local stem="$1"
  find target/debug/deps -maxdepth 1 -type f -name "${stem}-*" -executable -print \
    | grep -vE '\.(d|rlib|rmeta|dSYM)$' \
    | xargs -r ls -t \
    | head -n1
}

run_loom_target() {
  local target="$1"
  scripts/resource-limit cargo rustc --test "$target" -- --cfg loom
  local binary
  binary="$(find_test_binary "$target")"
  if [[ -z "$binary" ]]; then
    echo "failed to locate compiled loom binary for $target" >&2
    exit 1
  fi
  "$binary" --nocapture
}

run_loom_target avio_loom
run_loom_target ring_migration_loom

scripts/resource-limit cargo test health_endpoint_exposes_probe_and_egress_fault_fields --test api -- --nocapture
scripts/resource-limit cargo build --bin restream --bin test_harness

RESTREAM_BIN=target/debug/restream \
  WORK_DIR=test/artifacts/concurrency-contract \
  target/debug/test_harness fault-resilience
