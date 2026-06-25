# Testing

## Rust Test Suite

Run the full suite:

```sh
cargo test
```

As of June 25, 2026 this runs 441 passing non-doctest tests:

| Suite | Tests | Source |
|---|---:|---|
| Library/unit | 350 | `src/` modules (ring buffer, SRT, RTMP, MPEG-TS, codec, HLS, engine, etc.) |
| API integration | 38 | `tests/api.rs` |
| AV sync integration | 14 | `tests/av_sync.rs` |
| Codec integration | 17 | `tests/codec.rs` |
| Database integration | 15 | `tests/db.rs` |
| Transcoder integration | 7 | `tests/transcoder.rs` |
| **Total** | **441** | |

The doctest suite also runs; the single codec example is intentionally ignored.

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

The API suite covers authentication, configuration, pipeline/output CRUD,
ingests, HLS aliases, status, graph, diagnostics preconditions, custom
encoding persistence/rejection for runtime outputs, HLS upload output
acceptance, RTMPS output acceptance, egress-pipeline association in `/health`,
and deletion-cancellation of egress tasks.

## Live Integration Tests

All live integration tests are unified under one entry point:

```sh
./test/run-integration.sh [--host] [--fast] [--json path] [--only checks] <mode>
```

By default every mode that manages its own server processes runs inside a
private loopback network namespace (`unshare --net`) so ports never conflict
with the host. Pass `--host` to skip the namespace wrapper.

Required tools: `ffmpeg`, `ffprobe`, `mediamtx`, `curl`, `jq`.

Common runner flags:

| Flag | Purpose |
|---|---|
| `--preflight` | Check binary, dependencies, namespace support, and host-mode port conflicts without starting the test. |
| `--fast` | Set `N_PER_GROUP=1`, `N_OUTPUTS=1`, `SNAP_EVERY=999`, and skip snapshot sleeps for quick agent loops. |
| `--json <path>` | Write JSONL assertion records alongside the human-readable log. Failed ffprobe assertions include stderr and log tails. |
| `--only <checks>` | Run selected `mixed-scale` assertion groups. Supported checks: `smoke`, `ffprobe`, `hls`, `lifecycle`, `tc-spawns`, `load`. |
| `--skip-load` | Skip resource snapshot sleeps and load assertion records while preserving correctness setup. |
| `--resume-from <id>` | Skip named assertion records until the requested assertion ID is reached. |
| `--baseline <path>` | Compare `mixed-scale` RSS summary against a saved CSV baseline. `RSS_BASELINE_THRESHOLD_PCT` defaults to 5. |
| `--save-baseline <path>` | Save the current `mixed-scale` RSS summary as a baseline CSV. |

Typical quick agent loop:

```sh
./test/run-integration.sh --preflight --json /tmp/restream-preflight.jsonl mixed-scale
./test/run-integration.sh --fast --json /tmp/restream-mixed.jsonl --only smoke,hls,lifecycle mixed-scale
```

### `ramp` — Sequential output ramp

```sh
N_OUTPUTS=10 ./test/run-integration.sh ramp
```

Sweeps eight ingest×egress×encoding combinations (RTMP/SRT ingest × RTMP/SRT
output × source/720p encoding). For each config, outputs are added one by one
and RSS + FFmpeg subprocess counts are snapshotted at every step. Useful for
spotting per-output memory growth and spotting encoding-stage leaks.

Env: `N_OUTPUTS` (default 10), `ISOLATE=1` (restart restream+mediamtx per
config for a clean baseline), `SNAP_EVERY` (default 1, snapshot every N outputs).

### `mixed-scale` — Concurrent group load

```sh
N_PER_GROUP=25 ./test/run-integration.sh mixed-scale
```

Exercises four ingest configurations covering every codec/protocol/audio-track
combination used in production. Each config fans out to `4×N_PER_GROUP` outputs
added group-by-group (all RTMP-src, then all RTMP-720p, then all SRT-src, then
all SRT-720p):

| Config | Ingest | Codec | Audio | Role |
|---|---|---|:---:|---|
| `h264-srt` | SRT | H.264 | 1 | **anchor**: HLS + smoke + fatal ffprobe + stop lifecycle |
| `h265-srt` | SRT | H.265 | 1 | TC_SPAWNS=1 assertion |
| `h264-srt-multi` | SRT | H.264 | 2 | multi-audio track routing |
| `h265-srt-multi` | SRT | H.265 | 2 | HEVC + multi-audio |

**Anchor config (`h264-srt`)** runs three merged correctness checks in addition
to the resource measurements:

1. **Smoke** — after source outputs are live, asserts no external transcoder has
   fired (source passthrough must not trigger the 720p encoder).
2. **Fatal ffprobe** — after all groups, `verify_stream` (fatal, 30×2s retries)
   on RTMP-src, RTMP-720p, SRT-src, SRT-720p, HLS/mediamtx, and HLS/restream
   endpoints.
3. **Stop lifecycle** — calls `/stop` on every output and polls `/config` until
   all reach `"stopped"` within 60 s.

**h265-srt** asserts `1 ≤ TC_SPAWNS ≤ ext_ffmpeg# + 1`: the number of shared
internal h264-tc transcoders must be bounded by the number of distinct consumer
paths (one for RTMP source outputs, one feeding each external ffmpeg for 720p),
not proportional to N. With both source and 720p output groups this bound is 2
regardless of N. If sharing breaks, each output spawns its own h264-tc and
TC_SPAWNS would equal N (or more).

Expected resource counts (see
[media-pipeline.md § Scale Test Pipeline Paths](media-pipeline.md#scale-test-pipeline-paths)):

| Config | `ext_ffmpeg#` | `TC_SPAWNS` bound |
|---|:---:|:---:|
| `h264-srt` | 1 | N/A (H.264 ingest, no h264-tc needed) |
| `h265-srt` | 1 | ≤ 2 (1 source path + 1 720p path) |
| `h264-srt-multi` | 1 | N/A |
| `h265-srt-multi` | 1 | N/A |

Env: `N_PER_GROUP` (default 25), `ISOLATE=1` (default, restarts per config).

### `bonding` — SRT socket bonding

```sh
./test/run-integration.sh bonding
```

Verifies libsrt group-socket bonding using dedicated C helper binaries compiled
from `test/srt-bond-server.c` and `test/srt-bond-client.c` against a statically
linked libsrt 1.5.5 built with `ENABLE_BONDING=ON`. The script calls
`scripts/setup-static-build.sh` automatically on first run.

Two bonding modes are tested:

| Mode | Members | `failover` | Messages |
|---|:---:|:---:|:---:|
| `broadcast` | 2 | 0 | 1 |
| `backup` | 2 | 1 | 2 |

Fails if `SRTO_GROUPCONNECT` is unavailable, the two member sockets do not
attach to the group, or backup delivery does not continue after the primary
member closes.

Note: `bonding` runs on the host network (random ports) so it is exempt from
the netns wrapper even without `--host`.

### `burst-verify` — Closed-GOP reader telemetry matrix

```sh
./test/run-integration.sh burst-verify
```

Streams a closed-GOP RTMP/SRT matrix across H.264/H.265, 1080p/4K, selected
frame rates, and single/dual-audio variants. Each case starts a source output,
waits for ingest, samples `/pipelines/:id/graph`, and fails if active ring
readers do not report positive `burstCount` and `avgBurstSize` telemetry.

Env: `BURST_SETTLE_SECS` (default 8), `BURST_CONFIGS` (optional
space-separated config allow-list, e.g. `BURST_CONFIGS="srt-h265-1080p-24fps-1a"`).

### `hls-put` — HTTP HLS upload dummy sink

```sh
./test/run-integration.sh hls-put
```

Publishes one SRT H.264/AAC input, starts an HTTP/YouTube-style HLS PUT output
with a `file=` query parameter, and verifies that a local dummy sink receives
both `seg<N>.ts` media segments and the playlist with the expected content
types. The uploaded segment is also probed with `ffprobe`. The mode then
restarts the dummy sink and requires a fresh segment PUT after recovery.

Env: `HLS_PUT_PORT` (default 8990), `HLS_PUT_SETTLE_SECS` (default 8),
`HLS_PUT_RESTART_SECS` (default 12).

### `bframe-rtmp` — RTMP B-frame timestamp round-trip

```sh
./test/run-integration.sh bframe-rtmp
```

Publishes one RTMP H.264/AAC input with B-frames, starts an RTMP source output,
and probes the egress packet stream with `ffprobe -show_packets`. The mode
requires at least one video packet with `PTS > DTS` and verifies DTS stays
monotone across the captured egress packets.

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

Use both engine snapshots (`/pipelines/:id/probe`) and external `ffprobe` via
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
test/run-integration.sh ramp
test/run-integration.sh mixed-scale
test/run-integration.sh bonding
test/run-integration.sh burst-verify
test/run-integration.sh hls-put
test/run-integration.sh bframe-rtmp
test/run-media-validation.sh
test/run-bitrate-scale-test.py
```

`test/run-integration.sh` writes `manifest.json` in the selected `WORK_DIR`
for each checked-in mode. The manifest starts as `RUNNING` and is finalized to
`PASS` or `FAIL` with timestamps, git head, network mode, and primary artifact
paths. This applies even to setup failures after the mode has initialized its
artifact directory, making failed matrix attempts auditable instead of silent.

Planned wrappers for the remaining matrix:

```text
test/run-ingest-equivalence.sh
test/run-egress-matrix.sh
test/run-h265.sh
test/run-recovery.sh
test/run-scale-inprocess.sh
test/run-scale-500.sh
```

Each completed matrix run should write artifacts to `test/artifacts/<run-id>/` with manifest,
environment, per-case results (PASS/FAIL/EXPECTED_FAIL/SKIPPED/INFRA_FAILURE),
ffprobe output, captures, metrics, logs, and summary.

## Capability Gates

These capabilities must be treated as test results, not assumptions:

| Capability | Gate |
|---|---|
| RTMP H.264/AAC ingest and egress | B-frame timestamp round-trip through `test/run-integration.sh bframe-rtmp` |
| SRT H.264 and H.265 ingest/egress | Full correctness matrix |
| H.265 source to RTMP egress | Live H.265→H.264 edge conversion through `cargo run --bin test_harness -- correctness-hevc-rtmp` |
| Built-in video presets (`h264`, `720p`, `1080p`) | Decode/filter/encode loop is covered by transcoder integration tests |
| Additional/custom video presets | Must be explicitly profiled and matrix-tested before advertising |
| Cross-protocol SRT→RTMP | Packetization helpers are covered; live matrix must prove end-to-end behavior |
| HLS live segments | Native TsMuxer validates in-memory |
| HLS upload egress | HTTP PUT delivery and destination restart recovery are covered by unit plus `test/run-integration.sh hls-put` dummy sink tests |
| Recording | Readable file with correct streams/timestamps |
| Audio remap/downmix | Channel-level filtering is implemented for the default runtime; full audio-content matrix remains required |
| Custom encoding | Runtime output selection must stay rejected until custom args are applied by a transcoder backend |
| Bonded SRT ingest | Separate-process broadcast + backup tests |

## Bitrate Scaling and Load Test Results (June 23, 2026)

This section presents the results of the automated bitrate scaling and load test executed across the 5 ingest configurations at bitrates of **1.5M, 4.0M, and 8.0M**. 

Correctness was verified first via `ffprobe` on all 4 output streams (`rtmp-src`, `rtmp-720p`, `srt-src`, `srt-720p`) at each stage, ensuring the outputs had stabilized before recording resource stats.

### 📊 Summary of Measurements

Below is the structured data of parent and child process resources collected directly from the `/proc` filesystem:

| Ingest Config | Bitrate | Restream RSS (KB) | Restream Delta (KB) | Restream CPU (%) | FFmpeg Subprocesses | FFmpeg Total RSS (KB) | Total memory (KB) | Correctness (ffprobe) |
| :--- | :---: | :---: | :---: | :---: | :---: | :---: | :---: | :---: |
| **h264-rtmp** | 1.5M | 93,096 | 20,092 | 4.5% | 1 | 430,732 | 523,828 | **PASS** |
| | 4.0M | 100,832 | 27,416 | 5.2% | 1 | 422,752 | 523,584 | **PASS** |
| | 8.0M | 115,944 | 41,952 | 6.3% | 1 | 425,100 | 541,044 | **PASS** |
| **h264-srt** | 1.5M | 94,628 | 20,628 | 4.5% | 1 | 430,280 | 524,908 | **PASS** |
| | 4.0M | 98,388 | 23,620 | 5.3% | 1 | 423,404 | 521,792 | **PASS** |
| | 8.0M | 114,952 | 39,000 | 6.1% | 1 | 425,480 | 540,432 | **PASS** |
| **h265-srt** | 1.5M | 237,756 | 163,556 | 67.7% | 1 | 426,628 | 664,384 | **PASS** |
| | 4.0M | 274,160 | 199,692 | 73.5% | 1 | 426,944 | 701,104 | **PASS** |
| | 8.0M | 305,088 | 229,240 | 81.3% | 1 | 424,908 | 729,996 | **PASS** |
| **h264-srt-multi**| 1.5M | 95,020 | 21,048 | 5.4% | 1 | 431,260 | 526,280 | **PASS** |
| | 4.0M | 99,032 | 24,008 | 5.7% | 1 | 424,212 | 523,244 | **PASS** |
| | 8.0M | 115,404 | 39,160 | 7.6% | 1 | 426,284 | 541,688 | **PASS** |
| **h265-srt-multi**| 1.5M | 224,708 | 150,488 | 74.3% | 1 | 425,000 | 649,708 | **PASS** |
| | 4.0M | 250,008 | 175,176 | 76.4% | 1 | 424,948 | 674,956 | **PASS** |
| | 8.0M | 273,164 | 197,092 | 82.9% | 1 | 423,640 | 696,804 | **PASS** |

### 🔍 Key Findings and Analysis

#### 1. FFmpeg Subprocess RSS Stability
* **Observation:** Regardless of the ingest bitrate (from `1.5M` to `8.0M`), the external `ffmpeg` transcoder process RSS remains extremely stable at around **422 MB to 431 MB**.
* **Reasoning:** The FFmpeg subprocess allocates decoder/encoder frames and scale filters statically on startup depending on the configuration and preset resolutions (scaling from `1920x1080` down to `1280x720`) rather than scaling dynamically with raw payload bitrate.

#### 2. Restream Parent Process Memory (RSS)
* **H.264 Ingests:** Restream consumes only **~93 MB to ~115 MB** (with a tiny delta of `20 MB` to `41 MB`). This represents a very flat memory scaling with increasing bitrate.
* **H.265 Ingests:** Restream memory grows to **~224 MB to ~305 MB**. This additional **~130-180 MB** overhead is caused by the in-process `hevc_to_h264` stage running inside the Rust process using FFmpeg C-FFI bindings.

#### 3. CPU Utilization
* **H.264 Ingests:** Restream CPU usage is extremely low (**4.5% to 7.6%**) because all video transcoding is delegated to the external `ffmpeg` subprocess. The Rust process only handles basic packet routing and demuxing.
* **H.265 Ingests:** Restream CPU usage increases significantly (**67% to 83%**) because the in-process `hevc_to_h264` stage performs decoding/encoding directly inside the Rust parent process.

#### 4. Verification and Correctness
* **Result:** Correctness checks successfully verified all 4 output streams (RTMP source, RTMP 720p, SRT source, SRT 720p) for every test case.
* **Fix details:** Setting a GOP size of 30 frames (`-g 30`) on the publisher allowed `ffprobe` to receive keyframes and sequence headers immediately, validating the stream resolution on all egress types.
