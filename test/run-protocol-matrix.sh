#!/usr/bin/env bash
# Thin compatibility wrapper for the Rust protocol matrix orchestrator.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

exec cargo run --quiet --bin protocol_matrix -- "$@"
