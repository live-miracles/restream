# CLAUDE.md

Instructions for AI coding agents in this repository.

## Principles

- Keep changes small, intentional, and consistent with existing Rust/TypeScript patterns.
- Read relevant code and docs before editing, especially for media-pipeline behavior.
- Preserve unrelated user or agent changes. Check `git status` before broad edits, staging, or commits.
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
cargo test
cargo clippy
cargo fmt
```

Frontend work:

```sh
npx tsc -p tsconfig.json
npx tailwindcss -i public/input.css -o public/output.css
npx playwright test
```

Edit `public/ts/` and `public/input.css`. Do not hand-edit generated files in `public/js/`.

Benchmarks and integration tests:

```sh
cargo bench --bench <name>
cargo bench --bench <name> --profile bench-dev
./test/run-integration.sh mixed-scale
./test/run-integration.sh ramp
./test/run-integration.sh bonding
```

Integration tests run in a private loopback namespace by default. Use `--host` only when host networking is required.

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

- Run the narrowest meaningful test first, then broaden based on risk.
- Use `cargo test av_sync` for timestamp, DTS/PTS, and cross-stream sync changes.
- Use protocol-matched probes: RTMP changes with RTMP probes, SRT changes with SRT probes.
- For UI changes, run TypeScript compile and the relevant Playwright tests.
- For integration or scale behavior, prefer `./test/run-integration.sh mixed-scale`; use `ramp` for memory growth and `bonding` for SRT bonding.

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
