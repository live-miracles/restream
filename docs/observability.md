# Observability and Diagnostics

The Rust runtime exposes JSON and SSE diagnostics directly from process state.
It does not expose a Prometheus text endpoint, proxy Grafana, or poll a sidecar.

## Endpoints

| Surface | Authentication | Purpose |
|---|---|---|
| `GET /healthz` | None | Process liveness: `{ "status": "ok" }` |
| `GET /health` | None | Pipeline input/output state, transport quality, recording state, SRT listener pressure |
| `GET /metrics/system` | Session | CPU, memory, disk, and host-wide network rates (JSON, not Prometheus) |
| `GET /api/status` | Session | Restream build/toolchain, linked native-library versions, SBOM summary, and System information: OS, kernel, memory, CPU topology/features, and virtualization context |
| `GET /api/status/sbom` | Session | CycloneDX 1.5 runtime SBOM for resolved Rust crates and linked native libraries |
| `GET /pipelines/:id/probe` | Session | Active input codec, dimensions, audio tracks, bitrate, and GOP summary |
| `GET /pipelines/:id/graph` | Session | Processing stages, buffers, and output connections |
| `GET /pipelines/:id/diagnostics` | Session | Nine-check diagnostic run streamed over SSE |
| `GET /api/v1/overview` | Session | Engine-wide operator summary: pipeline counts, alert rollup, SRT listener |
| `GET /api/v1/alerts` | Session | Aggregate alerts across all pipelines with `firstSeen`/`lastSeen` tracking |
| `GET /api/v1/events` | Session | Lifecycle event log (ingest, stage, egress transitions) |
| `GET /api/v1/pipelines/:id/summary` | Session | Operator pipeline view: source, outputs, alerts |
| `GET /api/v1/pipelines/:id/alerts` | Session | Per-pipeline alert list |
| `GET /api/v1/engine/telemetry` | Session | Engineer: all ingests, stages, egresses, transcoder buffers |
| `GET /api/v1/pipelines/:id/telemetry` | Session | Engineer: pipeline-scoped ingest, ring, stages, egresses |
| `GET /api/v1/stages/:key/telemetry` | Session | Engineer: single-stage throughput and pipe metrics |

See [API Reference](api-reference.md) for request/response details.

## `/health` Field Derivation

`GET /health` is built on demand from native `MediaEngine` state and per-pipeline
recording settings in SQLite.

### Top-Level Shape

```json
{
  "generatedAt": "2026-06-20T12:00:00Z",
  "status": "ready",
  "pipelines": {},
  "srtListener": {
    "bondingAvailable": false,
    "udpRxQueueBytes": 0,
    "udpRxQueuePeakBytes": 0,
    "udpDrops": 0
  }
}
```

`status` is currently always `ready` when the handler returns.

### Input Status

| Condition | `input.status` |
|---|---|
| `MediaEngine::active_ingests` contains the pipeline ID | `on` |
| No active ingest is registered | `off` |

The Rust implementation does not emit `warning` or `error` for input state.

Active input fields:

| Field | Source |
|---|---|
| `publishStartedAt` | Current UTC time minus the ingest's monotonic uptime |
| `bytesReceived` | Ingest `AtomicU64` counter |
| `bitrateKbps` | Average bytes received over total ingest uptime |
| `video` | RTMP FLV parser or SRT native TsDemuxer metadata |
| `audio` | Primary audio metadata |
| `audioTracks` | Full active audio track list, including PID/language/title when available |
| `publisher.protocol` | `rtmp`, `srt`, or `file` |
| `publisher.remoteAddr` | Accepted peer address when available |
| `publisher.quality` | Protocol-specific live transport snapshot |

`bytesSent` is derived from active egress counters. `readers` is the count of
live source-ring readers, and `readerMetrics` exposes each reader's lag slots,
overflow count, unread packet age, read/write indexes, and burst-size stats.
`unexpectedReaders.count` remains a placeholder.

### RTMP Publisher Quality

On Linux, the accepted socket is queried with `TCP_INFO` and `SO_MEMINFO` about
every two seconds. Fields include RTT, receive RTT, bytes received,
last-receive age, receive space/window, out-of-order packets, receive-buffer
occupancy, and a rate derived from consecutive byte samples.

The first rate sample is unavailable (no prior counter exists). On unsupported
hosts or collection failure, `tcpStatsUnavailableReason` explains the absence.

### SRT Publisher Quality

The receive loop samples `srt_bistats()` approximately once per second.
Cumulative loss/drop/retransmit/undecrypt counters are retained for context;
alerting should use per-second delta fields so a recovered connection can
return to healthy.

The snapshot also includes SRT buffer occupancy and packets in flight. For
bonded publishers it additionally reports:

| Field | Meaning |
|---|---|
| `srtBonded` | Whether libsrt accepted this publisher as a socket group |
| `srtGroupMemberCount` | Total member tuples currently reported |
| `srtGroupConnectedMembers` | Members in the connected state |
| `srtGroupActiveMembers` | Members carrying the active backup-group path |
| `srtGroupBrokenMembers` | Members reported broken |

The member-count fields are omitted for ordinary single-link publishers.

### Output Status

Active native egresses appear in `pipelines[id].outputs`:

- `register_egress()` stores `active_egresses[outputId]` with an explicit
  `pipeline_id`;
- `health_snapshot()` includes entries whose `ActiveEgress.pipeline_id`
  matches the pipeline being rendered.

| Field | Source |
|---|---|
| `status` | `ActiveEgress.status` (normally `running`) |
| `protocol` | URL-derived protocol (`rtmp`, `srt`, `hls`, or `unknown`) |
| `phase` | Current sender lifecycle phase such as `starting`, `connecting`, `handshaking`, `publishing`, `sending`, `uploading`, or `failed` |
| `targetAddr` | Resolved peer address when known, without credentials |
| `totalSize` | Atomic bytes-sent counter |
| `bytesOut` | Same atomic bytes-sent counter, named for v1 telemetry consistency |
| `bitrateKbps` | Byte delta divided by elapsed sample time; cached between samples |
| `startedAt` | Egress registration timestamp |
| `lastProgressAt` | Last successful protocol send or HLS PUT completion |
| `lastProgressAgeMs` | Age of the last successful send/upload sample |
| `lastError`, `lastErrorAt`, `failurePhase` | Last structured sender failure observed while the egress is active |
| `quality` | Egress transport quality. RTMP/RTMPS expose sender-side `TCP_INFO`/`SO_MEMINFO`; SRT exposes sender-side `srt_bistats()` and bonded group member state when available. |

Stopped configured outputs are absent rather than being emitted with `off`.
The dashboard merges those definitions from `/config`; active output counters
come from `/health`.

The egress bitrate updates only after a sample window longer than 0.5 seconds
and only when the byte counter advances.

RTMP egress phases cover URL parsing, TCP/TLS connect, RTMP handshake,
application connect, publish acceptance, and packet send. SRT egress phases
cover resolve, connect, sender-capacity rejection, and `srt_send()` failures.
RTMP ingest and RTMP/RTMPS egress quality include the kernel TCP congestion
control algorithm. RTMP/RTMPS egress quality also includes sender-side RTT,
bytes sent/acked/retrans, unacked/lost/retrans packet counts,
congestion/window state, not-sent bytes, pacing and delivery rate,
send-buffer limitation time, RTO counters, and send-side socket memory.

SRT egress quality includes `srt_bistats()` sender rate, RTT, link capacity,
send buffer occupancy, flight/flow/congestion windows, sent loss/drop/retrans
totals and per-second rates, received NAKs, and bonded group member counts.
HLS PUT egress reports upload progress for segment and playlist PUTs. These
signals are local sender evidence; they do not prove that a third-party platform
accepted or played the stream unless a readback/verification probe is also run.

### Egress Telemetry Parity Status

Implemented egress parity:

- protocol classification and resolved target address where available
- sender lifecycle phase and structured failure phase/error
- last successful send/upload timestamp and progress age
- per-output bytes, bitrate, and StageMetrics output counters
- RTMP/RTMPS sender-side TCP quality
- SRT sender-side `srt_bistats()` quality and bonded egress member state
- graph, health, v1 telemetry, diagnostics, and alert surfacing for failed or
  stale active egresses
- process-lifetime egress failure events through `GET /api/v1/events`

Remaining egress parity gaps:

- durable egress failure history across process restarts. The current
  implementation records `EgressFailed` in the bounded in-memory event log; DB
  persistence should be a later retention/audit decision with schema and pruning
  policy.
- post-egress media metadata, GOP cadence, and timestamp validation
- optional loopback/readback probe evidence for RTMP/SRT outputs

Post-egress validation should be modeled as readback evidence, not inferred from
sender counters. For RTMP/SRT, this means optionally routing an egress to a
readable loopback sink or destination-provided playback endpoint, running
`ffprobe`/native probes on that read side, and attaching an evidence object such
as:

```json
{
  "outputId": "out-rtmp",
  "source": "loopback",
  "protocol": "rtmp",
  "validatedAt": "...",
  "video": { "codec": "h264", "width": 1920, "height": 1080, "fps": 30.0 },
  "audio": [{ "codec": "aac", "sampleRate": 48000, "channels": 2 }],
  "gop": { "avgMs": 2000, "maxMs": 2200 },
  "timestamps": { "monotonicDts": true, "maxGapMs": 40 },
  "result": "pass"
}
```

### Recording State

```json
{
  "recording": {
    "enabled": true,
    "active": true
  }
}
```

- `enabled` comes from SQLite key `recording_enabled:<pipelineId>`.
- `active` reflects a live recording cancellation token.

### SRT Listener State

The shared SRT listener monitor reads Linux `/proc/net/udp` and tracks:

- whether the linked libsrt accepted `SRTO_GROUPCONNECT` at startup
- current receive-queue bytes
- peak receive-queue bytes since process start
- cumulative kernel UDP drops

These are listener-wide values, not per-pipeline. `bondingAvailable: false`
means ordinary SRT works but the pinned repo-managed libsrt build was not
prepared with bonding support or the wrong binary was linked.

## Diagnostic Checks

The SSE diagnostic run (`GET /pipelines/:id/diagnostics`) emits nine checks:

| # | Check | Notes |
|---|---|---|
| 1 | Engine Status | Ingest/egress state, uptime, bytes, source ring, max reader lag, total overflows, and max unread packet age |
| 2 | Stream Info | Codec and track metadata |
| 3 | GOP Analysis | Keyframe interval; uses media PTS when available |
| 4 | Publisher Transport | RTMP `TCP_INFO`/`SO_MEMINFO` or SRT `srt_bistats()`, including bonded member state |
| 5 | Ring Buffer Health | Buffer state plus per-reader lag slots, overflow counters, and unread packet age |
| 6 | Active Outputs | Output state and bytes; egresses associated via `ActiveEgress.pipeline_id` |
| 7 | System Resources | CPU, RAM, disk |
| 8 | Network Bandwidth | Host-wide interface rates (not pipeline-specific latency) |
| 9 | SRT Listener Socket | Bonding availability, shared UDP queue/peak/drops (listener-wide, Linux-specific) |

The diagnostic runner warns above 50% SRT queue occupancy, alerts above 75%,
and reports any kernel drop count.

An optional `probe=rtmp|srt` query must match the active ingest protocol.
Returns `404` without an active ingest and `400` for a protocol mismatch.

## Known Instrumentation Gaps

These should be fixed before adding new timing work:

- `RingBuffer::fill_and_capacity()` reports source-ring occupancy from the
  slowest live reader and caps it at capacity. Per-reader snapshots expose live
  lag and unread packet age.
- `MemoryQueue::stats()` exposes current depth, capacity, high-water bytes,
  blocked write count, blocked write time, and closed state. These counters
  still need to be surfaced in higher-level graph/API snapshots where useful.
- The frontend still describes diagnostic step 5 as ffprobe wall-clock packet
  timing, but native step 5 is Active Outputs.
- The native runner no longer emits `probe-raw`, while the report still claims
  raw ffprobe packets and frames are attached.
- HLS, recording, and in-process transcoder input share the TS packet feeder.
  Diagnostics must still avoid implying those mux paths are healthy merely
  because their task/token is active.

## Application Residency Design

The diagnostics design treats application residency, reader lag, packet
lineage, and transcode lineage as future instrumentation work.

### Design Principles

- **Protect the hot path**: no per-packet allocation, lock, logging, or
  serialization for diagnostics.
- **Measure only where lineage exists**: start timing at the first boundary
  where a media packet has a stable identity; stop at the last boundary where
  that identity is still available.
- **Preserve media time separately**: PTS/DTS describe the media timeline;
  application timestamps describe processing and queue residence.

### Minimal Packet Timing Contract (not yet implemented)

```rust
pub struct PacketTiming {
    pub pipeline_enter_ns: u64,
    pub ring_push_ns: u64,
}
```

### Traceable Boundaries

| Path | Start | End | Notes |
|---|---|---|---|
| RTMP ingest → RTMP egress | `MediaPacket` creation | `socket.write_all()` completion | Full lineage available |
| SRT ingest → SRT egress | TsDemuxer packet output | `srt_send()` completion | Full lineage available |
| Through transcoder | Source packet | Transcoder `MemoryQueue` write | Lineage ends at queue write; transcoder creates new packets |
| HLS / recording | Source packet | Component `MemoryQueue` write | Lineage ends at queue write |

### Low-Overhead Aggregation

Keep mutable diagnostic aggregates with the task or reader that already owns
the operation. Do not update a shared global histogram from every packet. Once
per health/diagnostic interval, publish a compact immutable snapshot to the
engine.

### Implementation Sequence

1. Correct existing diagnostics (ring-buffer semantics, stale ffprobe claims).
2. Add `packet_id` and two initial timestamps to `MediaPacket`.
3. Add fixed-size residency histograms and packet-lineage timestamps.
4. Instrument direct RTMP and SRT egress call boundaries.
5. Port useful old ffprobe analyses (bitrate, GOP, PTS/DTS validation,
   interleaving, stall detection) from existing packet metadata.
6. Benchmark with diagnostics enabled vs disabled.

### Analyses From the Old ffprobe Code

These should move in-process when the required packet data exists:

- codec, profile, dimensions, FPS, sample rate, channels, and format checks
- packet counts and bitrate by media type and track
- PTS/DTS monotonicity, duplicates, discontinuities, and missing timestamps
- audio/video packet interleaving and startup gap
- keyframe interval and GOP stability
- A/V clock drift
- decode warnings and missing references
- publisher stalls from sampled counters

Decoded-frame-only checks should wait until frames naturally exist in the
processing path.

## Prometheus and Grafana

No Rust equivalent exists yet. The old MediaMTX Prometheus/Grafana setup
belongs to the archived implementation under `old/`.

Recommended next step: export bounded process, pipeline, transport, ring-reader,
and egress counters in Prometheus format without putting labels or allocation
work on the packet hot path.
