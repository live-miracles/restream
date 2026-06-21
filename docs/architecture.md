# Architecture

Restream is a Rust application that owns the control plane and the production
media path. The previous Node.js/MediaMTX runtime is archived under `old/`.

## System Shape

```text
Publisher
  | RTMP or SRT
  v
+---------------------------- restream -----------------------------+
| native ingest -> source RingBuffer                                |
|                     |                                             |
|                     +-> RTMP egress                               |
|                     +-> SRT MPEG-TS egress                        |
|                     +-> HLS segmenter scaffold                    |
|                     +-> Matroska recorder scaffold                |
|                     `-> transform scaffold -> RingBuffer -> egress|
|                                                                   |
| Axum dashboard/API -> SQLite                                      |
| reconciler (1 second) -> output and recording lifecycle           |
+-------------------------------------------------------------------+
```

MediaMTX may be used as an independent test sink, but it is not a production
dependency.

## Concurrency

Tokio tasks handle:

- Axum HTTP
- RTMP connections
- SRT connection coordination
- output reconciliation
- egress lifecycle

Dedicated OS threads handle blocking/native FFmpeg work:

- SRT demux
- MPEG-TS mux
- HLS mux/segment splitting
- Matroska recording
- transcoding

Native worker entry points are wrapped with `catch_unwind` where the code needs
to contain FFmpeg failures.

## Packet Flow

```text
RTMP:
socket -> rml_rtmp -> FLV audio/video payload -> MediaPacket -> RingBuffer

SRT:
libsrt socket -> MPEG-TS bytes -> MemoryQueue -> FFmpeg demux
             -> MediaPacket -> RingBuffer

egress:
RingBuffer Reader -> protocol/container packaging -> socket or local store
```

`MediaPacket` carries media type, track index, PTS, DTS, keyframe state, and a
reference-counted payload.

## Ring Buffer

Each pipeline uses a 4096-slot single-producer/multi-consumer buffer.
`ArcSwapOption` slots permit lock-free reader loads, and payloads are shared
through `Arc`/`Bytes`.

Single-producer is an architectural assumption, not currently enforced. A
second independent publisher for the same pipeline can write concurrently and
invalidate it. A proper SRT bonded publisher is different: libsrt presents the
bond as one accepted group ID and one application receive path.

When a reader falls behind by at least the full capacity, it fast-forwards to
the latest known keyframe. The code does not yet expose per-reader lag,
overflow, or queue-residency metrics; current diagnostics must not describe the
write count as live occupancy.

The 4096-slot value is sized as a working target for high-rate streams, not a
certified number of seconds. Actual depth depends on packetization, frame rate,
audio-track count, and encoder behavior.

## Shared Processing Stages

Output encoding strings are split into two stage identities:

1. video preset, shared across outputs using the same transform;
2. audio routing, keyed by both routing mode and upstream video stage.

Example:

```text
source ring
  +-> video:720p -> audio:atrack:0:from:720p -> output A
  |             `-> audio:atrack:1:from:720p -> output B
  `-> source --------------------------------> output C
```

The stage cache is intended to prevent one encoder per destination. The current
transcoder creates output encoder parameters but then stream-copies compressed
input packets; it does not run a decode/filter/encode loop. Resolution,
crop/rotate, and H.265-to-H.264 presets therefore remain non-functional
transforms even though their stages appear in the graph. Stage lifecycle cleanup
is also an area for further hardening.

Task “active” state is generally cancellation-token presence, not a worker
health signal. A native worker thread can fail while its feeder task/token
remains active.

## Protocol and Codec Boundaries

| Area | Current state |
|---|---|
| RTMP H.264/AAC | Native ingest/play/egress implemented; video uses DTS and carries the FLV composition offset, with full B-frame round-trip still an end-to-end gate |
| SRT H.264/AAC | Native ingest/read/egress code exists with MPEG-TS remux; prior local evidence, current matrix rerun required |
| SRT H.265 | Codec mapping implemented; full matrix remains an end-to-end gate |
| RTMP H.265 | Enhanced RTMP is not implemented; an H.264 stage is selected, but actual decode/encode conversion is incomplete |
| Multi-track audio | SRT ingest preserves audio track indices |
| Audio remap/downmix | Stream selection only; channel-level filtering is open |
| HLS pull routes/store | Implemented; live segment generation is blocked by the packet/container contract |
| HLS upload | Not implemented |
| RTMPS output | Parser support exists, but reconciler dispatch is not wired |

## SRT Transport

The listener and single-link egress call the high-bitrate option helper.
Accepted sockets may inherit listener settings through libsrt, but the code does
not explicitly apply or verify them. The bonded egress group is created from a
parsed `bond=` list but does not call the helper after group creation.

The listener requests `SRTO_GROUPCONNECT`. With a libsrt build configured using
`ENABLE_BONDING=ON`, `srt_accept` returns the group ID when the first link
connects; later members attach in the background. The application reads the
logical group through one `srt_recvmsg2` loop, so only one ingest endpoint and
one ring producer are needed. Startup warns when the linked libsrt rejects the
option; single-link ingest remains available in that case.

StreamID alone does not create a group. Two ordinary caller sockets using the
same StreamID are independent publishers and should be rejected as duplicates.

Linux listener monitoring reads `/proc/net/udp` for receive-queue occupancy and
kernel drops. Per-connection quality comes from `srt_bistats()`; accepted groups
also export member counts and connected/active/broken state from
`srt_group_data()`.

URL parsing, option constants, group-state summarization, and duplicate
publisher rejection have unit coverage. Separate-process loopback tests pass
for two-member broadcast and backup groups, including closing the primary
member and receiving the next message through the standby. Bonded egress and
the full H.265 matrix remain open.

## HLS and Recording

HLS segments are stored in memory in a ten-segment sliding window and served by
Axum. The store and playlist behavior are tested. The live feeder currently
concatenates `MediaPacket.payload` values and asks FFmpeg to detect an input
format; those payloads are raw codec/FLV media payloads, not a complete container
stream. Live HLS generation is therefore not considered correct yet. There is no
disk-backed HLS path and no HTTP upload worker.

Recordings use the Matroska muxer and are written under `media/`. Recordings
shorter than five seconds are removed automatically. Recording uses the same
packet-payload-to-`CustomInput` pattern and needs the same contract repair before
it is considered reliable.

## File Ingest Exception

Most media processing is linked in-process. Configured file ingest still spawns:

```text
ffmpeg -re ... -c copy -f flv rtmp://localhost:1935/live/<key>
```

The child is tracked by ingest ID and can be stopped through the API.

## State and Authentication

SQLite stores pipelines, outputs, jobs, logs, file-ingest definitions, metadata,
and sessions. The default password is created on first startup and stored as a
scrypt hash. Session cookies are `HttpOnly` and `SameSite=Strict`.

Deletion handlers cancel active output/ingest tasks before removing their
database rows, and file-ingest deletion kills its tracked child. Reaping
naturally exited file-ingest children remains open.

## Key Files

| File | Responsibility |
|---|---|
| `src/lib.rs` | App composition and reconciliation |
| `src/api.rs` | Router, auth, REST/SSE handlers, embedded assets |
| `src/db.rs` | SQLite schema and queries |
| `src/diag.rs` | Native diagnostics |
| `src/media/engine.rs` | Active state and health/graph snapshots |
| `src/media/ring_buffer.rs` | Packet fan-out |
| `src/media/avio.rs` | In-memory FFmpeg AVIO |
| `src/media/rtmp.rs` | RTMP server/client |
| `src/media/srt.rs` | SRT server/client, MPEG-TS, bonding, stats |
| `src/media/tcp_stats.rs` | Linux RTMP receiver socket metrics |
| `src/media/hls.rs` | In-memory HLS |
| `src/media/recording.rs` | Matroska recording |
| `src/media/transcoder.rs` | Shared video/audio stages |
