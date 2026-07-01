# Concurrency Proof Coverage Report - 2026-07-02

Baseline: `0efa0d4` on `feat/rust-backend-rewrite-v2` after merging the proof work from the isolated `codex/proof-*` branches.

This report summarizes the model, property, unit, and live-harness proof surface for concurrency primitives and thread/process hop boundaries. It is intentionally proof-oriented rather than line-coverage-oriented.

## Summary

| Boundary | Proof type | Mandatory gate coverage |
|---|---|---|
| Ring seal/migration/read ordering | Loom + proptest + deterministic tests | `ring_migration_loom`, `prop_no_loss_no_gap_no_duplication`, `prop_multi_reader_migration_preserves_each_reader_order` |
| TS chunk ring wait/cancel/live readers | Loom + unit tests | `ts_chunk_ring_loom`, `live_reader_starts_after_existing_chunks` |
| AVIO/MemoryQueue close/wake/backpressure | Loom + unit/proptest tests | `avio_loom`, `media::avio::tests`, `write_batch_round_trips_random_chunks` |
| Stage registry replacement and TS muxer sweep | Loom + lifecycle unit tests | `transcoder_stage_loom`, `ts_muxer_stage_loom`, stale attempt tests |
| External transcoder pipe/output/SRT path | Unit + proptest + focused live harness | external transcoder marker tests, DTS routing proptest, `mixed-file-h264-single` smoke |
| SRT protocol boundaries | Unit/stress tests | stream-id normalization tests, sender semaphore tests, `epoll_waiter_coordination` |
| Child process lifecycle and cleanup | Static script guard + unit test + live contract cleanup checks | `kill_and_wait_child_terminates_spawned_process`, process lifecycle guard, post-harness orphan checks |
| Runtime status after cleanup/recovery | API/status tests + live harness | API lifecycle tests, `fault-resilience`, `fault-egress-retry`, `fault-output-stall`, `recovery` |

## New Proof Coverage Added In This Sweep

### Ring And TS Chunk Ring

- `tests/ring_migration_loom.rs`
  - Added multi-reader seal wake coverage: a seal must wake all blocked readers, not only one waiter.
- `tests/ring_migration.rs`
  - Added `prop_multi_reader_migration_preserves_each_reader_order`, covering two readers with different pre-seal drain positions and checking per-reader ordering through migration.
- `src/media/ts_chunk_ring.rs`
  - Added `TsChunkReader::new_live` and `live_reader_starts_after_existing_chunks`, proving live readers skip existing buffered TS chunks and consume only future chunks.

### AVIO / Memory Queue

- `tests/avio_loom.rs`
  - Added loom coverage for batch writers blocked on backpressure.
  - Added close/read wake coverage for batch writer paths.
- `src/media/avio.rs`
  - Existing unit/proptest coverage remains the deterministic gate for read/write/batch behavior and poisoned-lock recovery.

### Stage Lifecycle

- `tests/transcoder_stage_loom.rs`
  - Added cleanup/replacement atomicity coverage for stage registry state.
- `tests/ts_muxer_stage_loom.rs`
  - Added sweep-vs-reader-registration coverage using a loom-compatible liveness model.

### Critical External Transcoder / SRT Path

- `src/media/external_transcoder.rs`
  - Added testable `external_output_stream_idx` routing.
  - Added deterministic routing coverage ensuring known audio tracks map to distinct DTS streams and unknown/disabled audio does not alias to video or the first audio track.
  - Added `proptest_external_output_dts_routing_preserves_per_stream_monotonicity`, covering random audio-track permutations and mixed packet sequences.
  - Existing marker-fixture checks cover file-mode transcode control and live external-stage output.
- `src/media/srt.rs`
  - Shared SRT TS muxers and SRT egress readers attach at the live edge to avoid replaying stale ring/chunk backlog to live consumers.
- `src/media/ts_chunk_ring.rs`
  - `TsChunkReader::new_live` backs the SRT egress live-edge proof.

### Recording / HLS Timestamp Boundaries

- `src/media/hls.rs`
  - Added `hls_segment_boundaries_preserve_non_decreasing_dts_per_stream`, a deterministic in-memory proof that demuxed DTS values stay non-decreasing per stream across consecutive HLS MPEG-TS segment boundaries.
  - Coverage includes both packet-level DTS monotonicity and explicit first-packet-vs-previous-segment-last boundary checks after HLS keyframe-triggered segmentation.

### SRT Protocol Boundaries

- `src/media/srt.rs`
  - Equivalent percent-encoded and literal SRT stream IDs normalize to the same key before auth/duplicate registration checks.
  - The libsrt listener policy callback is panic-contained instead of unwinding across the C callback boundary.
  - Sender semaphore acquisition routes through a production helper exercised by existing semaphore tests.

### Process Lifecycle / Harness

- `src/bin/test_harness.rs`
  - Added `kill_and_wait_child` coverage through `tests::kill_and_wait_child_terminates_spawned_process`.
- `scripts/resource-limit`
  - Honors `RESTREAM_BUILD_LOCK_FILE` and rejects relative paths.
- `scripts/check-concurrency-contract.sh`
  - Defaults a host-global build lock when unset.
  - Adds static lifecycle guards for child process handling.
  - Checks for orphaned runtime processes after harness-mode cleanup.

## Gate Inventory

### Fast Proof Gate

`bash ./scripts/check-concurrency-proof-fast.sh` runs:

- Loom targets:
  - `avio_loom`
  - `ring_migration_loom`
  - `ts_chunk_ring_loom`
  - `ts_muxer_stage_loom`
  - `transcoder_stage_loom`
- Focused API/status lifecycle tests.
- Ring migration property test: `prop_no_loss_no_gap_no_duplication`.
- AVIO batch property test: `write_batch_round_trips_random_chunks`.
- SRT epoll stress test: `epoll_waiter_coordination`.
- Ingest/egress lifecycle proptests.

### Full Contract Gate

`bash ./scripts/check-concurrency-contract.sh` runs everything in the fast proof gate plus:

- `scripts/check-history-grouping.sh`
- static process lifecycle guards
- debug binary build for `restream` and `test_harness`
- live harness modes:
  - `fault-resilience`
  - `fault-egress-retry`
  - `fault-output-stall`
  - `recovery`
- post-mode orphan process checks for `restream`, `mediamtx`, `ffmpeg`, `ffprobe`, and `test_harness`

## Focused Validation Performed During The Sweep

The following focused checks passed serially after merging the isolated proof branches:

```sh
cargo fmt --all --check
bash -n scripts/resource-limit scripts/check-concurrency-contract.sh
RESTREAM_BUILD_LOCK_FILE=relative scripts/resource-limit true # expected exit 2
./scripts/run-loom-target.sh ring_migration_loom
./scripts/run-loom-target.sh avio_loom
./scripts/run-loom-target.sh transcoder_stage_loom
./scripts/run-loom-target.sh ts_muxer_stage_loom
scripts/resource-limit cargo test prop_multi_reader_migration_preserves_each_reader_order --test ring_migration -- --nocapture
scripts/resource-limit cargo test media::avio::tests --lib -- --nocapture
scripts/resource-limit cargo test srt_stream_ids_normalize_equivalent --lib -- --nocapture
scripts/resource-limit cargo test srt_sender_semaphore --lib -- --nocapture
scripts/resource-limit cargo test --bin test_harness tests::kill_and_wait_child_terminates_spawned_process -- --exact --nocapture
env N_PER_GROUP=1 ONLY_CHECKS=ffprobe SKIP_LOAD=1 scripts/resource-limit cargo run --bin test_harness -- mixed-file-h264-single
```

The full live `scripts/check-concurrency-contract.sh` gate remains the sign-off gate for broad lifecycle changes, but it should be run serially on a stable host because it starts several live harness modes.

## Remaining Gaps

- The full contract gate is intentionally heavier than the focused checks above; run it before final sign-off when host resources allow.
- More live chaos coverage would still be valuable for slow-sink isolation across high output counts.
- Internal transcoder/libavcodec paths have separate correctness concerns from the external FFmpeg subprocess path and should get their own proof slice when touched.
- Recording/HLS timestamp monotonicity is now covered at HLS segment boundaries, but recording remux continuity (TS -> MP4 -> TS timestamp continuity under source-retention permutations) still lacks a dedicated proof test.
