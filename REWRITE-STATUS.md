# Rust Backend Rewrite — Status

Branch: `feat/rust-backend-rewrite-v2`
Code snapshot reviewed: June 25, 2026 (current working-tree changes)

## Executive Status

The production media/control path has moved from Node.js + MediaMTX + one
FFmpeg child per output to a Rust application with native RTMP/SRT transport,
in-process FFmpeg library stages, SQLite state, and an embedded dashboard.

The rewrite is structurally substantial and the Rust test suite is green, but
it should not yet be described as feature-complete or production-certified.
Protocol correctness, the full live matrix, vertical transform semantics, and
several high-rate/bonded combinations remain open gates. HLS upload and
channel-level audio remap/downmix are implemented for the default runtime path;
custom output encoding remains explicitly unavailable rather than advertised as
a working runtime path.

## Evidence

`cargo test` on June 25, 2026:

| Suite | Result |
|---|---|
| Library/unit | 350 passed |
| API integration | 38 passed |
| AV sync integration | 14 passed |
| Codec integration | 17 passed |
| Database integration | 15 passed |
| Transcoder integration | 7 passed |
| Total | **441 passed, 0 failed** |

The doctest suite also runs; the single codec example is intentionally ignored.

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
- external FFmpeg HLS PUT upload segment/playlist delivery
- FFmpeg-backed channel remap/downmix audio stages
- internal decode/scale/encode coverage for built-in video presets
- ring buffer push/pull ordering, overflow fast-forward to keyframe,
  multi-reader isolation, fill/capacity reporting
- DTS monotonicity enforcement (equal, decreasing, PTS < DTS correction,
  per-stream independence, B-frame composition-time preservation)
- engine lifecycle: ingest/egress register/unregister/cancel, idempotent
  unregister, pipeline create/remove, egress byte counters, health snapshot
  pipeline filtering, recording lifecycle, noop on nonexistent pipelines

The API suite covers authentication, configuration, pipeline/output CRUD,
ingests, HLS aliases, status, graph, diagnostics preconditions, custom
encoding persistence/rejection for runtime outputs, HLS upload output
acceptance, RTMPS output acceptance, egress-pipeline association in `/health`,
and deletion-cancellation of egress tasks.

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
  -> MPEG-TS recording
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
- typed stage identity and canonical stage-key planning

### Native media path

- RTMP ingest, play, and egress through `rml_rtmp`
- SRT ingest, read, and egress code paths through libsrt
- MPEG-TS demux/remux for SRT
- per-pipeline lock-free packet fan-out
- in-memory HLS store and HTTP pull routes
- MPEG-TS recording code path
- shared processing-stage identities and audio-stage cache
- shared TS packet feeder for recording, HLS, and in-process transcoder input
- centralized stage backend-selection policy
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
| SRT ingest to RTMP egress | Packetization implemented; live matrix pending | RTMP egress converts Raw Annex-B/ADTS packets with `video_for_rtmp_into`/`audio_for_rtmp_into`, sends AVCC/AAC sequence headers, and has codec conversion coverage |
| SRT H.265 passthrough | **Implemented and live-verified** | `cargo run --bin test_harness -- correctness-hevc-srt` publishes H.265 over SRT, loops it through native SRT egress, and probes HEVC video plus AAC audio |
| RTMP output from H.265 source | **Implemented** | `hevc_to_h264` stage (`h264_transcoder.rs`) does full libavcodec H.265 decode → H.264 encode; audio passthrough. Verified by `h265-srt` / `h265-srt-multi` scale tests and `cargo run --bin test_harness -- correctness-hevc-rtmp` |
| Built-in transforms via external transcoder (default) | **Implemented** | Subprocess `ffmpeg -vf scale=WxH -c:v libx264`; working and tested for default runtime presets |
| Built-in transforms via internal transcoder (`RESTREAM_USE_INTERNAL_TRANSCODER=1`) | Implemented for `h264`, `720p`, and `1080p` | `run_ffmpeg_transcode_with_scale` performs decode/scale/encode; transcoder integration tests exercise every built-in profile |
| Vertical crop/rotate | **Not implemented** | Only output dimensions are selected; no scale/crop/rotate filter runs |
| Multi-track SRT audio ingest | Implemented | Demux maps all audio streams with track indices |
| `atrack` | Implemented at stream-selection level | Parser tests |
| `remap` / `downmix` | Implemented for default runtime | Audio DSP routes use the external FFmpeg stage with `pan`/stereo resample filters; `atrack` remains a packet-only selector |
| HLS store and HTTP pull routes | Implemented | Playlist/window, route, and segmenter shutdown cleanup tests |
| Live HLS media generation | Native TsMuxer, structurally sound | Inline MPEG-TS mux with shared segmenter per pipeline |
| MPEG-TS recording | **Implemented** | Writes raw MPEG-TS to `.ts` file via `MemoryQueue`; no FFmpeg dependency. Container upgrade (MP4/MKV via avformat) is a roadmap item |
| HLS HTTP upload | Implemented | HTTP/HTTPS output URLs run the shared HLS segmenter and PUT new segments plus playlist to the target |
| Custom encoding arguments | **Not applied** | `/encodings/custom` still persists future args; output create/update rejects `custom` so the UI/API no longer advertises it as active |
| RTMPS output | Implemented | URL parser accepts RTMPS; reconciler dispatches RTMP/RTMPS URLs to RTMP egress, which wraps the TCP stream in Rustls before the RTMP handshake |
| SRT bonded egress | Constructed, live failover unproven | URL/group code exists; bonded group does not receive the high-bitrate option helper |
| SRT bonded ingest | Implemented and locally validated | One listener accepts a group ID, reads it through one `srt_recvmsg2` path, exports `srt_group_data`, and rejects unrelated duplicate publishers. Separate-process tests pass for two-member broadcast and backup groups, including primary-member failure and standby delivery |
| File ingest | Implemented with child FFmpeg | Not fully in-process; running state is checked from the tracked child process |

CRUD/lifecycle: deleting an output now cancels its egress task before removing
the DB row. Deleting a pipeline cancels all its outputs and its ingest.
Deleting a file-ingest kills its child process. Naturally exited children are
reaped by the reconciler and by running-state checks.

## Health and Diagnostics Accuracy

`/health` is native and on-demand. It no longer polls MediaMTX or ffprobe.

Current semantics:

- input is `on` when an active ingest is registered, otherwise `off`;
- outputs are keyed by `output_id` and filtered by `pipeline_id` on `ActiveEgress`;
- stopped configured outputs are absent and are merged by the frontend from
  `/config`;
- RTMP and SRT publisher metrics are connection-scoped;
- SRT listener queue/drop metrics are listener-wide;
- `bytesSent` is egress-derived and unexpected-reader fields on input remain
  placeholders;
- health, graph, and diagnostics expose per-reader source-ring lag, overflow,
  burst, and unread packet-age metrics.
- Engine Status and Active Outputs diagnostics filter by `pipeline_id` field.

The diagnostics design in `docs/observability.md` still treats packet lineage,
transcode lineage, and deeper residency histograms as future instrumentation
work.

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

1. ~~Replace the stale Node-based GitHub Actions workflow with Rust build/test
   and live native integration jobs~~ — done; `.github/workflows/ci.yml`
   contains Rust format, clippy, workspace tests, coverage, native integration,
   and HLS player E2E jobs. The repository now satisfies the workflow's
   `cargo fmt --all --check` gate.
2. ~~Fix active-egress pipeline association in `/health` and diagnostics~~ — done;
   `ActiveEgress` now stores `pipeline_id`, regression tests added.
3. ~~Use DTS—not PTS—as the RTMP message timestamp for video packets~~ — done;
   video uses DTS in both play and egress paths, and `bframe-rtmp` verifies
   live RTMP egress preserves B-frame composition offsets while DTS remains
   monotone.
4. ~~Make output/pipeline/ingest deletion stop runtime tasks~~ — done; egress
   cancellation before DB delete, pipeline delete cancels all outputs and
   ingest, file-ingest delete kills child process.
5. ~~Reap exited file-ingest children and report actual running state~~ — done;
   `MediaEngine::reap_file_ingests()` runs from the reconciler, and file-ingest
   list/detail responses call `is_file_ingest_running()`.
6. ~~Detect/log accepted SRT group IDs, expose `srt_group_data`, and reject a
   second independent publisher that only reuses the same StreamID~~ — done.
   Static release packaging now builds libsrt with `ENABLE_BONDING=ON`;
   separate-process broadcast and backup failover tests pass.
7. ~~Replace the transcoder byte-stream reconstruction~~ — done; output reader
   now demuxes MPEG-TS to recover timestamps and keyframes. HLS, recording,
   and in-process transcoder input now share the TS packet feeder.
8. Run and publish a clean protocol matrix from `docs/testing.md`. Minimum
   release evidence should include current `test/run-integration.sh` modes
   (`ramp`, `mixed-scale`, `bonding`, `burst-verify`, `hls-put`,
   `bframe-rtmp`), cross-protocol SRT→RTMP packaging, and a manifest under
   `test/artifacts/<run-id>/`. The `hls-put` mode covers HTTP PUT delivery and
   destination restart recovery with a dummy sink; `bframe-rtmp` covers live
   RTMP B-frame timestamp round-trip behavior; `correctness-hevc-rtmp` covers
   live H.265-to-H.264 RTMP edge conversion; `correctness-hevc-srt` covers
   live H.265 SRT passthrough.
9. ~~Implement the decode/filter/encode packet loop, then prove every built-in
   video preset~~ — done for `h264`, `720p`, and `1080p`; the opt-in internal
   path now has matrix coverage through `run_ffmpeg_transcode_with_scale`.
10. ~~Implement HLS HTTP PUT upload or remove HLS upload choices from the UI~~
   — done by implementing HTTP/HTTPS HLS PUT upload; local HLS remains
   available as `hls://`.
11. ~~Apply custom encoding configuration or mark it unavailable in the UI~~
   — done by removing `custom` from the output modal and rejecting custom
   output encodings in API create/update.
12. ~~Implement channel-level audio remap/downmix semantics~~ — done for the
   default runtime path; remap/downmix audio stages route through external
   FFmpeg filters and re-encode stereo AAC.
13. ~~Decide whether RTMPS is supported and wire TLS egress if required~~
   — done; RTMPS output wraps the client stream in Rustls before the RTMP
   handshake.

### Hardening work

- ~~clean up unused shared transcoder stages~~ — done; reconciler sweeps stale
  shared transcoder entries and regression tests cover video preset and
  codec-edge stages.
- ~~make graph/task “active” reflect worker health rather than token presence~~
  — done; health and graph active flags now treat cancelled recording/HLS
  tokens as inactive, with regression coverage.
- ~~remove stale HLS stores when their last segmenter stops~~ — done; segmenter
  shutdown removes the consumer token and in-memory store, with regression
  coverage.
- ~~add per-reader ring lag, overflow, and packet-age metrics~~ — done; source
  ring reader snapshots are exposed through health, graph, and diagnostics.
- ~~preserve trustworthy packet metadata across transcoder output~~ — done;
- ~~add bounded queue-depth/backpressure telemetry for `MemoryQueue`~~ — done;
  `MemoryQueue::stats()` reports current depth, capacity, high-water bytes,
  blocked write count, blocked write time, and closed state.
- ~~secure public HLS playlist and segment routes~~ — done; HLS playlist and
  segment routes require the dashboard session cookie.
- ~~replace or hide stale Grafana and status-page UI tied to MediaMTX~~ — done;
- ~~make listener ports, database path, media path, and operational tuning
  configurable~~ — done; environment overrides cover ports, SQLite path, media
  directory, fd limit, reconciler cadence, retry backoff, HLS idle timeout, and
  HLS segment/window sizing.
- full engine-native graph registries remain pending; graph rendering now uses
  typed stage helpers, but runtime ownership is still mostly in `MediaEngine`.

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
