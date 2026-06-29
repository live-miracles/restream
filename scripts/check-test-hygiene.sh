#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LOG_FILE="$(mktemp "${TMPDIR:-/tmp}/restream-test-hygiene.XXXXXX.log")"
trap 'rm -f "$LOG_FILE"' EXIT

cd "$ROOT_DIR"

echo "[test-hygiene] checking Rust formatting with pinned toolchain"
CARGO_TERM_COLOR=never cargo fmt --all --check

echo "[test-hygiene] running Rust test graph with captured output"
if ! CARGO_TERM_COLOR=never scripts/resource-limit cargo test --workspace -- --nocapture \
  2>&1 | tee "$LOG_FILE"; then
  echo "[test-hygiene] cargo test failed before noise scan" >&2
  exit 1
fi

declare -a NOISE_PATTERNS=(
  'warning:'
  'panicked at'
  'proptest:'
  'failed to find lib.rs or main.rs'
  'specified frame type is not compatible with max B-frames'
  'Could not find codec parameters'
  'not enough frames to estimate rate'
  'ensure_ffmpeg_extracted\(\) must be called before ffmpeg_bin_path\(\)'
  '405 Method Not Allowed'
  'Blocking waiting for file lock on build directory'
  'resource-limit: waiting for another build to finish'
)

echo "[test-hygiene] scanning passing log for known noisy patterns"
if rg -n -e "$(printf '%s|' "${NOISE_PATTERNS[@]}" | sed 's/|$//')" "$LOG_FILE"; then
  cat >&2 <<'EOF'
[test-hygiene] noisy output detected in a passing test run.
Quiet the helper or test harness at the source instead of teaching CI to ignore it.
EOF
  exit 1
fi

echo "[test-hygiene] passed"
