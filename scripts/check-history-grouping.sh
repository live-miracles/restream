#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP_DIR="${TMPDIR:-/tmp}/restream-history-grouping-js"

cd "$ROOT_DIR"
rm -rf "$TMP_DIR"

npx tsc -p tsconfig.json --outDir "$TMP_DIR"
API_CONTRACT_JS_DIR="$TMP_DIR" node --test \
  test/history-nearby-render.test.mjs \
  test/overview-activity-render.test.mjs \
  test/frontend-chaos-scenarios.test.mjs
