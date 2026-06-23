# Rust Backend Rewrite — Status

Branch: `feat/rust-backend-rewrite`
Code snapshot reviewed: June 22, 2026 (current working-tree changes)

## Executive Status

The production media/control path has moved from Node.js + MediaMTX + one
FFmpeg child per output to a Rust application with native RTMP/SRT transport,
in-process FFmpeg library stages, SQLite state, and an embedded dashboard.

The rewrite is structurally substantial and the Rust test suite is green, but
it should not yet be described as feature-complete or production-certified.
Protocol correctness, deployment/CI replacement, HLS upload, custom encoding,
channel-level audio processing, and several high-rate/bonded combinations remain
open gates.

## Evidence

`cargo test` on June 22, 2026:

| Suite | Result |
|---|---|
| Library/unit | 92 passed |
| API integration | 24 passed |
| Database integration | 12 passed |
| Transcoder integration | 4 passed |
| Total | **132 passed, 0 failed** |

Release-build validation on June 21, 2026 also passed:

- conventional statically linked x86-64 ELF with no dynamic interpreter or
  `DT_NEEDED` entries;
- pinned SRT 1.5.5 built with bonding, FFmpeg 6.1.5, and x264;
- runtime-dispatched FFmpeg/x264 x86 assembly enabled, with the intended
  HEVC-decode/scale/H.264-encode chain measuring 2.19× faster than the matched
  FFmpeg-no-x86asm build;
- static codec probe for libx264, H.264/H.265, AAC, MP3, AC-3, and E-AC-3;
- separate-process SRT broadcast bonding and backup-link failover tests;
- five-second isolated-network startup smoke test with HTTP, RTMP, and the
  bonding-enabled SRT listener active.
- authenticated CycloneDX 1.5 runtime SBOM endpoint with target-filtered Cargo
  dependencies, linked native-library versions, provenance, and licenses.

The current unit coverage includes:

- RTMP FLV H.264/AAC parsing and signed composition time
- HLS playlist/window behavior
- SRT stream-ID normalization, URL/bond parsing, codec mapping, payload
  extraction, rate deltas, socket option IDs, and listener UDP-stat parsing
- Linux `TCP_INFO`/`SO_MEMINFO` conversion and live socket collection
- transcoder stage sharing and audio-routing parsing
- ring buffer push/pull ordering, overflow fast-forward to keyframe,
  multi-reader isolation, fill/capacity reporting
- DTS monotonicity enforcement (equal, decreasing, PTS < DTS correction,
  per-stream independence, B-frame composition-time preservation)
- engine lifecycle: ingest/egress register/unregister/cancel, idempotent
  unregister, pipeline create/remove, egress byte counters, health snapshot
  pipeline filtering, recording lifecycle, noop on nonexistent pipelines

The API suite covers authentication, configuration, pipeline/output CRUD,
ingests, HLS aliases, status, graph, diagnostics preconditions, custom
encoding persistence, egress-pipeline association in `/health`, and
deletion-cancellation of egress tasks.

The 2×3 live script exists and targets native RTMP/SRT ingest with six outputs.
The checked-in `test/artifacts/latest/` files are local evidence, not a
replacement for a clean reproducible CI run.

## Runtime Architecture

```text
Publisher
  -> native RTMP/SRT ingest
  -> per-pipeline RingBuffer
  -> native RTMP/SRT egress
  -> in-memory HLS scaffolding
  -> Matroska recording scaffolding
  -> shared transform-stage scaffolding

Axum dashboard/API -> SQLite
reconciler (1 second) -> desired output/recording state
```

MediaMTX has been removed from the production runtime. It remains useful as an
external test sink.

Most FFmpeg work uses linked libraries and in-memory AVIO. File ingest remains
an intentional exception: it spawns the system `ffmpeg` executable and publishes
to the local RTMP listener.

## Implemented

### Control plane

- Axum dashboard/API on port 3030
- SQLite pipelines, outputs, jobs, logs, ingests, metadata, and sessions
- Scrypt password hashing and persisted session cookies
- one-second desired-state reconciler
- pipeline/output CRUD and explicit start/stop intent
- file-ingest CRUD and start/stop
- media listing/deletion safety
- recording enable/disable
- build/runtime status endpoint

### Native media path

- RTMP ingest, play, and egress through `rml_rtmp`
- SRT ingest, read, and egress code paths through libsrt
- MPEG-TS demux/remux for SRT
- per-pipeline lock-free packet fan-out
- in-memory HLS store and HTTP pull routes
- Matroska recording code path
- shared processing-stage identities and audio-stage cache
- H.264/H.265 codec mapping in FFmpeg paths
- automatic insertion of an intended H.265-to-H.264 stage for standard RTMP
  output

### Transport hardening

- 8 MiB RTMP accepted-ingest socket buffers
- SRT latency, reorder, UDP buffer, internal buffer, flow-control, and bandwidth
  tuning
- SRT effective-option logging and Linux sysctl warnings
- listener-wide `/proc/net/udp` queue/drop monitoring
- RTMP receiver metrics directly from `TCP_INFO` and `SO_MEMINFO`
- SRT publisher metrics from `srt_bistats()` with current-rate deltas
- SRT backup-group egress construction when a `bond=` URL parameter is supplied

### Operator surfaces

- `GET /health`
- `GET /healthz`
- `GET /metrics/system`
- `GET /api/status`
- `GET /pipelines/:id/probe`
- `GET /pipelines/:id/graph`
- `GET /pipelines/:id/diagnostics`
- HLS pull at `/hls/:id/index.m3u8`

Diagnostics currently run nine checks, including publisher transport and the
shared SRT listener socket.

## Capability Matrix

The labels below distinguish implementation from proof.

| Capability | Status | Evidence / boundary |
|---|---|---|
| RTMP H.264/AAC ingest and same-shape RTMP egress | Basic interoperability observed; timestamp fix applied | Video uses DTS as RTMP timestamp (audio uses PTS); composition offset is carried in FLV payload |
| SRT H.264/AAC ingest/read/egress | Implemented, prior local validation | Unit tests plus prior live read/egress evidence |
| SRT ingest to RTMP egress | **Not protocol-correct** | RTMP egress forwards raw demuxed codec payload as though it were FLV media payload |
| SRT H.265 passthrough | Implemented, needs full E2E matrix | Codec mapping is tested; decode/probe combinations remain a release gate |
| RTMP output from H.265 source | **Implemented** | `hevc_to_h264` stage (`h264_transcoder.rs`) does full libavcodec H.265 decode → H.264 encode; audio passthrough. Verified by `h265-srt` and `h265-srt-multi` scale tests |
| 720p/1080p/2160p transforms via external transcoder (default) | **Implemented** | Subprocess `ffmpeg -vf scale=WxH -c:v libx264`; working and tested |
| 720p/1080p/2160p transforms via internal transcoder (`RESTREAM_USE_INTERNAL_TRANSCODER=1`) | **Not functionally complete** | `run_ffmpeg_transcoder_stage` demuxes MPEG-TS and copies compressed packets to the output ring without decode, scale filter, or encode |
| Vertical crop/rotate | **Not implemented** | Only output dimensions are selected; no scale/crop/rotate filter runs |
| Multi-track SRT audio ingest | Implemented | Demux maps all audio streams with track indices |
| `atrack` | Implemented at stream-selection level | Parser tests |
| `remap` / `downmix` | Partial | Select streams; no channel-level `pan`/mix filter |
| HLS store and HTTP pull routes | Implemented | Playlist/window and route tests |
| Live HLS media generation | Native TsMuxer, structurally sound | Inline MPEG-TS mux with shared segmenter per pipeline |
| Matroska recording | **Unproven / structurally suspect** | Same packet-payload-to-`CustomInput` contract as HLS |
| HLS HTTP upload | **Not implemented** | HTTP/HTTPS output URL starts local segmenter and ignores destination |
| Custom encoding arguments | **Not applied** | API persists value; reconciler treats `custom` as passthrough |
| RTMPS output | **Not wired** | URL parser accepts RTMPS, reconciler dispatches only `rtmp://` |
| SRT bonded egress | Constructed, live failover unproven | URL/group code exists; bonded group does not receive the high-bitrate option helper |
| SRT bonded ingest | Implemented and locally validated | One listener accepts a group ID, reads it through one `srt_recvmsg2` path, exports `srt_group_data`, and rejects unrelated duplicate publishers. Separate-process tests pass for two-member broadcast and backup groups, including primary-member failure and standby delivery |
| File ingest | Implemented with child FFmpeg | Not fully in-process; list endpoint reports `running:false` placeholder |

CRUD/lifecycle: deleting an output now cancels its egress task before removing
the DB row. Deleting a pipeline cancels all its outputs and its ingest.
Deleting a file-ingest kills its child process. Naturally exited children
remain tracked.

## Health and Diagnostics Accuracy

`/health` is native and on-demand. It no longer polls MediaMTX or ffprobe.

Current semantics:

- input is `on` when an active ingest is registered, otherwise `off`;
- outputs are keyed by `output_id` and filtered by `pipeline_id` on `ActiveEgress`;
- stopped configured outputs are absent and are merged by the frontend from
  `/config`;
- RTMP and SRT publisher metrics are connection-scoped;
- SRT listener queue/drop metrics are listener-wide;
- `readers`, `bytesSent`, and unexpected-reader fields on input are placeholders;
- ring diagnostics do not yet expose per-reader lag or true occupancy.
- Engine Status and Active Outputs diagnostics filter by `pipeline_id` field.

The diagnostics design in `docs/observability.md` correctly treats application
residency, reader lag, packet lineage, and transcode lineage as future
instrumentation work.

## API Migration Notes

The Rust router implements the dashboard's pipeline/output-oriented API, but it
is not a one-for-one copy of every old route.

Removed with MediaMTX:

- `/internal/mediamtx/auth`
- `/api/status/mediamtx-config`
- MediaMTX control/config proxy routes

Changed:

- `/api/status` now reports Restream, linked FFmpeg, and host information.
- `/stream-keys` is read-only and returns 20 built-in keys.
- HLS uses `/hls/...`; `/preview/hls/...` remains as a compatibility alias.
- `/health` is public native state rather than a cached MediaMTX merge.

See `docs/api-reference.md` for the executable route surface.

## Known Gaps and Risks

### Release blockers

1. Replace the stale Node-based GitHub Actions workflow with Rust build/test and
   live native integration jobs.
2. ~~Fix active-egress pipeline association in `/health` and diagnostics~~ — done;
   `ActiveEgress` now stores `pipeline_id`, regression tests added.
3. ~~Use DTS—not PTS—as the RTMP message timestamp for video packets~~ — done;
   video uses DTS in both play and egress paths. B-frame round-trip tests
   remain desirable.
4. ~~Make output/pipeline/ingest deletion stop runtime tasks~~ — done; egress
   cancellation before DB delete, pipeline delete cancels all outputs and
   ingest, file-ingest delete kills child process.
5. Reap exited file-ingest children and report actual running state.
6. ~~Detect/log accepted SRT group IDs, expose `srt_group_data`, and reject a
   second independent publisher that only reuses the same StreamID~~ — done.
   Static release packaging now builds libsrt with `ENABLE_BONDING=ON`;
   separate-process broadcast and backup failover tests pass.
7. ~~Replace the transcoder byte-stream reconstruction~~ — done; output reader
   now demuxes MPEG-TS to recover timestamps and keyframes. HLS and recording
   muxers still use the raw-byte approach.
8. Run the protocol matrix from `docs/testing.md`, including H.265,
   B-frame timestamps, cross-protocol packaging, and destination restart.
9. Implement the decode/filter/encode packet loop, then prove every advertised
   video preset.
10. Implement HLS HTTP PUT upload or remove HLS upload choices from the UI.
11. Apply custom encoding configuration or mark it unavailable in the UI.
12. Implement channel-level audio remap/downmix semantics.
13. Decide whether RTMPS is supported and wire TLS egress if required.

### Hardening work

- clean up unused shared transcoder stages;
- make graph/task “active” reflect worker health rather than token presence;
- remove stale HLS stores when their last segmenter stops;
- add per-reader ring lag, overflow, and packet-age metrics;
- ~~preserve trustworthy packet metadata across transcoder output~~ — done;
- add bounded queue-depth/backpressure telemetry for `MemoryQueue`;
- secure public HLS playlist and segment routes;
- ~~replace or hide stale Grafana and status-page UI tied to MediaMTX~~ — done;
- make listener ports, database path, media path, and operational tuning
  configurable.

### Claims intentionally not made

- A static glibc binary is not claimed to be universally portable across every
  Linux kernel, NSS setup, or architecture; the current artifact is an x86-64
  GNU/Linux release build.
- 4K60 is a sizing target, not a benchmarked throughput guarantee.
- SRT bonded egress is not production-proven; only bonded ingest broadcast and
  backup/failover modes have live loopback evidence.
- A MediaMTX sink accepting a stream is interoperability evidence, not platform
  certification.

## Current File-Level Snapshot

Approximate lines in the reviewed working tree:

| Area | Lines |
|---|---:|
| `src/api.rs` | 1,887 |
| `src/db.rs` | 776 |
| `src/diag.rs` | 987 |
| `src/lib.rs` | 500 |
| `src/media/engine.rs` | 1,382 |
| `src/media/mpegts.rs` | 2,065 |
| `src/media/rtmp.rs` | 1,496 |
| `src/media/srt.rs` | 2,290 |
| `src/media/codec.rs` | 544 |
| `src/media/ring_buffer.rs` | 568 |
| `src/media/transcoder.rs` | 403 |
| `tests/api.rs` | 965 |
| `tests/db.rs` | 396 |
| `tests/transcoder.rs` | 120 |

Line counts are descriptive only and should not be treated as a completion
metric.

## Bottom Line

The rewrite has crossed the architectural milestone: the production runtime no
longer depends on Node.js or MediaMTX, and the core Rust test suite passes.

The honest status is **native rewrite implemented, correctness and operational
productization in progress**.
