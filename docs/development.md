# Developer Guide

This guide is the longer companion to the top-level README. Use it when you
need setup details, the normal edit/test loop, or release-build notes.

## Quick Start

For a fresh Debian/Ubuntu machine:

```sh
./scripts/bootstrap-dev.sh
scripts/resource-limit ./scripts/build-native.sh
cargo run
```

`bootstrap-dev.sh` installs host packages, the pinned Rust toolchain, frontend
dependencies, a pinned `mediamtx` binary for the live harness, and the
repo-managed native dependency prefix used by the build.

After `cargo run`, the service is available at `http://localhost:3030`.

Default runtime ports:

- `3030`: dashboard and HTTP API
- `1935`: RTMP ingest/play
- `10080`: SRT ingest/read

First-run dashboard password: `admin`

## Running The Binary Directly

There are two different stories here:

- Building from source: requires the host toolchain and native dependencies
- Running a static release binary: does not require those runtime dependencies

If you already have a binary built with:

```sh
scripts/resource-limit ./scripts/build-static.sh
```

you can run that artifact directly with:

```sh
./restream
```

`build-static.sh` verifies that the produced binary is statically linked, so
the release artifact does not depend on the host having FFmpeg, libsrt, or
other shared runtime libraries installed.

## Manual Prerequisites

If you are not using `bootstrap-dev.sh`, you will need:

- Rust toolchain pinned in `rust-toolchain.toml`
- FFmpeg development packages available through `pkg-config`
- `clang`, `nasm`, `mold`, `cmake`, `pkg-config`, `perl`
- `ffmpeg` / `ffprobe`, `curl`, `bzip2`, `jq`, `mediamtx`
- Node.js `>= 20` plus `npm` for frontend work

On Debian/Ubuntu, the bootstrap script installs:

```sh
apt-get install -y build-essential bzip2 ca-certificates clang cmake curl ffmpeg \
  git jq libavcodec-dev libavdevice-dev libavfilter-dev libavformat-dev \
  libavutil-dev libswresample-dev libswscale-dev mold nasm \
  ninja-build perl pkg-config
```

Then install a current Node.js toolchain for Tailwind/TypeScript work
(the bootstrap script uses NodeSource `22.x` by default because Tailwind 4's
native tooling requires Node `>= 20`).

Before the first Rust build, make sure the repo-managed native prefix exists:

```sh
scripts/resource-limit ./scripts/setup-static-build.sh
```

That native setup builds SRT against a repo-managed Mbed TLS instead of the
host's OpenSSL. [scripts/mbedtls-config-srt.h](../scripts/mbedtls-config-srt.h)
is intentionally a whole-build replacement config, not a small override: it
keeps only the AES-CTR, PBKDF2-HMAC-SHA1, entropy/CTR-DRBG, and version-report
pieces that SRT's CRYSPR backend actually calls. The goal is a smaller static
artifact, a tighter SBOM, and less unused crypto surface in the shipped binary.

## Inner Loop

The usual backend loop is:

```sh
scripts/resource-limit ./scripts/build-native.sh
scripts/resource-limit cargo test
scripts/resource-limit cargo clippy
cargo fmt --all
```

`build-native.sh` verifies that the debug build is using the expected native
linkage, including the repo-managed static `libsrt`.

### Frontend

Only needed when editing `public/ts/` or `public/input.css`:

```sh
npm run build:frontend
npm run test:frontend
```

Edit `public/ts/`, not generated files in `public/js/`. The build now re-syncs
the browser HLS runtime from the `hls.js` npm dependency automatically.
Frontend orchestration entrypoints live in `public/ts/app/`, shared transport
and state helpers in `public/ts/core/`, bounded UI modules in
`public/ts/features/`, and history-specific UI in `public/ts/history/`.
The Node-based frontend suite now uses a temporary sourcemapped test build so
coverage reports point at `public/ts/**`, while `npm run test:frontend:js-smoke`
keeps a smaller direct check against the shipped `public/js/**` bundle.
Use `npm run test:frontend:coverage` for the Node-scope TypeScript coverage
gate. That covered surface now includes the dashboard/history/status transport
modules that own the polling-vs-SSE split, plus the small reactive helpers for
output control intent and Rust-process lifecycle indication. Use
`npm run test:frontend:coverage:all` when you want the broader all-files
report as a diagnostic view.

The dashboard runtime surface now prefers a single `/api/v1/dashboard/runtime`
snapshot whenever a refresh needs both engine health and host metrics; only
metrics-only modes still hit `/metrics/system` directly. In selected-pipeline
detail modes, summary health requests now include the selected `pipeline_id` so
the backend can keep summary liveness for every pipeline while upgrading the
active pipeline entry to the full runtime shape in the same response.
Output start/stop now reuse the mutation response to patch local desired state
immediately, then let the already-open lifecycle SSE drive the runtime re-sync
with a short `/api/v1/dashboard/runtime` fallback if no wakeup arrives. The
button busy state now stays pinned until the selected output actually reaches
the requested runtime state, so unrelated lifecycle wakeups do not clear
operator feedback early.
File-ingest start/stop now follow the same pattern when a lifecycle stream is
already open, while cold/no-stream file-ingest controls still fall back
directly to a runtime refresh. The file-ingest button now also shows its own
`Starting...` / `Stopping...` in-flight state immediately so operators do not
have to infer whether the backend accepted the click. Recording start/stop is
different: the mutation
response already contains the operator-facing `enabled` / `active` state, so
the dashboard patches local recording state immediately instead of forcing a
follow-up runtime fetch. Status mode now reuses its own restream log SSE
instead of opening a second lifecycle-only dashboard stream on top. Settings
and media modes also use their existing metrics refresh to mark the Rust
process indicator as running immediately, rather than waiting for a later
lifecycle event to clear the initial "Connecting" state. Output create/update
flows, output deletes, pipeline create/update flows, and pipeline deletes now
reuse returned mutation payloads or apply targeted local removals to patch
dashboard state immediately instead of following each mutation with another
`/api/v1/settings?view=dashboard` fetch.

## Testing

For the broader testing story, use [Testing](testing.md). The short version:

```sh
scripts/resource-limit cargo test
scripts/resource-limit target/bench/test_harness mixed-h264-srt-single
```

Prefer scoped tests first, then broaden when the change crosses module or
protocol boundaries.

## Benchmarks

Run benchmarks before and after hot-path work:

```sh
scripts/resource-limit cargo bench --bench <name>
scripts/resource-limit cargo bench
```

Available suites include:

- `ring_buffer`
- `avio_throughput`
- `high_performance_data_path`
- `hls_cost`
- `matrix_throughput`
- `srt_ingest_latency`
- `transcoder_throughput`
- `codec_conversions`
- `stage_metrics`
- `alert_tracker`
- `stage_feeder`
- `simd_alternatives`

For the SRT crypto migration specifically, compare plaintext vs encrypted local
socket cost with:

```sh
scripts/resource-limit cargo bench --bench srt_ingest_latency -- srt_(ingest|egress)
```

That bench fixes the transport shape at `8 x 1316-byte` live-mode packets per
timed iteration and compares `plain`, `aes128`, `aes192`, and `aes256` via
`SRTO_PBKEYLEN=16/24/32`. That keeps the MPEG-TS-over-SRT packet shape stable
and makes the benchmark answer the narrower question we actually care about:
whether stronger SRT encryption changes hot-path cost.

For the optimization roadmap behind those benches, see
[High-Performance Data Path](high-performance-data-path.md).

## Static Release Build

The release path builds pinned native dependencies into
`.build/static/prefix/`, then links the Rust binary against them:

```sh
scripts/resource-limit ./scripts/setup-static-build.sh
scripts/resource-limit ./scripts/build-static.sh
```

Use this path when you need the pinned FFmpeg/x264/x265/libsrt toolchain rather
than the faster debug-iteration path.

Helpful variants:

```sh
RESTREAM_REBUILD_NATIVE=1 scripts/resource-limit ./scripts/setup-static-build.sh
RESTREAM_BUILD_PROFILE=fast-release scripts/resource-limit ./scripts/build-static.sh
```

See [FFmpeg Version Configuration](ffmpeg-versions.md) for version-selection
details.

## Recommended Reading Order

For a new contributor, this sequence keeps the context load reasonable:

1. [README](../README.md)
2. [Architecture](architecture.md)
3. [Configuration](configuration.md)
4. [Testing](testing.md)
5. Area-specific docs only when your change needs them
