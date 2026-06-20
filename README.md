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
- HLS generation and recording feed concatenated `MediaPacket.payload` bytes
  into FFmpeg format detection. Because packet payloads are not a self-describing
  container stream, those paths need end-to-end repair/validation before being
  called working media outputs.
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
                    |        -> HLS scaffolding (contract repair required)
                    |        -> MKV scaffolding (contract repair required)
                    `--------> transform scaffolding -> output RingBuffer

Axum API/dashboard -> SQLite
```

Most media work is in-process through linked FFmpeg libraries. File ingest is
the exception and requires the `ffmpeg` executable on `PATH`.

## Build and Run

The pinned Rust toolchain is defined in `rust-toolchain.toml`. Native development
packages must provide pkg-config metadata for:

- `libavcodec`
- `libavformat`
- `libavfilter`
- `libswscale`
- `libswresample`
- `libavutil`
- `libsrt`

Bonded SRT ingest additionally requires a libsrt build configured with
`ENABLE_BONDING=ON`. Some distribution packages, including the library linked
on the current development host, ship the group API symbols as disabled stubs.
Restream detects this at listener startup instead of silently claiming support.

Build and start:

```sh
cargo build
cargo run
```

Listeners are currently fixed:

| Port | Protocol | Purpose |
|---|---|---|
| `3030` | TCP/HTTP | Dashboard and API |
| `1935` | TCP/RTMP | RTMP ingest and play |
| `10080` | UDP/SRT | SRT ingest and read |

Runtime files:

| Path | Purpose |
|---|---|
| `data.db` | SQLite database |
| `media/` | File-ingest sources and `.mkv` recordings |
| `public/js/` | Generated frontend JavaScript |
| `public/output.css` | Generated frontend CSS |

The first-run dashboard password is `admin`. Change it immediately through the
Settings page or `POST /api/auth/change-password`.

## Testing

Run the full Rust suite:

```sh
cargo test
```

As of June 20, 2026 this runs 72 passing tests:

- 37 library/unit tests
- 23 API integration tests
- 12 database integration tests

Run the 2-pipeline × 3-output live test against a running application:

```sh
./test/run-2x3.sh
```

Required tools: `ffmpeg`, `curl`, and `jq`. MediaMTX is not required by the
script itself; output targets in the selected manifest must be reachable.
Output-health association is covered by API regression tests.

For the broader media/scale harness:

```sh
./test/run-media-validation.sh
```

Detailed correctness and scale gates are in
[End-to-End Testing](docs/end-to-end-testing.md).

## Operational APIs

| Endpoint | Purpose | Authentication |
|---|---|---|
| `GET /healthz` | Process liveness | No |
| `GET /health` | Native pipeline and transport snapshot | No |
| `GET /metrics/system` | CPU, memory, disk, and host-network JSON | Session |
| `GET /api/status` | Build, linked FFmpeg, and host information | Session |
| `GET /pipelines/:id/probe` | Active ingest metadata | Session |
| `GET /pipelines/:id/graph` | Active processing DAG | Session |
| `GET /pipelines/:id/diagnostics` | SSE diagnostic run | Session |

HLS pull routes are currently unauthenticated. Treat them as trusted-network
surfaces until signed URLs or equivalent authorization are implemented.

## Documentation

- [Rewrite Status](REWRITE-STATUS.md): implementation status, evidence, and gaps
- [Architecture](docs/architecture.md): runtime and packet flow
- [Configuration](docs/configuration.md): fixed ports and SQLite-backed settings
- [API Reference](docs/api-reference.md): current Rust routes
- [Health Mapping](docs/health-mapping.md): `/health` field derivation
- [Diagnostics](docs/diagnostics.md): current diagnostics and residency design
- [Media Pipeline Stage Design](docs/media-pipeline-stage-design.md): processing
  stages, protocol contracts, and buffer sizing
- [Observability](docs/observability.md): available JSON observability surfaces
- [Protocol Correctness](docs/protocol-correctness-notes.md): codec and transport
  correctness requirements
- [Legacy MediaMTX Monitoring](docs/mediamtx-control-api-monitoring.md): retained
  migration context only
