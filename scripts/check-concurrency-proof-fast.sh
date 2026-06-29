#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

for target in avio_loom ring_migration_loom ts_chunk_ring_loom ts_muxer_stage_loom; do
  ./scripts/run-loom-target.sh "$target"
done

scripts/resource-limit cargo test \
  health_endpoint_exposes_probe_and_egress_fault_fields \
  --test api -- --nocapture
scripts/resource-limit cargo test \
  output_status_and_health_preserve_recent_egress_failure_after_unregister \
  --test api -- --nocapture
scripts/resource-limit cargo test \
  active_output_status_ignores_stale_retry_state_after_restart \
  --test api -- --nocapture
scripts/resource-limit cargo test \
  stale_job_update_cannot_clobber_replacement_attempt \
  --test db -- --nocapture

scripts/resource-limit cargo test recent_egress --lib -- --nocapture
scripts/resource-limit cargo test late_retry_state_update_is_ignored_after_output_restarts --lib -- --nocapture
scripts/resource-limit cargo test output_status_surfaces_retry_backoff_after_failure --lib -- --nocapture
scripts/resource-limit cargo test --bin test_harness -- --nocapture
