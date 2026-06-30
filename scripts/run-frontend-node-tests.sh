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
  test/frontend-core-helpers.test.mjs
  test/frontend-history-helpers.test.mjs
  test/history-nearby-render.test.mjs
  test/overview-activity-render.test.mjs
  test/frontend-chaos-scenarios.test.mjs
  test/frontend-output-scenarios.test.mjs
  test/frontend-pipeline-info-scenarios.test.mjs
  test/frontend-dom-render.test.mjs
)

NODE_COVERAGE_EXCLUDES=(
  "public/ts/core/api.ts"
  "public/ts/core/state.ts"
  "public/ts/features/control-room.ts"
  "public/ts/features/dashboard-entry.ts"
  "public/ts/features/dashboard.ts"
  "public/ts/features/diagnostics.ts"
  "public/ts/features/editor.ts"
  "public/ts/features/graph.ts"
  "public/ts/features/hls-player.ts"
  "public/ts/features/input-preview.ts"
  "public/ts/features/media-library.ts"
  "public/ts/features/metric-format.ts"
  "public/ts/features/metrics.ts"
  "public/ts/features/modes.ts"
  "public/ts/features/pipeline-dependencies.ts"
  "public/ts/features/publisher-health.ts"
  "public/ts/features/settings.ts"
  "public/ts/features/status.ts"
  "public/ts/history/controller.ts"
  "public/ts/history/render.ts"
  "public/ts/history/state.ts"
)

if [[ "${1:-}" == "--coverage" ]]; then
  COVERAGE_ARGS=(
    "--enable-source-maps"
    "--experimental-test-coverage"
    "--test"
    "--test-coverage-exclude=test/**"
  )
  for file in "${NODE_COVERAGE_EXCLUDES[@]}"; do
    COVERAGE_ARGS+=("--test-coverage-exclude=$file")
  done
  node "${COVERAGE_ARGS[@]}" "${TEST_FILES[@]}"
  exit 0
fi

if [[ "${1:-}" == "--coverage-all" ]]; then
  node \
    --enable-source-maps \
    --experimental-test-coverage \
    --test \
    --test-coverage-exclude='test/**' \
    "${TEST_FILES[@]}"
  exit 0
fi

node --enable-source-maps --test "${TEST_FILES[@]}"
