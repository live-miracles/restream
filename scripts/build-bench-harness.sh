#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

if [[ -z "${TMPDIR:-}" || ! -d "${TMPDIR:-}" || ! -w "${TMPDIR:-}" ]]; then
  export TMPDIR=/tmp
fi

scripts/resource-limit cargo build --profile bench --bin restream --bin test_harness

for binary in target/bench/restream target/bench/test_harness; do
  if [[ ! -x "$binary" ]]; then
    echo "expected bench-profile binary missing: $binary" >&2
    exit 1
  fi
done

cat <<'EOF'
Bench-profile measurement binaries are ready:
  target/bench/restream
  target/bench/test_harness

Run measurement modes from target/bench/test_harness so the harness can reject
debug/release launches and keep benchmark numbers comparable.
EOF
