# Testing

## Rust Test Suite

Run the repo gate:

```sh
./scripts/check-test-hygiene.sh
```

For fixture-first media discipline:

```sh
./scripts/check-fixture-discipline.sh
```

For a plain full-suite run without the hygiene scan:

```sh
scripts/resource-limit cargo test
```

Keep successful logs quiet. New tests should not land with compiler warnings,
panic text, FFmpeg probe chatter, or similar “expected noise” in passing runs;
fix or suppress that output at the helper level instead.

## Frontend Test Split

Frontend confidence is intentionally split between TypeScript ownership and
compiled-bundle smoke coverage:

- `npm run test:frontend` runs the Node-based frontend suites from a temporary
  sourcemapped build of `public/ts/**`, then finishes with a smaller smoke pass
  against the shipped `public/js/**` bundle.
- `npm run test:frontend:coverage` keeps the same split, but reports coverage
  back onto the deterministic TypeScript modules that the Node/fake-DOM suite
  is meant to own. This is the main frontend coverage gate.
- `npm run test:frontend:coverage:all` keeps the same runtime path but emits a
  broader all-files TypeScript report for diagnostic use; expect browser-heavy
  modules to stay lower until they get Playwright or browser-native coverage.
- `npm run test:frontend:js-smoke` is the minimal direct guard for generated
  `public/js/**`; use it when you only need to verify the compiled artifact.

This keeps detailed behavior and coverage attached to the TypeScript source of
truth without dropping confidence in the emitted browser bundle, while avoiding
misleading Node-only coverage targets for browser-heavy modules.

### Layered UI Strategy

Treat frontend confidence as four layers, each owning a different kind of risk:

| Layer | Purpose | Typical command |
|---|---|---|
| TypeScript/source logic | Keep parsing, helpers, API choke points, and pure UI state logic deterministic and cheap. | `npm run test:frontend` |
| Fake-DOM scenario matrices | Replace repetitive manual "check every state" work for state-heavy renderers. | `npm run test:frontend` |
| Browser-native DOM checks | Prove real DOM events, focus/ARIA behavior, overlay positioning hooks, and browser-only widget behavior without starting the full Rust app. | `npm run test:frontend:browser-dom` |
| Full app/browser integration | Prove login, navigation, media playback, real network wiring, and end-to-end runtime behavior against the running dashboard. | `npm run test:e2e` |

Use the lowest layer that can actually catch the bug. Move upward only when the
lower layer cannot prove the behavior.

### UI Scenario Matrices

When a dashboard surface starts accumulating too many manual "click every state"
checks, add a fake-DOM scenario matrix instead of growing Playwright coverage
for every badge and branch.

- Use `test/helpers/ui-scenario-harness.mjs` to mount the minimum DOM, load the
  compiled frontend module, and run a named state matrix under `npm run test:frontend`.
- Current examples:
  `test/frontend-output-scenarios.test.mjs` and
  `test/frontend-pipeline-info-scenarios.test.mjs`.
- Feed renderers a bounded set of important states such as healthy, retrying,
  flapping, stalled, stopped, long text, and missing optional metadata.
- Assert operator-visible structure and state: the right action label,
  warning/error affordance, hidden/visible controls, and critical metrics.
- Keep browser-native checks in Playwright for things the fake DOM cannot prove:
  navigation, focus, media playback, sizing, and real browser APIs.
- Use `npm run test:frontend:browser-dom` for a self-contained browser-native
  slice that serves the compiled frontend assets from a lightweight local static
  server instead of requiring the full Rust dashboard app to be started first.

As of June 29, 2026 `cargo test -- --list` enumerates 621 tests across unit,
integration, harness, and doctest targets.

Checked-in fixture contracts now cover the committed benchmark/test media under
`test/fixtures/`, so the transcoder and fixture-dependent suites no longer rely
on ad-hoc local artifacts. Tests, benches, and harness publishers should resolve
those assets through `src/test_fixtures.rs` so missing files fail loudly and new
fixtures are added to one explicit contract.

## Parallelism Policy

Keep correctness throughput high, but treat measurement fidelity as a separate
constraint.

- Rust unit and integration tests: prefer a single `scripts/resource-limit cargo test ...`
  invocation and let Cargo own compile and test-thread parallelism. Avoid
  launching multiple heavy `cargo test` commands against the same worktree at
  once; that just trades useful concurrency for lock contention and noisier logs.
- Live harness correctness modes: `src/bin/test_harness.rs` may batch
  correctness-only suite modes in parallel when each mode is isolated in its
  own network namespace and work directory.
- Measurement-oriented harness modes: keep them serial and bench-profile only.
  CPU, RSS, and throughput numbers are only comparable when the harness runs one
  measurement slice at a time from `target/bench/`.
- Criterion benches: parallelize compilation and fixture preparation, not timed
  measurement. `scripts/resource-limit cargo bench --no-run` is the safe fan-out
  step; actual `cargo bench --bench ...` execution should stay serial unless the
  runs are explicitly resource-isolated.

## Scoped Verification Loop

Prefer the smallest test and benchmark set that directly covers the changed
behavior, then broaden only when the risk calls for it. This keeps agent and
developer loops fast while still making the verification signal precise.

Good scoped Rust patterns:

```sh
scripts/resource-limit cargo test --lib <test-name-or-module-filter>
scripts/resource-limit cargo test --test api <test-name-filter>
scripts/resource-limit cargo test --test transcoder <test-name-filter>
```

Good scoped benchmark patterns:

```sh
scripts/resource-limit cargo bench --bench <bench-name> -- <criterion-filter>
scripts/resource-limit cargo bench --bench high_performance_data_path -- data_path/egress_progress
scripts/resource-limit cargo bench --bench srt_ingest_latency -- 'srt_(ingest|egress)'
```

The SRT bench is a socket-pair microbenchmark, not a live pipeline test. It is
meant to answer narrow questions such as "what did enabling SRT encryption cost
on loopback?" by comparing:

- `srt_ingest/plain|aes128|aes192|aes256/recv_path`
- `srt_egress/plain|aes128|aes192|aes256/send_path`

Each case uses the same fixed transfer shape: `8` live-mode SRT packets of
`1316` bytes per timed iteration. The only benchmark variable is the negotiated
SRT encryption key length through `SRTO_PBKEYLEN`.

Use the full `cargo test` suite, full benchmark suites, or live integration
modes as a broader confidence pass when a change crosses module boundaries,
changes a shared contract, affects protocol behavior, or touches a hot path
whose blast radius is unclear. If an unrelated full-suite test or benchmark
fails, report it separately from the scoped signal for the current change.

### Composable Verification Stages

Large suites should be broken into named stages that can run independently and
compose into larger gates. A failure in one stage should identify the affected
behavior slice instead of turning the entire test or benchmark program into an
opaque blocker.

| Stage | Purpose | Typical commands |
|---|---|---|
| 0. Preflight/static | Prove the environment and cheap invariants before spending runtime. | `cargo fmt --all --check`, integration `--preflight` |
| 1. Changed behavior | Fastest proof for the exact code path touched by a change. | `cargo test --lib <filter>`, `cargo test --test api <filter>` |
| 2. Contract slice | Neighboring API, graph, stage, protocol, or lifecycle contracts that consume the changed behavior. | Filtered package/integration tests by module, endpoint, protocol, or stage kind |
| 3. Hot-path cost | Criterion group that measures the touched hot path only. | `cargo bench --bench <bench> -- <criterion-filter>` |
| 4. Live protocol slice | One live protocol/topology check with minimal fanout and targeted assertions. | `target/bench/test_harness mixed-h264-srt-single` |
| 5. Scale/degradation slice | A bounded load, ramp, restart, queue-pressure, or bonding slice for resource shape. | `N_OUTPUTS=<small>` ramp, `N_PER_GROUP=<small>` mixed-input-matrix, `bonding` |
| 6. Full confidence gate | Release or milestone pass assembled from the relevant stages above. | Full `cargo test`, selected full benches, full integration modes |

When a suite grows too large, split it along composable axes instead of adding
more mandatory work to a single command:

- behavior: ingest, egress, HLS, recording, graph, diagnostics, alerts
- protocol: RTMP, SRT, HLS, RTMPS, SRT bonding
- codec/media shape: H.264, H.265, B-frames, multi-audio, audio remap/downmix
- topology: passthrough, one shared stage, mixed presets, package sharing
- load shape: smoke, small fanout, ramp, soak, downstream restart, queue pressure
- evidence: unit assertion, API snapshot, graph invariant, ffprobe/readback,
  resource baseline, Criterion benchmark

Prefer adding selectors, manifest entries, and result artifacts over adding a
new all-or-nothing suite. A milestone can still require multiple stages, but it
should state which slices are required and preserve each slice's separate
pass/fail result.

Unit coverage includes:

- RTMP FLV H.264/AAC parsing and signed composition time
- HLS playlist/window behavior
- SRT stream-ID normalization, URL/bond parsing, codec mapping, payload
  extraction, rate deltas, socket option IDs, listener UDP-stat parsing
- Linux `TCP_INFO`/`SO_MEMINFO` conversion and live socket collection
- Transcoder stage sharing and audio-routing parsing
- External HLS PUT upload delivery through a dummy HTTP sink
- FFmpeg-backed audio remap/downmix stage argument generation and fixture-backed
  execution
- Internal decode/scale/encode coverage for the built-in video profiles
- Ring buffer push/pull ordering, overflow fast-forward to keyframe,
  multi-reader isolation, fill/capacity reporting, burst APIs
- DTS monotonicity enforcement (equal, decreasing, PTS < DTS correction,
  per-stream independence, B-frame composition-time preservation)
- Engine lifecycle: ingest/egress register/unregister/cancel, idempotent
  unregister, pipeline create/remove, egress byte counters, health snapshot
  pipeline filtering, recording lifecycle, noop on nonexistent pipelines
- MPEG-TS demux/mux: packet parsing, PID dispatch, PES assembly, continuity
  counters, Annex-B NAL scanning, vectorized resync
- Codec helpers: FLV stripping, video/audio payload conversion for TsMuxer

The API suite covers authentication, configuration, pipeline/output
CRUD, ingests, HLS aliases, status, graph, diagnostics preconditions, custom
encoding persistence/rejection for runtime outputs, HLS upload output
acceptance, RTMPS output acceptance, egress-pipeline association in `/api/v1/engine/health`,
deletion-cancellation of egress tasks, media list / analysis / rename / delete
behavior, pipeline and aggregate alerts response shape, system metrics
structured response, agent graph-diff-preview compiled-out behavior, and
operator telemetry/events/overview/summary endpoints.

## API Route Coverage Matrix

Every route in `src/api.rs` audited against unit tests (`tests/api.rs`) and
live integration tests (`src/bin/test_harness.rs`). As of June 27, 2026 all
59 routes have at least one test. Legend: ✓ = covered, — = not covered,
~ = precondition only.

**Auth**

| Method | Route | Unit | Live | Notes |
|---|---|:---:|:---:|---|
| `POST` | `/api/auth/login` | ✓ | ✓ | |
| `POST` | `/api/auth/logout` | ✓ | — | |
| `POST` | `/api/auth/change-password` | ✓ | — | |

**Config**

| Method | Route | Unit | Live | Notes |
|---|---|:---:|:---:|---|
| `GET` | `/api/v1/settings` | ✓ | ✓ | |
| `PATCH` | `/api/v1/settings` | ✓ | — | 3 tests incl. transcode profiles |
| `GET` | `/audio-caps` | ✓ | — | |
| `GET` | `/api/v1/stream-keys` | ✓ | — | |

**Pipelines**

| Method | Route | Unit | Live | Notes |
|---|---|:---:|:---:|---|
| `GET` | `/api/v1/pipelines` | ✓ | ✓ | |
| `POST` | `/api/v1/pipelines` | ✓ | ✓ | Create |
| `PATCH` | `/api/v1/pipelines/:id` | ✓ | — | Update |
| `DELETE` | `/api/v1/pipelines/:id` | ✓ | ✓ | fault-resilience SRT test |

**File ingest**

| Method | Route | Unit | Live | Notes |
|---|---|:---:|:---:|---|
| `GET` | `/api/v1/pipelines/:id/file-ingest` | ✓ | — | |
| `PUT` | `/api/v1/pipelines/:id/file-ingest` | ✓ | ✓ | |
| `DELETE` | `/api/v1/pipelines/:id/file-ingest` | ✓ | — | |

**Outputs**

| Method | Route | Unit | Live | Notes |
|---|---|:---:|:---:|---|
| `POST` | `/api/v1/pipelines/:id/outputs` | ✓ | ✓ | Create |
| `PATCH` | `/api/v1/pipelines/:id/outputs/:oid` | ✓ | — | Update |
| `DELETE` | `/api/v1/pipelines/:id/outputs/:oid` | ✓ | — | |
| `POST` | `/api/v1/pipelines/:id/outputs/:oid/start` | ✓ | ✓ | |
| `POST` | `/api/v1/pipelines/:id/outputs/:oid/stop` | ✓ | ✓ | |
| `GET` | `/api/v1/pipelines/:id/outputs/:oid/status` | ✓ | ✓ | |

**Pipeline detail**

| Method | Route | Unit | Live | Notes |
|---|---|:---:|:---:|---|
| `GET` | `/api/v1/pipelines/:id/probe` | — | ✓ | mixed-input, correctness-* |
| `GET` | `/api/v1/pipelines/:id/graph` | ✓ | ✓ | |
| `GET` | `/api/v1/pipelines/:id/alerts` | ✓ | — | auth + response shape |
| `GET` | `/api/v1/pipelines/:id/diagnostics` | ~ | — | SSE; precondition only |
| `POST` | `/api/v1/pipelines/:id/recording/start` | — | ✓ | mixed-h264-srt-single |
| `POST` | `/api/v1/pipelines/:id/recording/stop` | — | ✓ | mixed-h264-srt-single |

**Encodings**

| Method | Route | Unit | Live | Notes |
|---|---|:---:|:---:|---|
| `GET` | `/api/v1/encodings/custom` | ✓ | — | |
| `PUT` | `/api/v1/encodings/custom` | ✓ | — | |

**Ingests**

| Method | Route | Unit | Live | Notes |
|---|---|:---:|:---:|---|
| `GET` | `/api/v1/ingests` | ✓ | ✓ | |
| `POST` | `/api/v1/ingests` | ✓ | — | |
| `PUT` | `/api/v1/ingests/:id` | ✓ | — | |
| `DELETE` | `/api/v1/ingests/:id` | ✓ | — | |
| `POST` | `/api/v1/ingests/:id/start` | ✓ | ✓ | |
| `POST` | `/api/v1/ingests/:id/stop` | — | ✓ | fault-resilience |

**Status and health**

| Method | Route | Unit | Live | Notes |
|---|---|:---:|:---:|---|
| `GET` | `/api/v1/engine` | ✓ | — | |
| `GET` | `/api/v1/engine/sbom` | ✓ | — | |
| `GET` | `/api/v1/media` | ✓ | — | |
| `GET` | `/api/v1/media/:filename/analysis` | ✓ | — | |
| `PATCH` | `/api/v1/media/:filename` | ✓ | — | Rename + ingest reference update |
| `DELETE` | `/api/v1/media/:filename` | ✓ | — | Path traversal tested |
| `GET` | `/api/v1/engine/health` | ✓ | ✓ | |
| `GET` | `/healthz` | ✓ | ✓ | |
| `GET` | `/metrics/system` | ✓ | — | Structured cpu/memory/disk/network |

**V1 operator API**

| Method | Route | Unit | Live | Notes |
|---|---|:---:|:---:|---|
| `GET` | `/api/logs` | — | — | New; unit tests pending |
| `GET` | `/api/logs/stream` | — | — | SSE; new; unit tests pending |
| `GET` | `/api/v1/alerts` | ✓ | — | Aggregate across all pipelines |
| `GET` | `/api/v1/events` | ✓ | — | Filtering tested |
| `GET` | `/api/v1/overview` | ✓ | — | |
| `GET` | `/api/v1/engine/telemetry` | ✓ | — | |
| `GET` | `/api/v1/pipelines/:id/telemetry` | ✓ | — | |
| `GET` | `/api/v1/stages/:key/telemetry` | ✓ | — | |
| `GET` | `/api/v1/pipelines/:id/summary` | ✓ | — | |

**Agent API**

| Method | Route | Unit | Live | Notes |
|---|---|:---:|:---:|---|
| `GET` | `/api/v1/agent/capabilities` | ✓ | — | |
| `GET` | `/api/v1/agent/context` | ✓ | — | |
| `POST` | `/api/v1/agent/investigations` | ✓ | — | |
| `POST` | `/api/v1/agent/plans` | ✓ | — | |
| `POST` | `/api/v1/agent/plans/validate` | ✓ | — | |
| `POST` | `/api/v1/agent/graph-diff-preview` | ✓ | — | 404 when compiled out |
| `POST` | `/api/v1/agent/operations` | ✓ | — | |
| `GET` | `/api/v1/agent/operations/:id` | ✓ | — | |
| `POST` | `/.../operations/:id/approve` | ✓ | — | |
| `POST` | `/.../operations/:id/apply` | ✓ | — | |
| `POST` | `/.../operations/:id/verify` | ✓ | — | |
| `POST` | `/api/v1/agent/verify` | ✓ | — | 404 when compiled out |

## Code Coverage

Line coverage from `cargo llvm-cov` (unit tests only, June 29, 2026):

Compared with the June 27, 2026 snapshot, covered lines increased from
`13,250` to `13,784` (`+534`), but total instrumented lines increased from
`23,918` to `25,399` (`+1,481`), so overall unit-only line coverage moved from
`55.4%` to `54.3%` (`-1.1` percentage points).

![Coverage by module](coverage-by-module.svg)

| Module | Lines | Covered | Coverage |
|---|---:|---:|---:|
| `pipe_metrics` | 21 | 21 | 100.0% |
| `engine_registries` | 49 | 49 | 100.0% |
| `events` | 284 | 274 | **96.5%** |
| `alerts` | 517 | 506 | 97.9% |
| `security` | 220 | 210 | 95.5% |
| `ring_buffer` | 1,096 | 1,040 | 94.9% |
| `feeder` | 226 | 215 | 95.1% |
| `file_ingest` | 558 | 515 | 92.3% |
| `codec` | 730 | 660 | 90.4% |
| `mpegts` | 2,444 | 2,028 | 83.0% |
| `hls_upload` | 232 | 207 | 89.2% |
| `profiles` | 333 | 285 | 85.6% |
| `stage_metrics` | 44 | 37 | 84.1% |
| `engine` | 3,551 | 2,743 | 77.3% |
| `domain/stage` | 227 | 180 | 79.3% |
| `avio` | 502 | 388 | 77.3% |
| `hls` | 565 | 428 | **75.8%** |
| `recording` | 309 | 193 | **62.5%** |
| `external_transcoder` | 581 | 364 | **62.7%** |
| `srt` | 2,471 | 1,183 | 47.9% |
| `rtmp` | 1,660 | 644 | 38.8% |
| `api` | 3,951 | 385 | 9.7%† |
| `db` | 801 | 0 | 0.0%† |
| **Total** | **25,399** | **13,784** | **54.3%** |

† `api.rs` is tested via 66 integration tests in `tests/api.rs` which `llvm-cov --lib` does not instrument. `db.rs` is tested via `tests/db.rs`. Their unit-only coverage is not representative.

These numbers reflect unit-test-only instrumentation. `api.rs` shows 7% because
`cargo llvm-cov` does not instrument `tests/api.rs` integration tests by
default — the real API test coverage is much higher (66 tests across all 59
routes). Similarly, `db.rs`, `rtmp.rs`, and `srt.rs` are primarily exercised by
the live integration harness which is not captured by `llvm-cov`.

### Coverage interpretation

- **≥80% (14 modules)**: core media pipeline logic — ring buffer, codec,
  MPEG-TS, engine, HLS upload, file ingest, alerts, events, security, profiles,
  feeder, stage_metrics. Well covered by unit tests.
- **50–79% (6 modules)**: socket-heavy protocol handlers, HLS store, and
  recording logic. Primarily exercised by the live harness with real ffmpeg;
  unit-testing their socket loops would require significant mocking for little
  added benefit.
- **<50% (7 modules)**: API/DB/diagnostics layers tested through integration
  tests not captured by `llvm-cov`, or FFmpeg-dependent transcoder code that
  requires the binary running.

## Live Integration Tests

All live integration tests are unified under one entry point:

```sh
scripts/resource-limit target/debug/test_harness [--no-netns] <mode>
```

By default every mode that manages its own server processes runs inside a
private loopback network namespace (`unshare --net`) so ports never conflict
with the host. Pass `--no-netns` to skip namespace re-exec.
When no explicit `RESTREAM_*` or `MTX_*` port env vars are set, the harness
also synthesizes a per-process high-port bundle instead of reusing the legacy
3030/1935/10080 defaults, so correctness runs stay isolated even when the
namespace wrapper is unavailable or constrained.

For concurrency-sensitive changes, run the focused proof gate first:

```sh
bash ./scripts/check-concurrency-proof-fast.sh
```

Then run the full live contract gate:

```sh
bash ./scripts/check-concurrency-contract.sh
```

Required tools: `ffmpeg`, `ffprobe`, `mediamtx`, `curl`, `jq`.
On Debian/Ubuntu, `./scripts/bootstrap-dev.sh` installs everything above.

Common runner flags:

| Flag | Purpose |
|---|---|
| `--preflight` | Check binary, dependencies, namespace support, and host-mode port conflicts without starting the test. |
| `--fast` | Set `N_PER_GROUP=1`, `N_OUTPUTS=1`, `SNAP_EVERY=999`, and skip snapshot sleeps for quick agent loops. |
| `--json <path>` | Write JSONL assertion records alongside the human-readable log. Failed ffprobe assertions include stderr and log tails. |
| `ONLY_CHECKS=<checks>` | Run selected mixed-input assertion groups. Supported checks: `smoke`, `ffprobe`, `hls`, `recording`, `stage-sharing`, `lifecycle`, `load`. |
| `--skip-load` | Skip resource snapshot sleeps and load assertion records while preserving correctness setup. |
| `--resume-from <id>` | Skip named assertion records until the requested assertion ID is reached. |
| `RSS_BASELINE=<path>` | Compare mixed-input RSS summaries against a saved CSV baseline. `RSS_BASELINE_THRESHOLD_PCT` defaults to 5. |
| `SAVE_RSS_BASELINE=<path>` | Save the current mixed-input RSS summary as a baseline CSV. |

The runner kills only processes it starts. Set `ALLOW_GLOBAL_PROCESS_CLEANUP=1`
only when you explicitly want the legacy host-wide `restream`/`mediamtx`
cleanup before a run.

### Artifact Disk Guards

Live integration runs write logs, JSONL assertions, ffprobe stderr, SQLite
fixtures, generated media, and manifests under `test/artifacts/` by default.
The runner applies two disk-safety guards before starting live services:

- `RESTREAM_ARTIFACT_MIN_FREE_MB` (default `2048`) fails the run when the
  artifact filesystem has less free space than the configured floor. Set it to
  `0` only for an intentional no-floor diagnostic run.
- old top-level `test/artifacts/` directories are pruned so only the latest
  three runs remain. The active run directory is protected. Set
  `KEEP_ARTIFACTS=1` only for a deliberate manual-retention/debug session.

`preflight` emits an `artifact-disk` JSON record with the artifact root,
current free MB, configured floor, and pass/fail status. Protocol-matrix runs
inherit the same guard for each delegated mode. When `--no-netns` is used,
preflight emits the candidate `ports` list and checks the actual ports a mode
binds: legacy live modes check the configured Restream/MediaMTX ports, while
Rust-only harness modes check the harness loopback ports (`11935` for RTMP,
`11080` for SRT, and `HLS_PUT_PORT` for the dummy HLS PUT sink).
If a measurement live mode needs `target/bench/restream` and the repo-managed
static SRT archive is also missing, the binary check points agents at
`scripts/resource-limit ./scripts/setup-static-build.sh` before the bench-profile
build step.

Typical quick agent loop:

```sh
scripts/resource-limit target/debug/test_harness preflight
scripts/resource-limit target/bench/test_harness mixed-h264-srt-single
```

### Manual Dashboard Live Env

For UI/debug sessions it is useful to run a long-lived dashboard plus an
independent MediaMTX sink outside the integration wrapper. This is not a
release certification gate; it is an operator-facing smoke setup that makes the
dashboard, processing graph, status page, HLS preview, output history, and
media library easy to inspect while real traffic is flowing.

Current local shape used on June 27, 2026:

| Component | Ports / paths |
|---|---|
| Restream dashboard/API | `http://127.0.0.1:39280` |
| Restream RTMP ingest | `rtmp://127.0.0.1:32080/live/<streamKey>` |
| Restream SRT ingest | `srt://127.0.0.1:31280?streamid=publish:live/<streamKey>` |
| MediaMTX RTMP sink | `rtmp://127.0.0.1:33080/live/<path>` |
| MediaMTX SRT sink | `srt://127.0.0.1:34080?streamid=publish:live/<path>` |
| MediaMTX HLS sink | `http://127.0.0.1:35080/<path>/index.m3u8` |
| Runtime work dir | `/tmp/restream-live-current` |

The live traffic is published by one combined FFmpeg process with three looping
inputs and multiple outputs:

| Pipeline | Ingest | Expected input | Sink outputs |
|---|---|---|---|
| `RTMP 1080p50 H264` | RTMP | H.264 `1920x1080` 50 fps, at least 8 Mbps | RTMP source sink, SRT source sink |
| `SRT 4K60 H264` | SRT | H.264 `3840x2160` 60 fps, at least 20 Mbps, two AAC tracks | SRT source sink, RTMP source sink |
| `SRT 4K60 H265` | SRT | HEVC `3840x2160` 60 fps, at least 20 Mbps, two AAC tracks | SRT HEVC passthrough sink, RTMP H.264 compatibility sink |

Expected sink-probe behavior:

- SRT sinks should preserve the source codec, dimensions, and frame rate.
- RTMP sinks from H.264 sources should remain H.264 at source dimensions.
- RTMP from the H.265 source uses the `hevc_to_h264` compatibility stage. It is
  expected to probe as H.264, not HEVC, and may not preserve the source frame
  rate exactly while that compatibility path is under active tuning.
- MediaMTX accepting these streams is interop evidence for the live setup, not
  proof that every protocol-matrix release gate has passed.

### `ramp` — Sequential output ramp

```sh
N_OUTPUTS=10 scripts/resource-limit target/debug/test_harness ramp
```

Sweeps eight ingest×egress×encoding combinations (RTMP/SRT ingest × RTMP/SRT
output × source/720p encoding). For each config, outputs are added one by one
and RSS + FFmpeg subprocess counts are snapshotted at every step. Useful for
spotting per-output memory growth and spotting encoding-stage leaks.

Env: `N_OUTPUTS` (default 10), `ISOLATE=1` (restart restream+mediamtx per
config for a clean baseline), `SNAP_EVERY` (default 1, snapshot every N outputs).

The public shell mode has begun moving behind typed Rust harness slices. By
default, all eight ramp configs are delegated to
`cargo run --bin test_harness -- ramp-family`, which starts the production
`restream` binary and MediaMTX, drives the HTTP API, and appends the same
`scale.csv` and `summary.txt` formats. Set `RAMP_RUST_FAMILY=0` to force the
legacy all-bash ramp path while bisecting harness behavior, or set
`RAMP_FAMILY_CONFIGS` to hand a subset back to bash for focused comparisons.

### `mixed-input-matrix` — Mixed input/output correctness

```sh
./scripts/build-bench-harness.sh
N_PER_GROUP=2 ONLY_CHECKS=hls scripts/resource-limit target/bench/test_harness mixed-input-matrix
```

Exercises the table-driven input matrix. Names follow
`mixed-<codec>-<protocol>-<single|multi>` for live inputs and
`mixed-file-<codec>-<single|multi>` for file ingest. RTMP ingest intentionally
has only one row because standard RTMP input is H.264 with a single audio track;
there is no `h265-rtmp-*` or `*-rtmp-multi` input case unless the product
contract changes.

| Config | Ingest | Codec | Audio | Role |
|---|---|---|:---:|---|
| `file-h264-single` | file | H.264 | 1 | file-ingest H.264 baseline |
| `file-h265-single` | file | H.265 | 1 | file-ingest HEVC baseline |
| `file-h264-multi` | file | H.264 | 2 | file-ingest multi-audio routing |
| `file-h265-multi` | file | H.265 | 2 | file-ingest HEVC + multi-audio routing |
| `h264-rtmp-single` | RTMP | H.264 | 1 | RTMP/FLV ingest baseline |
| `h264-srt-single` | SRT | H.264 | 1 | HLS + smoke + fatal ffprobe + stop lifecycle |
| `h265-srt-single` | SRT | H.265 | 1 | HEVC bridge and stage-sharing assertion |
| `h264-srt-multi` | SRT | H.264 | 2 | multi-audio track routing |
| `h265-srt-multi` | SRT | H.265 | 2 | HEVC + multi-audio |

Each row can be run directly as `target/bench/test_harness mixed-<row>`, for
example `target/bench/test_harness mixed-h265-srt-multi`. The aggregate
`mixed-input-matrix` runs every row sequentially under its own work directory so
artifacts and HLS segments from one case cannot contaminate another.

The row structure is deliberately two-layered:

| Scope | Coverage |
|---|---|
| Input-scoped | One HLS preview assertion per input row; one recording assertion per input row. |
| Output-scoped | Every legal RTMP/SRT egress row for the input track layout. |

Single-track inputs run this egress table:

| Protocol | Encodings | Expected audio tracks |
|---|---|---:|
| RTMP | `source`, `720p`, `1080p` | 1 |
| SRT | `source`, `720p`, `1080p` | 1 |

Multi-track inputs, including file multi fixtures, run the expanded table:

| Protocol | Encodings | Expected audio tracks |
|---|---|---:|
| RTMP | `source+atrack:0`, `source+atrack:1`, `720p+atrack:0`, `720p+atrack:1`, `1080p+atrack:0`, `1080p+atrack:1` | 1 |
| SRT | `source`, `720p`, `1080p` | 2 |
| SRT | `source+atrack:0`, `source+atrack:1`, `720p+atrack:0`, `720p+atrack:1`, `1080p+atrack:0`, `1080p+atrack:1` | 1 |

HLS preview currently asserts the browser-compatible preview contract:
H.264 input remains source-size H.264 MPEG-TS HLS, while H.265 input is
converted to 720p H.264 before the MPEG-TS HLS preview path. The HLS assertion
also checks that the master playlist exposes the source audio-track count as
audio renditions so the browser can select all available tracks.

Recording is intentionally input-scoped rather than multiplied by every egress:
it starts/stops once after the input is live, then validates the operator-visible
MP4 has the source video codec (`h264` or `hevc`) and the expected source
audio-track count.

**H.264 SRT single (`h264-srt-single`)** runs three merged correctness checks in
addition to the resource measurements:

1. **Smoke** — after source outputs are live, asserts no external transcoder has
   fired (source passthrough must not trigger the 720p encoder).
2. **Fatal ffprobe** — after all groups, `verify_stream` (fatal, 30×2s retries)
   on RTMP-src, RTMP-720p, SRT-src, SRT-720p, HLS/mediamtx, and HLS/restream
   endpoints.
3. **Stop lifecycle** — calls `/stop` on every output and polls `/api/v1/settings` until
   all reach `"stopped"` within 60 s.

`ONLY_CHECKS=stage-sharing` asserts that the live processing graph has exactly
the expected unique `transcoder`, `audio_filter`, and `codec_edge` nodes for the
row. This is the live `N_PER_GROUP` sharing check: increasing output count must
not add processing stages when the output encoding/protocol shape is identical.
`N_PER_GROUP=2` is enough to catch accidental per-output stage duplication
because the second output in every group has the same processing shape as the
first. It does not, by itself, cover every processing graph shape; coverage of
source/720p/1080p, RTMP/SRT, all-audio/subset-audio, HLS preview, and recording
comes from the table-driven mixed input/output matrix.
Because `ONLY_CHECKS=stage-sharing` does not attach every readback consumer,
the live assertion treats audio-route nodes as an upper bound. Video transcode
and codec-edge nodes are exact, and `codec_edge=3` for H.265 multi is the
expensive-stage invariant the check is designed to protect.

Expected resource counts (see
[media-pipeline.md § Scale Test Pipeline Paths](media-pipeline.md#scale-test-pipeline-paths)):

| Config | Video stages | Audio-route stages | Codec-edge stages |
|---|:---:|:---:|:---:|
| `h264-*-single` | 2 (`720p`, `1080p`) | 0 | 0 |
| `h265-*-single` | 2 (`720p`, `1080p`) | 0 | 3 (`source`, `720p`, `1080p` RTMP paths) |
| `h264-*-multi` | 2 (`720p`, `1080p`) | 6 | 0 |
| `h265-*-multi` | 2 (`720p`, `1080p`) | 12 | 3 (`source`, `720p`, `1080p` RTMP paths) |

Env: `N_PER_GROUP` (default 25).

The mixed input configs are Rust-owned through `test_harness` entry points and
preserve the manifest, CSV, summary, and JSONL assertion layout. The multi-audio
rows preserve the two-audio SRT/file fixture, RTMP selected-audio egresses, and
SRT all-tracks plus selected-track egresses.

Set `FFMPEG_BIN_PATH=/usr/bin/ffmpeg` explicitly only for streaming-logic
diagnosis against the system binary. Normal runs use the embedded standalone
`public/bin/ffmpeg` through the production `restream` child. All selected
`ffprobe` checks emit fatal JSONL assertions and honor `RESUME_FROM`.

### `resource-sweep` — CPU and memory attribution sweep

```sh
./scripts/build-bench-harness.sh
./target/bench/test_harness resource-sweep
```

Measures current-code CPU and memory across baseline, ingest-only, ingest
growth, egress growth, source-vs-transcode, and HEVC bridge scenarios. The
Rust harness records:

- process RSS plus `smaps_rollup` (`Anonymous`, `Private_Dirty`, `Shared_Clean`, `Pss`)
- internal memory accounting from `/api/v1/engine/telemetry`
- child FFmpeg RSS and CPU
- 1 Hz raw samples and per-stage aggregates

See [resource-sweep.md](resource-sweep.md) for the artifact layout and env
knobs.

Useful narrow-loop knobs:

- `RESOURCE_SWEEP_SCENARIOS=...` to run only a named slice such as
  `egress-growth-transcode-mixed`, `egress-growth-transcode-dual-mixed`, or
  `egress-growth-hevc-bridge`
- `RESOURCE_SWEEP_EGRESS_COUNTS=10` or `RESOURCE_SWEEP_INGEST_COUNTS=5` to pin
  the fanout/fanin you care about
- `RESOURCE_SWEEP_LIFECYCLE=isolated|continuous|cumulative` to compare clean
  attribution against additive growth
- `RESOURCE_SWEEP_SAMPLE_SECS=30` when you want enough time to attach `perf`
  during a single scenario

### `bitrate-sweep` — bitrate sensitivity sweep

```sh
./scripts/build-bench-harness.sh
./target/bench/test_harness bitrate-sweep
```

This Rust harness mode runs the five ingest shapes at configurable bitrate
points and records a focused bitrate-sensitivity report:

- parent and child RSS/CPU
- retained payload min/max/final plus growth rate
- source/transcoder/tsmux ring peaks
- AVIO queue fill and HWM
- source-ring overflow counts
- correctness of RTMP source, RTMP 720p, SRT source, and SRT 720p outputs

Artifacts are written to `test/artifacts/bitrate-sweep/` by default:

- `bitrate-sweep-results.json`
- `bitrate-sweep-results.csv`
- `bitrate-sweep-samples.jsonl`
- `restream.log`, `mediamtx.log`, and publisher logs

Useful env vars:

- `BITRATE_SWEEP_CONFIGS=h264-rtmp,h264-srt,h265-srt,h264-srt-multi,h265-srt-multi`
- `BITRATE_SWEEP_BITRATES=1.5M,4M,8M`
- `BITRATE_SWEEP_OUTPUT_GROUPS=1`
- `BITRATE_SWEEP_STABILIZE_SECS=30`
- `BITRATE_SWEEP_SAMPLE_INTERVAL_SECS=5`

### `branch-matrix` — passthrough vs transcode family baseline

```sh
./scripts/build-bench-harness.sh
./target/bench/test_harness branch-matrix
```

This is a focused current-code baseline for one question: how much cost comes
from passthrough fanout versus adding another distinct transcode family.

It runs five fixed H.264 SRT egress shapes:

- source only
- one shared transcode family (`720p`)
- source + one shared transcode family
- two shared transcode families (`720p` + `1080p`)
- source + two shared transcode families

Artifacts are written to `test/artifacts/branch-matrix/` by default:

- `branch-matrix-results.json`
- `branch-matrix-results.csv`
- `branch-matrix-summary.md`
- `branch-matrix-samples.jsonl`

Useful env vars:

- `BRANCH_MATRIX_EGRESS_COUNT=10`
- `BRANCH_MATRIX_SCENARIOS=egress-growth-source-mixed,egress-growth-transcode-mixed`
- `RESOURCE_SWEEP_SAMPLE_SECS=6`
- `RESOURCE_SWEEP_SETTLE_SECS=4`
- `RESOURCE_SWEEP_LIFECYCLE=isolated|continuous|cumulative`
- `RESTREAM_USE_INTERNAL_TRANSCODER=1` to capture the internal-backend baseline
- `HARNESS_SRT_PASSPHRASE=0123456789abcd` and `HARNESS_SRT_PBKEYLEN=16` to
  rerun the matrix with encrypted SRT ingest

### `srt-crypto-matrix` — plaintext vs AES-128/192/256 ingest

```sh
./scripts/build-bench-harness.sh
RESTREAM_BIN=target/bench/restream \
./target/bench/test_harness srt-crypto-matrix
```

Runs the branch-matrix sweep four times against the same focused H.264 SRT
ingest scenario family:

- plaintext
- encrypted AES-128 (`pbkeylen=16`)
- encrypted AES-192 (`pbkeylen=24`)
- encrypted AES-256 (`pbkeylen=32`)

Each variant gets its own subdirectory under `test/artifacts/branch-matrix/`.
The harness configures both sides of each ingest consistently: the Restream SRT
listener gets the matching passphrase/key length, and every SRT publisher URL in
that run gets the corresponding `passphrase`/`pbkeylen` query parameters.

Useful env vars:

- `SRT_CRYPTO_MATRIX_VARIANTS=plaintext,enc16,enc24,enc32`
- `BRANCH_MATRIX_SCENARIOS=egress-growth-source-mixed`
- `BRANCH_MATRIX_EGRESS_COUNT=1`
- `RESOURCE_SWEEP_SAMPLE_SECS=2`
- `RESOURCE_SWEEP_SETTLE_SECS=1`

### `bonding` — SRT socket bonding

```sh
scripts/resource-limit target/debug/test_harness bonding
```

Verifies libsrt group-socket bonding using dedicated C helper binaries compiled
from `test/srt-bond-server.c` and `test/srt-bond-client.c` against a statically
linked libsrt 1.5.5 built with `ENABLE_BONDING=ON`. The script calls
`scripts/resource-limit ./scripts/setup-static-build.sh` automatically on first
run.

Two bonding modes are tested:

| Mode | Members | `failover` | Messages |
|---|:---:|:---:|:---:|
| `broadcast` | 2 | 0 | 1 |
| `backup` | 2 | 1 | 2 |

Fails if `SRTO_GROUPCONNECT` is unavailable, the two member sockets do not
attach to the group, or backup delivery does not continue after the primary
member closes.

Note: `bonding` runs on the host network (random ports) so it is exempt from
netns re-exec even without `--no-netns`.
For real multi-NIC or dual-WAN validation, remember that upstream SRT also
recommends `ENABLE_PKTINFO=ON`; otherwise a wildcard listener may reply from
the wrong source IP and make a healthy bonding implementation look broken.

### `mixed-h264-srt-single` — Closed-GOP probe bundle

```sh
scripts/resource-limit target/bench/test_harness mixed-h264-srt-single
```

Streams a closed-GOP RTMP/SRT matrix across H.264/H.265, 1080p/4K, selected
frame rates, and single/dual-audio variants. Each case starts a source output,
waits for ingest, samples `/api/v1/pipelines/:id/graph`, and fails if active ring
readers do not report positive `burstCount` and `avgBurstSize` telemetry.

Env: `BURST_SETTLE_SECS` (default 8), `BURST_CONFIGS` (optional
space-separated config allow-list, e.g. `BURST_CONFIGS="srt-h265-1080p-24fps-1a"`).

The former anchor checks now live in the normalized `mixed-h264-srt-single`
table row; the matrix publishers, burst graph assertions, and HLS PUT probe
checks are implemented in Rust.

### HLS PUT probe

Publishes one SRT H.264/AAC input, starts both HTTP/YouTube-style `file=` and
path-style HLS PUT outputs, and verifies that a local dummy sink receives
`seg<N>.ts` media segments plus playlists with the expected content types. The
path-style output also verifies signed query preservation. Uploaded segments
from both output shapes are probed with `ffprobe`. The mode then restarts the
dummy sink and requires fresh segment PUTs after recovery for both shapes.

Env: `HLS_PUT_PORT` (default 8990), `HLS_PUT_SETTLE_SECS` (default 8),
`HLS_PUT_RESTART_SECS` (default 12).

The HLS PUT probe runs as part of `mixed-h264-srt-single` and validates dummy PUT
sink delivery, signed-query preservation, ffprobe readability, and restart
recovery behavior.

### `bframe-rtmp` — RTMP B-frame timestamp round-trip

```sh
scripts/resource-limit target/debug/test_harness bframe-rtmp
```

Publishes one RTMP H.264/AAC input with B-frames, starts an RTMP source output,
and probes the egress packet stream with `ffprobe -show_packets`. The mode
requires at least one video packet with `PTS > DTS` and verifies DTS stays
monotone across the captured egress packets.

The public shell mode is now a thin artifact/summary wrapper around
`cargo run --bin test_harness -- bframe-rtmp`; the live scenario, packet probe,
and assertions are implemented in Rust.

## Validation Results: June 20, 2026

Environment: WSL2, 20 logical CPUs, 7.6 GiB RAM, 2 GiB swap.

### Correctness

An eight-second generated H.264/AAC MPEG-TS file was looped through real FFmpeg
publishers.

| Test | Result | External `ffprobe` |
|---|---|---|
| File → RTMP ingest → RTMP read | PASS | H.264 640x360 + AAC 48 kHz mono |
| File → SRT ingest → SRT read | PASS | H.264 640x360 + AAC 48 kHz mono |
| RTMP source → RTMP egress → RTMP sink read | PASS | H.264 640x360 + AAC 48 kHz mono |
| RTMP source → SRT egress → SRT sink read | PASS | H.264 640x360 + AAC 48 kHz mono |

Every probe contained exactly one video and one audio stream.

### In-Process Load

```text
500 RingBuffer readers, 2,000 source packets, 1,316-byte payload
→ 1,000,000/1,000,000 deliveries, 1.316 GB logical, 51.36 M deliveries/s
→ 27,516 KiB peak RSS
```

### Bounded Network Load

```text
32 RTMP egress sessions, in-process RTMP handshake-and-discard sink, 5s hold
→ 32/32 connections, 9,408 media messages, 9.686 Mbps aggregate
→ 28,800 KiB peak RSS
```

### FFmpeg Assembly Benchmark (June 21, 2026)

Matched static FFmpeg 6.1.5, pinned single-CPU, median of seven runs:

| Workload | No x86 asm | x86 asm | Speedup |
|---|---:|---:|---:|
| 4K HEVC decode, 3s | 2.48 s | 1.27 s | 1.95× |
| 1080p H.264 decode, 5s | 0.62 s | 0.29 s | 2.14× |
| 4K HEVC decode + 1080p scale, 2s | 3.82 s | 1.22 s | 3.13× |
| 4K HEVC → 1080p H.264/x264, 2s | 5.45 s | 2.49 s | 2.19× |

## End-to-End Test Plan

### Deterministic Fixtures

**Dual-Audio H.264:**
```bash
ffmpeg -y \
  -f lavfi -i "testsrc2=size=1920x1080:rate=30" \
  -f lavfi -i "sine=frequency=440:sample_rate=48000" \
  -f lavfi -i "sine=frequency=880:sample_rate=48000" \
  -t 120 \
  -map 0:v -map 1:a -map 2:a \
  -c:v libx264 -preset slow -g 60 -bf 2 \
  -c:a aac -b:a 128k \
  -metadata:s:a:0 title=track-440hz \
  -metadata:s:a:1 title=track-880hz \
  test/artifacts/dual-audio-h264.mkv
```

**Dual-Audio H.265:**
```bash
ffmpeg -y \
  -f lavfi -i "testsrc2=size=1920x1080:rate=30" \
  -f lavfi -i "sine=frequency=440:sample_rate=48000" \
  -f lavfi -i "sine=frequency=880:sample_rate=48000" \
  -t 120 \
  -map 0:v -map 1:a -map 2:a \
  -c:v libx265 -preset slow -x265-params "keyint=60:bframes=2" \
  -c:a aac -b:a 128k \
  test/artifacts/dual-audio-h265.mkv
```

Also retain short 10-second versions for smoke tests.

### Phase 1: Ingest Equivalence

Publish the same H.264 fixture to both RTMP and SRT pipelines. Verify:

- both active within 10 seconds
- correct protocol reported
- bytes and bitrate increase continuously
- process survives sequence headers, B-frames, reconnects, and shutdown
- no subtitle, data, or unknown streams in the media ring

### Phase 2: Probe Matching

Use both engine snapshots (`/api/v1/pipelines/:id/probe`) and external `ffprobe` via
matching protocol. Compare: video codec, dimensions, frame rate, audio codec,
sample rate, channels, track count, GOP interval.

| Field | Tolerance |
|---|---|
| Codec, dimensions, sample rate, channels | Exact |
| Frame rate | ±0.01 fps |
| GOP interval | ±1 frame |
| Average bitrate | ±10% after warm-up |
| A/V start offset | ≤ 50 ms |
| A/V drift over 10 min | ≤ 20 ms |

### Phase 3: Egress Correctness Matrix

2 ingests × 6 video shapes × 6 audio modes × 3 protocols = 216 cases.
Use pairwise reduction for CI; full Cartesian nightly. Always include collision
cases (`720p+atrack:0`, `720p+atrack:1`, `1080p+atrack:0`, `source+atrack:0`)
to prove stage sharing and audio isolation.

Per-output assertions:

- correct stream count and types
- resolution matches preset
- all packets decode for 30s with `-xerror`
- DTS monotonic per stream
- valid PTS/DTS reordering for B-frames
- A/V start offset ≤ 50 ms
- no drift beyond 20 ms over long test
- stopping one output does not interrupt shared stages

Audio routing content assertions (via `astats`, `channelsplit`, frequency
detection):

| Routing | Assertion |
|---|---|
| `passthrough` | Both 440 Hz and 880 Hz tracks remain |
| `atrack:0` | Only 440 Hz |
| `atrack:1` | Only 880 Hz |
| `atrack:0,1` | Both in requested order |
| `remap:0:1:0` | Correct channel derivation |
| `downmix:0` | Stereo with expected contribution |

### Phase 4: H.265 Coverage

Publish H.265 via SRT. Verify SRT passthrough preserves HEVC identity, RTMP
egress capability test, no silent HEVC-as-H.264 mislabeling.

`cargo run --bin test_harness -- correctness-hevc-rtmp` covers the RTMP edge:
it ingests H.265 over SRT, runs the shared `hevc_to_h264` stage, and verifies
the RTMP egress as H.264 video plus AAC audio.

`cargo run --bin test_harness -- correctness-hevc-srt` covers native SRT
passthrough: it ingests H.265 over SRT, loops it through SRT egress, and
verifies HEVC video plus AAC audio at the SRT read endpoint.

`cargo run --bin test_harness -- correctness-srt-rtmp` covers the direct
cross-protocol packetization path: it ingests H.264/AAC over SRT, loops it
through RTMP egress, and verifies H.264 video plus AAC audio at the RTMP read
endpoint.

### Phase 5: Recovery and Isolation

- publisher stop/restart
- sink restart during active outputs
- 1%, 3%, 5% packet loss + 50 ms jitter on SRT
- add/remove outputs sharing video stages
- one slow sink does not stall others
- readers recover at keyframe after ring overflow
- shared stages survive while dependents exist, terminate after last stops

### Phase 6: Scale Benchmarks

**In-process** (no network): 500 null consumers, deterministic packet replay.
Measures engine CPU/memory independent of network.

**Networked**: custom separate-process sink (RTMP/SRT/HLS PUT listeners),
ramp 1→10→50→100→250→500 outputs, hold 30 min at 500, 2-hour soak.

Functional gates: 500/500 publishing, all receive bytes, no unexpected
termination, aggregate bitrate ±5%, no ring overflow, resources return to
baseline on stop.

### Automation

Currently checked in:

```text
scripts/resource-limit target/debug/test_harness ramp-family
scripts/resource-limit target/bench/test_harness mixed-h264-srt-single
scripts/resource-limit target/debug/test_harness bonding
scripts/resource-limit target/debug/test_harness bframe-rtmp
scripts/resource-limit target/debug/test_harness correctness-srt-rtmp
scripts/resource-limit target/debug/test_harness correctness-hevc-rtmp
scripts/resource-limit target/debug/test_harness correctness-hevc-srt
./target/bench/test_harness resource-sweep
./target/bench/test_harness bitrate-sweep
test/run-media-validation.sh
```

Aggregate release-evidence runner:

```sh
cargo run --bin test_harness -- suite --run-id <run-id>
```

Use `test_harness suite` as the canonical aggregate orchestrator. It creates
`test/artifacts/<run-id>/manifest.json`, runs each checked-in integration mode
in its own subdirectory, and records one JSONL result per mode in
`test/artifacts/<run-id>/results.jsonl`. Supported suite options are:

- `--run-id <id>` to choose the artifact run id
- `--work-root <path>` to choose the aggregate artifact directory
- `--only-modes mixed-h264-srt-single,bframe-rtmp` to run a subset
- `--preflight-only` to run readiness checks without starting live services
- `--continue-on-fail` to keep collecting artifacts after the first failure

Why the aggregate runner lives in `test_harness` instead of a separate
`protocol_matrix` binary:

- The suite and the per-mode scenarios already share the same artifact layout,
  loopback namespace handling, fixture generation, child-process helpers, and
  result serialization.
- Keeping orchestration and mode execution in one binary avoids a second Rust
  surface that can drift in CLI semantics, manifest shape, or per-mode naming.
- `suite_run()` can spawn the same executable for each mode, which keeps the
  aggregate runner honest: it exercises the exact per-mode entrypoints used in
  focused runs instead of re-implementing them in a parallel binary.
- The old shell-plus-`protocol_matrix` path no longer buys us anything. The
  aggregate orchestration logic is already implemented in `test_harness`, so
  the extra wrapper only adds another compatibility surface to maintain.

`mixed-h264-srt-single`, `bframe-rtmp`, `correctness-srt-rtmp`,
`correctness-hevc-rtmp`, and `correctness-hevc-srt` are behind typed Rust
harness entry points, and `ramp-family` runs the full eight-config ramp matrix.
`mixed-h264-srt-single` owns the former anchor probe bundle.

`test_harness` writes `manifest.json` in the selected `WORK_DIR`
for each checked-in mode. The manifest starts as `RUNNING` and is finalized to
`PASS` or `FAIL` with timestamps, git head, network mode, and primary artifact
paths. This applies even to setup failures after the mode has initialized its
artifact directory, making failed matrix attempts auditable instead of silent.

Planned scenario families for the remaining matrix should be added as
`test_harness` entries:

```text
ingest-equivalence
egress-matrix
h265
recovery
scale-inprocess
scale-500
```

Each completed matrix run should write artifacts to `test/artifacts/<run-id>/` with manifest,
environment, per-case results (PASS/FAIL/EXPECTED_FAIL/SKIPPED/INFRA_FAILURE),
ffprobe output, captures, metrics, logs, and summary.

## Capability Gates

These capabilities must be treated as test results, not assumptions:

| Capability | Gate |
|---|---|
| RTMP H.264/AAC ingest and egress | B-frame timestamp round-trip through `target/debug/test_harness bframe-rtmp` |
| SRT H.264 and H.265 ingest/egress | Full correctness matrix |
| H.265 SRT passthrough | Live HEVC identity preservation through `target/debug/test_harness correctness-hevc-srt` |
| H.265 source to RTMP egress | Live H.265→H.264 edge conversion through `target/debug/test_harness correctness-hevc-rtmp` |
| Cross-protocol SRT→RTMP | Live H.264/AAC packetization through `target/debug/test_harness correctness-srt-rtmp` |
| Built-in video presets (`h264`, `720p`, `1080p`) | Decode/filter/encode loop is covered by transcoder integration tests |
| Additional/custom video presets | Must be explicitly profiled and matrix-tested before advertising |
| Embedded FFmpeg subprocess feature set | `scripts/build-static.sh` runs `restream-ffmpeg-capabilities` to prove the required codecs, `file`/`pipe` protocols, and `mov`/`matroska`/`mpegts` mux/demux surface are present |
| HLS live segments | Native TsMuxer validates in-memory |
| HLS upload egress | YouTube-style `file=` and path-style signed-query HTTP PUT delivery plus destination restart recovery are covered by unit tests and the `mixed-h264-srt-single` HLS PUT probe |
| Recording | Readable file with correct streams/timestamps |
| Audio remap/downmix | Channel-level filtering is implemented for the default runtime; full audio-content matrix remains required |
| Custom encoding | Runtime output selection must stay rejected until custom args are applied by a transcoder backend |
| Bonded SRT ingest | Separate-process broadcast + backup tests |

## Current Resource Measurements (2026-06-28)

These numbers are authoritative current-code measurements generated by the Rust
harness:

- `test/artifacts/resource-sweep-authoritative/resource-sweep-results.csv`
- `test/artifacts/bitrate-sweep-authoritative/bitrate-sweep-results.csv`

Both sweeps use live ingest/egress, sample `/proc`, and cross-check against
`/api/v1/engine/telemetry`.

### Resource Sweep Snapshot

Isolated sweep, current default code:

| Scenario | Restream MB | Child FFmpeg MB | Combined MB | Restream CPU % | Child FFmpeg CPU % | Total CPU % |
|---|---:|---:|---:|---:|---:|---:|
| Empty baseline | 72.8 | 0.0 | 72.8 | 1.15 | 0.00 | 1.15 |
| Same ingest growth, 5x H.264 SRT | 82.6 | 0.0 | 82.6 | 7.27 | 0.00 | 7.27 |
| Mixed ingest growth, 5 ingest types | 75.9 | 0.0 | 75.9 | 8.92 | 0.00 | 8.92 |
| Mixed source egress, 20 outputs | 83.8 | 0.0 | 83.8 | 11.86 | 0.00 | 11.86 |
| Mixed 720p transcode egress, 20 outputs | 120.3 | 166.5 | 286.8 | 17.96 | 33.69 | 51.65 |
| HEVC bridge, 10 RTMP source outputs | 158.7 | 0.0 | 158.7 | 71.82 | 0.00 | 71.82 |

Current queue/ring peaks for those same rows:

| Scenario | Source Ring MB | Transcoder Ring MB | TsMux Ring MB | AVIO HWM MB |
|---|---:|---:|---:|---:|
| Empty baseline | 0.1 | 0.0 | 0.0 | 0.0 |
| Same ingest growth, 5x H.264 SRT | 19.0 | 0.0 | 0.0 | 0.0 |
| Mixed ingest growth, 5 ingest types | 15.6 | 0.0 | 0.0 | 0.0 |
| Mixed source egress, 20 outputs | 5.8 | 0.0 | 1.5 | 0.5 |
| Mixed 720p transcode egress, 20 outputs | 5.7 | 8.3 | 4.3 | 4.6 |
| HEVC bridge, 10 RTMP source outputs | 5.8 | 8.2 | 0.0 | 0.0 |

Takeaways:

- Idle baseline is about `73 MB` in the Restream process before live traffic.
- Ingest fan-in without transcode is cheap in RSS: `~76-83 MB` for five live
  pipelines depending on mix.
- Mixed `720p` transcode egress is the main external-process memory consumer:
  `~120 MB` in Restream plus `~166 MB` in the child FFmpeg at 20 outputs.
- HEVC bridge remains expensive in-process: `~159 MB` and `~72%` CPU at 10
  source outputs, with no external child involved.

### Bitrate Sweep

Bitrate sweep runs one pipeline with four outputs (`RTMP source`, `RTMP 720p`,
`SRT source`, `SRT 720p`) and verifies all four with `ffprobe`.

| Ingest Config | Bitrate | Restream MB | Child FFmpeg MB | Combined MB | Restream CPU % | Child FFmpeg CPU % | Total CPU % | Correctness |
|---|---:|---:|---:|---:|---:|---:|---:|---|
| `h264-rtmp` | 1.5M | 86.9 | 167.0 | 253.9 | 7.63 | 33.49 | 41.12 | PASS |
| `h264-rtmp` | 4M | 103.2 | 166.9 | 270.1 | 6.43 | 33.13 | 39.56 | PASS |
| `h264-rtmp` | 8M | 118.2 | 169.2 | 287.3 | 5.69 | 31.99 | 37.68 | PASS |
| `h264-srt` | 1.5M | 93.7 | 166.5 | 260.2 | 7.16 | 34.79 | 41.95 | PASS |
| `h264-srt` | 4M | 112.3 | 167.4 | 279.7 | 7.76 | 37.85 | 45.61 | PASS |
| `h264-srt` | 8M | 136.7 | 160.8 | 297.5 | 7.66 | 36.76 | 44.42 | PASS |
| `h265-srt` | 1.5M | 220.1 | 317.0 | 537.1 | 173.84 | 256.39 | 430.23 | PASS |
| `h265-srt` | 4M | 241.8 | 299.7 | 541.5 | 146.59 | 160.12 | 306.71 | PASS |
| `h265-srt` | 8M | 278.4 | 303.3 | 581.6 | 161.90 | 148.84 | 310.75 | PASS |
| `h264-srt-multi` | 1.5M | 93.8 | 167.4 | 261.2 | 7.89 | 38.91 | 46.80 | PASS |
| `h264-srt-multi` | 4M | 111.2 | 168.1 | 279.3 | 8.19 | 39.01 | 47.20 | PASS |
| `h264-srt-multi` | 8M | 135.1 | 170.3 | 305.4 | 8.99 | 37.95 | 46.94 | PASS |
| `h265-srt-multi` | 1.5M | 215.8 | 317.9 | 533.7 | 168.13 | 243.90 | 412.03 | PASS |
| `h265-srt-multi` | 4M | 240.8 | 300.1 | 541.0 | 125.86 | 141.00 | 266.86 | PASS |
| `h265-srt-multi` | 8M | 252.0 | 316.7 | 568.6 | 160.18 | 159.96 | 320.14 | PASS |

Current bitrate-sweep takeaways:

- H.264 ingest scales upward with bitrate mostly in retained memory, not in a
  proportional jump in CPU. Combined memory ends up in the `~254-305 MB` range
  for the four-output shape.
- External FFmpeg RSS is comparatively flat for H.264 cases, roughly
  `161-170 MB`, while Restream parent RSS grows with bitrate and protocol mix.
- H.265 ingest is much more expensive because the bridge/transcode path is
  active. Combined memory is `~534-582 MB`, and total CPU is `~267-430%`
  depending on bitrate and audio shape.
- All 15 current cases passed output correctness.

## Media Correctness Findings (2026-07-01)

These issues were found while hardening the `mixed-h265-srt-multi` live matrix
around the checked-in H.265 + two-audio fixture.

### Fixed Runtime Issues

- RTMP egress could emit equal or backward timestamps when source packets had
  repeated millisecond DTS/PTS. Runtime now guards RTMP video and audio
  timestamps independently, and unit tests cover repeated video DTS, repeated
  audio PTS, and A/V stream independence.
- MPEG-TS muxing could emit equal DTS when packet timestamps repeated at
  millisecond precision. The muxer now enforces strictly increasing 90 kHz DTS
  per elementary stream, with unit coverage for repeated timestamps and
  independent audio tracks.
- SRT selected-track egress could advertise ingest audio tracks that were not
  present in the routed output ring. The shared TS muxer now prefers routed
  `RingBuffer::audio_tracks()` metadata when available, and the regression test
  verifies the PMT contains only the selected audio track.
- ADTS audio payloads can contain multiple AAC frames inside one PES. Treating
  only the PES start timestamp as occupied allowed the next PES to collide with
  the final internal AAC frame after FFprobe split the frames. The muxer now
  reserves the full ADTS frame span before accepting the next DTS. Unit coverage
  includes deterministic multi-frame AAC and a property test for ADTS frame
  counting.
- RTMP egress wrapped Raw Annex B H.264 as FLV/AVCC with composition time `0`.
  B-frame fixtures therefore lost their `PTS-DTS` offset on RTMP output and the
  mixed-file multi live row exposed downstream duplicate/non-monotonic DTS
  warnings. The Raw H.264 RTMP wrapper now preserves signed 24-bit FLV
  composition time, with unit coverage for positive and negative offsets.

### Validator Lessons

- MediaMTX remains valuable as an interoperability sink, but it is not the only
  correctness oracle. Direct `ffprobe`/`ffmpeg` sinks are required when debugging
  muxer-level timestamp failures.
- MediaMTX SRT readback reproduced non-monotonic DTS with Restream bypassed in
  a direct FFmpeg-to-MediaMTX control. That specific path is therefore treated
  as a compatibility/readback signal, not strict proof of Restream muxer output.
- FFmpeg decode-to-`null` can introduce muxer-layer DTS warnings after decode,
  especially with multi-audio PCM output. The direct SRT sink now uses
  `ffprobe` compact packet output for stream shape and packet timestamp checks,
  avoiding false positives from a newly-created output muxer.
- FFprobe packet dumps may print elementary streams in demuxer flush order, not
  raw physical TS packet order. The harness validates duplicate DTS and large
  per-stream gaps after sorting each stream's timestamps instead of requiring
  the printed order to be monotonic.

### Required Controls

- Probe the checked-in fixture before blaming Restream:
  `ffprobe -v warning -show_entries program=:stream=index,codec_type,width,height:packet=stream_index,dts_time,pts_time -of compact=p=1:nk=0 test/fixtures/bench-h265-1_5m-2a.ts`.
- For sink disputes, run FFmpeg/FFprobe directly against the sink path with
  Restream bypassed. If the control reproduces the warning, keep MediaMTX in
  the matrix for interoperability but use a direct FFmpeg-family sink for muxer
  correctness.
- The direct SRT correctness mode is `SRT_SINK=ffmpeg` on
  `mixed-h265-srt-multi`; it validates stream dimensions, selected audio-track
  count, duplicate DTS, large DTS gaps, and FFmpeg-family probe warnings.
