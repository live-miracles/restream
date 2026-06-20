# Media Diagnostics and NIC-to-NIC Timing Design

Status: design and implementation guide, based on the working tree on 2026-06-20.

## Objective

Restream diagnostics should answer four questions without requiring a second
probe connection:

1. Is the publisher sending valid, decodable, correctly timed media?
2. Is the transport healthy, and where are network stalls occurring?
3. How long does each media packet or decoded frame spend at every stage inside
   Restream?
4. When an output is late, is the delay at ingress, parsing/demux, buffering,
   transcoding, muxing, the local transmit queue, or the remote network?

The target local-host measurement is:

```text
ingress NIC receive
  -> kernel/socket receive
  -> protocol parse or SRT recovery
  -> demux
  -> source ring buffer
  -> optional decode/filter/encode
  -> output ring buffer
  -> mux/protocol serialization
  -> socket send
  -> qdisc/driver
  -> egress NIC transmit
```

For each sampled packet, Restream should produce the total local NIC-to-NIC
residence time and a breakdown for every boundary above.

“Publisher NIC to destination NIC” cannot be measured exactly by Restream
alone. That requires timestamping at both remote endpoints and synchronized
clocks, normally PTP. Restream can measure its own ingress NIC to egress NIC
exactly when both NICs and drivers support hardware timestamping. It can provide
software-timestamp fallbacks with an explicit accuracy grade.

## Diagnostic Surfaces Offered Today

The Rust rewrite exposes these operator surfaces:

| Surface | Purpose | Current source |
|---|---|---|
| `/health` | Pipeline input/output state, metadata, byte counters, bitrate, publisher quality | `MediaEngine::health_snapshot()` |
| `/metrics/system` | Host CPU, memory, disk, and aggregate network activity | `sysinfo` |
| `/pipelines/:id/probe` | In-process ingest metadata, bitrate, audio tracks, and basic GOP summary | `MediaEngine::probe_snapshot()` |
| `/pipelines/:id/graph` | Processing stages and connections for one pipeline | `MediaEngine::processing_graph()` |
| `/pipelines/:id/diagnostics` | Streaming per-pipeline diagnostic run over SSE | `src/diag.rs` |
| Download/copy report | Operator-readable diagnostic bundle | `public/ts/features/diagnostics.ts` |

The native SSE runner currently intends to offer:

| Check | Signals | Important limitation today |
|---|---|---|
| Engine Status | Active ingest/egress count, uptime, bytes, remote address, source ring state | Ring “fill” is total writes capped at capacity, not current occupancy or reader lag |
| Stream Info | Video/audio codec, dimensions, FPS, sample rate, channels | Metadata only; no packet or decoded-frame validation |
| GOP Analysis | Keyframe count, interval min/max/average/stddev | Uses process observation time, not media PTS or frame type |
| Publisher Transport | RTMP receiver-side TCP or SRT quality | Live samples are connection-scoped; first rate samples display as unavailable until a second counter snapshot exists |
| Ring Buffer Health | Capacity and apparent fill | No per-reader depth, wait time, overflow count, or age |
| Active Outputs | Target, status, bytes, start time | No queue time, socket pressure, or per-packet latency |
| System Resources | CPU, RAM, disk | Point-in-time host view; no process/thread attribution |
| Network Bandwidth | Per-interface RX/TX rate | Host-wide traffic, not pipeline/socket attributed |

### Current implementation state

The diagnostics design must not assume every kernel or protocol field is always present:

- Active RTMP connections sample `ss -tinmH` every two seconds and match the accepted peer tuple to the local RTMP listener. Missing `ss`, unsupported platforms, collection failures, and unmatched sockets are reported explicitly.
- Active SRT receive loops call `srt_bistats()` approximately once per second. Loss, drop, retransmit, and undecrypt alerts use deltas between snapshots, while cumulative totals remain available for context.
- The first RTMP receive-rate sample and first SRT counter-rate sample are intentionally unavailable because no prior snapshot exists.
- The frontend still labels diagnostic step 5 as ffprobe wall-clock timing.
  In the native runner, step 5 is now Active Outputs.
- The native runner no longer emits `probe-raw`, while the report download and
  AI prompt still describe raw ffprobe packets and frames.
- The transcoder output reader creates `MediaPacket`s with zero PTS/DTS and no
  keyframe flag, so downstream timing/GOP analysis cannot be trusted on that
  branch.
- `MemoryQueue` exposes neither byte depth nor enqueue time, so AVIO and
  transcoder queueing are invisible.

These are implementation gaps, not merely missing dashboard fields.

## Diagnostics From the Previous ffprobe Implementation

The previous Node/MediaMTX diagnostics ran a 20-second ffprobe capture with
`-show_packets`, `-show_frames`, `-show_streams`, and `-show_format`. It offered:

| Analysis | Method |
|---|---|
| Path status | MediaMTX path API |
| Publisher health | Ten one-second connection-stat samples |
| Reader health | Ten one-second samples across RTMP, SRT, HLS, and WebRTC |
| Codec/format validation | ffprobe stream and format metadata |
| Packet interleaving | Consecutive A/V packet runs and A/V timestamp gap |
| Startup A/V gap | Difference between audio and video `start_time` |
| GOP analysis | Keyframe PTS intervals, B-frame percentage, duplicate PTS, largest frame |
| A/V clock drift | Change in nearest audio/video frame PTS delta over the capture |
| Warning classification | Discontinuities, missing references, invalid/non-monotonic DTS |
| Logs | MediaMTX journal lines correlated to the path and connection IDs |
| Raw evidence | Full packet/frame JSON and stderr in the downloaded report |

These analyses should be retained, but run against data already passing through
the engine.

### What the old approach did well

- Preserved raw evidence instead of returning only a verdict.
- Analyzed packets and decoded frames separately.
- Used media timestamps for mux, GOP, startup, and drift checks.
- Sampled counters over time, allowing stall detection instead of relying only
  on cumulative totals.
- Correlated transport, codec, media timeline, readers, and logs into one report.

### What must be improved

For RTMP, ffprobe was run with `-use_wallclock_as_timestamps 1` and the resulting
timestamps were described as physical socket arrival times. They were not NIC
timestamps. They represented when ffprobe observed data after a second local
RTMP connection, protocol parsing, buffering, scheduling, and MediaMTX
forwarding. Replacing the original PTS/DTS also prevented transport timing and
media-timeline analysis from being performed on the same sample.

The in-process engine has a better option:

- Preserve original PTS, DTS, duration, time base, and codec/frame metadata.
- Independently attach transport and processing timestamps.
- Avoid creating a diagnostic reader that changes reader counts, buffering, or
  CPU load.
- Observe the actual production packet path, including each output branch.
- Keep packet-to-frame and input-to-output lineage through transcodes.

## Timestamp Model

Every timestamp must declare its clock, source, accuracy, and semantic event.
An integer called `timestamp` is not sufficient.

### Clock domains

Use both of these:

- `CLOCK_MONOTONIC_RAW` for durations inside the process. It is monotonic and is
  not stepped by NTP/PTP clock corrections.
- NIC PHC/realtime timestamps for kernel and hardware network events. Store the
  raw hardware value and the converted host value.

At startup and periodically, record clock-correlation samples containing
monotonic, realtime, TAI when available, and PHC time for every timestamping
NIC. Store the conversion uncertainty. Run `ptp4l`/`phc2sys` where sub-
millisecond cross-NIC or cross-host accuracy is required.

Never subtract timestamps from different clock domains without a recorded
conversion.

### Accuracy grades

| Grade | Ingress | Egress | Meaning |
|---|---|---|---|
| A | RX hardware timestamp | TX hardware timestamp | Local wire/NIC-to-NIC, subject to PHC conversion uncertainty |
| B | RX software timestamp in kernel | TX software/driver timestamp | Kernel-to-driver path; excludes some NIC residence |
| C | Timestamp immediately after receive API | Timestamp immediately before/after send API | Userspace approximation; includes scheduler uncertainty and excludes kernel queues |
| D | Periodic counters or inferred media timing | Periodic counters or inferred timing | Diagnostic hint only, not packet residence time |

Every report must show the grade and fallback reason.

## Linux Socket and NIC Timestamping Requirements

Linux exposes receive and transmit timestamps through `SO_TIMESTAMPING_NEW` and
ancillary data read with `recvmsg()`.

### Requested flags

Request the supported subset of:

```text
SOF_TIMESTAMPING_RX_HARDWARE
SOF_TIMESTAMPING_RX_SOFTWARE
SOF_TIMESTAMPING_TX_HARDWARE
SOF_TIMESTAMPING_TX_SOFTWARE
SOF_TIMESTAMPING_TX_SCHED
SOF_TIMESTAMPING_TX_ACK          # TCP diagnostic, not wire time
SOF_TIMESTAMPING_SOFTWARE
SOF_TIMESTAMPING_RAW_HARDWARE
SOF_TIMESTAMPING_OPT_ID
SOF_TIMESTAMPING_OPT_ID_TCP      # always pair with OPT_ID on TCP
SOF_TIMESTAMPING_OPT_TSONLY
SOF_TIMESTAMPING_OPT_STATS
SOF_TIMESTAMPING_OPT_PKTINFO
SOF_TIMESTAMPING_OPT_RX_FILTER
```

Do not fail the stream when an optional flag is unsupported. Negotiate once,
read back the active configuration, record the fallback, and continue.

Receive timestamps arrive in `SCM_TIMESTAMPING` control messages on normal
`recvmsg()` calls. Hardware receive timestamps are in the raw-hardware slot.
`SOF_TIMESTAMPING_OPT_PKTINFO`, where supported, adds the real ingress ifindex
and layer-2 packet length.

Transmit timestamps arrive asynchronously through the socket error queue and
must be drained with `recvmsg(MSG_ERRQUEUE)`. Record:

- userspace send start and completion;
- `TX_SCHED`, before qdisc;
- TX software/driver timestamp;
- TX hardware timestamp;
- TCP `TX_ACK`, clearly labeled as remote acknowledgment time rather than wire
  transmit time;
- timestamp ID or TCP byte offset;
- optional transport statistics from `SCM_TIMESTAMPING_OPT_STATS`;
- egress ifindex, when available.

Hardware timestamping must also be enabled on each interface through the
ethtool timestamp configuration API, with legacy `SIOCSHWTSTAMP` as a fallback.
The process should report:

- interface name and ifindex;
- driver and firmware versions;
- requested and effective RX filter/TX mode;
- timestamp provider;
- PHC device and clock-correlation error;
- offload state: GRO/LRO/GSO/TSO, checksum, VLAN, and qdisc;
- MTU, link speed, duplex, carrier, RX/TX queue count;
- whether the path crosses a bridge, bond, VLAN, veth, namespace, or virtual
  NIC.

Changing interface-wide hardware timestamp configuration requires administrative
privilege and may affect other applications. Production setup should configure
it explicitly at service installation, while the Restream process only verifies
and reports it unless granted a narrowly scoped helper.

## Protocol-Specific Capture

### RTMP over TCP

Tokio `TcpStream::read()` and `write_all()` discard ancillary timestamp data.
RTMP needs a timestamp-aware socket wrapper built on nonblocking `recvmsg()` and
`sendmsg()`, integrated with Tokio readiness, plus a task that drains
`MSG_ERRQUEUE`.

TCP is a byte stream. One receive timestamp belongs to a delivered byte range,
not necessarily one RTMP message or one media packet. Reads, TCP segments, RTMP
chunks, and media messages can split or coalesce independently.

Maintain:

- monotonically increasing ingress and egress TCP byte offsets;
- a timestamped range for every `recvmsg()` result;
- RTMP parser provenance mapping output messages to input byte ranges;
- serialized byte ranges for every output media packet;
- `SOF_TIMESTAMPING_OPT_ID_TCP` transmit IDs based on the last byte written.

If the RTMP library cannot expose consumed-byte provenance, use an arrival
envelope:

- `first_rx_ns`: earliest receive range contributing bytes;
- `last_rx_ns`: latest receive range completing the message;
- `rx_uncertainty_ns`: envelope width.

This is honest and still identifies TCP head-of-line stalls. “Exact packet
arrival” should only be claimed after parser byte-range lineage exists.

Also capture `TCP_INFO`, `SO_INCOMING_CPU`, `SO_INCOMING_NAPI_ID`, receive/send
queue depth (`SIOCINQ`/`SIOCOUTQ`), `SO_RCVBUF`/`SO_SNDBUF`, and where available:
RTT/variance, retransmits, out-of-order packets, congestion window, unacked
bytes, pacing/delivery rate, receiver-window limitation, receive-buffer
occupancy, and last-receive age. Prefer direct socket queries over shelling out
to `ss`; retain `ss` only as a compatibility fallback.

For every socket, inventory and retain:

- socket cookie, network namespace, address family, protocol, local/remote
  tuple, bound device, mark, priority, and current state;
- accepted/listening socket relationship and pipeline/output owner;
- effective MSS, PMTU, TCP window scaling, ECN state, congestion algorithm,
  keepalive, Nagle/cork state, and busy-poll settings;
- bytes/segments sent, received, retransmitted, delivered, not-yet-sent,
  unacked, reordered, lost, and SACKed where the kernel exposes them;
- minimum/current RTT, delivery rate, pacing rate, app-limited state, busy time,
  receiver-window-limited time, and send-buffer-limited time;
- socket receive overflow (`SO_RXQ_OVFL`), incoming CPU/NAPI ID, RX/TX queue
  bytes, and effective buffer limits;
- ingress/egress ifindex and packet information from `IP_PKTINFO`/
  `IPV6_PKTINFO` or timestamp packet-info ancillary data;
- asynchronous network errors from `IP_RECVERR`/`IPV6_RECVERR`, including
  PMTU and ICMP failures.

Fields vary by kernel, protocol, and privilege. Reports should include a
capability bitmap and “unsupported”/“permission denied” states instead of
inventing zeros.

### SRT

The current `srt_recv()` timestamp is after libsrt has received UDP datagrams,
performed loss recovery/reordering, waited for the configured latency, and
released application payload. It is an important “SRT delivery to application”
event, but it is not an ingress NIC timestamp.

Collect all available libsrt statistics at a regular cadence and at diagnostic
start/end, including RTT, negotiated latency, receive/send rate, link capacity,
flight size, flow window, NAKs, loss, retransmit, drop, undecrypt, send/receive
buffer occupancy, and packet filter/FEC fields when enabled.

To correlate SRT with NIC events, choose one of:

1. Instrument libsrt’s UDP send/receive path to preserve kernel ancillary
   timestamps and SRT packet sequence numbers. This gives the best lineage.
2. Use an eBPF/tracepoint collector keyed by network namespace, socket cookie,
   five-tuple, and packet identifiers. This is less invasive but correlation
   through retransmit/reassembly is more complex. Treat it as kernel timing
   unless the selected hook can also read a valid skb hardware timestamp.
3. Capture only `srt_recv()`/`srt_send()` application events and label the
   result Grade C. This still exposes SRT recovery/latency residence when
   compared with instrumented libsrt internals.

Do not describe `srt_send()` return time as egress NIC transmit time.

## Packet, Frame, and Lineage Data Model

Extend the canonical packet contract. The hot-path object should keep compact
IDs and timestamps; verbose events belong in a sampled trace store.

```rust
struct MediaPacket {
    packet_id: u64,
    root_packet_id: u64,
    branch_id: u32,
    media_type: MediaType,
    track_index: u32,
    codec: CodecId,
    time_base: Rational,
    pts: Option<i64>,
    dts: Option<i64>,
    duration: Option<i64>,
    is_keyframe: bool,
    payload: Bytes,
    timing: PacketTiming,
}
```

`PacketTiming` should include:

```text
ingress_hw_ns / ingress_sw_ns / ingress_app_ns
protocol_message_complete_ns
demux_start_ns / demux_complete_ns
source_ring_push_ns
reader_pull_ns per branch
decode_start_ns / decode_complete_ns
frame_ready_ns
filter_start_ns / filter_complete_ns
encode_start_ns / encode_complete_ns
mux_start_ns / mux_complete_ns
egress_send_start_ns / egress_send_complete_ns
tx_sched_ns / tx_sw_ns / tx_hw_ns / tx_ack_ns
clock_domain, accuracy_grade, uncertainty_ns
```

For transcoding, input packets do not map one-to-one to output packets. Create
frame and lineage records:

```text
InputPacket(s) -> DecodedFrame -> FilteredFrame -> EncodedPacket(s)
```

Each decoded frame should retain codec PTS, best-effort timestamp, packet DTS,
duration, picture type, keyframe flag, repeat/interlace/corruption flags, decode
error flags, width/height/pixel format, and stage timestamps. Encoded packets
must reference the frame ID and contributing root packet IDs or a compact
lineage range.

Sequence headers, metadata/config packets, MPEG-TS PAT/PMT/PCR, retransmitted
network packets, and ordinary media packets must be distinguishable.

## Stage Measurements

For every pipeline, branch, media type, and track, calculate histograms and
rates for:

| Measurement | Calculation |
|---|---|
| Kernel receive to app | `ingress_app - ingress_hw/sw` |
| Protocol assembly | `protocol_complete - ingress_app/envelope_start` |
| SRT recovery/latency | libsrt release minus UDP receive, when instrumented |
| Demux | `demux_complete - demux_start` |
| Source ring wait | `reader_pull - source_ring_push` |
| Decode | `decode_complete - decode_start` |
| Filter | `filter_complete - filter_start` |
| Encode | `encode_complete - encode_start` |
| Output ring wait | output reader pull minus encoded packet push |
| Mux/serialization | `mux_complete - mux_start` |
| Userspace send | send completion minus send start |
| Kernel/qdisc | `tx_sw/hw - tx_sched` |
| Engine residence | egress send start minus ingress app/protocol complete |
| Local NIC-to-NIC | `tx_hw - rx_hw` |
| Remote ACK | `tx_ack - send_start`, TCP only |

Report p50, p90, p95, p99, maximum, count, and dropped/unsampled count. Preserve
the slowest exemplars with packet ID and complete stage waterfall.

Media-time latency and processing-time latency are different:

- PTS/DTS describe the media timeline.
- Monotonic stage timestamps describe residence and work.
- Comparing them can reveal whether Restream is gaining or losing real-time,
  but they must not be substituted for one another.

## Diagnostic Checks to Offer

### Transport and socket

- Hardware/software timestamp capability and active accuracy grade.
- Per-socket RX/TX queue occupancy and high-water marks.
- TCP receive throughput, RTT/variance, receive RTT, last-receive age,
  out-of-order packets, receive-window, and receive-buffer saturation trends.
- SRT RTT, latency buffer, loss, retransmit, drop, NAK, undecrypt, flow window,
  flight size, buffer pressure, and rate-vs-link-capacity trends.
- Receive stalls followed by bursts, using kernel/NIC time rather than rewritten
  media PTS.
- Interface errors/drops, qdisc drops/backlog, NIC ring drops, softnet drops,
  and socket overflow counters.
- Effective socket buffer sizes; Linux doubles configured `SO_RCVBUF` and
  `SO_SNDBUF`, so show requested and effective values.

### Packet and container

- Packet counts, bytes, bitrate, size distribution, and packet-rate by track.
- PTS/DTS monotonicity, duplicates, discontinuities, missing timestamps, and
  invalid `PTS < DTS` cases with codec-aware exceptions.
- A/V interleave depth in both packet count and media time.
- MPEG-TS continuity-counter errors, PAT/PMT cadence, PCR interval/jitter,
  program/PID changes, sync loss, and transport-error indicators.
- RTMP chunk/message assembly stalls and sequence-header changes.
- Startup time to first byte, first config, first audio packet, first video
  packet, first keyframe, first decoded frame, and first egress transmit.

### Frame and codec

- Decode success/error counts and FFmpeg error flags.
- Actual decoded FPS and frame-duration jitter.
- GOP length by frames and media time, keyframe cadence, open/closed GOP where
  detectable, I/P/B distribution, and reference-frame failures.
- Duplicate, dropped, repeated, corrupt, and reordered frames.
- A/V clock offset and drift using decoded frame/sample timelines.
- Codec/profile/level, pixel/sample format, dimensions, sample rate, channels,
  channel layout, color metadata, HDR metadata, and mid-stream changes.
- Largest encoded frame and burst bitrate over short windows.

### Engine and branch

- Per-reader ring depth, oldest packet age, overflow/fast-forward count, wakeup
  delay, and packet drop reason.
- `MemoryQueue` bytes, oldest-byte age, read/write rate, blocked time, and
  high-water mark.
- Per-stage service time, queue time, throughput, and utilization.
- Input packet to decoded frame to encoded packet lineage.
- Per-output divergence: which output branch adds delay or drops packets.
- Event-loop lag, worker-thread CPU, scheduling delay, allocator pressure, and
  process RSS.
- HLS segment duration, creation latency, playlist publication delay, keyframe
  alignment, and store eviction.
- Recording write latency, filesystem queueing, free space, and fsync/write
  errors.

### End-to-end verdict

Return:

- overall health: healthy, strained, degraded, or broken;
- timestamp accuracy grade;
- local NIC-to-NIC p50/p95/p99/max;
- engine-only p50/p95/p99/max;
- top three latency-contributing stages;
- transport, media timeline, codec/frame, and output-branch findings;
- exact packet/frame IDs and timestamps for every finding;
- confidence and missing-data reasons;
- concrete remediation.

## Collection Architecture

Use two telemetry paths.

### Always-on aggregate path

Keep low-overhead counters, gauges, and HDR-style histograms per pipeline and
output. Avoid packet IDs, remote addresses, and stream keys as Prometheus
labels. Export stable IDs and resolve names in the UI.

### Bounded trace path

Maintain a fixed-size per-pipeline flight recorder containing sampled packet,
frame, socket, and stage events. Recommended modes:

- baseline sampling, for example 1 in 1,000 packets;
- always sample keyframes, errors, discontinuities, overflows, and packets above
  a latency threshold;
- temporarily increase sampling during an operator diagnostic;
- retain pre-trigger and post-trigger windows;
- cap bytes and event count so diagnostics cannot destabilize media processing.

The downloadable report should contain:

```text
manifest.json             versions, config, clocks, capability/fallback data
summary.json              verdicts and aggregate measurements
stage-histograms.json     per-stage percentiles
packet-events.ndjson      sampled packet lineage
frame-events.ndjson       sampled frame lineage
socket-events.ndjson      RX/TX timestamps and transport snapshots
slow-exemplars.ndjson     complete waterfalls for worst packets
warnings.log              decoder, protocol, socket, and engine warnings
```

Payload bytes should be omitted by default. Provide an explicit, bounded,
security-sensitive packet capture option when payload inspection is necessary.

## Implementation Plan

### Phase 0: make existing diagnostics truthful

1. Fix the `PublisherQuality`/`src/diag.rs` field mismatch.
2. Remove the stale frontend ffprobe timing banner and raw-ffprobe claims, or
   restore a correctly labeled compatibility probe.
3. Wire direct per-socket TCP and libsrt stats into active ingest/egress state.
4. Replace ring “fill” with per-reader lag, overflow count, and packet age.
5. Preserve timestamps and keyframe metadata through transcoder outputs.
6. Add `MemoryQueue` depth, age, blocked-time, and high-water metrics.

### Phase 1: application-level packet timing

1. Add packet IDs, branch IDs, time base, duration, and compact timing fields.
2. Instrument protocol complete, demux, ring push/pull, mux, and send-call
   boundaries with `CLOCK_MONOTONIC_RAW`.
3. Add per-stage histograms and a bounded flight recorder.
4. Port the old ffprobe packet/GOP/startup/drift analyses to native packet and
   frame records.

This produces Grade C timing and immediately identifies in-engine queueing.

### Phase 2: RTMP kernel/NIC timestamps

1. Introduce `TimestampedTcpStream` using nonblocking `recvmsg()`/`sendmsg()`.
2. Enable and verify `SO_TIMESTAMPING_NEW`.
3. Drain the transmit error queue without blocking the media task.
4. Track byte ranges and transmit IDs.
5. Add RTMP parser provenance or arrival envelopes.
6. Capture direct `TCP_INFO` and queue depth snapshots.

### Phase 3: SRT correlation

1. Bind the complete `srt_bistats()` structure and poll without clearing unless
   explicitly requested.
2. Add application delivery/send timestamps around `srt_recv()`/`srt_send()`.
3. Instrument libsrt UDP I/O or add an eBPF collector for NIC/kernel timing.
4. Correlate SRT sequence numbers, retransmits, recovery delay, and released
   media payload.

### Phase 4: decoded frames and transcode lineage

1. Replace opaque byte-only transcoder queues with packet-aware input/output
   adapters where possible.
2. Instrument FFmpeg send/receive packet/frame calls.
3. Preserve frame IDs through filters and encoders.
4. Add decode errors, frame types, A/V drift, drops, duplicates, and actual FPS.

### Phase 5: operator product

1. Add a latency waterfall and percentile comparison by output.
2. Add a capability panel showing NIC/driver/clock accuracy and fallbacks.
3. Generate the structured diagnostic bundle.
4. Add alerts based on sustained percentile and error-rate thresholds rather
   than one-off maxima.
5. Add a reproducible test harness using `tc netem`, constrained socket buffers,
   CPU pressure, packet loss/reordering, and known timestamp discontinuities.

## Acceptance Criteria

The design is complete when a diagnostic can:

- show the active timestamp source and accuracy grade for each ingress/egress;
- identify the ingress and egress interface for a sampled packet;
- show local RX hardware to TX hardware residence when supported;
- show a stage waterfall whose components reconcile with total residence within
  clock-conversion uncertainty;
- identify the output branch responsible for excess latency;
- preserve original PTS/DTS while independently measuring wall-clock transport;
- trace transcoded output packets back to decoded frames and source packets;
- detect all analyses previously offered through ffprobe without creating a
  second media reader;
- explain missing measurements rather than silently returning zero;
- keep diagnostic overhead bounded and measurable.

## Primary References

- Linux kernel timestamping documentation:
  <https://docs.kernel.org/networking/timestamping.html>
- Linux ethtool netlink documentation:
  <https://docs.kernel.org/networking/ethtool-netlink.html>
