#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: $0 <loom-test-target>" >&2
  exit 1
fi

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

if ! command -v jq >/dev/null 2>&1; then
  echo "jq is required to resolve compiled loom test binaries" >&2
  exit 1
fi

target="$1"
manifest_json="$(mktemp)"
trap 'rm -f "$manifest_json"' EXIT

scripts/resource-limit cargo rustc \
  --test "$target" \
  --message-format=json-render-diagnostics \
  -- \
  --cfg loom \
  > "$manifest_json"

binary="$(
  jq -r --arg target "$target" '
    select(
      .reason == "compiler-artifact"
      and .target.name == $target
      and .executable != null
    )
    | .executable
  ' "$manifest_json" | tail -n 1
)"

if [[ -z "$binary" || "$binary" == "null" ]]; then
  echo "failed to locate compiled loom binary for $target" >&2
  exit 1
fi

"$binary" --nocapture
