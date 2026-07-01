# Concurrency Proofing

This repo has several correctness-sensitive boundaries:

- Tokio task ↔ OS thread handoff
- Wait/notify or cancel/wake synchronization
- Shared stage registry create/reuse/cancel/recreate paths
- Live teardown/recovery behavior across sockets, child processes, and ring readers
- Status retention after cleanup so operators can see what failed

The goal is not just "tests exist". The goal is that each boundary has the
right kind of proof for the failure mode it can exhibit.

## Proof Ladder

Use the narrowest proof that can actually catch the bug:

1. Unit/regression tests
   - Use for API-visible state, lifecycle transitions, idempotence, and
     recreate-after-cancel behavior.
   - Example: recent ingest/egress status surviving unregister.

2. Loom model checks
   - Use for wait/cancel, close/wake, seal/migrate, and get-or-create registry
     races where scheduling order matters more than payload content.
   - Current loom targets:
     - `avio_loom`
     - `ring_migration_loom`
     - `ts_chunk_ring_loom`
     - `ts_muxer_stage_loom`
    - `transcoder_stage_loom`

3. Property tests
   - Use when ordering, permutations, or randomized lifecycle sequences matter.
   - Good fit for ring invariants, packet ordering, or lifecycle state machines.

4. Live harness chaos tests
   - Use when the behavior crosses real sockets, child processes, FFmpeg, or
     OS-thread boundaries.
   - Current live contract slices: `fault-resilience`, `fault-egress-retry`,
     `fault-output-stall`, and `recovery`.
     `fault-egress-retry` proves RTMP and SRT dead sinks surface `retrying`
     first and then settle to `failed` once the configured retry budget is
     exhausted.
     `fault-output-stall` proves a connected RTMP sink that stops draining media
     surfaces `stalled` in both output status and health instead of looking
     healthy or failing immediately.
     `recovery` also covers hung HLS PUT destinations timing out, surfacing
     retry/error state, recovering after the sink restarts, rapid same-pipeline
     SRT publisher replacement races, and repeated RTMP and SRT downstream sink
     flaps surfacing as recovered-but-unstable output health.

5. Benchmarks
   - Use only for hot-path or end-to-end performance-sensitive changes.
   - Do not mix measurement claims into correctness-only gates.

## Which Proof To Add

Add loom when a change introduces any of:

- Wait-for-data + cancellation
- Notify/condvar wakeups
- Lock-free atomic state transitions
- Shared registry create/reuse/remove races
- Shutdown sequencing where "sleep forever" is a possible failure mode

Add a harness test when a change affects any of:

- Destination disconnect/reconnect
- Ingest disconnect/reconnect
- Child process teardown
- Shared stage survival across transient upstream loss
- Runtime status after cleanup or restart

Add API/frontend contract coverage when:

- Status fields are added, removed, renamed, or change semantics
- Cleanup now preserves or clears more runtime context
- Frontend badges or summaries depend on a new backend state

## Gate Commands

Fast proof gate for local loops:

```sh
bash ./scripts/check-concurrency-proof-fast.sh
```

Full contract gate:

```sh
bash ./scripts/check-concurrency-contract.sh
```

The fast gate runs the loom targets, focused API tests, and harness unit tests.
The full gate also builds the binaries and runs the live `fault-resilience`,
`fault-egress-retry`, `fault-output-stall`, and `recovery` harness modes.
`fault-egress-retry` owns the retry-budget exhaustion contract for RTMP and SRT
dead sinks.
`fault-output-stall` owns the stalled-output contract for connected-but-not-
draining RTMP sinks. `recovery` is the focused reconnect/grace/retry contract
so we can target that behavior directly without depending on the broader
teardown bucket.

The current proof inventory is summarized in
[Concurrency Proof Coverage Report - 2026-07-02](concurrency-proof-coverage-2026-07-02.md).

Both gates also carry explicit property/stress coverage for lifecycle
permutations and thread-hop wakeups, rather than relying on the general
workspace test job to catch those indirectly.

## Required Update Discipline

When touching concurrency-sensitive code:

1. Add or update the narrowest proof test that can catch the bug.
2. Extend a gate script if the new proof is supposed to be mandatory.
3. Update the live harness if operator-visible teardown/recovery behavior changed.
4. Update API/docs if status semantics changed.

If a change is concurrency-sensitive and does **not** need a loom/proptest or
harness addition, the PR or commit message should say why the existing proof
surface already covers it.

## Common Failure Patterns

- Silent wake loss: task/thread blocks forever after cancel or close.
- Cancelled-stage reuse: registry returns a dead shared stage instead of a new one.
- Teardown erases diagnosis: runtime cleanup drops the last structured error.
- Harness drift: the live test still expects old cleanup behavior after runtime
  semantics intentionally improved.
- "Fast" local validation skips the actual proof gate, so model checks rot.

## Current Mandatory Surfaces

- `scripts/check-concurrency-proof-fast.sh`
- `scripts/check-concurrency-contract.sh`
- `tests/api.rs`
  - `health_endpoint_exposes_probe_and_egress_fault_fields`
  - `health_endpoint_surfaces_repeated_transient_disconnects_as_flapping`
  - `recovered_output_surfaces_flapping_after_repeated_sink_failures`
  - `output_status_and_health_preserve_recent_egress_failure_after_unregister`
- `tests/output_status_contract.rs`
  - `active_output_status_matches_health_runtime_fields`
  - `stalled_output_status_matches_health_runtime_fields`
- `tests/ring_migration.rs`
  - `prop_no_loss_no_gap_no_duplication`
  - `prop_multi_reader_migration_preserves_each_reader_order`
- `src/media/engine.rs`
  - `stale_ingest_unregister_cannot_clobber_replacement_attempt`
  - `stale_ingest_disconnect_cannot_poison_replacement_attempt`
  - `stale_egress_unregister_cannot_clobber_replacement_attempt`
  - `stale_egress_error_cannot_poison_replacement_attempt`
  - `stale_egress_queue_removal_cannot_drop_replacement_queue`
  - `build_recent_ingest_outcome_resets_flap_streak_outside_window`
  - `prop_ingest_lifecycle_preserves_health_invariants`
  - `build_recent_egress_outcome_resets_flap_streak_outside_window`
  - `health_snapshot_surfaces_flapping_after_repeated_reconnects`
  - `health_snapshot_surfaces_flapping_after_repeated_egress_recoveries`
  - `output_status_surfaces_retry_backoff_after_failure`
  - `prop_egress_lifecycle_preserves_runtime_and_health_invariants`
- `src/media/avio.rs`
  - close/wake/backpressure loom coverage in `tests/avio_loom.rs`
  - `write_batch_round_trips_random_chunks`
- `src/media/external_transcoder.rs`
  - `external_output_stream_idx_routes_known_tracks_without_aliasing`
  - `proptest_external_output_dts_routing_preserves_per_stream_monotonicity`
  - `external_720p_stage_emits_live_packets_for_h264_marker_fixture`
  - `external_1080p_stage_remuxes_marker_fixture_with_monotone_dts`
- `src/media/srt.rs`
  - `epoll_waiter_coordination`
  - `srt_stream_ids_normalize_equivalent_publish_keys_before_registration`
  - `srt_stream_ids_normalize_equivalent_read_keys_before_auth`
  - `srt_sender_semaphore_is_bounded`
  - `srt_sender_semaphore_releases_on_drop`
- `src/media/ts_chunk_ring.rs`
  - `live_reader_starts_after_existing_chunks`
- `src/bin/test_harness.rs`
  - `kill_and_wait_child_terminates_spawned_process`
  - `fault-egress-retry`
  - `fault-output-stall`
  - `fault-resilience`
  - `recovery`

## Next Gaps

The proof surface is stronger, but the full objective is still larger than the
current gate set. Remaining high-value areas include:

- More model-checked coverage for lifecycle registries beyond the TS muxer seam
- Property tests for lifecycle permutations where loom is not the right tool
- More live chaos cases for slow-sink isolation and any remaining repeated downstream flap scenarios beyond the current RTMP/SRT coverage
