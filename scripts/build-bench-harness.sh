#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

scripts/resource-limit cargo build --profile bench --bin restream --bin test_harness

install -Dm755 target/release/restream target/bench/restream
install -Dm755 target/release/test_harness target/bench/test_harness

cat <<'EOF'
Staged bench-profile measurement binaries:
  target/bench/restream
  target/bench/test_harness

Run measurement modes from target/bench/test_harness so the harness can reject
debug/release launches and keep benchmark numbers comparable.
EOF
