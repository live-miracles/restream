# End-to-End and Scale Testing

## Purpose

This plan validates the complete media path:

```text
publisher
  -> RTMP or SRT ingest
  -> protocol-matched probe
  -> shared video and audio processing stages
  -> RTMP, SRT, or HLS packaging
  -> local protocol correctness sink
  -> independent probe and content assertions
```

It defines two separate 500-output benchmarks:

```text
in-process packet replay
  -> Restream stages
  -> 500 in-process null consumers

one live network ingest
  -> hottest Restream fan-out path
  -> 500 protocol sessions
  -> lightweight handshake-and-discard sink
```

Correctness and performance are deliberately separate. A discard sink can
measure connection and fan-out cost, but it cannot prove that the emitted media
is valid.

## Production and Test Boundaries

MediaMTX is used only as a local integration-test sink and probe surface. It is
not part of the production data path.

Production egress destinations are streaming platforms:

```text
Restream -> RTMP/RTMPS platform ingest
Restream -> SRT platform ingest
Restream -> YouTube/Akamai-style HLS HTTP upload
```

The production release gate therefore tests protocol behavior, authentication,
packaging, timestamps, retry semantics, and backpressure expected by those
platforms. A local MediaMTX pass is useful interoperability evidence, but it
does not replace platform sandbox or staging tests.

## Current Capability Gates

The following must be treated as test results, not assumed capabilities.

| Area | Current implementation | Required test outcome |
| --- | --- | --- |
| RTMP ingest/read/egress | Native `rml_rtmp` path; outbound video uses DTS and audio uses PTS | Must pass H.264/AAC B-frame timestamp checks |
| SRT ingest/read | libsrt plus MPEG-TS demux/remux | Must pass H.264 and H.265 correctness |
| SRT egress | Uses an MPEG-TS mux stage before `srt_send` | Must pass H.264/H.265 packaging, timestamp, and sink-readability checks |
| Video presets | Encoder parameters are created, but compressed input packets are currently copied instead of decoded, filtered, and encoded | `720p`, `1080p`, crop, and rotate are expected to fail visual/codec assertions until the encode loop is complete |
| Transcoder output contract | MPEG-TS byte chunks are pushed back as timestamp-zero video packets | Expected to fail protocol-neutral packet and timestamp assertions |
| Cross-protocol egress | Ring payload shape depends on ingest and stage origin | Expected to fail combinations that require a missing protocol package stage |
| HLS preview | In-memory MPEG-TS segments served by Axum | Must pass pull/playback tests |
| Recording | Matroska mux path exists, but raw packet payloads are concatenated into format detection | Must produce a readable file with correct streams/timestamps before being marked supported |
| HLS upload egress | Not implemented: a non-RTMP/SRT URL starts the local segmenter and ignores the destination URL | Must not be marked supported until HTTP PUT is implemented |
| `remap` and `downmix` | Select the requested track but currently perform stream copy | Expected to fail channel-semantic assertions until filtering/encoding is implemented |
| Custom encoding | Configuration API exists, but the in-process reconciler treats `custom` as source | Expected to fail transformation assertions until custom arguments are applied |
| RTMP H.265 | Requires Enhanced RTMP handling | Capability test; do not infer support from SRT H.265 |
| `/health` output telemetry | `ActiveEgress` stores `pipeline_id`; API regression tests cover association | Must pass live bitrate/status polling |
| Bonded SRT ingest | One listener accepts a group ID, exposes member state, and rejects unrelated duplicate publishers; requires `ENABLE_BONDING=ON` | Must run against a bonding-enabled libsrt and validate one receive path plus member failover |

The HLS upload behavior to match is documented by
[`mediamtx_push_targets_v1.17.1.patch`](https://github.com/live-miracles/restream/blob/master/patches/mediamtx/mediamtx_push_targets_v1.17.1.patch):

- destination is an `http://` or `https://` URL ending in `.m3u8`;
- MPEG-TS segments and the playlist are uploaded with HTTP PUT;
- video segmentation rotates on IDR boundaries;
- audio-only segmentation rotates by PTS/time;
- `TARGETDURATION` reflects the maximum actual segment duration;
- multiple equivalent HLS destinations share one muxer;
- uploads use bounded concurrency.

Restream does not currently provide this upload behavior.

For local HLS upload testing, first determine whether the selected MediaMTX test
build accepts YouTube-style HLS upload as an ingest:

```text
HTTP PUT playlist
HTTP PUT MPEG-TS segments
playlist URL ending in .m3u8
```

If it accepts that upload shape, use it as the local sink. If it does not,
use the dedicated HLS PUT receiver described below. MediaMTX pulling Restream's
HLS playlist is only a fallback playback test in that situation; it is not the
primary HLS egress test and is not evidence of upload support.

The HLS push description currently present in `docs/api-reference.md` reflects
the earlier FFmpeg-process architecture and does not match the current
in-process reconciler. Executable behavior is authoritative until the
implementation and API documentation are brought back into agreement.

## Test Environment

Run the components on the same isolated host for functional testing:

| Component | Suggested address |
| --- | --- |
| Restream API | `127.0.0.1:3030` |
| Restream RTMP | `127.0.0.1:1935` |
| Restream SRT | `127.0.0.1:10080` |
| MediaMTX sink RTMP | `127.0.0.1:2935` |
| MediaMTX sink SRT | `127.0.0.1:20080` |
| MediaMTX sink HLS playback | `127.0.0.1:28888` |
| HLS PUT receiver | `127.0.0.1:28080` |
| Scale sink control/metrics | `127.0.0.1:29090` |

Use separate Restream and sink ports. Looping an egress into Restream's ingest
listener can hide routing mistakes and can create a publishing cascade.

Required tools:

```text
ffmpeg
ffprobe
curl
jq
MediaMTX v1.17.1
cargo
/usr/bin/time
pidstat
perf
ss
lsof
```

Record the following before every run:

```text
git commit and dirty-worktree diff
rustc and cargo versions
FFmpeg library and CLI versions
MediaMTX version and patch commit
kernel and CPU model
physical/logical CPU count
RAM
NIC speed and MTU
```

## Deterministic Fixtures

Generate fixtures rather than relying only on arbitrary MP4 files.

### Dual-Audio H.264

The two tracks use different tones so routing can be verified:

```bash
ffmpeg -y \
  -f lavfi -i "testsrc2=size=1920x1080:rate=30" \
  -f lavfi -i "sine=frequency=440:sample_rate=48000" \
  -f lavfi -i "sine=frequency=880:sample_rate=48000" \
  -t 120 \
  -map 0:v -map 1:a -map 2:a \
  -c:v libx264 -preset veryfast -g 60 -bf 2 \
  -c:a aac -b:a 128k \
  -metadata:s:a:0 title=track-440hz \
  -metadata:s:a:1 title=track-880hz \
  test/artifacts/dual-audio-h264.mkv
```

### Dual-Audio H.265

```bash
ffmpeg -y \
  -f lavfi -i "testsrc2=size=1920x1080:rate=30" \
  -f lavfi -i "sine=frequency=440:sample_rate=48000" \
  -f lavfi -i "sine=frequency=880:sample_rate=48000" \
  -t 120 \
  -map 0:v -map 1:a -map 2:a \
  -c:v libx265 -preset veryfast -x265-params "keyint=60:bframes=2" \
  -c:a aac -b:a 128k \
  test/artifacts/dual-audio-h265.mkv
```

Also retain short 10-second versions for smoke tests. The long fixtures are for
sync, leak, and recovery tests.

## Phase 1: Publish Equivalent RTMP and SRT Inputs

Create two pipelines with different stream keys:

```text
e2e-rtmp
e2e-srt
```

Publish the same H.264 fixture to both:

```bash
ffmpeg -nostdin -re -stream_loop -1 \
  -i test/artifacts/dual-audio-h264.mkv \
  -map 0 -c copy -f flv \
  rtmp://127.0.0.1:1935/live/e2e-rtmp
```

```bash
ffmpeg -nostdin -re -stream_loop -1 \
  -i test/artifacts/dual-audio-h264.mkv \
  -map 0 -c copy -f mpegts \
  "srt://127.0.0.1:10080?streamid=publish:live/e2e-srt&pkt_size=1316"
```

The RTMP publisher may reject multiple audio tracks because standard RTMP/FLV
interoperability commonly exposes one audio stream. If so, run the equivalence
test with one audio track, and retain dual-audio coverage for SRT and the
processing matrix.

Acceptance:

- both ingests become active within 10 seconds;
- ingest protocol is reported correctly;
- bytes received and bitrate increase continuously;
- the process remains alive after sequence headers, B-frames, reconnects, and
  publisher shutdown;
- no subtitle, data, attachment, or unknown stream enters the media ring.

## Phase 2: Probe Both Inputs Through Matching Protocols

Use both the engine snapshot and an external protocol probe.

Engine snapshots:

```bash
curl -s -b "$COOKIE_JAR" \
  "$API_URL/pipelines/$RTMP_PIPELINE_ID/probe" | jq .

curl -s -b "$COOKIE_JAR" \
  "$API_URL/pipelines/$SRT_PIPELINE_ID/probe" | jq .
```

External probes must match the active ingest protocol:

```bash
ffprobe -v error -show_streams -show_format -of json \
  rtmp://127.0.0.1:1935/live/e2e-rtmp
```

```bash
ffprobe -v error -show_streams -show_format -of json \
  "srt://127.0.0.1:10080?streamid=read:live/e2e-srt&mode=caller"
```

Normalize the results before comparing. Compare:

```text
video codec
coded width and height
frame rate
video profile and level when exposed by both containers
audio codec
sample rate
channel count
intended audio-track count
average bitrate after a 30-second observation window
GOP interval
```

Do not compare container-specific fields such as stream index, PID, start time,
FLV tags, MPEG-TS program IDs, or exact reported format bitrate.

Acceptance tolerances:

| Field | Tolerance |
| --- | --- |
| Codec, dimensions, sample rate, channels | Exact |
| Frame rate | `0.01 fps` |
| GOP interval | One frame |
| Average bitrate | `±10%` after warm-up |
| A/V start offset | `<= 50 ms` |
| A/V drift over 10 minutes | `<= 20 ms` |

The diagnostics endpoint must reject an explicitly requested probe protocol when
it differs from the active ingest protocol.

## Phase 3: Egress Correctness Matrix

### Matrix Dimensions

Test both ingest protocols against every supported combination:

Video shape:

```text
source
720p
1080p
vertical-crop
vertical-rotate
custom
```

Audio routing:

```text
passthrough
atrack:0
atrack:1
atrack:0,1
remap:0:1:0
downmix:0
```

Egress protocol:

```text
rtmp
srt
hls
```

Protocol dispatch also needs negative tests:

```text
rtmps
rtsp
rtsps
http URL not ending in .m3u8
unknown scheme
malformed URL
```

Unsupported schemes must be rejected during validation or output startup. They
must not silently enter the HLS preview branch.

This is `2 ingests x 6 video shapes x 6 audio modes x 3 protocols = 216`
cases. Unsupported combinations must produce a clear skipped or expected-fail
result; they must not silently pass.

Use pairwise reduction for every commit and run the full Cartesian matrix
nightly. Always include these collision cases together:

```text
720p+atrack:0
720p+atrack:1
1080p+atrack:0
source+atrack:0
```

They prove that video stages are shared where appropriate and audio stages do
not cross-contaminate different upstream video presets.

### Local MediaMTX Sink

MediaMTX is a local correctness sink for protocols it accepts. Configure
explicit paths for every case:

```yaml
rtmp: yes
rtmpAddress: :2935

srt: yes
srtAddress: :20080

hls: yes
hlsAddress: :28888
hlsAlwaysRemux: no
hlsVariant: mpegts

api: yes
apiAddress: 127.0.0.1:29997

paths:
  all_others:
    source: publisher
```

Use unique sink paths:

```text
matrix/<ingest>/<video>/<audio>/<protocol>
```

Examples:

```text
rtmp://127.0.0.1:2935/matrix/rtmp/720p/atrack0/rtmp
srt://127.0.0.1:20080?streamid=publish:matrix/srt/source/passthrough/srt
```

For every case:

1. Create the Restream output.
2. Start it and wait for MediaMTX to report a publisher.
3. Probe the MediaMTX path independently.
4. Capture at least 30 seconds to a file.
5. Run structural, codec, routing, timestamp, and decode checks.
6. Stop and delete the output.
7. Confirm the MediaMTX publisher disappears and Restream releases the task.

For production qualification, repeat the applicable cases against platform
sandbox or staging endpoints. At minimum, cover:

```text
RTMP/RTMPS authentication and reconnect
SRT stream ID and latency parameters
HLS upload URL/query handling and HTTP response behavior
platform codec, bitrate, GOP, audio, and track-count constraints
```

### Per-Output Assertions

Run:

```bash
ffprobe -v error -show_streams -show_format -of json "$SINK_READ_URL"
ffmpeg -v error -xerror -t 30 -i "$SINK_READ_URL" -f null -
```

Assert:

- exactly one intended video stream;
- exactly the requested audio tracks;
- no subtitle, data, attachment, or unknown streams;
- output resolution matches the video preset;
- H.264/H.265 identity is preserved or transformed as specified;
- all packets decode for 30 seconds with `-xerror`;
- DTS is monotonic per stream;
- PTS/DTS reordering is valid for B-frames;
- first video/audio timestamps differ by no more than 50 ms;
- no drift beyond 20 ms over the long test;
- output bitrate remains within the preset range;
- stopping one output does not interrupt another output sharing its stages.

Audio routing requires content assertions, not just track counts:

| Routing | Assertion |
| --- | --- |
| `passthrough` | Both 440 Hz and 880 Hz tracks remain |
| `atrack:0` | Only the 440 Hz track remains |
| `atrack:1` | Only the 880 Hz track remains |
| `atrack:0,1` | Both tracks remain in the requested order |
| `remap:0:1:0` | Output channel 0 derives from source channel 0 and channel 1 from source channel 1 |
| `downmix:0` | Output is stereo and contains the expected contribution from every source channel |

Use `astats`, `channelsplit`, and frequency detection to verify channel content.
The current stream-copy implementation of `remap` and `downmix` should be marked
expected-fail until it performs the requested transformation.

### HLS Upload Sink Selection

Test the local MediaMTX build for YouTube-style HLS ingest before choosing the
fallback:

1. PUT a valid segment to the intended object URL.
2. PUT a playlist referencing that segment to a URL ending in `.m3u8`.
3. Confirm MediaMTX exposes a readable path containing the uploaded media.
4. Confirm subsequent PUTs advance the live playlist without restarting the
   path.

If MediaMTX does not implement this ingest shape, use the dedicated HLS PUT
receiver for all upload-egress correctness and load tests.

### HLS Pull/Playback Fallback

The currently implemented Restream HLS surface is pull-style HLS:

```text
GET /hls/<pipeline-id>
GET /hls/<pipeline-id>/index.m3u8
GET /hls/<pipeline-id>/seg<N>.ts
```

The first two URLs return the same playlist and share the same `HlsStore`.
`index.m3u8` is the conventional URL for players and pullers; the shorter URL
is retained for convenience. `/preview/hls/<pipeline-id>/...` remains as a
deprecated compatibility alias and must return identical content.

Validate:

- playlist syntax with an HLS parser;
- every referenced segment returns `200`;
- each segment begins with valid MPEG-TS synchronization;
- media sequence increases;
- segment duration and `TARGETDURATION` are valid;
- playlist window remains bounded;
- continuous playback survives at least five rotations;
- old segments are evicted without unbounded RSS growth.

Use this only when MediaMTX cannot receive YouTube-style HLS upload, or when
specifically testing the local preview surface. Configure MediaMTX to pull the
Restream playlist:

```yaml
paths:
  restream-hls-pull:
    source: http://127.0.0.1:3030/hls/PIPELINE_ID/index.m3u8
    sourceOnDemand: no
```

Then probe the MediaMTX HLS output:

```bash
ffprobe -v error -show_streams -of json \
  http://127.0.0.1:28888/restream-hls-pull/index.m3u8
```

This validates HLS pull/playback only. It does not validate the production
upload path.

Authorization is a future release item. Until signed or tokenized HLS access is
implemented, run external pull tests only on a trusted test network. The future
test matrix must cover playlist and segment authorization, expiry, tampering,
revocation, cross-pipeline access, rate limiting, and token-safe logging.

### HLS HTTP PUT Upload Test

Direct HLS upload support requires a destination such as:

```text
http://127.0.0.1:28080/upload/matrix/index.m3u8
```

The receiver must accept and record PUT requests for:

```text
index.m3u8
segment files referenced by index.m3u8
```

Acceptance:

- the configured destination URL is actually contacted;
- segment PUT completes before the playlist references that segment;
- playlist and segments use stable, correctly resolved URLs;
- retries do not corrupt ordering;
- reconnect resumes with a valid media sequence;
- identical HLS outputs share one package stage;
- upload concurrency is bounded;
- audio-only streams rotate by time;
- no retained-segment memory leak;
- deleting the output stops uploads.

Until these assertions pass, report:

```text
HLS playlist/store routes: supported
HLS live media generation: not yet supported
HLS upload-style egress: not supported
```

## Phase 4: H.265 Coverage

Publish the H.265 fixture through SRT and run:

```text
SRT ingest -> SRT read
SRT ingest -> source SRT egress
SRT ingest -> 720p SRT egress
SRT ingest -> HLS preview
SRT ingest -> RTMP egress capability test
```

Expected behavior:

- SRT/MPEG-TS paths retain `hevc`;
- H.265 transcodes use an HEVC encoder when that is the selected output policy;
- no path silently labels HEVC bytes as H.264;
- RTMP H.265 passes only when Enhanced RTMP sequence headers and coded frames
  are understood by both Restream and the sink.

Probe every H.265 result. A successful TCP/RTMP connection is not a codec pass.

## Phase 5: Recovery and Isolation

For representative matrix cases:

1. Stop the publisher for 10 seconds and restart it.
2. Restart MediaMTX while outputs are running.
3. Drop the sink connection.
4. Introduce 1%, 3%, and 5% packet loss plus 50 ms jitter on SRT.
5. Start and stop a second output sharing the same video stage.
6. Start an intentionally slow sink.

Assert:

- no process crash or deadlock;
- retry/backoff is bounded;
- stopped outputs do not restart against operator intent;
- one slow output does not stall unrelated outputs;
- readers recover at a keyframe after ring overflow;
- shared stages remain alive while at least one dependent output exists;
- shared stages terminate after their last dependent output stops;
- A/V timestamps do not reset independently after recovery.

## Phase 6: Custom 500-Output Sink

Build a separate sink executable. Do not embed it into Restream; process
separation keeps sink CPU, allocations, and failures out of the system under
test.

Suggested layout:

```text
test/scale-sink/
  Cargo.toml
  src/main.rs
  README.md
```

The sink exposes:

```text
RTMP listener
SRT listener
HTTP PUT listener
Prometheus or JSON metrics endpoint
```

### RTMP Scale Mode

Perform the minimum valid server flow:

```text
C0/C1/C2 handshake
connect acceptance
createStream response
publish acceptance
read and discard RTMP chunks/messages
```

Track:

```text
connections accepted/current/closed
handshake failures
bytes and messages received
per-connection first-byte time
read errors
idle timeouts
```

### SRT Scale Mode

Use libsrt or a proven SRT library. Accept caller connections, complete the SRT
handshake, read message payloads, and discard them.

Track:

```text
connections accepted/current/closed
handshake failures
bytes/messages received
SRT retransmission and loss statistics
read errors
```

The scale mode may discard payloads, but a sampled subset of connections must
also pass MPEG-TS sync and PAT/PMT validation.

### HLS Scale Mode

Accept HTTP PUT, consume request bodies, and return a configurable status:

```text
200 success
429 throttling
500 retry test
delayed response for backpressure
connection reset for recovery
```

Record object name, size, request duration, status, and concurrent PUT count.

## Phase 7: In-Process Pipeline Benchmark

Add a benchmark mode with no network listener, publisher process, MediaMTX,
custom sink process, or kernel socket dependency.

```text
deterministic packet generator
  -> engine ingest boundary
  -> source ring
  -> optional shared video stages
  -> optional audio-routing stages
  -> optional protocol package stages
  -> 500 in-process null consumers
```

This mode answers:

```text
How much CPU and memory does the engine itself require?
Does work scale with unique stages or output count?
Can the ring and scheduler sustain 500 consumers?
Do internal queues remain bounded?
```

It does not measure network syscalls, protocol handshakes, TLS, kernel socket
buffers, congestion control, or external sink behavior.

### In-Process Source

Use a deterministic generator that emits canonical `MediaPacket` values at a
real-time cadence or as fast as possible:

```text
H.264 or H.265 video packet payloads
AAC audio packet payloads
realistic PTS and DTS
B-frame composition ordering
configurable GOP
one or more audio tracks
configurable payload sizes and bitrate
```

Provide two source modes:

```text
realtime: sleeps to reproduce 30/50/60 fps and audio cadence
saturation: emits without sleeping to determine maximum throughput
```

The preferred fixture source is a pre-demuxed packet trace loaded once into
memory and replayed. This avoids FFmpeg decode/demux cost while retaining real
packet sizes, keyframes, track layout, and timestamps.

### In-Process Null Consumers

Each null consumer owns a normal engine `Reader` and performs the same pull,
overflow, and cancellation behavior as a real output. It consumes packet
metadata and payload length, updates counters, and drops the `Arc<MediaPacket>`.

The optimizer must not remove the work. Accumulate at least:

```text
packet count
byte count
PTS/DTS checksum
keyframe count
track-index histogram
```

Use `std::hint::black_box` around the accumulated result in Criterion
benchmarks.

### Package-Stage Null Writers

When measuring packetization, run the real RTMP, MPEG-TS, or HLS package stage
but replace its transport writer with an in-process counting writer:

```text
RTMP serializer -> CountingWriter
MPEG-TS muxer -> MemoryQueue or CountingWriter
HLS segmenter/uploader -> NullObjectStore
```

`NullObjectStore` records playlist and segment metadata without HTTP:

```text
object name
object length
upload order
media sequence
concurrent write count
checksum
```

This allows package-stage scaling to be measured independently from networking.

### In-Process Workloads

Run at least:

```text
A. source ring -> 500 null readers
B. source + one shared package stage -> 500 null outputs
C. source + two shared video presets + audio routes -> 500 null outputs
D. 500 deliberately unshared package stages
E. one slow null reader plus 499 fast readers
F. cancellation and teardown of all 500 readers
```

For workload C use the same distribution as the external benchmark:

```text
200 source+atrack:0
100 source+atrack:1
100 720p+atrack:0
50 720p+atrack:1
50 1080p+atrack:0
```

### In-Process Metrics

Record:

```text
packets and media bytes per second
fan-out deliveries per second
CPU cycles and instructions per source packet
CPU cycles per delivered output packet
allocations and bytes allocated per packet
Arc clone/drop rate
ring occupancy and overflow count
queue high-water marks
unique stage count
threads and Tokio tasks
startup and teardown latency
steady-state and post-teardown RSS
```

Run Criterion microbenchmarks for stable comparisons and a long-running
release-mode harness for scheduler, memory, and teardown behavior.

Acceptance:

- all 500 consumers receive the expected packet and byte counts;
- one slow consumer does not stall the other 499;
- work for identical outputs is fan-out work, not duplicated encode/package
  work;
- internal queues remain bounded;
- cancellation releases all readers and stages;
- post-teardown resource counts return close to baseline;
- saturation throughput remains above the equivalent real-time aggregate load
  with recorded headroom.

## Phase 8: Networked 500-Output Benchmark

Do not use scale results as a release gate until one sampled output of the same
protocol and processing shape passes the correctness matrix. The benchmark can
still be run earlier to locate scheduler or fan-out bottlenecks, but it must be
labeled a transport-load experiment rather than a valid-media benchmark.

### Workload A: Hottest Fan-Out Path

Use one 1080p30 H.264/AAC ingest and 500 identical `source` RTMP outputs.

This measures:

```text
one ingest parse
one source ring
500 readers
500 RTMP sessions
500 socket writers
```

Ramp:

```text
1 -> 10 -> 50 -> 100 -> 250 -> 500 outputs
```

Hold each level for 2 minutes. Hold 500 for at least 30 minutes and run a
two-hour soak separately.

Repeat for SRT after valid MPEG-TS egress packaging exists.

### Workload B: Shared Processing

Create 500 outputs distributed across:

```text
200 source+atrack:0
100 source+atrack:1
100 720p+atrack:0
50 720p+atrack:1
50 1080p+atrack:0
```

Expected unique processing stages:

```text
video: source, 720p, 1080p
audio: one per unique audio operation and video upstream
package: one per final media shape and protocol, when package sharing exists
```

Verify stage count through the processing graph endpoint. CPU should scale with
unique encodes, not output count.

### Workload C: Mixed Protocol

After every protocol is correct:

```text
300 RTMP
150 SRT
50 HLS upload
```

This validates scheduler fairness and protocol-specific backpressure.

### Measurement

Collect at one-second resolution:

```text
Restream CPU by thread
RSS, virtual memory, and allocator statistics
thread and Tokio task count
open file descriptors and sockets
network bytes and packets
ring occupancy and reader overflow count
per-stage input/output bitrate
per-output bytes and reconnect count
sink accepted/current connections
sink bytes per connection
p50/p95/p99 handshake and first-byte latency
context switches and run-queue pressure
```

Use:

```bash
pidstat -durwt -p "$RESTREAM_PID" 1
watch -n1 "ls /proc/$RESTREAM_PID/fd | wc -l"
ss -s
perf stat -p "$RESTREAM_PID" \
  -e cycles,instructions,cache-misses,context-switches,cpu-migrations
```

Take heap/RSS samples for at least 30 minutes after reaching 500 connections.

### Scale Acceptance

Functional gates:

- 500/500 sessions reach publishing state;
- every session receives media bytes;
- zero unexpected output termination during the 30-minute hold;
- aggregate sink bitrate is within `±5%` of source bitrate multiplied by active
  passthrough outputs;
- no ring overflow for a healthy local discard sink;
- stopping all outputs returns connections, file descriptors, threads, and RSS
  near the pre-run baseline.

Performance gates must be recorded against named hardware rather than using one
universal CPU number:

- CPU growth is approximately linear with output protocol work;
- adding identical outputs does not create additional video encoders;
- RSS has no sustained positive slope after warm-up;
- p99 handshake and first-byte latency do not grow without bound;
- no single output or worker monopolizes a runtime thread;
- package sharing, once implemented, reduces mux work for identical outputs.

Suggested leak gate:

```text
RSS growth after warm-up < 1% of process RSS per 10 minutes
```

Investigate any monotonic growth even when it remains below that threshold.

## Automation and Artifacts

Extend `test/run-2x3.sh` into independent scripts:

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

`test/run-media-validation.sh` is the bounded developer/WSL profile. It runs:

```text
one RTMP file publisher and matching RTMP probe
one SRT file publisher and matching SRT probe
500 in-process readers over 2,000 shared packets
32 loopback RTMP egress sessions for five seconds
```

Use the larger scripts and longer soak periods only on a dedicated benchmark
host.

Every run writes:

```text
test/artifacts/<run-id>/manifest.json
test/artifacts/<run-id>/environment.json
test/artifacts/<run-id>/cases.jsonl
test/artifacts/<run-id>/ffprobe/
test/artifacts/<run-id>/captures/
test/artifacts/<run-id>/metrics/
test/artifacts/<run-id>/logs/
test/artifacts/<run-id>/summary.md
```

Each case result must be one of:

```text
PASS
FAIL
EXPECTED_FAIL
SKIPPED_UNSUPPORTED
INFRA_FAILURE
```

Do not convert an expected failure into a pass. Include the issue or capability
gate that must change before the expectation can be updated.

## Release Gate

A release candidate is acceptable only when:

1. RTMP and SRT ingest probes match the normalized source description.
2. The required pairwise matrix passes on every commit.
3. The full matrix passes nightly, apart from explicitly approved unsupported
   combinations.
4. H.264 and H.265 SRT paths decode without structural errors.
5. Audio routing passes content-level assertions.
6. HLS support is reported accurately as preview/pull and/or HTTP PUT push.
7. Recovery tests do not produce cross-output interruption or timestamp drift.
8. The in-process 500-consumer benchmark passes its engine-only functional and
   resource gates.
9. The networked 500-output benchmark passes its protocol-session gates on
   recorded hardware.
10. Shutdown returns resources close to baseline and the soak test shows no
   sustained memory growth.
