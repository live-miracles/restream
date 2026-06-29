#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

echo "[fixture-discipline] validating checked-in fixture contract"
scripts/resource-limit cargo test --test fixtures -- --nocapture

declare -a SCAN_ROOTS=(
  "src"
  "tests"
  "test"
  "benches"
)
declare -a INLINE_GENERATOR_PATTERNS=(
  'lavfi'
  'testsrc'
  'testsrc2'
  'smptebars'
  'mandelbrot'
  'anullsrc'
  'anoisesrc'
  'sine='
)

echo "[fixture-discipline] scanning test and benchmark code for inline media generators"
if rg -n -S \
  -e "$(printf '%s|' "${INLINE_GENERATOR_PATTERNS[@]}" | sed 's/|$//')" \
  "${SCAN_ROOTS[@]}"; then
  cat >&2 <<'EOF'
[fixture-discipline] inline generator patterns found in test-facing code.
Use checked-in assets from test/fixtures/ or media/ via src/test_fixtures.rs.
If a case truly cannot be covered by an existing asset, add a dedicated fixture
generation workflow and document why the committed fixture set is insufficient.
EOF
  exit 1
fi

echo "[fixture-discipline] passed"
