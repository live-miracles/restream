#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

bash scripts/check-history-grouping.sh

for target in avio_loom ring_migration_loom ts_chunk_ring_loom ts_muxer_stage_loom transcoder_stage_loom; do
  ./scripts/run-loom-target.sh "$target"
done

scripts/resource-limit cargo test health_endpoint_exposes_probe_and_egress_fault_fields --test api -- --nocapture
scripts/resource-limit cargo test output_status_and_health_preserve_recent_egress_failure_after_unregister --test api -- --nocapture
scripts/resource-limit cargo test active_output_status_ignores_stale_retry_state_after_restart --test api -- --nocapture
scripts/resource-limit cargo test health_endpoint_clears_recent_disconnect_details_after_reconnect --test api -- --nocapture
scripts/resource-limit cargo test health_endpoint_surfaces_repeated_transient_disconnects_as_flapping --test api -- --nocapture
scripts/resource-limit cargo test recovered_output_surfaces_flapping_after_repeated_sink_failures --test api -- --nocapture
scripts/resource-limit cargo test stale_job_update_cannot_clobber_replacement_attempt --test db -- --nocapture
scripts/resource-limit cargo test multiple_stale_job_updates_cannot_clobber_newest_attempt --test db -- --nocapture
scripts/resource-limit cargo test stale_ingest_unregister_cannot_clobber_replacement_attempt --lib -- --nocapture
scripts/resource-limit cargo test stale_ingest_disconnect_cannot_poison_replacement_attempt --lib -- --nocapture
scripts/resource-limit cargo test stale_egress_unregister_cannot_clobber_replacement_attempt --lib -- --nocapture
scripts/resource-limit cargo test stale_egress_error_cannot_poison_replacement_attempt --lib -- --nocapture
scripts/resource-limit cargo test stale_egress_queue_removal_cannot_drop_replacement_queue --lib -- --nocapture
scripts/resource-limit cargo test prop_no_loss_no_gap_no_duplication --test ring_migration -- --nocapture
scripts/resource-limit cargo test write_batch_round_trips_random_chunks --lib -- --nocapture
scripts/resource-limit cargo test epoll_waiter_coordination --lib -- --nocapture
scripts/resource-limit cargo test recent_ingest_disconnect_respects_grace_window --lib -- --nocapture
scripts/resource-limit cargo test build_recent_ingest_outcome_resets_flap_streak_outside_window --lib -- --nocapture
scripts/resource-limit cargo test prop_ingest_lifecycle_preserves_health_invariants --lib -- --nocapture
scripts/resource-limit cargo test build_recent_egress_outcome_resets_flap_streak_outside_window --lib -- --nocapture
scripts/resource-limit cargo test health_snapshot_surfaces_flapping_after_repeated_reconnects --lib -- --nocapture
scripts/resource-limit cargo test health_snapshot_surfaces_flapping_after_repeated_egress_recoveries --lib -- --nocapture
scripts/resource-limit cargo test late_retry_state_update_is_ignored_after_output_restarts --lib -- --nocapture
scripts/resource-limit cargo test repeated_late_retry_updates_cannot_poison_newest_output_attempt --lib -- --nocapture
scripts/resource-limit cargo test output_status_surfaces_retry_backoff_after_failure --lib -- --nocapture
scripts/resource-limit cargo test prop_egress_lifecycle_preserves_runtime_and_health_invariants --lib -- --nocapture
scripts/resource-limit cargo build --bin restream --bin test_harness

RESTREAM_BIN=target/debug/restream \
  WORK_DIR=test/artifacts/concurrency-contract \
  target/debug/test_harness fault-resilience

RESTREAM_BIN=target/debug/restream \
  WORK_DIR=test/artifacts/concurrency-recovery \
  target/debug/test_harness recovery
