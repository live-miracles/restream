#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

LOG_DIR="$ROOT_DIR/test/artifacts/concurrency-contract-logs"
mkdir -p "$LOG_DIR"

cleanup_runtime() {
  pkill -x restream >/dev/null 2>&1 || true
  pkill -x mediamtx >/dev/null 2>&1 || true
  pkill -x ffmpeg >/dev/null 2>&1 || true
  pkill -x test_harness >/dev/null 2>&1 || true
}

run_logged() {
  local label="$1"
  shift
  local log_file="$LOG_DIR/${label}.log"

  if ! "$@" >"$log_file" 2>&1; then
    cat "$log_file"
    return 1
  fi
}

run_harness_mode() {
  local mode="$1"
  local work_dir="$2"
  local log_file="$LOG_DIR/${mode}.log"

  cleanup_runtime
  if ! RESTREAM_BIN=target/debug/restream \
    WORK_DIR="$work_dir" \
    target/debug/test_harness "$mode" >"$log_file" 2>&1; then
    cat "$log_file"
    return 1
  fi
  cleanup_runtime
}

trap cleanup_runtime EXIT

run_logged history-grouping bash scripts/check-history-grouping.sh

for target in avio_loom ring_migration_loom ts_chunk_ring_loom ts_muxer_stage_loom transcoder_stage_loom; do
  run_logged "loom-${target}" ./scripts/run-loom-target.sh "$target"
done

run_logged api-health \
  scripts/resource-limit cargo test health_endpoint_exposes_probe_and_egress_fault_fields --test api -- --nocapture
run_logged api-output-recent-failure \
  scripts/resource-limit cargo test output_status_and_health_preserve_recent_egress_failure_after_unregister --test api -- --nocapture
run_logged api-output-restart-retry \
  scripts/resource-limit cargo test active_output_status_ignores_stale_retry_state_after_restart --test api -- --nocapture
run_logged output-status-active \
  scripts/resource-limit cargo test active_output_status_matches_health_runtime_fields --test output_status_contract -- --nocapture
run_logged output-status-stalled \
  scripts/resource-limit cargo test stalled_output_status_matches_health_runtime_fields --test output_status_contract -- --nocapture
run_logged api-disconnect-clears \
  scripts/resource-limit cargo test health_endpoint_clears_recent_disconnect_details_after_reconnect --test api -- --nocapture
run_logged api-disconnect-flapping \
  scripts/resource-limit cargo test health_endpoint_surfaces_repeated_transient_disconnects_as_flapping --test api -- --nocapture
run_logged api-egress-flapping \
  scripts/resource-limit cargo test recovered_output_surfaces_flapping_after_repeated_sink_failures --test api -- --nocapture
run_logged db-stale-job-update \
  scripts/resource-limit cargo test stale_job_update_cannot_clobber_replacement_attempt --test db -- --nocapture
run_logged db-multiple-stale-job-updates \
  scripts/resource-limit cargo test multiple_stale_job_updates_cannot_clobber_newest_attempt --test db -- --nocapture
run_logged lib-stale-ingest-unregister \
  scripts/resource-limit cargo test stale_ingest_unregister_cannot_clobber_replacement_attempt --lib -- --nocapture
run_logged lib-stale-ingest-disconnect \
  scripts/resource-limit cargo test stale_ingest_disconnect_cannot_poison_replacement_attempt --lib -- --nocapture
run_logged lib-stale-egress-unregister \
  scripts/resource-limit cargo test stale_egress_unregister_cannot_clobber_replacement_attempt --lib -- --nocapture
run_logged lib-stale-egress-error \
  scripts/resource-limit cargo test stale_egress_error_cannot_poison_replacement_attempt --lib -- --nocapture
run_logged lib-stale-egress-queue \
  scripts/resource-limit cargo test stale_egress_queue_removal_cannot_drop_replacement_queue --lib -- --nocapture
run_logged ring-proptest \
  scripts/resource-limit cargo test prop_no_loss_no_gap_no_duplication --test ring_migration -- --nocapture
run_logged lib-avio-batch \
  scripts/resource-limit cargo test write_batch_round_trips_random_chunks --lib -- --nocapture
run_logged lib-srt-epoll \
  scripts/resource-limit cargo test epoll_waiter_coordination --lib -- --nocapture
run_logged lib-ingest-grace \
  scripts/resource-limit cargo test recent_ingest_disconnect_respects_grace_window --lib -- --nocapture
run_logged lib-ingest-flap-window \
  scripts/resource-limit cargo test build_recent_ingest_outcome_resets_flap_streak_outside_window --lib -- --nocapture
run_logged lib-ingest-proptest \
  scripts/resource-limit cargo test prop_ingest_lifecycle_preserves_health_invariants --lib -- --nocapture
run_logged lib-egress-flap-window \
  scripts/resource-limit cargo test build_recent_egress_outcome_resets_flap_streak_outside_window --lib -- --nocapture
run_logged lib-health-reconnect-flapping \
  scripts/resource-limit cargo test health_snapshot_surfaces_flapping_after_repeated_reconnects --lib -- --nocapture
run_logged lib-health-egress-flapping \
  scripts/resource-limit cargo test health_snapshot_surfaces_flapping_after_repeated_egress_recoveries --lib -- --nocapture
run_logged lib-late-retry-state \
  scripts/resource-limit cargo test late_retry_state_update_is_ignored_after_output_restarts --lib -- --nocapture
run_logged lib-multi-late-retry-state \
  scripts/resource-limit cargo test repeated_late_retry_updates_cannot_poison_newest_output_attempt --lib -- --nocapture
run_logged lib-output-retry-backoff \
  scripts/resource-limit cargo test output_status_surfaces_retry_backoff_after_failure --lib -- --nocapture
run_logged lib-egress-proptest \
  scripts/resource-limit cargo test prop_egress_lifecycle_preserves_runtime_and_health_invariants --lib -- --nocapture
run_logged build-harness-bins scripts/resource-limit cargo build --bin restream --bin test_harness

run_harness_mode fault-resilience test/artifacts/concurrency-contract

run_harness_mode fault-egress-retry test/artifacts/concurrency-fault-egress-retry

run_harness_mode fault-output-stall test/artifacts/concurrency-fault-output-stall

run_harness_mode recovery test/artifacts/concurrency-recovery
