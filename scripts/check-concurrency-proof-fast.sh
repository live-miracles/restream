#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

source "$ROOT_DIR/scripts/concurrency-proof-common.sh"

run_step() {
  local _label="$1"
  shift
  "$@"
}

run_common_concurrency_checks run_step

scripts/resource-limit cargo test --bin test_harness -- --nocapture
