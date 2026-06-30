#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP_BASE="${TMPDIR:-/tmp}"
BUILD_DIR="$(mktemp -d "${TMP_BASE}/restream-frontend-node-test-js.XXXXXX")"
KEEP_BUILD_DIR="${FRONTEND_NODE_TEST_KEEP_BUILD_DIR:-0}"

cleanup() {
  if [[ "$KEEP_BUILD_DIR" != "1" ]]; then
    rm -rf "$BUILD_DIR"
  fi
}

trap cleanup EXIT

cd "$ROOT_DIR"

npx tsc -p tsconfig.frontend-node-test.json --outDir "$BUILD_DIR"

export FRONTEND_MODULES_DIR="$BUILD_DIR"
export TMPDIR="$TMP_BASE"

TEST_FILES=(
  test/frontend-api-contract.test.mjs
  test/history-nearby-render.test.mjs
  test/overview-activity-render.test.mjs
  test/frontend-chaos-scenarios.test.mjs
  test/frontend-dom-render.test.mjs
)

if [[ "${1:-}" == "--coverage" ]]; then
  node \
    --enable-source-maps \
    --experimental-test-coverage \
    --test \
    --test-coverage-exclude='test/**' \
    "${TEST_FILES[@]}"
  exit 0
fi

node --enable-source-maps --test "${TEST_FILES[@]}"
