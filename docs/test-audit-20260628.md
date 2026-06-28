# Test Audit - 2026-06-28

Purpose: track every warning, error, anomaly, and mitigation encountered while
running the full Rust test suite and the live integration/harness surfaces.

This note is intentionally live. Items stay open until they are either:

- mitigated in code or test infrastructure,
- reclassified as expected fixture/tool noise with evidence, or
- proven to be an environment-only artifact outside the product/runtime.

## Scope

- Rust unit and integration tests via `scripts/resource-limit cargo test`
- Live harness aggregate runs via `cargo run --bin test_harness -- suite ...`
- Supporting per-suite reruns used to isolate warnings or failures

## Findings

| ID | Status | Surface | Finding | Evidence | Mitigation |
|---|---|---|---|---|---|
| AUDIT-001 | Open | Rust build/test | `unexpected cfg condition name: loom` warning from `tests/ring_migration_loom.rs` | `/tmp/restream-cargo-test.log` | Add `cfg(loom)` to `check-cfg` or move loom gating behind a declared Cargo feature |
| AUDIT-002 | Open | API test media decode | AAC decoder warns about assuming non-spec `7.1(wide)` layout | `/tmp/restream-cargo-test.log` and `cargo test --test api` output | Determine fixture/source and decide whether to replace fixture, suppress expected stderr, or document as accepted decoder noise |
| AUDIT-003 | Open | Transcoder tests | MPEG-TS probe warns `not enough frames to estimate rate; consider increasing probesize` | `/tmp/restream-cargo-test.log` and focused transcoder test output | Determine whether larger probe settings or a cleaner fixture removes noise without weakening coverage |
| AUDIT-004 | Mitigated | Internal transcode paths | `libx264` warned `specified frame type is not compatible with max B-frames` | Reproduced during focused transcoder runs before fix | Fixed by clearing inherited picture type before re-encode in commit `d25985b` |
| AUDIT-005 | Open (environment) | Full-suite execution | Earlier sandboxed runs hit `Operation not permitted` on socket/PUT-sink style tests | Prior captured test failures in this thread | Run release-confidence suite outside sandbox constraints; keep separate from product/runtime findings |

## Mitigation Log

| Date | Change | Related IDs | Evidence |
|---|---|---|---|
| 2026-06-28 | Cleared inherited frame types before internal re-encode | AUDIT-004 | Commit `d25985b`, focused transcoder rerun no longer emitted x264 B-frame conflict warning |

## Execution Log

| Status | Command / Surface | Notes |
|---|---|---|
| Partial | `scripts/resource-limit cargo test` | Captured to `/tmp/restream-cargo-test.log`; run timed out at turn boundary, but log already contains warnings and multiple passing suite summaries |
| Complete | `cargo test --test api` | Passed locally; emitted AAC layout warning |
| Complete | `cargo test internal_transcode_builtin_video_presets_produce_video --test transcoder -- --nocapture` | Passed locally; used to validate AUDIT-004 mitigation and observe remaining MPEG-TS probe warnings |
| Pending | `cargo test --lib` | To isolate all library-only warnings/errors with a bounded log |
| Pending | Remaining integration tests (`av_sync`, `codec`, `db`, `transcoder`) | To classify stderr and anomalies suite-by-suite |
| Pending | `cargo run --bin test_harness -- suite ...` | Aggregate live integration/harness audit still to be completed |

## Next Actions

1. Finish segmented Rust test-surface runs and harvest every warning/error line.
2. Run the full `test_harness suite` aggregate and inspect per-mode `run.log`.
3. Classify every emitted warning as product issue, fixture noise, tool noise, or environment artifact.
4. Implement mitigations for all open product/test-infra issues.
5. Leave this note empty of open actionable items before calling the audit complete.
