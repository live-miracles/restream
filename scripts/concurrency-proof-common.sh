#!/usr/bin/env bash

run_common_concurrency_checks() {
  local run_step_fn="$1"

  for target in avio_loom ring_migration_loom ts_chunk_ring_loom ts_muxer_stage_loom transcoder_stage_loom; do
    "$run_step_fn" "loom-${target}" ./scripts/run-loom-target.sh "$target"
  done

  "$run_step_fn" api-health \
    scripts/resource-limit cargo test health_endpoint_exposes_probe_and_egress_fault_fields --test api -- --nocapture
  "$run_step_fn" api-output-recent-failure \
    scripts/resource-limit cargo test output_status_and_health_preserve_recent_egress_failure_after_unregister --test api -- --nocapture
  "$run_step_fn" api-output-restart-retry \
    scripts/resource-limit cargo test active_output_status_ignores_stale_retry_state_after_restart --test api -- --nocapture
  "$run_step_fn" output-status-active \
    scripts/resource-limit cargo test active_output_status_matches_health_runtime_fields --test output_status_contract -- --nocapture
  "$run_step_fn" output-status-stalled \
    scripts/resource-limit cargo test stalled_output_status_matches_health_runtime_fields --test output_status_contract -- --nocapture
  "$run_step_fn" api-disconnect-clears \
    scripts/resource-limit cargo test health_endpoint_clears_recent_disconnect_details_after_reconnect --test api -- --nocapture
  "$run_step_fn" api-disconnect-flapping \
    scripts/resource-limit cargo test health_endpoint_surfaces_repeated_transient_disconnects_as_flapping --test api -- --nocapture
  "$run_step_fn" api-egress-flapping \
    scripts/resource-limit cargo test recovered_output_surfaces_flapping_after_repeated_sink_failures --test api -- --nocapture
  "$run_step_fn" db-stale-job-update \
    scripts/resource-limit cargo test stale_job_update_cannot_clobber_replacement_attempt --test db -- --nocapture
  "$run_step_fn" db-multiple-stale-job-updates \
    scripts/resource-limit cargo test multiple_stale_job_updates_cannot_clobber_newest_attempt --test db -- --nocapture
  "$run_step_fn" lib-stale-ingest-unregister \
    scripts/resource-limit cargo test stale_ingest_unregister_cannot_clobber_replacement_attempt --lib -- --nocapture
  "$run_step_fn" lib-stale-ingest-disconnect \
    scripts/resource-limit cargo test stale_ingest_disconnect_cannot_poison_replacement_attempt --lib -- --nocapture
  "$run_step_fn" lib-stale-egress-unregister \
    scripts/resource-limit cargo test stale_egress_unregister_cannot_clobber_replacement_attempt --lib -- --nocapture
  "$run_step_fn" lib-stale-egress-error \
    scripts/resource-limit cargo test stale_egress_error_cannot_poison_replacement_attempt --lib -- --nocapture
  "$run_step_fn" lib-stale-egress-queue \
    scripts/resource-limit cargo test stale_egress_queue_removal_cannot_drop_replacement_queue --lib -- --nocapture
  "$run_step_fn" ring-proptest \
    scripts/resource-limit cargo test prop_no_loss_no_gap_no_duplication --test ring_migration -- --nocapture
  "$run_step_fn" lib-avio-batch \
    scripts/resource-limit cargo test write_batch_round_trips_random_chunks --lib -- --nocapture
  "$run_step_fn" lib-srt-epoll \
    scripts/resource-limit cargo test epoll_waiter_coordination --lib -- --nocapture
  "$run_step_fn" lib-recent-egress \
    scripts/resource-limit cargo test recent_egress --lib -- --nocapture
  "$run_step_fn" lib-ingest-grace \
    scripts/resource-limit cargo test recent_ingest_disconnect_respects_grace_window --lib -- --nocapture
  "$run_step_fn" lib-ingest-flap-window \
    scripts/resource-limit cargo test build_recent_ingest_outcome_resets_flap_streak_outside_window --lib -- --nocapture
  "$run_step_fn" lib-ingest-proptest \
    scripts/resource-limit cargo test prop_ingest_lifecycle_preserves_health_invariants --lib -- --nocapture
  "$run_step_fn" lib-egress-flap-window \
    scripts/resource-limit cargo test build_recent_egress_outcome_resets_flap_streak_outside_window --lib -- --nocapture
  "$run_step_fn" lib-health-reconnect-flapping \
    scripts/resource-limit cargo test health_snapshot_surfaces_flapping_after_repeated_reconnects --lib -- --nocapture
  "$run_step_fn" lib-health-egress-flapping \
    scripts/resource-limit cargo test health_snapshot_surfaces_flapping_after_repeated_egress_recoveries --lib -- --nocapture
  "$run_step_fn" lib-late-retry-state \
    scripts/resource-limit cargo test late_retry_state_update_is_ignored_after_output_restarts --lib -- --nocapture
  "$run_step_fn" lib-multi-late-retry-state \
    scripts/resource-limit cargo test repeated_late_retry_updates_cannot_poison_newest_output_attempt --lib -- --nocapture
  "$run_step_fn" lib-output-retry-backoff \
    scripts/resource-limit cargo test output_status_surfaces_retry_backoff_after_failure --lib -- --nocapture
  "$run_step_fn" lib-egress-proptest \
    scripts/resource-limit cargo test prop_egress_lifecycle_preserves_runtime_and_health_invariants --lib -- --nocapture
}
