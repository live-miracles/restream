#!/usr/bin/env bash
set -euo pipefail

# Bounded validation for development machines and WSL:
# - real RTMP and SRT publishers/readers
# - 500 in-process readers
# - 32 loopback RTMP egress sessions

mkdir -p test/artifacts/latest
cargo build --release --bin test_harness

: > test/artifacts/latest/run.log
for mode in correctness-rtmp correctness-srt egress in-process network; do
  echo "== $mode ==" | tee -a test/artifacts/latest/run.log
  target/release/test_harness "$mode" \
    | tee -a test/artifacts/latest/run.log
done
