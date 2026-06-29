#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

run_loom_target() {
  local target="$1"
  local manifest_json
  manifest_json="$(mktemp)"
  scripts/resource-limit cargo rustc \
    --test "$target" \
    --message-format=json-render-diagnostics \
    -- \
    --cfg loom \
    > "$manifest_json"
  local binary
  binary="$(
    python3 - "$target" "$manifest_json" <<'PY'
import json
import sys

target_name = sys.argv[1]
manifest_path = sys.argv[2]
for line in open(manifest_path, encoding="utf-8"):
    line = line.strip()
    if not line:
        continue
    try:
        obj = json.loads(line)
    except json.JSONDecodeError:
        continue
    if obj.get("reason") == "compiler-artifact" and obj.get("target", {}).get("name") == target_name:
        executable = obj.get("executable")
        if executable:
            print(executable)
            break
PY
  )"
  rm -f "$manifest_json"
  if [[ -z "$binary" ]]; then
    echo "failed to locate compiled loom binary for $target" >&2
    exit 1
  fi
  "$binary" --nocapture
}

run_loom_target avio_loom
run_loom_target ring_migration_loom
run_loom_target ts_chunk_ring_loom
run_loom_target ts_muxer_stage_loom

scripts/resource-limit cargo test \
  health_endpoint_exposes_probe_and_egress_fault_fields \
  --test api -- --nocapture
scripts/resource-limit cargo test \
  output_status_and_health_preserve_recent_egress_failure_after_unregister \
  --test api -- --nocapture

scripts/resource-limit cargo test recent_egress --lib -- --nocapture
scripts/resource-limit cargo test --bin test_harness -- --nocapture
