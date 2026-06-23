# Testing

## Rust Test Suite

Run the full suite:

```sh
cargo test
```

As of June 22, 2026 this runs 132 passing tests:

| Suite | Tests | Source |
|---|---:|---|
| Library/unit | 92 | `src/` modules (ring buffer, SRT, RTMP, MPEG-TS, codec, HLS, engine, etc.) |
| API integration | 24 | `tests/api.rs` |
| Database integration | 12 | `tests/db.rs` |
| Transcoder integration | 4 | `tests/transcoder.rs` |
| **Total** | **132** | |

Unit coverage includes:

- RTMP FLV H.264/AAC parsing and signed composition time
- HLS playlist/window behavior
- SRT stream-ID normalization, URL/bond parsing, codec mapping, payload
  extraction, rate deltas, socket option IDs, listener UDP-stat parsing
- Linux `TCP_INFO`/`SO_MEMINFO` conversion and live socket collection
- Transcoder stage sharing and audio-routing parsing
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
encoding persistence, egress-pipeline association in `/health`, and
deletion-cancellation of egress tasks.

## Live Integration Tests

### 2×3 Matrix

Run the 2-pipeline × 3-output live test against a running application:

```sh
./test/run-2x3.sh
```

For the external-transcoder routing check, use the broader matrix smoke test:

```sh
./test/run-2x3-matrix.sh
```

It covers two ingest protocols (RTMP/SRT), three egress protocols (RTMP/SRT/HLS),
and two encoding modes (passthrough and 720p transcode) while also creating
multiple same-type outputs so the shared transcoder stage is exercised.

Required tools: `ffmpeg`, `curl`, and `jq`. The script targets native RTMP/SRT
ingest with six outputs.

### Mixed Scale Test

```sh
N_PER_GROUP=25 ./test/run-mixed-scale-test.sh
```

Exercises the five ingest configurations that cover every combination of codec
and protocol used in production, each fanned out to `4×N_PER_GROUP` mixed outputs
(RTMP-src + RTMP-720p + SRT-src + SRT-720p):

| Config | Ingest | Codec | Audio tracks |
|---|---|---|---|
| `h264-rtmp` | RTMP | H.264 | 1 |
| `h265-srt` | SRT | H.265 | 1 |
| `h264-srt` | SRT | H.264 | 1 |
| `h264-srt-multi` | SRT | H.264 | 2 |
| `h265-srt-multi` | SRT | H.265 | 2 |

After each config it prints RSS delta, per-output overhead, and the count of
external FFmpeg subprocesses. Expected results (see
[media-pipeline.md § Scale Test Pipeline Paths](media-pipeline.md#scale-test-pipeline-paths)):

| Config | `ext_ffmpeg#` | Int OS threads |
|---|:---:|:---:|
| `h264-rtmp` | 1 | 0 |
| `h264-srt` | 1 | 0 |
| `h265-srt` | 1 | 1 |
| `h264-srt-multi` | 1 | 0 |
| `h265-srt-multi` | 1 | 1 |

Set `ISOLATE=1` to restart restream and mediamtx between configs so each
baseline is clean. Requires `ffmpeg`, `ffprobe`, `mediamtx`, `curl`, and `jq`.

### 8-Config Structured Scale Test

```sh
N_OUTPUTS=10 ./test/run-scale-test.sh
```

Sweeps eight ingest×output×encoding combinations (RTMP/SRT ingest × RTMP/SRT
output × source/720p encoding) and records RSS + FFmpeg subprocess counts as
outputs are added one by one. Useful for spotting per-output memory growth.

### Media Validation

```sh
./test/run-media-validation.sh
```

Bounded developer/WSL profile:

- one RTMP file publisher and matching RTMP probe
- one SRT file publisher and matching SRT probe
- 500 in-process readers over 2,000 shared packets
- 32 loopback RTMP egress sessions for five seconds

### Isolation

Both scripts require an isolated network namespace to avoid port conflicts:

**Docker:**
```sh
docker run --rm \
  -v "$(pwd)":/app -w /app \
  rust:1-bookworm bash -c '
    apt-get update -qq && apt-get install -y -qq ffmpeg jq libavformat-dev libavcodec-dev libavutil-dev libswresample-dev libswscale-dev libavfilter-dev libavdevice-dev pkg-config clang > /dev/null 2>&1
    cargo build --release
    ./target/release/restream &
    until curl -sf http://localhost:3030/healthz > /dev/null 2>&1; do sleep 1; done
    ./test/run-2x3.sh
  '
```

**Linux Unshare (lighter):**
```sh
unshare --net --map-root-user bash -c '
  ip link set lo up
  ./target/release/restream &
  until curl -sf http://localhost:3030/healthz > /dev/null 2>&1; do sleep 1; done
  ./test/run-2x3.sh
'
```

### SRT Bonding

```sh
./scripts/test-srt-bonding.sh
```

Builds and runs separate client/server processes for two-member broadcast and
backup groups. Fails if `SRTO_GROUPCONNECT` is unavailable, two member tuples
do not attach, or backup delivery does not continue after primary member close.

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

```text
test/run-ingest-equivalence.sh
test/run-egress-matrix.sh
test/run-hls-put.sh
test/run-h265.sh
test/run-recovery.sh
test/run-scale-inprocess.sh
test/run-scale-500.sh
test/run-media-validation.sh
```

Each run writes artifacts to `test/artifacts/<run-id>/` with manifest,
environment, per-case results (PASS/FAIL/EXPECTED_FAIL/SKIPPED/INFRA_FAILURE),
ffprobe output, captures, metrics, logs, and summary.

## Capability Gates

These capabilities must be treated as test results, not assumptions:

| Capability | Gate |
|---|---|
| RTMP H.264/AAC ingest and egress | B-frame timestamp round-trip |
| SRT H.264 and H.265 ingest/egress | Full correctness matrix |
| Video presets (720p, 1080p, etc.) | Decode/filter/encode loop must exist |
| Cross-protocol SRT→RTMP | Protocol packaging must be correct |
| HLS live segments | Native TsMuxer validates in-memory |
| HLS upload egress | HTTP PUT must be implemented |
| Recording | Readable file with correct streams/timestamps |
| Audio remap/downmix | Channel-level filtering must be implemented |
| Custom encoding | Custom args must be applied by transcoder |
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

