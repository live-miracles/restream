# Media Pipeline

This document covers the ingest-to-egress media pipeline: current shape,
protocol/codec boundaries, stage sharing, buffer sizing, and correctness
requirements.

For the performance optimization plan and benchmark results, see
[High-Performance Data Path](high-performance-data-path.md).

## Current Shape

```mermaid
flowchart TD
    subgraph INGESTS["Ingest"]
        RI["RTMP ingest\nFLV payload"]
        SI["SRT ingest\nMPEG-TS"]
    end

    subgraph DEMUX["Ingest demux (inline, async)"]
        RD["RTMP parser\nFlv packets"]
        SD["TsDemuxer\nRaw packets"]
    end

    SR[("source_ring\nSPMC RingBuffer 4096\nMediaPacket · Flv ∣ Raw")]

    subgraph PASSTHROUGH["Passthrough — encoding = source"]
        direction TB
        PT1["Flv · dest=RTMP\nBytes::clone → FLV tag\n→ RTMP socket"]
        PT2["Flv · dest=SRT/HLS\nvideo_for_ts strip hdr\nTsMuxer → MPEG-TS\n→ SRT socket / HLS store"]
        PT3["Raw · dest=RTMP\nbuild_avcc_seq_hdr\nvideo_for_rtmp → FLV tag\n→ RTMP socket"]
        PT4["Raw · dest=SRT/HLS\nTsMuxer → MPEG-TS\n→ SRT socket / HLS store"]
    end

    subgraph TRANSCODE["Transcoded — encoding = 720p (shared once per preset per pipeline)"]
        direction TB
        TIN["video_for_ts\nFlv: strip hdr  ∣  Raw: inject SPS/PPS\nTsMuxer → MPEG-TS"]
        FSTDIN[/"FFmpeg stdin\npipe"/]
        FF(["FFmpeg subprocess\nscale=1280:720 · libx264\n─────────────────\nstdin → stdout"])
        FSTDOUT[/"FFmpeg stdout\npipe"/]
        TDEM["TsDemuxer\nRaw packets"]
        OR[("output_ring\nSPMC RingBuffer 4096\nMediaPacket · Raw")]
        TOUT_R["dest=RTMP\nvideo_for_rtmp → FLV tag\n→ RTMP socket"]
        TOUT_S["dest=SRT/HLS\nTsMuxer → MPEG-TS\n→ SRT socket / HLS store"]
        TIN --> FSTDIN --> FF --> FSTDOUT --> TDEM --> OR
        OR --> TOUT_R
        OR --> TOUT_S
    end

    RI --> RD --> SR
    SI --> SD --> SR

    SR -->|"Flv · source · RTMP"| PT1
    SR -->|"Flv · source · SRT/HLS"| PT2
    SR -->|"Raw · source · RTMP"| PT3
    SR -->|"Raw · source · SRT/HLS"| PT4
    SR -->|"any format · 720p"| TIN
```

## Transcoder Stages

Every non-passthrough encoding creates a **shared stage**: one process per
`(pipeline_id, preset)` pair regardless of how many outputs use that preset.

### Stage graph

```
source_ring
    │  [if needs_h264_transcode]  H.265→H.264 stage  (get_or_create_h264_transcoder)
    │  [if needs_video_transcode] video preset stage  (get_or_create_transcoder)
    │  [if audio routing suffix]  audio filter stage  (get_or_create_transcoder)
    ▼
ring_buf  ◄── all egresses for this (pipeline, encoding) read here
```

### Passthrough rule

`source` and `custom` encodings **never** enter any transcoder stage.
The egress reads directly from `source_ring`. This is enforced in the
reconciler (`src/lib.rs`) before any `get_or_create_transcoder` call.

### Stage-key naming

| Stage | Key format | Example |
|---|---|---|
| Video preset | `video:<preset>` | `video:720p` |
| H.265→H.264 | `hevc_to_h264` | `hevc_to_h264` |
| Audio filter | `audio:<op>:from:<video_key>` | `audio:atrack:0:from:720p` |

The video-preset key is shared across all compound encodings with the same
video part (e.g. `720p`, `720p+atrack:0`, `720p+remap:0:1` all use key
`video:720p`). The audio key embeds the upstream video key to prevent
cross-contamination between presets.

### External transcoder (default)

```
source_ring
    │  (Reader + TsMuxer → MPEG-TS bytes)
    ▼
FFmpeg stdin ──► [scale + libx264 + …] ──► FFmpeg stdout (MPEG-TS)
                                                   │
                                       TsDemuxer → MediaPackets (Raw)
                                                   │
                                             output_ring ◄── shared
                                                   │
                              ┌────────────────────┼──────────────┐
                           RTMP-out1           SRT-out1       HLS-out1
```

One `ffmpeg` subprocess per `(pipeline, preset)`. FFmpeg reads MPEG-TS from
stdin and writes transcoded MPEG-TS to `pipe:1` (stdout). A Tokio task reads
stdout, runs it through `TsDemuxer`, and pushes the resulting `MediaPacket`s
into `output_ring`.

This is the **default** backend. It is robust because FFmpeg errors are
isolated to the subprocess and logged to stderr; a crash restarts cleanly on
the next reconciler cycle.

### Internal transcoder (opt-in)

Set `RESTREAM_USE_INTERNAL_TRANSCODER=1` to use the in-process libavcodec path
(`src/media/transcoder.rs`). The data flow is identical — the same
`source_ring → output_ring` contract holds — but uses `MemoryQueue`/`avio`
callbacks instead of a subprocess pipe.

Prefer the external backend until the Rust FFI layer (`ffmpeg-next`) hardens.

### Muxing stages summary

| Stage | Role |
|---|---|
| SRT Ingest | `TsDemuxer` — demux MPEG-TS into `MediaPacket`s (inline async) |
| External transcoder | subprocess FFmpeg stdin→stdout; `TsMuxer` writes stdin, `TsDemuxer` reads stdout |
| Internal transcoder | in-process FFmpeg via `MemoryQueue`+`avio`; `TsMuxer` feeds input, output packets pushed directly to ring |
| SRT Egress | `TsMuxer` remux to MPEG-TS (inline in async feed loop) |
| HLS | `TsMuxer` remux to MPEG-TS, then segment in memory (inline async) |
| Recording | FFmpeg remux to MKV file (OS thread) |



## Protocol and Codec Boundaries

| Area | Current state |
|---|---|
| RTMP H.264/AAC | Native ingest/play/egress; video uses DTS and carries FLV composition offset. B-frame round-trip still an E2E gate |
| SRT H.264/AAC | Native ingest/read/egress with MPEG-TS demux/remux |
| SRT H.265 | Codec mapping implemented; full E2E matrix remains a gate |
| RTMP H.265 | Enhanced RTMP is not implemented; an H.264 stage is selected but actual decode/encode is incomplete |
| Multi-track audio | SRT ingest preserves audio track indices |
| Audio remap/downmix | Stream selection only; channel-level filtering is open |
| HLS pull routes/store | Implemented and tested; live segment generation uses native TsMuxer |
| HLS upload | Not implemented; HTTP/HTTPS output URL starts local segmenter and ignores destination |
| RTMPS output | `rtmps://` URLs accepted by API; reconciler routes to external transcoder (FFmpeg) which handles TLS natively. Source-passthrough also uses FFmpeg path. |

## Resolution Presets

The external transcoder stage applies `scale=WxH` and re-encodes with `libx264
-preset veryfast`. The internal transcoder (when enabled) uses the same preset
table via `run_ffmpeg_transcoder_stage`.

| Preset | Resolution | Scale filter |
|---|---|---|
| `source` / `custom` | passthrough | none — never enters transcoder |
| `480p` | 854×480 | `scale=854:480` |
| `720p` | 1280×720 | `scale=1280:720` |
| `1080p` | 1920×1080 | `scale=1920:1080` |
| `h264` | source resolution | H.265→H.264 only (`get_or_create_h264_transcoder`) |


## H.265 Egress Policy

Standard RTMP (non-Enhanced) does not carry H.265. The reconciler enforces:

| Egress protocol | H.265 input | Behavior |
|---|---|---|
| RTMP | H.265 source | Auto-inserts intended `h264` stage; conversion incomplete |
| RTMP | H.265 + preset | Intended H.264 transform; incomplete |
| SRT | H.265 source | Passthrough (MPEG-TS carries HEVC natively) |
| SRT | H.265 + preset | Intended HEVC transform; incomplete |
| HLS | H.265 source | Intended passthrough |

Enhanced RTMP/HEVC packetization is not implemented.

## Current Protocol Matrix

| Ingest | RTMP egress | SRT egress | HLS preview | Recording |
|---|---|---|---|---|
| RTMP H.264 | Basic interop; B-frame timestamp gate | Implemented; full matrix gate | Store/routes exist; live TsMuxer | Mux path exists; contract broken |
| RTMP H.265 | Not supported without Enhanced RTMP | Not assumed | Not assumed | Not assumed |
| SRT H.264 | Not protocol-correct (raw payload as FLV) | Locally validated | Store/routes exist; live TsMuxer | Mux path exists; contract broken |
| SRT H.265 | H.264 conversion incomplete | Passthrough implemented; E2E gate | Store/routes exist; live TsMuxer | Mux path exists; contract broken |
| File | RTMP-shaped via child FFmpeg | Implemented for compatible FLV codecs | Live TsMuxer | Contract broken |

## Minimum Work Per Consumer

All consumers that process packets from a ring buffer avoid per-packet heap
allocation by using the zero-allocation `_into` variants:

| Consumer | Video conversion | Audio conversion | Burst size |
|---|---|---|---|
| RTMP egress | `video_for_rtmp_into` | `audio_for_rtmp_into` | `pull_burst` 32 |
| SRT egress | `video_for_ts_into` | `audio_for_ts_into` | `pull_burst` 32 |
| SRT play subscriber | `video_for_ts_into` | `audio_for_ts_into` | `pull_burst` 32 |
| HLS segmenter | `video_for_ts_into` | `audio_for_ts_into` | `pull_burst` 32 |
| Recording | `video_for_ts_into` | `audio_for_ts_into` | `pull_burst` 32 |
| Transcoder feed | `video_for_ts` (Raw→Raw passthrough) | `audio_for_ts` | `pull_burst` 32 |

Scratch buffers (`video_conv_buf`, `audio_conv_buf`) are allocated once at
consumer startup and reused across packets. For `PayloadFormat::Raw` video, the
borrowed payload slice is returned directly (zero copy).

## What Is Shared When Multiple Outputs Use the Same Encoding

Stage sharing is keyed by `(pipeline_id, stage_key)`:

```
2 outputs: encoding="720p"
  → get_or_create_transcoder("720p")  — returns same Arc<RingBuffer>
  → 1 transcoder subprocess
  → 2 independent RTMP egress tasks, each read from the shared ring
  → per-packet codec work (video_for_rtmp_into) is done independently per egress
```

The per-packet format conversion (AVCC wrap, ADTS strip) is NOT shared between
egress tasks. This is intentional: sharing would require synchronization and
outweigh the ~700 ns per frame conversion cost. What IS shared is the far more
expensive encode stage (CPU-bound, seconds of latency). This invariant is
covered by `same_encoding_outputs_share_one_transcoder_stage` in engine tests.

### Resource Sharing Footprint (Verified June 23, 2026)

Bitrate scaling and load tests of the pipeline configurations at 1.5M, 4.0M, and 8.0M verified the following sharing footprints:
* **External Transcoder Subprocess (Shared):** The CPU-bound H.264 transcoding runs in an external `ffmpeg` subprocess. The memory footprint of this child process remains fixed at **~422 MB to 431 MB** regardless of ingest bitrate (from 1.5M to 8.0M), as frames and scale filters are allocated statically on startup. Only one subprocess is spawned per unique `(pipeline_id, preset)`.
* **In-Process Transcoding (Shared):** In-process video transcoding (such as H.265 `hevc_to_h264` conversion) runs inside the Restream parent process using FFmpeg C-FFI bindings. This is shared across all downstream outputs requesting the same preset. It scales CPU usage (consuming **67% to 83%** of a core) and adds a **~130 MB to 180 MB** RSS overhead directly to the parent process.
* **Egress Senders (Not Shared):** Each downstream egress stream (RTMP/SRT) runs as an independent Tokio task. Each task does its own lightweight packet formatting (e.g. `video_for_rtmp_into` or `video_for_ts_into`) and network socket writes. The resource overhead per output is extremely lightweight, scaling at **~350 KB to 1 MB** RSS delta per output with negligible CPU usage.

## Audio Stage Cache

Output reconciliation splits compound encodings into a video stage and an audio
stage. Audio stages are keyed by the upstream stage identity as well as the
audio operation (e.g. `audio:atrack:0:from:video:720p`), preventing outputs
using different presets from cross-contaminating.

## Buffer Sizing (4K 60fps Target)

| Component | Size | Constraint | Source |
|---|---|---|---|
| RingBuffer capacity | 4096 slots | ~24s at 170 pkt/s (4K60). Overflow fast-forwards to most recent keyframe | `engine.rs` |
| AVIO buffer | 32 KB | FFmpeg internal read/write chunk | `avio.rs` |
| MemoryQueue | Unbounded `VecDeque<u8>` | Backpressure is structural: consumer blocks on `read()` | `avio.rs` |
| HLS segment accumulator | 8 MB initial | 4K60 H.264 segment at 6s can reach 12 MB; grows if needed | `hls.rs` |
| HLS MAX_SEGMENTS | 10 | ~60s sliding window. 10 × 8 MB = 80 MB worst case per pipeline at 4K | `hls.rs` |
| HLS TARGET_DURATION | 6s | MIN_SEGMENT (1s) prevents micro-segments from keyframe bursts | `hls.rs` |
| RTMP TCP SO_RCVBUF/SO_SNDBUF | 8 MB | Applied to accepted ingest sockets | `rtmp.rs` |
| SRT SRTO_LATENCY | 250 ms | Dejitter + retransmit window. At 50 Mbps = 1.56 MB in flight | `srt.rs` |
| SRT SRTO_LOSSMAXTTL | 256 packets | Reorder tolerance. At 50 Mbps/1316 B ≈ 54 ms | `srt.rs` |
| SRT UDP buffers | 8 MB | Kernel SO_RCVBUF/SNDBUF. Requires `rmem_max`/`wmem_max` ≥ 8 MB | `srt.rs` |
| SRT internal buffers | 12 MB | libsrt retransmission/reordering. ≥ latency × bitrate × (1+loss) | `srt.rs` |
| SRT SRTO_FC | 32768 packets | Flow control window. 32768 × 1316 B ≈ 43 MB window | `srt.rs` |
| SRT SRTO_MAXBW | unlimited | Auto-detect bandwidth from input rate | `srt.rs` |
| SRT recv buffer | 1316 bytes (single) / 2048 bytes (group) | One SRT payload per receive | `srt.rs` |

Runtime verification: `srt_log_effective_opts` reads back values after
`srt_setsockopt` and warns if the kernel clamped UDP buffers.

## SRT Bonding

### Ingest

The SRT listener requests `SRTO_GROUPCONNECT=1`. A publisher-created bonded
connection is accepted as one logical group: the first member returns a group
ID from `srt_accept`, later members attach in the background, and one
`srt_recv(group_id)` loop feeds one demuxer/ring producer. `srt_group_data()`
reports member state through health/diagnostics.

StreamID alone does not create a group. Two independent sockets with matching
StreamIDs are rejected as duplicate publishers.

Requires libsrt compiled with `ENABLE_BONDING=ON`; startup warns and retains
single-link ingest otherwise. The static release build supplies a
bonding-enabled libsrt; development builds depend on the system library.

### Egress

Backup links via `bond=` URL parameter:

```text
srt://primary:10080?streamid=publish:live/key&bond=backup1:10080,backup2:10080
```

Creates an `SRT_GTYPE_BACKUP` group. Both single-connection and bonded egress groups now call `srt_set_highbitrate_opts(client_sock)` immediately after creation to prevent packet drops and buffer overflows under high bitrates.

## Protocol Correctness Requirements

### Probe with matching ingest protocol

Probing must use the same read protocol as the active ingest. Cross-protocol
probing can create false positives (e.g., probing SRT ingest through RTMP
requires additional packetization). The diagnostics endpoint rejects mismatched
probe protocols.

### SRT Stream ID normalization

The listener accepts these shapes:

```text
publish:live/<key>        publisher:<key>
read:live/<key>           play:<key>           subscriber:<key>
<key>
#!::r=live/<key>,m=publish
#!::r=live/<key>,m=request
```

Query parameters are stripped before database validation.

### Media streams only

Read endpoints must emit media payload only. The pipeline selects the first
video stream and preserves all audio tracks. Subtitles, private data, second
video PIDs, and unknown stream types are excluded. The MPEG-TS remuxer rejects
unknown codec metadata rather than guessing H.264/AAC.

### Timestamp semantics

RTMP video timestamps are decode timestamps. AVC/HEVC packets carry a signed
24-bit composition-time offset:

```text
DTS = RTMP timestamp
PTS = DTS + signed composition-time offset
```

Ingest stores both values correctly. RTMP play and egress use `packet.dts` as
the RTMP message timestamp for video (audio uses PTS). B-frame round-trip tests
remain desirable.

### H.265

H.265 must be tested explicitly and cannot be inferred from H.264 results.
SRT/MPEG-TS should preserve HEVC codec identity. RTMP H.265 requires Enhanced
RTMP handling. Until RTMP H.265 is proven end-to-end, diagnostics should prefer
SRT read/probe for SRT H.265 publishers.

## Stage Sharing Design

### Near-Term Model

Share expensive video work and carry all audio through each unique video
preset, then apply audio selection as a cheap late step:

```mermaid
flowchart LR
  SRC["source"]
  V720["720p video stage\nvideo + all audio"]
  V1080["1080p video stage\nvideo + all audio"]
  A720_0["select: atrack:0"]
  A720_01["select: atrack:0,1"]
  A1080_0["select: atrack:0"]
  O1["output A"]
  O2["output B"]
  O3["output C"]

  SRC --> V720
  SRC --> V1080
  V720 --> A720_0 --> O1
  V720 --> A720_01 --> O2
  V1080 --> A1080_0 --> O3
```

### Protocol Package Sharing

Outputs can share a packaging stage when pipeline, video preset, audio routing,
codec parameters, container settings, and timing policy all match. For SRT,
sharing final TS packets is straightforward. For RTMP, the shareable layer is
the media message/FLV payload; each connection wraps those for its own session.

### Target Architecture

```mermaid
flowchart LR
  CANON["canonical packets"]
  VIDEO["shared video stages"]
  AUDIO["late audio select"]
  PACK["shared packaging"]
  SEND["per-destination sender"]

  CANON --> VIDEO --> AUDIO --> PACK --> SEND
```

```text
normalize once → share video → carry all audio → select audio late
→ share packaging for identical final shapes → separate senders
```

### Recommended Implementation Order

1. Implement the decode/filter/encode packet loop.
2. Introduce explicit stage identifiers (`Source`, `VideoPreset`, `AudioSelect`,
   `Package`, `Sender`).
3. Carry all audio through each unique video preset, select late.
4. Add package-stage sharing for identical final media shapes.
5. Strengthen `MediaPacket` or introduce a canonical packet type with codec
   parameters, time bases, and payload framing.
6. Replace file-ingest child processes with in-process demux/remux.

## Code Gaps

These are tracked in [REWRITE-STATUS.md](../REWRITE-STATUS.md) as release
blockers or hardening work:

- **Transcoder**: configures encoder parameters but copies compressed packets;
  no decode/filter/encode loop.
- **Recording**: packet-payload-to-`CustomInput` contract needs repair.
- **HLS upload**: HTTP/HTTPS URLs start local segmenter and ignore destination.
- **Custom encoding**: API persists value; reconciler treats `custom` as
  passthrough.
- **RTMPS**: URL parser accepts it, but reconciler does not dispatch TLS egress.
- **SRT→RTMP egress**: raw demuxed payload forwarded as FLV media payload
  (protocol-incorrect).
- **File ingest**: list endpoint reports `running: false` placeholder; exited
  children are not reaped.
- **Ring buffer**: no per-reader lag, overflow, or queue-residency metrics
  exposed.
- **MemoryQueue**: no depth, high-water mark, or blocked time exposed.
