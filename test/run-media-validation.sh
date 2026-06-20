#!/usr/bin/env bash
set -euo pipefail

# Bounded validation for development machines and WSL:
# - real RTMP and SRT publishers/readers
# - 500 in-process readers
# - 32 loopback RTMP egress sessions

mkdir -p test/artifacts/latest
cargo run --release --bin test_harness -- all \
  | tee test/artifacts/latest/run.log
