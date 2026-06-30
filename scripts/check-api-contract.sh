#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP_DIR="${TMPDIR:-/tmp}/restream-api-contract-js"

cd "$ROOT_DIR"
rm -rf "$TMP_DIR"

npx tsc -p tsconfig.json --noEmit
node ./scripts/check-api-drift.mjs
npx tsc -p tsconfig.json --outDir "$TMP_DIR"
API_CONTRACT_JS_DIR="$TMP_DIR" node --test test/frontend-api-contract.test.mjs
bash scripts/check-history-grouping.sh
scripts/resource-limit cargo test --test api -- --nocapture
scripts/resource-limit cargo build --bin restream --bin test_harness
RESTREAM_BIN=target/debug/restream \
  WORK_DIR=test/artifacts/api-contract-smoke \
  target/debug/test_harness api-smoke
