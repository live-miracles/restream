# CLAUDE.md

Instructions for AI coding agents in this repository.

## Principles

- Keep changes small, intentional, and consistent with existing Rust/TypeScript patterns.
- Read relevant code and docs before editing, especially for media-pipeline behavior.
- Preserve unrelated user or agent changes. Check `git status` before broad edits, staging, or commits.
- Assume parallel agents may be editing the same files. If `git status`, diffs, or file contents show
  overlapping work, use hunk-based file edits and hunk-based git operations; do not overwrite,
  reformat, stage, or revert whole files unless that exact whole-file operation was requested.
- Add or update tests for behavior changes. Benchmark before and after hot-path changes.
- Update docs when changing commands, configuration, architecture, protocols, or user-visible behavior.
- Prefer targeted fixes over rewrites. Add abstractions only when they remove real complexity.

## Repository Map

- Rust backend and control plane: `src/`
- Media engine and protocols: `src/media/`
- Frontend source: `public/ts/`
- Generated frontend output: `public/js/`
- Tests and validation: `test/`
- Benchmarks: `benches/`
- Docs: `docs/`
- Archived legacy implementation: `old/` (not production runtime)

## Standard Commands

Use the pinned Rust toolchain from `rust-toolchain.toml`.

```sh
cargo build
scripts/resource-limit cargo test
scripts/resource-limit cargo clippy
cargo fmt
```

### Resource-Aware Builds

When multiple agents may be building concurrently, use the resource-aware wrapper.
It acquires the shared repository build lock, then sizes build jobs to available
RAM and CPU budget:

```sh
scripts/resource-limit cargo build
scripts/resource-limit cargo build --profile bench
scripts/resource-limit cargo test
scripts/resource-limit cargo clippy
```

Release builds (`--release`) are only for CI. For local development and agent
work, use `--profile bench` which shares the same opt-level but supports
incremental compilation and keeps debug symbols for flamegraphs. The
integration test script uses `target/release/` (where `--profile bench`
outputs).

Do not run `cargo build --release` from agents; use `--profile bench` instead.

For other Cargo build commands, prefer the wrapper too:

```sh
scripts/resource-limit cargo bench --bench <name>
```

For non-Cargo heavy commands, use the same wrapper:

```sh
scripts/resource-limit ./test/run-integration.sh mixed-scale
scripts/resource-limit ./scripts/setup-static-build.sh
scripts/resource-limit ./scripts/build-static.sh
```

The lock uses `flock(1)` on `.build-lock`; it is released automatically if the
process exits or crashes. The wrapper exports `BUILD_JOBS`, `CARGO_BUILD_JOBS`,
`CMAKE_BUILD_PARALLEL_LEVEL`, and `MAKEFLAGS`; tune with
`RESTREAM_MB_PER_JOB`, `RESTREAM_CPU_RESERVE`, `RESTREAM_MIN_JOBS`, and
`RESTREAM_MAX_JOBS`.

Frontend work:

```sh
npx tsc -p tsconfig.json
npx tailwindcss -i public/input.css -o public/output.css
npx playwright test
```

Edit `public/ts/` and `public/input.css`. Do not hand-edit generated files in `public/js/`.

Benchmarks and integration tests:

```sh
scripts/resource-limit cargo bench --bench <name>
scripts/resource-limit ./test/run-integration.sh mixed-scale
scripts/resource-limit ./test/run-integration.sh ramp
scripts/resource-limit ./test/run-integration.sh bonding
```

Integration tests run in a private loopback namespace by default. Use `--host` only when host networking is required.

### Build Safety on Memory-Limited Systems (WSL2)

**Never run `cargo build`, `cargo test`, or `cargo clippy` while a live pipeline is running.**

The bench binary (`target/release/restream`) combined with its FFmpeg child processes commits a large virtual address space due to statically-linked FFmpeg libraries (~3.9 GB VSZ for restream alone). On an 8 GB WSL2 system without swap, adding a `clippy-driver`/`rustc` invocation (~690 MB RSS) on top of 4 running FFmpeg transcoders (~460 MB) can push `Committed_AS` past 8 GB, causing the WSL2 kernel to panic and restart.

Before any `cargo` command, kill the live setup:

```sh
pkill -x restream; pkill -x mediamtx; pkill -x ffmpeg
```

## Configuration Defaults

- HTTP: `3030` (`RESTREAM_HTTP_PORT`)
- RTMP: `1935` (`RESTREAM_RTMP_PORT`)
- SRT: `10080` (`RESTREAM_SRT_PORT`)
- SQLite DB: `data.db` (`RESTREAM_DB_PATH`)
- Media directory: `media/` (`RESTREAM_MEDIA_DIR`)

Frontend assets are embedded with `rust-embed`, with a disk-first fallback during development.

## Media Pipeline Rules

Before changing `src/media/`, read `docs/architecture.md`, `docs/media-pipeline.md`,
`docs/high-performance-data-path.md`, and `docs/testing.md`.

Core invariants:

- Tokio tasks own sockets, API handlers, timers, and inline mux/demux work.
- Blocking FFmpeg calls and blocking `srt_send()` belong on dedicated OS threads.
- Wrap FFmpeg/libsrt OS-thread entry points with `catch_unwind(AssertUnwindSafe(...))`.
- Write defensive, resilient engine code: no internal or external failure path may crash the engine; isolate faults and surface errors instead.
- Keep media timestamps separate from wall-clock/application time.
- Respect `MediaPacket.format`: consumers must handle `Flv` and `Raw` explicitly.
- RTMP video timestamps are DTS; signed FLV composition offset derives PTS.
- SRT Stream IDs must be normalized before lookup.
- Duplicate SRT publishers are not bonded ingest. Only libsrt group connections are bonds.
- HLS storage is in-memory; do not introduce segment disk I/O without an explicit design change.

## Hot-Path Rules

Hot paths include `src/media/`, ring buffers, mux/demux loops, AVIO queues,
SRT/RTMP packet loops, HLS segmenting, and transcoder data paths.

- Benchmark before and after hot-path changes with the relevant `benches/` suite.
- Avoid per-packet allocation, logging, serialization, locks, async channel sends, and system calls.
- Use burst APIs such as `push_batch`, `pull_burst`, and `write_batch` where available.
- Hoist reusable buffers outside loops and call `.clear()` inside the loop.
- Prefer `Bytes`/`BytesMut` ownership transfer and ref-counting over payload copies.
- Do not add diagnostic readers or metrics that alter production pipeline behavior.
- Keep protocol correctness tests at least as strong as performance validation.

SIMD/vectorization:

- Benchmark the scalar path first.
- Add SIMD only for a measured bottleneck.
- Keep a pure scalar fallback and test it as the oracle.
- Use runtime feature detection once, cache the chosen implementation, and gate target features locally.
- Keep `unsafe` blocks minimal and document safety invariants.

## Testing Expectations

- Scope verification to the changed behavior first for a fast loop: run filtered
  unit/integration tests and filtered Criterion benchmarks that exercise the
  touched code path before using full suites as a broader confidence pass.
- Treat unrelated full-suite failures as separate findings. Do not let them hide
  whether the scoped tests/benchmarks for the current change passed or failed.
- Broaden from scoped tests to package, integration, or full benchmark suites
  only when the change crosses module boundaries, alters shared contracts, or
  has enough risk to justify the slower loop.
- When a benchmark or integration suite has grown into a blocker, split the
  work into composable named slices by behavior, protocol, codec, topology, load
  shape, and evidence type; report each slice independently.
- Use `cargo test av_sync` for timestamp, DTS/PTS, and cross-stream sync changes.
- Use protocol-matched probes: RTMP changes with RTMP probes, SRT changes with SRT probes.
- For UI changes, run TypeScript compile and the relevant Playwright tests.
- For integration or scale behavior, prefer `scripts/resource-limit ./test/run-integration.sh mixed-scale`; use `ramp` for memory growth and `bonding` for SRT bonding.

## Key References

- Overview/setup: `README.md`
- Status/limits: `REWRITE-STATUS.md`
- Architecture: `docs/architecture.md`
- Media pipeline: `docs/media-pipeline.md`
- Performance: `docs/high-performance-data-path.md`
- Testing: `docs/testing.md`
- Configuration: `docs/configuration.md`
- Observability: `docs/observability.md`
- API: `docs/api-reference.md`
