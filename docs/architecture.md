# Architecture

Restream is a Rust application that owns the control plane and the production
media path. The previous Node.js/MediaMTX runtime is archived under `old/`.
MediaMTX may be used as an independent test sink, but it is not a production
dependency.

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
|                     +-> HLS segmenter (inline TsMuxer)            |
|                     +-> MPEG-TS recorder                         |
|                     `-> transform scaffold -> RingBuffer -> egress|
|                                                                   |
| Axum dashboard/API -> SQLite                                      |
| reconciler (1 second) -> output and recording lifecycle           |
+-------------------------------------------------------------------+
```

## Concurrency

Tokio tasks handle:

- Axum HTTP
- RTMP ingest and egress
- SRT connection coordination and ingest (inline native MPEG-TS demux)
- SRT egress feed and mux (inline TsMuxer)
- HLS segmenting and store (inline native MPEG-TS mux)
- Output reconciliation and egress lifecycle
- External transcoder: stdin feeder + stdout TsDemuxer task + stderr logger task
- Audio-routing stages (`atrack:`, `remap:`): pure packet-filter tokio tasks

Dedicated OS threads (`std::thread::spawn`) handle blocking FFmpeg or blocking
libsrt calls:

- SRT accept loop (blocks on `srt_accept()`)
- SRT egress sender (blocks on `srt_send()`)
- Internal transcoder video stage (`RESTREAM_USE_INTERNAL_TRANSCODER=1`): libavcodec decode+encode via MemoryQueue
- `hevc_to_h264` stage: libavcodec H.265→H.264 in-process, one OS thread per unique RTMP encoding with H.265 ingest (keyed `hevc_to_h264:from:<upstream>`)
- MPEG-TS recording (raw TS write via MemoryQueue)

The **external transcoder** (default) runs `ffmpeg` as a child subprocess — it does
**not** spawn an OS thread inside the parent. Per stage it uses three tokio tasks
(stdin feeder, stdout TsDemuxer, stderr logger) and one `Command::spawn` child.

All `std::thread::spawn` entry points are wrapped in `catch_unwind(AssertUnwindSafe(…))`
so FFmpeg or libsrt panics do not crash the process. SRT accept/sender threads log
the panic and stop; transcoder threads cancel their stage token so the reconciler
can restart the stage on the next tick.

## Thread Inventory

### Fixed threads (always running)

| Thread | Type | Spawned at | Purpose |
|---|---|---|---|
| Tokio runtime workers | OS threads | `#[tokio::main]` | Async task scheduling, epoll I/O polling |
| SRT accept loop | `std::thread` | `srt.rs` `SrtServer::run` | Blocks on `srt_accept()`, sends sockets via bounded `mpsc::channel(1024)` |
| SRT socket monitor | tokio task | `srt.rs` `SrtServer::run` | Polls `/proc/net/udp` every 1s for buffer occupancy |
| Reconciler | tokio task | `lib.rs` `run_app` | 1-second default tick: reconciles output desired vs active state; logs DB errors to stderr instead of silently skipping |
| RTMP listener | tokio task | `lib.rs` `run_app` | Accepts TCP connections on configurable port, default 1935 |
| Web server (Axum) | tokio task | `lib.rs` `run_app` | REST API + SSE health on configurable HTTP port, default 3030 |

Tokio worker count = `num_cpus` (tokio default, not configurable).

### Per-connection / per-output threads and tasks

| Thread / task | Type | Count | Lifetime |
|---|---|---|---|
| RTMP ingest handler | tokio task | 1 per RTMP publisher | TCP connection lifetime |
| RTMP egress handler | tokio task | 1 per RTMP output | Output lifetime |
| SRT ingest handler | tokio task | 1 per SRT publisher | SRT session; inline TsDemuxer |
| SRT shared egress muxer | tokio task | 1 per unique `(pipeline, preset)` | Shared `TsMuxer` task that feeds the SPMC `TsChunkRing` |
| SRT egress connection feeder | tokio task | 1 per SRT output | Drains `TsChunkRing` and writes to the connection's `MemoryQueue` |
| SRT egress sender | `std::thread` | 1 per SRT output, capped at 512 combined (play + egress) by `srt_sender_semaphore` | Blocks on `srt_send()`; connection is rejected gracefully when cap is reached |
| HLS segmenter | tokio task | 1 per active HLS pipeline | Inline TsMuxer + in-memory segment store |
| Ext transcoder stdin feeder | tokio task | 1 per `(pipeline, video_preset)` | source_ring → TsMuxer → FFmpeg stdin |
| Ext transcoder stdout reader | tokio task | 1 per `(pipeline, video_preset)` | FFmpeg stdout → TsDemuxer → output_ring |
| Ext transcoder stderr logger | tokio task | 1 per `(pipeline, video_preset)` | Drains and logs FFmpeg stderr |
| Ext FFmpeg subprocess | child process | 1 per `(pipeline, video_preset)` | Lives while stage is active |
| Int transcoder OS thread | `std::thread` | 1 per `(pipeline, video_preset)` when `RESTREAM_USE_INTERNAL_TRANSCODER=1` | libavcodec decode+encode via MemoryQueue |
| `hevc_to_h264` OS thread | `std::thread` | 1 per unique RTMP encoding with H.265 ingest | libavcodec H.265→H.264 in-process; keyed `hevc_to_h264:from:<upstream>` |
| `hevc_to_h264` feeder task | tokio task | 1 per unique RTMP encoding with H.265 ingest | upstream ring (source or preset output) → TsMuxer → MemoryQueue |
| Audio-routing stage | tokio task | 1 per `(pipeline, audio_key)` | Pure SelectTracks / Remap filter; no OS thread |
| Recording feeder | tokio task | 1 per active recording | source_ring → MemoryQueue |
| Recording writer | `std::thread` | 1 per active recording | MemoryQueue → raw MPEG-TS file write |

### OS thread count formula

```
total_os_threads =
    num_cpus                                    # tokio workers (fixed)
  + 1                                           # SRT accept loop (fixed)
  + min(N_srt_play + N_srt_egress, 512) × 1    # sender per SRT play subscriber or egress output
                                                #   capped at 512 by srt_sender_semaphore
  + N_hevc_to_h264_pipelines × 1               # libavcodec H.265→H.264 stage
  + N_int_video_stages × 1                     # libavcodec encode (internal backend only)
  + N_recordings × 1                           # TS writer per active recording
```

The following do **not** add OS threads:
- Tokio tasks (RTMP ingest/egress, SRT ingest/egress feed, HLS, recording feeder)
- External transcoder subprocess (child process, not a thread in the parent)
- Audio-routing stages (`atrack:`, `remap:`) — pure tokio task packet filters

### Example: 1 SRT ingest (H.264), 3 SRT egress, 720p transcode (ext), no recording

```
num_cpus (e.g. 8)    tokio workers
+ 1                  SRT accept loop
+ 3                  3 × SRT egress sender
─────
12 OS threads        (ext FFmpeg subprocess is a child process, not counted here)
```

### Example: 1 SRT ingest (H.265), 3 RTMP egress (source), 720p transcode, recording active

```
num_cpus (e.g. 8)    tokio workers
+ 1                  SRT accept loop
+ 1                  hevc_to_h264:from:source OS thread  (RTMP-src passthrough)
+ 1                  recording TS writer
─────
11 OS threads        + 1 ext FFmpeg child process (video:720p)
```

### Example: 1 RTMP ingest, 3 RTMP egress, no transcoding

```
num_cpus (e.g. 8)    tokio workers
+ 1                  SRT accept loop (always runs)
─────
9 OS threads total    (everything else is async tasks)
```

## Core affinity

No CPU pinning is configured. All threads use the kernel's default scheduler.
There is currently no active `core_affinity` wiring.

## Packet Flow

```text
RTMP:
socket -> rml_rtmp -> FLV audio/video payload -> MediaPacket -> RingBuffer

SRT:
libsrt socket -> MPEG-TS bytes -> TsDemuxer (inline async) -> MediaPacket -> RingBuffer

egress:
RingBuffer Reader -> protocol/container packaging -> socket or local store
```

`MediaPacket` carries media type, track index, PTS, DTS, keyframe state,
payload format tag, and a reference-counted payload.

### Payload format tagging

`MediaPacket.format` is a `PayloadFormat` enum (`Flv` or `Raw`) set by the
producer and checked by each consumer:

| Producer | Format | Payload content |
|---|---|---|
| RTMP ingest | `Flv` | FLV-wrapped: 5-byte video header, 2-byte audio header |
| SRT ingest TsDemuxer | `Raw` | Annex B (video), raw AAC (audio) |
| Transcoder stage | `Raw` | Annex B / raw AAC from FFmpeg demux |
| Rust MPEG-TS demuxer | `Raw` | Annex B / raw AAC extracted from PES |

Consumers use `format` to decide whether to strip FLV headers:

| Consumer | `Flv` action | `Raw` action |
|---|---|---|
| RTMP egress | Publish payload directly | Would need FLV re-wrap (not yet implemented) |
| SRT egress TsMuxer | Strip 5/2 byte FLV header, skip sequence headers | Pass through |
| HLS segmenter TsMuxer | Strip 5/2 byte FLV header, skip sequence headers | Pass through |
| Transcoder feeder | Strip FLV headers before muxing to input MPEG-TS | Pass through |
| Recording feeder | Passes raw bytes to FFmpeg MemoryQueue | Passes raw bytes |

## Ring Buffer

Each pipeline uses a 4096-slot single-producer/multi-consumer buffer.
`ArcSwapOption` slots permit lock-free reader loads, and payloads are shared
through `Arc`/`Bytes`. Slots are densely packed; only producer-owned indexes
are cache-line aligned.

Single-producer is an architectural assumption, not currently enforced. A
second independent publisher for the same pipeline can write concurrently and
invalidate it. A proper SRT bonded publisher is different: libsrt presents the
bond as one accepted group ID and one application receive path.

When a reader falls behind by at least the full capacity, it fast-forwards to
the latest known keyframe. Health, graph, and diagnostics expose per-reader lag,
overflow counts, burst-size stats, and unread packet age so operators can spot
slow consumers before they overflow.

The 4096-slot value is sized as a working target for high-rate streams (~24s at
4K60, ~48s at 1080p30). Actual depth depends on packetization, frame rate,
audio-track count, and encoder behavior.

## Packet Walk: RTMP ingest → RTMP egress

Zero thread hops. The entire path runs as tokio tasks on the async runtime.

```
 ┌─────────────────────────────────────────────────────────────────────────────┐
 │  INGRESS NIC                                                               │
 └────────┬────────────────────────────────────────────────────────────────────┘
          │ TCP segments
          ▼
 ┌────────────────────┐
 │ Kernel TCP stack   │  SO_RCVBUF = 8 MB
 │ default :1935 sock │
 └────────┬───────────┘
          │ socket ready (epoll)
          ▼
 ╔════════════════════════════════════════════════════════════════╗
 ║  TOKIO RUNTIME  (num_cpus worker threads, any core)          ║
 ║                                                               ║
 ║  ┌─────────────────────────────────────────────────────────┐  ║
 ║  │ Task: RTMP ingest handler                               │  ║
 ║  │                                                         │  ║
 ║  │  socket.read().await                                    │  ║
 ║  │    → RTMP handshake                                     │  ║
 ║  │    → FLV demux (video/audio chunk parse)                │  ║
 ║  │    → ring_buffer.push(MediaPacket)                      │  ║
 ║  │         ArcSwap store + AtomicUsize Release             │  ║
 ║  │    → notify.notify_waiters()                            │  ║
 ║  └────────────────────────┬────────────────────────────────┘  ║
 ║                           │                                   ║
 ║             ┌─────────────┼─────────────┐                     ║
 ║             ▼             ▼             ▼                     ║
 ║  ┌──────────────┐ ┌──────────────┐ ┌──────────────┐          ║
 ║  │   RingBuffer │ │   RingBuffer │ │   RingBuffer │          ║
 ║  │   Reader #1  │ │   Reader #2  │ │   Reader #3  │          ║
 ║  │   (Acquire)  │ │   (Acquire)  │ │   (Acquire)  │          ║
 ║  └──────┬───────┘ └──────┬───────┘ └──────┬───────┘          ║
 ║         │                │                │                   ║
 ║  ┌──────┴───────┐ ┌──────┴───────┐ ┌──────┴───────┐          ║
 ║  │ Task: RTMP   │ │ Task: RTMP   │ │ Task: RTMP   │          ║
 ║  │ egress #1    │ │ egress #2    │ │ egress #3    │          ║
 ║  │              │ │              │ │              │          ║
 ║  │ pull()       │ │ pull()       │ │ pull()       │          ║
 ║  │ → FLV mux    │ │ → FLV mux    │ │ → FLV mux    │          ║
 ║  │ → write_all  │ │ → write_all  │ │ → write_all  │          ║
 ║  │   .await     │ │   .await     │ │   .await     │          ║
 ║  └──────┬───────┘ └──────┬───────┘ └──────┬───────┘          ║
 ║         │                │                │                   ║
 ╚═════════╪════════════════╪════════════════╪═══════════════════╝
           │                │                │
           ▼                ▼                ▼
 ┌─────────────────────────────────────────────────────┐
 │ Kernel TCP stack  (3 × SO_SNDBUF = 8 MB each)      │
 └────────┬────────────────────────────────────────────┘
          │ TCP segments
          ▼
 ┌─────────────────────────────────────────────────────┐
 │  EGRESS NIC                                         │
 └─────────────────────────────────────────────────────┘

 Thread hops: 0
 Sync boundaries: 1 (RingBuffer push/pull, lock-free)
 OS threads spawned: 0 (pure async)
```

### RingBuffer internals

```
               RingBuffer internals
 ┌──────────────────────────────────────────────────┐
 │                                                  │
 │  slots: [RingSlot; 4096]                         │
 │    each slot: ArcSwapOption<MediaPacket>          │
 │    densely packed (8 bytes per slot)              │
 │                                                  │
 │  write_idx: AlignedAtomicUsize (cache-line)  ──┐ │
 │  last_keyframe: AtomicUsize                    │ │
 │  notify: tokio::sync::Notify                   │ │
 │                                                │ │
 │  Producer writes:                              │ │
 │    slot[idx % 4096].store(pkt)                 │ │
 │    write_idx.store(idx+1, Release) ────────────┘ │
 │    notify.notify_waiters()                       │
 │                                                  │
 │  Consumer reads:                                 │
 │    write_idx.load(Acquire)                       │
 │    slot[read_idx % 4096].load_full()             │
 │    each reader has independent read_idx          │
 │                                                  │
 │  Total memory: 4096 × 8B = 32 KiB               │
 │  MediaPacket: 56B, 8B aligned                    │
 └──────────────────────────────────────────────────┘
```

## Packet Walk: SRT ingest → transcoded SRT egress

Full path with transcoding. Each `═══▶` marks a thread hop across a
synchronization boundary.

```
 ┌───────────────────────────────────────────────────────────────────────┐
 │  INGRESS NIC                                                         │
 └───────┬──────────────────────────────────────────────────────────────┘
         │ UDP datagrams
         ▼
 ┌──────────────────────┐
 │ Kernel UDP stack     │  SO_RCVBUF = 8 MB
 │ default :10080 sock  │
 └───────┬──────────────┘
         │
         ▼
 ┌──────────────────────┐
 │ libsrt internals     │  opaque threads: retransmit, ACK,
 │ (not our threads)    │  reorder, loss recovery
 └───────┬──────────────┘
         │ reassembled MPEG-TS stream
         ▼
 ┌──────────────────────────────────────────────┐
 │ OS Thread: SRT accept loop                   │  std::thread::spawn
 │                                              │  blocks on srt_accept()
 │  accepted_sock ──── mpsc::send() ────────┐   │
 └──────────────────────────────────────────┼───┘
                                            │
  ══════════════════════════════════════════╪══  thread hop #1 (mpsc)
                                            │
 ╔══════════════════════════════════════════╪═══════════════════════════╗
 ║  TOKIO RUNTIME                          ▼                           ║
 ║  ┌──────────────────────────────────────────────────────────┐       ║
 ║  │ Task: SRT ingest handler                                 │       ║
 ║  │                                                          │       ║
 ║  │  loop:                                                   │       ║
 ║  │    srt_recv(sock) (non-blocking + long-lived epoll waiter)│       ║
 ║  │    demuxer.feed(buf)           ← inline TsDemuxer        │       ║
 ║  │    demuxer.drain_into(&mut packets)                      │       ║
 ║  │    ring_buffer.push_batch(&packets)                      │       ║
 ║  └─────────────────────────┬────────────────────────────────┘       ║
 ╚════════════════════════════╪════════════════════════════════════════╝
                              │
              ┌───────────────┴────────────┐
              ▼                            ▼
  ┌──────────────────────┐     ┌──────────────────────┐
  │    Source RingBuffer  │     │  (other consumers:   │
  │    4096 slots         │     │   HLS, recording,    │
  │    lock-free SPMC     │     │   direct egress)     │
  └──────────┬───────────┘     └──────────────────────┘
             │
  ═══════════╪═══════════════  thread hop #2 (Notify + Acquire)
             │
 ╔═══════════╪═══════════════════════════════════════════════════════╗
 ║  TOKIO    ▼                                                      ║
 ║  ┌─────────────────────────────────────────────────────────────┐  ║
 ║  │ Task: transcode feeder                                      │  ║
 ║  │                                                             │  ║
 ║  │  reader.wait_for_data().await                               │  ║
 ║  │  while reader.pull_burst():                                 │  ║
 ║  │    input_queue.write_batch(packet.payload)                  │  ║
 ║  └──────────────────────────┬──────────────────────────────────┘  ║
 ╚═════════════════════════════╪════════════════════════════════════╝
                               │
  ═════════════════════════════╪═══  thread hop #3 (MemoryQueue)
                               │      Mutex + Condvar
                               ▼
 ┌───────────────────────────────────────────────────────────────┐
 │ OS Thread: transcoder stage                                   │  std::thread
 │                                                               │  catch_unwind
 │  CustomInput ← input_queue                                    │
 │                                                               │
 │  loop:                                                        │
 │    av_read_frame() (demux input MPEG-TS)                      │
 │    → apply stream filter (audio routing)                       │
 │    → MediaPacket { pts, dts, payload, format: Raw }           │
 │    output_ring.push(packet) (direct RingBuffer push)          │
 └──────────────────────────┬────────────────────────────────────┘
                            │
               ┌────────────┴─────────────┐
               ▼                          ▼
  ┌───────────────────────┐   ┌───────────────────────┐
  │  Transcoded RingBuffer │   │  (shared: egress #2,  │
  │  4096 slots            │   │   egress #3 read      │
  │  lock-free SPMC        │   │   from same ring)     │
  └───────────┬───────────┘   └───────────────────────┘
              │
  ════════════╪═══════════════  thread hop #4 (Notify + Acquire)
              │
 ╔════════════╪═══════════════════════════════════════════════════╗
 ║  TOKIO     ▼                                                   ║
 ║  ┌──────────────────────────────────────────────────────────┐  ║
 ║  │ Task: Shared SRT Muxer (1 per pipeline + preset)         │  ║
 ║  │  reader.wait_for_data().await                            │  ║
 ║  │  while reader.pull_burst():                              │  ║
 ║  │    video/audio_payload_for_mux() (strip FLV if needed)   │  ║
 ║  │    dts_enforcer.enforce()                                │  ║
 ║  │    TsMuxer::mux_packet() (inline, ~0.6µs/pkt)            │  ║
 ║  │    ts_ring.push()                                        │  ║
 ║  └────────────────────────────┬─────────────────────────────┘  ║
 ║                               │ (Lock-free SPMC)               ║
 ║                               ▼                                ║
 ║  ┌──────────────────────────────────────────────────────────┐  ║
 ║  │ Task: SRT egress handler (1 per output connection)       │  ║
 ║  │  ts_reader.pull_burst()                                  │  ║
 ║  │  → out_queue.write(ts_batch).await                       │  ║
 ║  └────────────────────────────┬─────────────────────────────┘  ║
 ╚═══════════════════════════════╪════════════════════════════════╝
                                 │
  ═══════════════════════════════╪═══  thread hop #5 (MemoryQueue)
                                 │      Mutex + Condvar
                                 ▼
 ┌───────────────────────────────────────────────────────────────┐
 │ OS Thread: SRT egress sender                                  │  std::thread
 │                                                               │  catch_unwind
 │  loop:                                                        │
 │    out_queue.read(buf) ← Condvar::wait                        │
 │    srt_send(sock, buf, len) → libsrt                          │
 │    update_egress_bytes() (every 100 KB)                       │
 └──────────────────────────┬────────────────────────────────────┘
                            │
                            ▼
 ┌───────────────────────────────────────────────────────────────┐
 │ libsrt internals          opaque sender threads               │
 └──────────────────────────┬────────────────────────────────────┘
                            │
                            ▼
 ┌───────────────────────────────────────────────────────────────┐
 │ Kernel UDP stack          SO_SNDBUF = 8 MB                    │
 └──────────────────────────┬────────────────────────────────────┘
                            │ UDP datagrams
                            ▼
 ┌───────────────────────────────────────────────────────────────┐
 │  EGRESS NIC                                                   │
 └───────────────────────────────────────────────────────────────┘

 Thread hops: 4 (was 5 before inline TsDemuxer)
 Sync boundaries: 5 (2 lock-free rings, 1 MemoryQueue, 1 mpsc, 1 Notify wakeup)
 OS threads spawned: 2 (transcoder stage + sender)
```

## Packet Walk: SRT ingest → SRT egress (no transcoding)

When encoding is `source` (passthrough), no transcoder threads are spawned.
The egress reads directly from the source RingBuffer.

```
 INGRESS NIC
     │ UDP
     ▼
 Kernel → libsrt → SRT accept thread ──mpsc──▶ SRT ingest task
                                                    │
                                          srt_recv → inline TsDemuxer
                                                    │
                                          Source RingBuffer ◄── lock-free
                                                    │
           ┌────────────────────────────────────────┼────────────────┐
           ▼                                        ▼                ▼
  SRT egress task #1                    SRT egress task #2   SRT egress task #3
  pull → inline TsMux → MQ              pull → TsMux → MQ    pull → TsMux → MQ
           │                                        │                │
  SRT sender thread                     SRT sender           SRT sender
  srt_send()                            srt_send()           srt_send()
           │                                        │                │
           ▼                                        ▼                ▼
 Kernel → libsrt → EGRESS NIC

 Thread hops: 4 (per egress path)
 OS threads spawned: 3 (sender per egress)
```

## Packet Walk: HLS segmenter

```
 Source RingBuffer
     │
     │  Notify + Acquire
     ▼
 ╔════════════════════════════════════════════════════════════╗
 ║ TOKIO                                                      ║
 ║ ┌────────────────────────────────────────────────────────┐ ║
 ║ │ Task: HLS segmenter                                    │ ║
 ║ │   reader.pull_burst()                                  │ ║
 ║ │   TsMuxer::mux_packet() (inline, ~0.6µs/pkt)          │ ║
 ║ │   accumulate TS bytes in buffer                        │ ║
 ║ │   when keyframe + min_duration:                        │ ║
 ║ │     hls_store.push_segment(duration, bytes)            │ ║
 ║ │     ┌────────────────────────────────────────────────┐ │ ║
 ║ │     │  HlsStore (Mutex<VecDeque<HlsSegment>>)       │ │ ║
 ║ │     │  max_segments segments in memory               │ │ ║
 ║ │     │  served directly by Axum GET handler           │ │ ║
 ║ │     └────────────────────────────────────────────────┘ │ ║
 ║ └────────────────────────────────────────────────────────┘ ║
 ╚════════════════════════════════════════════════════════════╝

 Thread hops: 0
 OS threads spawned: 0 (inline TsMuxer in async task)
```

## Packet Walk: TS recording

```
 Source RingBuffer
     │
     │  Notify + Acquire
     ▼
 ╔══════════════════════════════════════╗
 ║ TOKIO                                ║
 ║ ┌──────────────────────────────────┐ ║
 ║ │ Task: recording feeder           │ ║
 ║ │   reader.pull_burst()            │ ║
 ║ │   queue.write_batch(ts bytes)    │ ║
 ║ └───────────────┬──────────────────┘ ║
 ╚═════════════════╪════════════════════╝
                   │
   ════════════════╪════  thread hop (MemoryQueue, Condvar)
                   ▼
 ┌──────────────────────────────────────┐
 │ OS Thread: TS writer                 │
 │   queue.read()                       │
 │   → raw MPEG-TS file write           │
 │   → file write (disk I/O)            │
 └──────────────────────────────────────┘

 Thread hops: 1
 OS threads spawned: 1
```

## Complete System Diagram

```
                            ┌───────────────────────────────────────┐
                            │          INGRESS NIC                  │
                            └──────┬──────────────┬─────────────────┘
                                   │ TCP          │ UDP
                                   ▼              ▼
                            ┌──────────┐   ┌──────────────┐
                            │  Kernel  │   │   Kernel     │
                            │default   │   │default       │
                            │ :1935    │   │ :10080       │
                            └────┬─────┘   └──────┬───────┘
                                 │                │
                                 │                ▼
                                 │         libsrt internals
                                 │                │
                                 │         SRT accept thread
                                 │                │ mpsc
                                 ▼                ▼
                            ╔════════════════════════════════════╗
                            ║        TOKIO RUNTIME               ║
                            ║  ┌────────────┐ ┌──────────────┐  ║
                            ║  │RTMP ingest │ │ SRT ingest   │  ║
                            ║  │  handler   │ │ handler +    │  ║
                            ║  │            │ │ TsDemuxer    │  ║
                            ║  └─────┬──────┘ └──────┬───────┘  ║
                            ╚════════╪═══════════════╪══════════╝
                                     │               │
                                     ▼               ▼
                            ┌────────────────────────────────────┐
                            │       SOURCE RINGBUFFER            │
                            │   4096 slots, lock-free SPMC       │
                            └──┬────────┬────────┬───────┬───────┘
                               │        │        │       │
                    ┌──────────┘   ┌────┘   ┌────┘  ┌────┘
                    ▼              ▼        ▼       ▼
              RTMP egress    transcode   HLS          recording
              tasks (async)  feeder     segmenter    feeder
                    │         (task)    (inline       (task)
                    │              │     TsMuxer)        │
                    │         encoder      │         TS writer
                    │         thread       │         thread
                    │              │       │            │
                    │         output       │          disk
                    │         reader       │
                    │         thread       │
                    │              │       ▼
                    │              ▼    HlsStore
                    │     ┌────────────────────┐
                    │     │TRANSCODED RINGBUFFER│
                    │     └──┬─────┬─────┬─────┘
                    │        │     │     │
                    │      SRT   SRT   SRT
                    │      egress tasks
                    │      (inline TsMux)
                    │        │     │     │
                    │      sendr sendr sendr
                    │      thrd  thrd  thrd
                    │        │     │     │
                    ▼        ▼     ▼     ▼
                ┌────────────────────────────────────┐
                │         EGRESS NIC                  │
                └────────────────────────────────────┘
```

## Synchronization at Each Boundary

| Boundary | Mechanism | Blocking? |
|---|---|---|
| SRT accept → tokio handler | `mpsc::channel(1024)` (bounded) | No (async recv) |
| Ingest handler → source RingBuffer | `push_batch()` (`ArcSwap` + `Release`) | No (lock-free) |
| Source ring → transcode feeder | `tokio::sync::Notify` + Acquire | No (async wait) |
| Feeder → transcoder | `MemoryQueue` (Mutex + Condvar) | Yes (Condvar wait) |
| Transcoder → transcoded ring | `ArcSwap` + Release | No (lock-free, direct push) |
| Transcoded ring → egress handler | `tokio::sync::Notify` + Acquire | No (async wait) |
| SRT egress task → SRT sender | `MemoryQueue` (Mutex + Condvar) | Yes (Condvar wait) |

## Memory Ordering (ring buffer hot path)

```rust
// Producer (ingest thread)
slots[idx].data.store(Some(Arc::new(packet)));   // ArcSwap store
write_idx.store(idx + 1, Ordering::Release);     // Release fence
notify.notify_waiters();                         // wake readers

// Consumer (egress task)
let w = write_idx.load(Ordering::Acquire);       // Acquire fence
let pkt = slots[idx].data.load_full();           // ArcSwap load
```

Release on the producer ensures all stores (slot data, keyframe index) are
visible before the write index increment. Acquire on the consumer establishes
a happens-before edge. Each reader has an independent `read_idx` — no
contention between consumers.

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

The stage cache prevents one encoder per destination. The current transcoder
creates output encoder parameters but then stream-copies compressed input
packets; it does not run a decode/filter/encode loop. Resolution, crop/rotate,
and H.265-to-H.264 presets therefore remain non-functional transforms even
though their stages appear in the graph.

Task "active" state is generally cancellation-token presence, not a worker
health signal. A native worker thread can fail while its feeder task/token
remains active.

## HLS and Recording

HLS segments are stored in memory in a twenty-segment sliding window and served by
Axum. The store and playlist behavior are tested. The live feeder uses the
native `TsMuxer` inline in the async task. One shared segmenter serves all
browser previews and HLS-type outputs per pipeline, kept alive by access
heartbeats and persistent output references.

Recordings are written as raw MPEG-TS files under `media/`. Recordings shorter
than five seconds are removed automatically. Recording uses the shared TS packet
feeder and a MemoryQueue-backed writer thread.

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
database rows, and file-ingest deletion kills its tracked child. Naturally
exited file-ingest children are reaped by the reconciler and by running-state
checks.

## libsrt Internal Threads

libsrt manages its own thread pool (opaque to the application):

- Sender threads: retransmission, ACKs, bandwidth probing
- Receiver threads: UDP recv, reordering, loss recovery

These are not controlled by restream. The application interacts via
`srt_recv()` / `srt_send()` / `srt_accept()` calls.

## Design Rationale: Why OS Threads for FFmpeg

FFmpeg codec calls (`avcodec_decode_video2`, `avcodec_encode_video2`,
`av_interleaved_write_frame`) block indefinitely. Running them on a tokio
worker would stall all tasks on that thread. Explicit `std::thread::spawn`
keeps the async runtime responsive.

All FFmpeg threads use `catch_unwind(AssertUnwindSafe(…))` so that corrupt
streams or codec bugs log errors without crashing the process. All three SRT
OS threads (accept, play sender, egress sender) carry the same guard.

## Legacy MediaMTX Migration

The previous Node.js backend used MediaMTX for RTMP/SRT transport, path
management, health APIs, Prometheus metrics, and HLS preview. All of those are
now handled natively by the Rust binary. MediaMTX remains useful only as an
isolated interoperability sink in protocol tests.

The old MediaMTX Prometheus/Grafana setup belongs to the archived implementation
under `old/`. The current Rust binary has no `/metrics` text endpoint.

## Key Files

| File | Lines | Responsibility |
|---|---:|---|
| `src/lib.rs` | 525 | App composition and reconciliation |
| `src/api.rs` | 1,946 | Router, auth, REST/SSE handlers, embedded assets |
| `src/db.rs` | 803 | SQLite schema and queries |
| `src/diag.rs` | 986 | Native diagnostics |
| `src/media/engine.rs` | 1,583 | Active state and health/graph snapshots |
| `src/media/ring_buffer.rs` | 568 | Lock-free packet fan-out |
| `src/media/mpegts.rs` | 2,111 | Native MPEG-TS demuxer and muxer |
| `src/media/codec.rs` | 798 | Codec helpers, Annex-B scanning, FLV stripping, zero-alloc `_into` variants |
| `src/media/avio.rs` | 340 | In-memory FFmpeg AVIO and MemoryQueue |
| `src/media/rtmp.rs` | 1,690 | RTMP server/client (rtmp:// + rtmps://) |
| `src/media/srt.rs` | 2,387 | SRT server/client, bonding, stats |
| `src/media/tcp_stats.rs` | 253 | Linux RTMP receiver socket metrics |
| `src/media/hls.rs` | 338 | In-memory HLS segmenter and store |
| `src/media/recording.rs` | 228 | MPEG-TS recording |
| `src/media/transcoder.rs` | 420 | Shared video/audio stages |
| `src/media/security.rs` | 221 | Ingest rate-limit, IP ban, bounded tracked-IP map |
