# Restream

Restream is a host-run live-stream routing service with a Rust control plane and
in-process media engine. A single application owns the dashboard, SQLite state,
RTMP/SRT ingest, RTMP/SRT egress, HLS preview, recording, diagnostics, and
transcoding stages.

The previous Node.js + MediaMTX implementation is archived under `old/`.
MediaMTX is not part of the production runtime; it is still useful as an
independent interoperability sink in end-to-end tests.

## Current Capabilities

- RTMP ingest and egress through `rml_rtmp`
- SRT ingest, read, and egress through libsrt with MPEG-TS demux/remux
- SRT listener/single-link tuning, UDP-buffer monitoring, bonded-ingest group
  telemetry, and an unproven backup-group egress code path
- Lock-free per-pipeline packet fan-out through `RingBuffer`
- Shared processing-stage scaffolding and stream-level audio selection
- In-memory HLS playlist/segment storage and pull routes
- Matroska recording scaffolding
- File ingest by spawning the system `ffmpeg` binary into the local RTMP ingest
- SQLite-backed pipelines, outputs, jobs, ingests, settings, and sessions
- Native health, probe, processing-graph, diagnostics, and host-metrics APIs

Important boundaries:

- HTTP/HTTPS output URLs currently create the local HLS segmenter. They do not
  upload playlists or segments to the destination URL.
- `remap` and `downmix` currently select streams; channel-level filtering is not
  implemented.
- The custom encoding value is stored by the API, but the reconciler currently
  treats `custom` as source passthrough.
- Standard RTMP does not carry H.265. The reconciler inserts an H.264 transcode
  stage for an H.265 source sent to RTMP, but that stage does not currently
  decode and re-encode packets.
- HLS generation uses the native inline `TsMuxer` and is structurally sound;
  recording feeds concatenated `MediaPacket.payload` bytes into FFmpeg format
  detection and needs contract repair before being called a working media output.
- A proper SRT bonding publisher is accepted as one libsrt group on the single
  SRT listener when libsrt was built with `ENABLE_BONDING=ON`. The runtime logs
  a warning if the linked library exposes only disabled bonding stubs. Two
  unrelated callers that merely reuse the same StreamID are rejected as
  duplicate publishers.
- RTMP play and egress use DTS for video message timestamps and PTS for audio.
  B-frame round-trip validation remains an end-to-end gate.
- Output and pipeline deletion cancel owned egress/ingest tasks; deleting a
  file-ingest kills its tracked child process.

See [Rewrite Status](REWRITE-STATUS.md) for the tested status and open gates.

## Runtime Shape

```text
RTMP/SRT publisher
        |
        v
native ingest -> RingBuffer -> RTMP/SRT output
                    |        -> HLS (native TsMuxer, in-memory segments)
                    |        -> MKV recording (contract repair required)
                    `--------> transform stage -> output RingBuffer

Axum API/dashboard -> SQLite
```

Most media work is in-process through linked FFmpeg libraries. File ingest is
the exception and requires the `ffmpeg` executable on `PATH`.

## Development

### Prerequisites

There are two build paths with different FFmpeg requirements:

**Debug / fast iteration** — links against system FFmpeg and libsrt:

| Dependency | Purpose |
|---|---|
| Rust toolchain | Pinned in `rust-toolchain.toml` (1.96.0, includes rustfmt + clippy) |
| FFmpeg dev libs | `libavcodec`, `libavformat`, `libavfilter`, `libswscale`, `libswresample`, `libavutil` (via pkg-config) |
| libsrt dev | SRT transport (via pkg-config) |
| nasm | Assembler for FFmpeg x86 codecs |
| clang | C compiler for FFmpeg/SRT bindings in `build.rs` |
| Node.js / npx | Frontend TypeScript compiler and Tailwind CSS (UI work only) |
| ffmpeg, curl, jq | Live integration test tools |

Debian/Ubuntu:

```sh
apt-get install -y ffmpeg jq pkg-config clang nasm libsrt-dev \
  libavformat-dev libavcodec-dev libavutil-dev libswresample-dev \
  libswscale-dev libavfilter-dev libavdevice-dev
```

Note: distribution FFmpeg and libsrt may not have x86 ASM or bonding enabled.
Performance and bonding behaviour will differ from the static release build.

**Static release** — builds its own FFmpeg, x264, and SRT from source (see
[Static Release Build](#static-release-build) below). This is the only path
that guarantees `--enable-x86asm`, pinned codec versions, and
`ENABLE_BONDING=ON`.

### Inner Loop

**Rust backend** — the primary edit cycle:

```sh
cargo build            # debug build
cargo run              # start on :3030 / :1935 / :10080
cargo test             # 132 tests (92 lib + 24 API + 12 DB + 4 transcoder)
cargo clippy           # lint
cargo fmt              # format
```

**Frontend** — only when editing `public/ts/` or `public/input.css`:

```sh
npx tsc -p tsconfig.frontend.json                          # TS → public/js/
npx tailwindcss -i public/input.css -o public/output.css   # rebuild CSS
```

Always edit `public/ts/` — never the generated `public/js/`. In dev mode
`rust-embed` serves assets from disk, so frontend rebuilds take effect without
restarting the backend.

### Benchmarks

Run before and after any hotpath change:

```sh
cargo bench --bench <name>    # one suite
cargo bench                   # all suites
```

| Suite | What it measures |
|---|---|
| `ring_buffer` | push/pull throughput, multi-reader fan-out |
| `avio_throughput` | MemoryQueue and AVIO bridge throughput |
| `hls_cost` | TsMuxer + segment + HlsStore push per profile |
| `matrix_throughput` | full pipeline matrix (mux → ring → egress) |
| `srt_ingest_latency` | SRT receive-to-ring latency |
| `transcoder_throughput` | FFmpeg demux → RingBuffer push |
| `simd_alternatives` | SIMD vs portable byte-search and memcpy |
| `high_performance_data_path` | end-to-end data path micro-benchmarks |

### Testing

Run the full Rust suite:

```sh
cargo test
```

As of June 22, 2026 this runs 132 passing tests:

- 92 library/unit tests
- 24 API integration tests
- 12 database integration tests
- 4 transcoder integration tests

**Live integration tests** require isolated network (ports 3030/1935/10080 must
be free):

```sh
# Lightweight: Linux network namespace
unshare --net --map-root-user bash -c '
  ip link set lo up
  ./target/release/restream &
  until curl -sf http://localhost:3030/healthz > /dev/null 2>&1; do sleep 1; done
  ./test/run-2x3.sh
'

# Alternative: Docker
docker run --rm -v "$(pwd)":/app -w /app rust:1-bookworm bash -c '
  apt-get update -qq && apt-get install -y -qq ffmpeg jq libavformat-dev libavcodec-dev libavutil-dev libswresample-dev libswscale-dev libavfilter-dev libavdevice-dev pkg-config clang > /dev/null 2>&1
  cargo build --release
  ./target/release/restream &
  until curl -sf http://localhost:3030/healthz > /dev/null 2>&1; do sleep 1; done
  ./test/run-2x3.sh
'
```

For the broader media/scale harness: `./test/run-media-validation.sh`

Detailed correctness and scale gates are in [Testing](docs/testing.md).

### Static Release Build

`setup-static-build.sh` builds the following from pinned sources into a local
prefix (`.build/static/prefix/`). These are **not** taken from the OS:

| Library | Pinned version | Why built from source |
|---|---|---|
| **FFmpeg** | n6.1.5 | `--enable-x86asm` — runtime-dispatched SSE/AVX codec paths in H.264/H.265 decode and encode |
| **x264** | b35605ac | Matches FFmpeg's `--enable-libx264`; static PIC build |
| **libsrt** | v1.5.5 | `ENABLE_BONDING=ON` — OS packages typically ship bonding as disabled stubs |

**FFmpeg components compiled in:**

| Component | Status | Used for |
|---|---|---|
| `mpegts` demuxer | Active | `CustomInput` AVIO probe — transcoder input |
| `matroska`, `mov` demuxers | Planned | In-process file ingest (`.mkv`/`.mp4`/`.mov` sources) |
| `mpegts`, `matroska` muxers | Active / Planned | `CustomOutput`; recording MKV (recording currently writes raw bytes — FFmpeg muxer not yet called) |
| `h264`, `hevc`, `aac`, `mp3`, `ac3`, `eac3` decoders | Active / Planned | Stream info via `avformat_find_stream_info`; decode loop (transcoder, not yet implemented) |
| `libx264`, `aac` encoders | Planned | Transcoder H.264 encode + audio re-encode for remap/downmix (not yet implemented) |
| `h264`, `hevc`, `aac`, `ac3` parsers | Active | Required by `avformat_find_stream_info` |
| `h264_mp4toannexb`, `hevc_mp4toannexb`, `aac_adtstoasc` BSFs | Active | Header conversion in `codec.rs` |
| `pipe` protocol | Active | AVIO callback bridge for `MemoryQueue` |
| `scale`, `crop`, `transpose` filters | Planned | Resolution change (720p/1080p/2160p); vertical/rotate |
| `format`, `aformat` filters | Planned | Pixel/sample format negotiation before encoder |
| `aresample`, `pan` filters | Planned | `AudioRouting::Downmix`, `AudioRouting::Remap` |
| `swscale`, `swresample` | Planned | Used by scale and aresample filters |
| `avfilter` | Planned | Filter graph for transcoder |

The following come from the **OS** and are not compiled by the script:

| Dependency | Why OS-provided |
|---|---|
| **OpenSSL** (`libssl`, `libcrypto`) | libsrt uses it for encryption; OS version is sufficient |
| **clang / cc, nasm, cmake, ninja, perl, python3** | Build toolchain only |

Build steps:

```sh
./scripts/setup-static-build.sh   # compile native deps (content-addressed, reuses cache)
./scripts/test-srt-bonding.sh     # verify broadcast + backup failover
./scripts/build-static.sh         # fat-LTO static binary → .build/static/cargo-target/release/restream
```

Force a full native rebuild: `RESTREAM_REBUILD_NATIVE=1 ./scripts/setup-static-build.sh`

For faster iteration, use thin LTO: `RESTREAM_BUILD_PROFILE=fast-release ./scripts/build-static.sh`
(output: `.build/static/cargo-target/fast-release/restream`)

The build script verifies the binary has no dynamic loader dependency, probes
the required H.264/H.265 and audio codec set, and asserts that FFmpeg's x86
assembly paths are active (2× faster transcoding vs no-asm build). Because
this build enables x264, redistribution must comply with GPL.

### Runtime

| Port | Protocol | Purpose |
|---|---|---|
| `3030` | TCP/HTTP | Dashboard and API |
| `1935` | TCP/RTMP | RTMP ingest and play |
| `10080` | UDP/SRT | SRT ingest and read |

Override via `RESTREAM_HTTP_PORT`, `RESTREAM_RTMP_PORT`, `RESTREAM_SRT_PORT`.

| Path | Purpose |
|---|---|
| `data.db` | SQLite database |
| `media/` | File-ingest sources and `.mkv` recordings |
| `public/js/` | Generated frontend JavaScript |
| `public/output.css` | Generated frontend CSS |

The first-run dashboard password is `admin`. Change it immediately through the
Settings page or `POST /api/auth/change-password`.

## Operational APIs

| Endpoint | Purpose | Authentication |
|---|---|---|
| `GET /healthz` | Process liveness | No |
| `GET /health` | Native pipeline and transport snapshot | No |
| `GET /metrics/system` | CPU, memory, disk, and host-network JSON | Session |
| `GET /api/status` | Build, toolchain, native-library, SBOM summary, and host information | Session |
| `GET /api/status/sbom` | CycloneDX runtime software bill of materials with versions and licenses | Session |
| `GET /pipelines/:id/probe` | Active ingest metadata | Session |
| `GET /pipelines/:id/graph` | Active processing DAG | Session |
| `GET /pipelines/:id/diagnostics` | SSE diagnostic run | Session |

HLS pull routes are currently unauthenticated. Treat them as trusted-network
surfaces until signed URLs or equivalent authorization are implemented.

## Documentation

- [Rewrite Status](REWRITE-STATUS.md): implementation status, evidence, and gaps
- [Architecture](docs/architecture.md): runtime shape, thread model, packet walks, key files
- [Configuration](docs/configuration.md): ports, SQLite settings, encoding strings, SRT socket policy
- [API Reference](docs/api-reference.md): route surface and request/response details
- [Media Pipeline](docs/media-pipeline.md): processing stages, protocol contracts, buffer sizing, correctness requirements
- [Observability](docs/observability.md): health mapping, diagnostics, publisher transport, residency design
- [Testing](docs/testing.md): test suite, live validation results, E2E test plan
- [High-Performance Data Path](docs/high-performance-data-path.md): optimization plan, benchmarks, progress log
