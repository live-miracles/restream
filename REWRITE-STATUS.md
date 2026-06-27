# Rust Backend Rewrite — Status

Branch: `feat/rust-backend-rewrite-v2`
Code snapshot reviewed: June 26, 2026 (current working-tree changes)

## Executive Status

The production media/control path has moved from Node.js + MediaMTX + one
FFmpeg child per output to a Rust application with native RTMP/SRT transport,
in-process FFmpeg library stages, SQLite state, and an embedded dashboard.

The rewrite is structurally substantial and the Rust test suite is green, but
it should not yet be described as production-certified. The full live protocol
matrix and several high-rate/bonded combinations remain open gates. HLS upload,
channel-level audio remap/downmix, and every built-in video preset transform
are implemented for the default runtime path; custom output encoding remains
explicitly unavailable rather than advertised as a working runtime path.

## Evidence

`cargo test` on June 26, 2026:

| Suite | Result |
|---|---|
| Library/unit | 379 passed |
| API integration | 50 passed |
| AV sync integration | 14 passed |
| Codec integration | 17 passed |
| Database integration | 15 passed |
| Transcoder integration | 7 passed |
| Total | **482 passed, 0 failed** |

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
deletion-cancellation of egress tasks, engineer telemetry endpoints (engine,
pipeline, stage), and telemetry auth enforcement.

The 2×3 live script exists and targets native RTMP/SRT ingest with six outputs.
The checked-in `test/artifacts/latest/` files are local evidence, not a
replacement for a clean reproducible CI run.

A focused aggregate-matrix smoke on June 25, 2026 also passed:

- `./test/run-protocol-matrix.sh --run-id protocol-smoke-20260625T141347Z
  --fast --continue-on-fail --only-modes hls-put,bframe-rtmp`
- aggregate manifest: `test/artifacts/protocol-smoke-20260625T141347Z/manifest.json`
- `hls-put`: YouTube-style playlist and TS segment uploaded via HTTP PUT,
  probed as 1280×720, then recovered after dummy sink restart
- `bframe-rtmp`: 63/151 probed video packets had `PTS > DTS`, and DTS stayed
  monotone

A full aggregate preflight on June 25, 2026 also passed for `ramp`,
`mixed-scale`, `bonding`, `burst-verify`, `hls-put`, and `bframe-rtmp`:

- `./test/run-protocol-matrix.sh --run-id protocol-preflight-20260625T142046Z
  --preflight-only --continue-on-fail`
- aggregate manifest:
  `test/artifacts/protocol-preflight-20260625T142046Z/manifest.json`
- every selected mode reported clean dependency, namespace, and port preflight
  checks; `bonding` correctly skips the regular `restream` binary check because
  it builds dedicated static SRT helper binaries

The same all-mode aggregate preflight passed again on June 26, 2026 after the
mixed-scale Rust launcher and embedded `libx265` work:

- `./test/run-protocol-matrix.sh --run-id protocol-preflight-20260626-codex
  --preflight-only`
- aggregate manifest:
  `test/artifacts/protocol-preflight-20260626-codex/manifest.json`

Phase 7 artifact safety was tightened on June 26, 2026:

- `test/run-integration.sh` now emits an `artifact-disk` preflight record and
  fails before live services start if the artifact filesystem has less free
  space than `RESTREAM_ARTIFACT_MIN_FREE_MB` (default 2048 MB).
- live runs prune old ignored top-level `test/artifacts/` directories so only
  the latest three runs remain, while protecting the active run directory;
  `KEEP_ARTIFACTS=1` disables pruning for deliberate retention/debug sessions.
- host-network preflight is now mode-aware: legacy live modes check the
  Restream/MediaMTX port set, while Rust-only harness modes check the concrete
  harness loopback ports they bind (`11935`, `11080`, and `HLS_PUT_PORT` for
  the HLS PUT dummy sink).
- `test/run-protocol-matrix.sh --help` and `--list-modes` are now answered by
  the shell wrapper without compiling the Rust orchestrator; `--only-modes` is
  validated before `cargo run`, and real matrix runs go through
  `scripts/resource-limit cargo run --bin protocol_matrix` with
  `RESTREAM_PROTOCOL_MATRIX_ONLY=1` so matrix preflight can run before the
  media-native static prefix is built.
- A focused aggregate preflight for the three first-class correctness gates
  passed on June 26, 2026 with
  `./test/run-protocol-matrix.sh --run-id phase7-correctness-preflight
  --preflight-only --only-modes
  correctness-srt-rtmp,correctness-hevc-rtmp,correctness-hevc-srt`.
- A current all-nine aggregate preflight on June 26, 2026 with
  `./test/run-protocol-matrix.sh --run-id phase7-nine-preflight-20260626-codex
  --preflight-only --continue-on-fail` reached every mode. Seven Rust-only or
  helper-backed modes passed readiness; `ramp` and `mixed-scale` failed the
  binary preflight because `target/release/restream` and the repo-managed
  static SRT prefix were absent from this worktree. The binary preflight hint
  now points agents at `scripts/resource-limit ./scripts/setup-static-build.sh`
  before `scripts/resource-limit cargo build --release` when that prefix is
  missing.
- Static native setup/build was repaired on June 26, 2026:
  `scripts/resource-limit ./scripts/setup-static-build.sh` now completes after
  cleaning FFmpeg's source-tree configure state before the standalone embedded
  binary build, and the static/standalone FFmpeg configs include the AC-3
  encoder required by `restream-ffmpeg-capabilities`. A follow-up
  `scripts/resource-limit ./scripts/build-static.sh` produced
  `.build/static/cargo-target/release/restream`, passed the FFmpeg capability
  probe, and verified the binary is statically linked.
- The current all-nine aggregate preflight passed on June 26, 2026 with
  `./test/run-protocol-matrix.sh --run-id
  phase7-nine-static-preflight-20260626-codex --preflight-only
  --continue-on-fail --restream-bin
  .build/static/cargo-target/release/restream`; aggregate manifest:
  `test/artifacts/phase7-nine-static-preflight-20260626-codex/manifest.json`.

The aggregate protocol-matrix orchestration has moved from bash into the Rust
`protocol_matrix` binary, aligning with `restream_platform_master_plan.md`'s
direction that Rust should be the canonical integration harness. The thin
`test/run-protocol-matrix.sh` compatibility launcher now delegates to Rust. The
current default matrix has nine modes: `ramp`, `mixed-scale`, `bonding`,
`burst-verify`, `hls-put`, `bframe-rtmp`, `correctness-srt-rtmp`,
`correctness-hevc-rtmp`, and `correctness-hevc-srt`. The remaining shell
surface is the per-mode compatibility runner in `test/run-integration.sh`;
continuation work should keep shrinking it toward argument parsing, preflight,
manifest lifecycle, and summary rendering while adding new scenario logic in
Rust harness entry points.

`bframe-rtmp` is the first per-mode scenario moved behind that typed Rust
harness boundary: `test/run-integration.sh bframe-rtmp` now preserves the
manifest and summary wrapper while delegating the live RTMP B-frame scenario,
packet capture, and assertions to `cargo run --bin test_harness -- bframe-rtmp`.

`hls-put` has also moved behind that typed Rust harness boundary:
`test/run-integration.sh hls-put` now preserves the manifest, summary, sink
directory, request log, publisher/restream sidecars, and public mode name while
delegating the dummy HTTP PUT sink, SRT ingest publisher, HLS segmenter/upload
tasks, ffprobe checks, signed-query assertions, and restart recovery to
`cargo run --bin test_harness -- hls-put`. A focused wrapper run passed on
June 25, 2026 with `WORK_DIR=test/artifacts/hls-put-rust-wrapper`,
`HLS_PUT_SETTLE_SECS=4`, and `HLS_PUT_RESTART_SECS=8`; both YouTube-style
`file=` and path-style `/akamai/out.m3u8?token=dummy` uploads produced
1280x720 TS segments and recovered after sink restart.

`burst-verify` has now moved behind the typed Rust harness boundary as well:
`test/run-integration.sh burst-verify` preserves the manifest, summary, per-case
publisher logs, graph snapshots, `BURST_CONFIGS` filtering, and public mode name
while delegating the full ten-config RTMP/SRT, H.264/H.265, 1080p/4K, fps, and
audio-variant matrix to `cargo run --bin test_harness -- burst-verify`. A
focused wrapper run passed on June 25, 2026 with
`WORK_DIR=test/artifacts/burst-rust-wrapper-full` and `BURST_SETTLE_SECS=2`;
all ten configs reported one live reader with non-zero `burstCount` and
`avgBurstSize`. The Rust result records both requested and published audio
track counts because RTMP/FLV can carry only one audio stream while SRT retains
dual-audio coverage.

`ramp` has moved behind the typed Rust harness boundary as well. The public
`test/run-integration.sh ramp` mode now delegates all eight
ingest×egress×encoding configs to `cargo run --bin test_harness -- ramp-family`
by default, while the shell wrapper keeps the public CLI, namespace setup,
manifest lifecycle, summary table rendering, and `RAMP_RUST_FAMILY=0` /
`RAMP_FAMILY_CONFIGS` fallback hooks. The Rust subcommand is still black-box
coverage: it starts the production `restream` binary plus MediaMTX, logs in
through the HTTP API, creates pipelines/outputs, snapshots `scale.csv`, appends
`summary.txt`, and performs the first/last output spot checks. A focused fast
wrapper run passed on June 25, 2026 with
`WORK_DIR=test/artifacts/ramp-rust-all-fast`, `RAMP_CONFIG_CLEANUP_SECS=1`, and
`./test/run-integration.sh --fast ramp`; the artifacts contain all eight
summary rows and `ramp-family.json` records all eight Rust-owned configs. A
focused aggregate preflight also passed with
`./test/run-protocol-matrix.sh --run-id ramp-rust-all-preflight
--preflight-only --continue-on-fail --only-modes ramp`.

The historical RTMP/H.264 `mixed-scale` slice is restored through the Rust
harness entry point `mixed-h264-rtmp` and delegated by
`test/run-integration.sh mixed-scale` by default. It preserves the RTMP/FLV
publisher shape, the four output groups, `scale.csv`, `rss-summary.csv`,
`MS-load-h264-rtmp`, and optional non-fatal spot checks.

For live mixed-scale runs, the wrapper no longer sets `FFMPEG_BIN_PATH` for the
Rust-owned slices by default, so the production `restream` child exercises the
embedded standalone `public/bin/ffmpeg`. Developers can still set
`FFMPEG_BIN_PATH=/usr/bin/ffmpeg` explicitly for streaming-logic diagnosis
against the system binary.
The native build script now builds pinned static x265 and configures both the
linked FFmpeg libraries and embedded standalone binary with `--enable-libx265`;
`public/bin/ffmpeg -hide_banner -encoders` reports both `libx264` and
`libx265`. The wrapper also fails early when `RESTREAM_BIN` is older than
`public/bin/ffmpeg`, because the binary embeds those asset bytes at build time.

The first `mixed-scale` slice has now moved behind the typed Rust harness:
`cargo run --bin test_harness -- mixed-anchor` owns the `h264-srt` anchor config
by default from `test/run-integration.sh mixed-scale`. The Rust slice preserves
the existing artifact layout (`scale.csv`, `rss-summary.csv`, `summary.txt`,
assertion JSONL, sidecar logs, and `mixed-anchor.json`), creates the HLS preview
plus four output groups, emits `MS-smoke`, `MS-ffprobe-*`, `MS-hls-*`, and
`MS-lifecycle`, and uses the authenticated session cookie for restream's
protected HLS pull route. A direct focused run passed on June 25, 2026 with
`ONLY_CHECKS=smoke,hls,lifecycle`, `N_PER_GROUP=1`, and
`SNAPSHOT_SLEEP_SECS=0`; the wrapper-level `--only smoke,ffprobe,hls,lifecycle`
run also passed the Rust anchor.

The `h265-srt` mixed-scale slice has also been moved behind a Rust harness
entry point, `cargo run --bin test_harness -- mixed-h265-srt`, and is delegated
by `test/run-integration.sh mixed-scale` by default. The Rust path preserves the
same group order, `scale.csv`, `rss-summary.csv`, optional non-fatal spot
checks, `MS-load-h265-srt`, and `MS-tc-spawns` bound (`1..ext_ffmpeg_n + 1`) as
the original bash path. A direct focused run passed on June 26, 2026 with
`ONLY_CHECKS=tc-spawns`, `N_PER_GROUP=1`, and `SNAPSHOT_SLEEP_SECS=0`. A wrapper
fast path with `--only tc-spawns --skip-load` also passed the Rust
`mixed-h265-srt` slice (`MS-tc-spawns`: `tc_spawns=2`, bound `2`).

The two multi-audio `mixed-scale` slices are now wired through Rust harness
entry points, `mixed-h264-srt-multi` and `mixed-h265-srt-multi`, and are
delegated by `test/run-integration.sh mixed-scale` by default. They preserve the
two-audio SRT publisher (`48000` Hz stereo plus `44100` Hz mono), the four
output groups, `scale.csv`, `rss-summary.csv`, optional non-fatal spot checks,
and the route-specific audio encodings (`720p+atrack:0` for RTMP 720p and
`720p+atrack:0,1` for SRT 720p). The Rust `ffprobe` path now emits fatal JSONL
assertions for those route-specific audio counts. Direct focused runs passed on
June 26, 2026 with `ONLY_CHECKS=load`, `N_PER_GROUP=1`, `SNAPSHOT_SLEEP_SECS=0`,
and custom non-default ports before the stronger audio-count assertion was added:
`mixed-h264-srt-multi` emitted `MS-load-h264-srt-multi` pass with two audio
tracks, and `mixed-h265-srt-multi` emitted `MS-load-h265-srt-multi` pass with
one external FFmpeg feed. A wrapper fast path also passed with
`./test/run-integration.sh --fast --skip-load --only load mixed-scale`,
delegating the then-current four configs to Rust and printing
`[rust] all mixed-scale configs delegated; skipping legacy bash runner`.
The shell fallback body has now been removed; `mixed-scale` is a Rust launcher
plus summary renderer. The wrapper writes a `mixed-scale-logs.json` index for
the five per-slice harness/restream logs, and all selected `ffprobe` checks now
use fatal JSONL-emitting assertions with `--resume-from` support instead of
print-only probes.

A namespaced default-port wrapper run with
`WORK_DIR=test/artifacts/mixed-scale-embedded-x265-ffprobe
./test/run-integration.sh --fast --only ffprobe mixed-scale` passed all five
slices on June 26, 2026 after rebuilding `public/bin/ffmpeg` and
`target/release/restream`; it did not set `FFMPEG_BIN_PATH`, so the live H.265
720p paths exercised the embedded standalone FFmpeg with `libx265`.

A manual dashboard live environment was refreshed on June 27, 2026 from commit
`3ea12f9` after rebuilding the embedded frontend and `target/debug/restream`.
It used a fresh SQLite DB under `/tmp/restream-live-current`, Restream on
HTTP/RTMP/SRT ports `39280`/`32080`/`31280`, and MediaMTX only as an external
sink on RTMP/SRT/HLS ports `33080`/`34080`/`35080`. One combined FFmpeg process
published three looping inputs: RTMP H.264 1080p50 at about 8 Mbps, SRT H.264
4K60 at about 21 Mbps with two AAC tracks, and SRT HEVC 4K60 at about 22 Mbps
with two AAC tracks. Six configured outputs were live and sending bytes:
RTMP+SRT sinks for the 1080p50 H.264 input, RTMP+SRT sinks for the 4K60 H.264
input, and SRT HEVC passthrough plus RTMP H.264 compatibility output for the
4K60 HEVC input. ffprobe readback confirmed the SRT sinks at the expected
codecs/resolutions/frame rates and the RTMP sinks as H.264. The RTMP output
from the HEVC input probed as H.264 around 30 fps, which is useful live
evidence for the compatibility path but is not a claim that 4K60 HEVC-to-RTMP
conversion is production-certified.

Earlier focused HLS PUT integration evidence from June 25, 2026:

- `WORK_DIR=test/artifacts/hls-put-dual-20260625T142444Z
  ./test/run-integration.sh --fast --json
  test/artifacts/hls-put-dual-20260625T142444Z/assertions.jsonl hls-put`
- manifest: `test/artifacts/hls-put-dual-20260625T142444Z/manifest.json`
- YouTube-style `file=` upload and path-style `/akamai/out.m3u8?token=dummy`
  upload both produced playlists plus 1280×720 TS segments, preserved expected
  content types, and recovered with fresh segment PUTs after dummy sink restart

HLS PUT was rechecked in the default private loopback namespace on June 26,
2026 with `WORK_DIR=test/artifacts/hls-put-default-20260626
./test/run-integration.sh --fast hls-put`; YouTube-style `file=` and
path-style signed-query uploads both delivered playlist plus segment PUTs,
probed as 1280×720, and recovered after dummy sink restart.

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
- engineer telemetry endpoints (engine, pipeline, stage)
- persistent alert tracking with firstSeen/lastSeen timestamps

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
- `GET /pipelines/:id/alerts` — typed alert list for one pipeline
- `GET /api/v1/alerts` — aggregate alerts across all pipelines
- `GET /api/v1/overview` — engine-wide operator summary
- `GET /api/v1/pipelines/:id/summary` — operator pipeline detail
- `GET /api/v1/events[?pipeline_id=&limit=]` — recent lifecycle events
- `GET /api/v1/engine/telemetry` — engineer engine-wide telemetry
- `GET /api/v1/pipelines/:id/telemetry` — engineer pipeline-scoped telemetry
- `GET /api/v1/stages/:key/telemetry` — engineer single-stage telemetry
- HLS pull at `/hls/:id/index.m3u8`

Diagnostics currently run nine checks, including publisher transport and the
shared SRT listener socket.

Alert derivation is a pure snapshot pass over the `health_snapshot()` result.
Conditions derived: publisher absent (Critical/Pipeline), reader lag above
threshold (Warning/Stage), ring overflow (Warning/Stage), output not running
while publisher is active (Warning/Output), SRT listener UDP drops
(Warning/Engine). Each alert carries `id`, `severity`, `scope`, `evidence`,
`recommended_action`, `firstSeen`, and `lastSeen` fields. `firstSeen`
records when the condition was first observed; `lastSeen` updates on each
subsequent observation. Resolved alerts are pruned from the persistent
tracker. Results are sorted Critical-first.

## Capability Matrix

The labels below distinguish implementation from proof.

| Capability | Status | Evidence / boundary |
|---|---|---|
| RTMP H.264/AAC ingest and same-shape RTMP egress | Basic interoperability observed; timestamp fix applied | Video uses DTS as RTMP timestamp (audio uses PTS); composition offset is carried in FLV payload |
| SRT H.264/AAC ingest/read/egress | Implemented, prior local validation | Unit tests plus prior live read/egress evidence |
| SRT ingest to RTMP egress | **Implemented and live-verified** | `cargo run --bin test_harness -- correctness-srt-rtmp` publishes H.264/AAC over SRT, loops it through native RTMP egress, and probes H.264 video plus AAC audio |
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

- `/api/status` now reports Restream, linked FFmpeg, SBOM summary, and a
  production-debug System section: OS/kernel, uptime, memory, CPU
  model/topology/features, and virtualization context.
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
   `bframe-rtmp`, `correctness-srt-rtmp`, `correctness-hevc-rtmp`,
   `correctness-hevc-srt`) through `test/run-protocol-matrix.sh`, which
   creates an aggregate manifest under `test/artifacts/<run-id>/`. The
   `hls-put` mode covers HTTP PUT delivery and destination restart recovery
   with a dummy sink; `bframe-rtmp` covers live RTMP B-frame timestamp
   round-trip behavior; `correctness-srt-rtmp` covers live SRT→RTMP
   packetization; `correctness-hevc-rtmp` covers live H.265-to-H.264 RTMP edge
   conversion; `correctness-hevc-srt` covers live H.265 SRT passthrough.
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
- ~~canonical stage-key building in `MediaEngine`~~ — done; stage registries now
  use typed `StageKey` values end-to-end. The intermediate legacy string-key
  helpers were removed during Phase 3; extracting finer-grained registry structs
  remains a future cleanup.
- ~~typed alert derivation model~~ — done; `src/alerts.rs` introduces `Alert`,
  `Severity`, and `Scope` types and a pure `derive_alerts(&snapshot)` function;
  `GET /pipelines/:id/alerts` and `GET /api/v1/alerts` expose the model.
  `AlertTracker` now stamps `firstSeen`/`lastSeen` on each alert and prunes
  resolved conditions automatically.
- ~~stage metrics wiring for transcoder stages~~ — done; `external_transcoder`
  fetches `Arc<StageMetrics>` via `get_or_create_stage_metrics` and calls
  `record_in` / `record_out` per packet; `h264_transcoder` does the same on the
  input (muxer) side. Metrics are removed from the engine map on stage exit.
  The graph endpoint now returns live `packetsIn`, `packetsOut`, `bytesIn`,
  `bytesOut`, and `packetsPerSec` for every active transcoder node.
- ~~MemoryQueue stats in processing graph~~ — done; `MediaEngine` now holds an
  `input_queues` registry (same storage-key scheme as `transcoder_buffers`).
  `h264_transcoder` and the internal transcoder register their `MemoryQueue`
  on creation and deregister on exit. `processing_graph()` includes a
  `queueMetrics` field on each transcoder node with live `len`, `capacity`,
  `highWaterBytes`, `blockedWrites`, and `blockedWriteUs` from `MemoryQueue::stats()`.
  External-subprocess stages have no `MemoryQueue` and emit `queueMetrics: null`.
- ~~operator overview and pipeline summary endpoints~~ — done; `GET /api/v1/overview`
  returns `totalPipelines`, `activePipelines`, `degradedPipelines`,
  `failedOutputs`, `alertCount {critical, warning}`, and `srtListener` in a
  single snapshot pass. `GET /api/v1/pipelines/:id/summary` returns the
  operator-focused pipeline view (source, outputs rollup, recording, hlsPreview,
  alerts) without raw graph data; returns 404 for unknown IDs.
- ~~secure public HLS playlist and segment routes~~ — done; HLS playlist and
  segment routes require the dashboard session cookie.
- ~~replace or hide stale Grafana and status-page UI tied to MediaMTX~~ — done;
- ~~make listener ports, database path, media path, and operational tuning
  configurable~~ — done; environment overrides cover ports, SQLite path, media
  directory, fd limit, reconciler cadence, retry backoff, HLS idle timeout, and
  HLS segment/window sizing.
- ~~engine-native typed registries~~ — done; `MediaEngine` internal maps
  (`stage_metrics`, `transcoder_buffers`, `input_queues`, `pipe_metrics`) now
  use typed `StageKey` keys. The legacy string layer (`parse_legacy_key`,
  `storage_key`, `legacy_key`, `Unknown` variant) has been removed entirely.
  All engine method signatures accept typed `StageKind`/`StageKey` parameters.
  Splitting into finer-grained registry structs (`StageRegistry`,
  `IngestRegistry`, etc.) remains a future cleanup.
- ~~lifecycle event log~~ — done; `src/events.rs` provides a bounded 1000-event
  FIFO ring (`EventLog`) with `EventKind` variants for ingest connect/disconnect,
  stage start/stop, and egress start/stop. `MediaEngine` emits events at each
  lifecycle transition. `GET /api/v1/events` exposes the log with optional
  `pipeline_id` and `limit` query params.
- ~~pipe back-pressure metrics for external transcoder~~ — done; `src/media/pipe_metrics.rs`
  introduces `PipeMetrics` with stdin-stall and stdout-idle counters. The external
  transcoder registers an `Arc<PipeMetrics>` in `MediaEngine::pipe_metrics` on
  startup and removes it on exit. `processing_graph()` includes a `pipeMetrics`
  field on external-subprocess nodes. `StageMetrics` no longer carries pipe-specific
  fields. The bench shows `record_stall` at ≈9 ns (2× `AtomicU64 fetch_add Relaxed`).
- ~~timing module with rdtsc / Instant fallback~~ — done; `src/media/timing.rs`
  provides `now()` / `delta_us()` backed by `rdtsc` (≈22 ns on x86_64 with
  invariant TSC) or `Instant` (≈36 ns fallback). Three validation gates before
  committing to rdtsc: invariant TSC CPUID bit, calibrated cycles/µs in
  [100, 10000], observed window ≥ 50 µs. `calibrate()` returns `bool`; stages
  log "Instant fallback" when rdtsc is not used. 6 unit tests cover validation
  bounds, monotonicity, and real elapsed time.
- ~~code organisation cleanup~~ — done; `StageMetrics`, `PipeMetrics`, and the
  timing module were extracted from `engine.rs` / `external_transcoder.rs` into
  `src/media/stage_metrics.rs`, `src/media/pipe_metrics.rs`, and
  `src/media/timing.rs`. `engine.rs` re-exports both metric types via `pub use`.
  `benches/stage_metrics.rs` now covers record_in/out cost (≈10 ns), snapshot
  cost (≈625 ns), and the full stdin-instrumentation path (≈36 ns per packet).

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

Approximate lines in the reviewed working tree (June 26, 2026):

| Area | Lines | Notes |
|---|---:|---|
| `src/api.rs` | 2,812 | v1 operator + engineer endpoints |
| `src/alerts.rs` | 549 | typed alert model + AlertTracker |
| `src/events.rs` | 210 | new: lifecycle event log |
| `src/db.rs` | 776 | |
| `src/diag.rs` | 987 | |
| `src/lib.rs` | 500 | |
| `src/media/engine.rs` | 2,823 | typed StageKey registries + telemetry |
| `src/media/stage_metrics.rs` | 81 | new: hot-path throughput counters |
| `src/media/pipe_metrics.rs` | 54 | new: subprocess pipe back-pressure |
| `src/media/timing.rs` | 210 | new: rdtsc/Instant elapsed timing |
| `src/media/external_transcoder.rs` | 649 | pipe metrics + timing wired |
| `src/media/mpegts.rs` | 2,918 | |
| `src/media/rtmp.rs` | 2,101 | |
| `src/media/srt.rs` | 3,185 | |
| `src/media/ring_buffer.rs` | 1,269 | |
| `src/media/transcoder.rs` | 1,185 | |
| `tests/api.rs` | 1,740 | v1 + engineer endpoint tests |
| `tests/db.rs` | 396 | |
| `tests/transcoder.rs` | 120 | |
| `benches/stage_metrics.rs` | 155 | new: hot-path cost measurements |

Line counts are descriptive only and should not be treated as a completion
metric.

## Bottom Line

The rewrite has crossed the architectural milestone: the production runtime no
longer depends on Node.js or MediaMTX, and the core Rust test suite passes.

The honest status is **native rewrite implemented, correctness and operational
productization in progress**.
