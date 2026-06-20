# Media Diagnostics and Application Residency

Status: focused implementation design based on the working tree on 2026-06-20.

## Goal

The first diagnostics implementation should answer:

1. Is the input media structurally and temporally healthy?
2. How long do media packets spend inside Restream?
3. Which currently traceable stage contributes that time?
4. Is a specific output reader falling behind or dropping packets?

The minimum latency measurement is **application residency**:

```text
packet enters the traceable application pipeline
  -> packet passes through traceable queues and stages
  -> packet reaches the last traceable application boundary
```

NIC, kernel, qdisc, hardware timestamp, remote endpoint, and reconstructed
transcode lineage are not required for the first implementation.

## Design Principles

### Protect the media critical path

Diagnostics must add minimal work to packet processing:

- no per-packet allocation for diagnostics;
- no locks, logging, serialization, or async channel sends on the hot path;
- no wall-clock or system calls per packet;
- no payload copies;
- no unbounded event collection;
- no diagnostic reader that changes the production pipeline;
- no second ffprobe connection for normal diagnostics;
- no high-cardinality metrics per packet.

The hot path should perform only a few monotonic timestamp reads, integer
assignments, and task-local counter/histogram updates.

If instrumentation causes measurable throughput or latency regression, reduce
the instrumentation before adding more diagnostics.

### Measure only where lineage exists

Do not infer or reconstruct packet lineage after the code loses it.

For each path:

- start timing at the first boundary where a media packet has a stable identity;
- carry that identity while the same packet or an explicit derivative remains
  traceable;
- stop at the last boundary where that identity is still available;
- report the measured scope accurately;
- do not estimate the untraceable remainder.

Examples:

- A source `MediaPacket` pulled from the source ring and written directly to
  RTMP egress can be measured through RTMP packet construction and the socket
  write call.
- SRT ingest can be measured from the FFmpeg demuxer producing a
  `MediaPacket`; the earlier SRT receive and MPEG-TS byte queue do not yet have
  packet lineage.
- The current transcoder converts packets to an opaque byte stream and creates
  new packets with zero timestamps. Source lineage ends when the source packet
  is written to the transcoder `MemoryQueue`. Do not claim end-to-end residency
  through the transcoder until the transcoder explicitly preserves lineage.
- HLS and recording currently write packet payloads into byte queues. Source
  packet lineage ends at that queue write. Segment/file completion is a
  separate aggregate measurement, not packet residency.

### Preserve media time separately

PTS/DTS describe the media timeline. Application timestamps describe processing
and queue residence.

Never replace PTS/DTS with wall-clock timestamps. Store and analyze both
independently.

## Current Diagnostic Surfaces

| Surface | Purpose |
|---|---|
| `/health` | Pipeline input/output state, metadata, byte counters, bitrate, and publisher quality |
| `/metrics/system` | Host CPU, memory, disk, and aggregate network activity |
| `/pipelines/:id/probe` | In-process metadata, bitrate, audio tracks, and basic GOP summary |
| `/pipelines/:id/graph` | Current processing graph |
| `/pipelines/:id/diagnostics` | Streaming per-pipeline diagnostic checks |
| Download/copy report | Operator-readable diagnostic results |

The native diagnostic runner currently intends to provide:

| Check | Current intent | Required correction |
|---|---|---|
| Engine Status | Ingest/egress state, uptime, bytes, source ring | Show reader lag rather than total historical writes |
| Stream Info | Codec and track metadata | Retain |
| GOP Analysis | Keyframe interval | Use media PTS when available |
| Publisher Transport | RTMP receiver-side TCP and SRT transport quality, including bonded member state | First rate sample is unavailable until a second counter snapshot exists |
| Ring Buffer Health | Buffer state | Add per-reader lag and overflow counters |
| Active Outputs | Output state and bytes | Add last traceable packet residency |
| System Resources | CPU, RAM, disk | Retain as contextual data |
| Network Bandwidth | Host interface rates | Label as host-wide, not pipeline latency |
| SRT Listener Socket | Bonding availability plus shared UDP queue, peak, and drops | Queue/drop values are listener-wide and Linux-specific |

## Existing Implementation Gaps

These should be fixed before adding new timing work:

- RTMP transport sampling is Linux-specific. It reads `TCP_INFO` and
  `SO_MEMINFO` directly from the accepted socket every two seconds; unsupported
  platforms and socket-query failures are reported explicitly.
- SRT receive loops call `srt_bistats()` approximately once per second.
  Loss/drop/retransmit/undecrypt alerts use snapshot deltas, while cumulative
  totals remain available for context.
- The frontend still describes diagnostic step 5 as ffprobe wall-clock packet
  timing, but native step 5 is Active Outputs.
- The native runner no longer emits `probe-raw`, while the report still claims
  raw ffprobe packets and frames are attached.
- `MemoryQueue` does not expose depth, high-water mark, or blocked time.
- `RingBuffer::fill_and_capacity()` reports total writes capped at capacity,
  not current occupancy or consumer lag.
- HLS and recording concatenate raw packet payloads into `CustomInput` format
  detection. Diagnostics must not imply those mux paths are healthy merely
  because their task/token is active.

## Diagnostics Retained From the Old ffprobe Code

The previous implementation provided useful analyses that should move
in-process when the required packet or frame data already exists:

- codec, profile, dimensions, FPS, sample rate, channels, and format checks;
- packet counts and bitrate by media type and track;
- PTS/DTS monotonicity, duplicates, discontinuities, and missing timestamps;
- audio/video packet interleaving;
- startup gap between audio and video;
- keyframe interval and GOP stability;
- A/V clock drift;
- decode warnings, missing references, and timestamp warnings;
- publisher stalls from counters that remain flat over a sample window.

Decoded-frame-only checks should wait until frames naturally exist in the
processing path. Do not add decoding solely for diagnostics.

The old RTMP use of `-use_wallclock_as_timestamps 1` should not be retained. It
observed a second local probe connection, did not provide NIC timestamps, and
destroyed the original media timeline for that capture.

## Minimal Packet Timing Contract

Add a compact timing structure to `MediaPacket`.

```rust
pub struct MediaPacket {
    pub packet_id: u64,
    pub media_type: MediaType,
    pub track_index: u32,
    pub pts: i64,
    pub dts: i64,
    pub is_keyframe: bool,
    pub payload: Bytes,
    pub timing: PacketTiming,
}

#[derive(Clone, Copy)]
pub struct PacketTiming {
    pub pipeline_enter_ns: u64,
    pub ring_push_ns: u64,
}
```

Keep the initial structure small. Add another timestamp only when it defines an
implemented and useful boundary.

`packet_id` should be generated once when the canonical source packet is
created. Cloning an `Arc<MediaPacket>` preserves the ID without additional
work.

Use one monotonic clock for all application-residency timestamps. The first
implementation may use the platform monotonic clock behind `Instant`; a more
specialized clock is only needed if measurement proves it necessary.

## Low-Overhead Aggregation Architecture

Keep mutable diagnostic aggregates with the task or reader that already owns the
operation:

- ingest packet counters stay with the single ingest producer;
- ring wait and lag statistics stay inside each `Reader`;
- egress call-duration and residency statistics stay with the egress task;
- `MemoryQueue` counters stay under its existing mutex and are updated while
  that mutex is already held.

Do not update a shared global histogram from every packet.

Once per health/diagnostic interval, publish a compact immutable snapshot to the
engine. The dashboard and diagnostic runner read the snapshot rather than
touching live hot-path state.

This keeps packet processing free of additional lock acquisition and avoids
cache-line contention between outputs.

## Traceable Boundaries

### RTMP ingest

Current lineage begins when `rml_rtmp` emits `VideoDataReceived` or
`AudioDataReceived` and Restream constructs a `MediaPacket`.

Record:

- `pipeline_enter_ns` immediately before constructing the packet;
- `ring_push_ns` immediately before `RingBuffer::push()`.

Do not claim socket-receive-to-packet time. RTMP messages can span or combine
TCP reads, and the current parser does not expose byte-range provenance.

### SRT ingest

Current lineage begins when the FFmpeg demuxer emits a codec packet and
Restream constructs a `MediaPacket`.

Record:

- `pipeline_enter_ns` after FFmpeg returns the demuxed packet;
- `ring_push_ns` immediately before `RingBuffer::push()`.

Do not attribute `srt_recv()`, MPEG-TS `MemoryQueue`, or demux discovery time to
an individual media packet.

Track those earlier stages only with aggregate queue/counter measurements if
needed.

### Ring-buffer readers

Each `Reader` should own a stable reader ID and counters:

```text
current_lag_packets
max_lag_packets
overflow_count
fast_forward_count
packets_read
last_read_ns
```

When a reader pulls a packet, calculate:

```text
source_ring_residency = reader_pull_ns - packet.ring_push_ns
application_age = reader_pull_ns - packet.pipeline_enter_ns
```

Update fixed-size task-local histograms or counters associated with the
reader/output.
Do not mutate the shared packet for reader-specific timestamps.

### Direct RTMP egress

Lineage remains available through:

```text
Reader::pull()
  -> RTMP packet construction
  -> socket.write_all()
```

Measure:

```text
source ring wait
RTMP construction time
write-call time
pipeline enter to completed write call
```

The write completion is a userspace/socket API boundary. It is not a NIC
transmit timestamp and must not be labeled as one.

### Direct SRT egress

Lineage remains available through the current `srt_send()` call when a
`MediaPacket` payload is sent directly.

Measure:

```text
source ring wait
srt_send call time
pipeline enter to completed srt_send call
```

The current direct payload path has protocol-correctness concerns described in
the media pipeline design. Timing it does not validate the payload format.

### Transcoder

Lineage currently ends here:

```text
source MediaPacket -> input_queue.write(packet.payload)
```

Measure only:

```text
source ring wait before transcoder
time from pipeline enter to transcoder queue write
transcoder input queue aggregates
```

Do not connect newly created transcoder output packets to source packets.
Do not report source-to-transcoded-egress packet residency.

Transcoder output may have its own new lineage once it produces valid packets
with valid PTS/DTS, but that is a separate timing scope.

### HLS and recording

Lineage currently ends when the source packet payload is written to the
component’s `MemoryQueue`.

Measure only:

```text
source ring wait
pipeline enter to component queue write
queue depth/high-water/blocked time
segment or file aggregate completion latency
```

Do not assign a completed HLS segment or recording write to an individual source
packet unless the component later preserves that relationship explicitly.

## Minimal Aggregates

For each pipeline and traceable output reader, retain:

```text
packet count
byte count
video/audio count
keyframe count
reader overflow count
reader lag current/max
application residency histogram
source ring residency histogram
stage call-duration histogram, where instrumented
maximum observed residency
last packet timestamp
```

Report p50, p95, p99, maximum, and sample count.

Use fixed-memory task-local histograms. Publish a summarized snapshot
periodically. Do not retain an event for every packet.

## Sampling

Measure aggregate counters for all packets.

Residency timestamps may initially be recorded for all packets if benchmark
impact is negligible. If timestamp reads or histogram updates are measurable,
sample deterministically:

- every Nth packet;
- every keyframe;
- every overflow or error;
- packets whose already-measured stage duration exceeds a threshold.

The sample decision should be a cheap integer operation based on `packet_id`.

Do not add dynamic tracing, payload capture, or per-packet report generation to
the media path.

## `MemoryQueue` Measurements

Add aggregate state to `MemoryQueue` without changing the byte storage model:

```text
current_bytes
high_water_bytes
total_bytes_written
total_bytes_read
writer_blocked_ns
reader_blocked_ns
write_count
read_count
```

Queue age requires per-write timestamp bookkeeping and should be omitted from
the first implementation unless queue depth and blocked time prove
insufficient.

## Diagnostic Output

The initial diagnostic report should contain:

### Pipeline summary

- active ingest protocol and uptime;
- media metadata and tracks;
- current bitrate and packet rate;
- GOP/keyframe summary;
- PTS/DTS and A/V interleaving findings;
- publisher transport counters that are genuinely available.

### Residency summary

For each traceable branch:

- measurement start boundary;
- measurement end boundary;
- p50/p95/p99/max application residency;
- p50/p95/p99/max source ring wait;
- packets sampled;
- reader current/max lag;
- overflow and fast-forward counts;
- explicit reason if lineage ends before network egress.

Example:

```text
Output: YouTube RTMP
Scope: RTMP MediaPacket creation -> socket write completion
Application residency: p50 1.2ms, p95 3.8ms, p99 8.1ms, max 21.4ms
Source ring wait: p50 0.3ms, p95 1.1ms
Reader lag: current 0 packets, max 7 packets
Overflows: 0
Accuracy: application monotonic timestamps
```

```text
Output: 720p transcoded RTMP
Source scope: MediaPacket creation -> transcoder input queue write
Source application residency: p95 2.1ms
End-to-end residency: unavailable
Reason: source packet lineage is not preserved through the current byte-stream
transcoder
```

## Implementation Sequence

### 1. Correct existing diagnostics

1. Fix the `PublisherQuality` field mismatch.
2. Remove stale ffprobe timing claims from the frontend/report.
3. Wire transport collectors that already exist, or report them unavailable.
4. Correct ring-buffer health semantics.
5. Preserve valid PTS/DTS and keyframe metadata where packets are currently
   reconstructed.

### 2. Add minimal residency timing

1. Add `packet_id` and two initial timestamps to `MediaPacket`.
2. Instrument RTMP packet creation and SRT demux packet creation.
3. Add reader IDs, lag, overflow, and fixed-size residency histograms.
4. Instrument direct RTMP and SRT egress call boundaries.
5. Stop measurements explicitly at transcoder, HLS, and recording queue writes.

### 3. Port packet-level analyses

Run the useful old ffprobe analyses from existing packet metadata:

- packet counts and bitrate;
- keyframe/GOP timing;
- PTS/DTS validation;
- A/V interleaving and startup gap;
- publisher stall detection from sampled counters.

Only add decoded-frame analyses when the production processing path already
decodes frames.

### 4. Benchmark

Benchmark with diagnostics disabled and enabled:

- ingest throughput;
- egress throughput;
- p50/p99 packet handling time;
- CPU use;
- allocations;
- ring-reader performance.

The enabled path should have no new packet allocation or payload copy.
Establish an acceptable overhead budget before enabling it by default.

## Deferred, Not Required for the Initial Design

These may be considered later if application residency is insufficient:

- kernel receive/transmit timestamps;
- NIC hardware timestamps;
- TCP byte-range-to-RTMP-message provenance;
- libsrt UDP packet lineage;
- eBPF socket correlation;
- remote publisher/destination timestamps;
- reconstructed source-to-transcoded-output lineage;
- full packet/frame flight recorders.

They should not complicate the initial packet path or data model.

## Acceptance Criteria

The initial implementation is complete when:

- every reported latency names its exact start and end boundary;
- direct source-to-egress paths report application residency percentiles;
- source ring wait is visible per output reader;
- reader lag, overflow, and fast-forward behavior are visible;
- measurements stop when lineage is lost;
- no latency is inferred across an opaque byte queue or transcode;
- original PTS/DTS remain intact;
- no diagnostic packet allocation, payload copy, logging, or channel send is
  added to the critical path;
- benchmarked overhead is documented and accepted.
