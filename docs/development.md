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
cargo fmt
```

`build-native.sh` verifies that the debug build is using the expected native
linkage, including the repo-managed static `libsrt`.

### Frontend

Only needed when editing `public/ts/` or `public/input.css`:

```sh
npm run build:frontend
npx tailwindcss -i public/input.css -o public/output.css
```

Edit `public/ts/`, not generated files in `public/js/`.

## Testing

For the broader testing story, use [Testing](testing.md). The short version:

```sh
scripts/resource-limit cargo test
scripts/resource-limit ./test/run-integration.sh mixed-scale
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
