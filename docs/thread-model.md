# Thread Model and Packet Walk

How packets move from ingress NIC to egress NIC, which OS threads and tokio
tasks handle each stage, and what determines total thread count.

## Thread inventory

### Fixed threads (always running)

| Thread | Type | Spawned at | Purpose |
|---|---|---|---|
| Tokio runtime workers | OS threads | `#[tokio::main]` | Async task scheduling, epoll I/O polling |
| SRT accept loop | `std::thread` | `srt.rs` `SrtServer::run` | Blocks on `srt_accept()`, sends sockets via mpsc |
| SRT socket monitor | tokio task | `srt.rs` `SrtServer::run` | Polls `/proc/net/udp` every 1s for buffer occupancy |
| Reconciler | tokio task | `lib.rs` `run_app` | 1-second tick: reconciles output desired vs active state |
| RTMP listener | tokio task | `lib.rs` `run_app` | Accepts TCP connections on port 1935 |
| Web server (Axum) | tokio task | `lib.rs` `run_app` | HTTP on :3030, REST API + SSE health |

Tokio worker count = `num_cpus` (tokio default, not configurable).

### Per-connection / per-output threads

| Thread | Type | Count | Lifetime |
|---|---|---|---|
| RTMP ingest handler | tokio task | 1 per RTMP publisher | TCP connection lifetime |
| RTMP egress handler | tokio task | 1 per RTMP output | Output lifetime |
| SRT ingest handler | tokio task | 1 per SRT publisher | SRT session lifetime (inline native TsDemuxer) |
| SRT egress feed+mux | tokio task | 1 per SRT output | Inline TsMuxer in async feed loop |
| SRT egress sender | `std::thread` | 1 per SRT output | Blocks on `srt_send()` |
| HLS segmenter | tokio task | 1 per HLS pipeline | TsMuxer + segment accumulator (inline async) |
| Transcoder stage | `std::thread` | 1 per unique (pipeline, preset) | FFmpeg demux → direct RingBuffer push |
| Recording muxer | `std::thread` | 1 per active recording | MKV mux via FFmpeg |

All `std::thread` spawns wrap FFmpeg work in `catch_unwind(AssertUnwindSafe(…))`
to prevent panics from crashing the process.

### Thread count formula

```
total_os_threads =
    num_cpus                           # tokio workers (fixed)
  + 1                                  # SRT accept loop (fixed)
  + N_srt_ingest × 0                   # SRT demuxer is inline in tokio task (no OS threads)
  + N_srt_egress × 1                   # sender per SRT output (TsMuxer is inline async)
  + N_hls_egress × 0                   # inline TsMuxer in async task (no OS threads)
  + N_unique_presets × 1               # transcoder stage per preset (direct RingBuffer push)
  + N_recordings × 1                   # MKV muxer per active recording
  + N_rtmp_ingest × 0                  # RTMP is pure async
  + N_rtmp_egress × 0                  # RTMP is pure async
```

Tokio tasks (not OS threads) are additionally spawned per connection and output
but multiplex onto the fixed worker pool.

### Example: 1 SRT ingest, 3 SRT egress, 720p transcode, recording active

```
num_cpus (e.g. 8)    tokio workers
+ 1                  SRT accept loop
+ 0                  SRT demuxer (inline)
+ 3                  3 × sender (TsMuxer inline)
+ 1                  transcoder stage (direct push)
+ 1                  recording MKV muxer
─────
14 OS threads total
```

### Example: 1 RTMP ingest, 3 RTMP egress, no transcoding

```
num_cpus (e.g. 8)    tokio workers
+ 1                  SRT accept loop (always runs)
─────
9 OS threads total    (everything else is async tasks)
```

## Core affinity

`core_affinity = "0.8"` is declared in Cargo.toml but **never called**. All
threads — tokio workers, FFmpeg OS threads, libsrt internals — use the kernel's
default scheduler with no CPU pinning. A packet may migrate across any core at
each thread hop.

## Packet walk: RTMP ingest → RTMP egress

Zero thread hops. The entire path runs as tokio tasks on the async runtime.

```
 ┌─────────────────────────────────────────────────────────────────────────────┐
 │  INGRESS NIC                                                               │
 └────────┬────────────────────────────────────────────────────────────────────┘
          │ TCP segments
          ▼
 ┌────────────────────┐
 │ Kernel TCP stack   │  SO_RCVBUF = 8 MB
 │ :1935 listen sock  │
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

The RingBuffer sits at the center:

```
               RingBuffer internals
 ┌──────────────────────────────────────────────────┐
 │                                                  │
 │  slots: [AlignedSlot; 4096]                      │
 │    each slot = 64 bytes (cache-line aligned)      │
 │    data: ArcSwapOption<MediaPacket>               │
 │                                                  │
 │  write_idx: AtomicUsize  ─── cache line ───┐     │
 │  last_keyframe: AtomicUsize               │     │
 │  notify: tokio::sync::Notify               │     │
 │                                            │     │
 │  Producer writes:                          │     │
 │    slot[idx % 4096].store(pkt)             │     │
 │    write_idx.store(idx+1, Release) ────────┘     │
 │    notify.notify_waiters()                       │
 │                                                  │
 │  Consumer reads:                                 │
 │    write_idx.load(Acquire)                       │
 │    slot[read_idx % 4096].load_full()             │
 │    each reader has independent read_idx          │
 │                                                  │
 │  Total memory: 4096 × 64B = 256 KiB             │
 │  MediaPacket: 56B, 8B aligned                    │
 │  AlignedSlot: 64B, 64B aligned                   │
 └──────────────────────────────────────────────────┘
```

## Packet walk: SRT ingest → transcoded SRT egress

Full path with transcoding. Each `═══▶` marks a thread hop across a
synchronization boundary. The diagram shows one egress; additional egresses
reading the same preset share the transcoded RingBuffer.

```
 ┌───────────────────────────────────────────────────────────────────────┐
 │  INGRESS NIC                                                         │
 └───────┬──────────────────────────────────────────────────────────────┘
         │ UDP datagrams
         ▼
 ┌──────────────────────┐
 │ Kernel UDP stack     │  SO_RCVBUF = 8 MB
 │ :10080 listen sock   │
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
 ║  │    srt_recv(sock, buf) ← blocks inside libsrt            │       ║
 ║  │    memory_queue.write(buf)                                │       ║
 ║  │    update_ingest_bytes()                                  │       ║
 ║  └─────────────────────────┬────────────────────────────────┘       ║
 ╚════════════════════════════╪════════════════════════════════════════╝
                              │
  ════════════════════════════╪═══════  thread hop #2 (MemoryQueue)
                              │          Mutex + Condvar
                              ▼
 ┌────────────────────────────────────────────────────┐
 │ OS Thread: SRT demuxer                             │  std::thread::spawn
 │                                                    │  catch_unwind
 │  loop:                                             │
 │    memory_queue.read(buf) ← Condvar::wait          │
 │    FFmpeg: av_read_frame()  (MPEG-TS demux)        │
 │    → MediaPacket { pts, dts, payload, is_keyframe }│
 │    ring_buffer.push(packet) ← ArcSwap + Release    │
 └─────────────────────────┬──────────────────────────┘
                           │
              ┌────────────┴────────────┐
              ▼                         ▼
  ┌──────────────────────┐   ┌──────────────────────┐
  │    Source RingBuffer  │   │  (other consumers:   │
  │    4096 slots         │   │   HLS, recording,    │
  │    lock-free SPMC     │   │   direct egress)     │
  └──────────┬───────────┘   └──────────────────────┘
             │
  ═══════════╪═══════════════  thread hop #3 (Notify + Acquire)
             │
 ╔═══════════╪═══════════════════════════════════════════════════════╗
 ║  TOKIO    ▼                                                      ║
 ║  ┌─────────────────────────────────────────────────────────────┐  ║
 ║  │ Task: transcode feeder                                      │  ║
 ║  │                                                             │  ║
 ║  │  reader.wait_for_data().await                               │  ║
 ║  │  while reader.pull():                                       │  ║
 ║  │    input_queue.write(packet.payload)                         │  ║
 ║  └──────────────────────────┬──────────────────────────────────┘  ║
 ╚═════════════════════════════╪════════════════════════════════════╝
                               │
  ═════════════════════════════╪═══  thread hop #4 (MemoryQueue)
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
  ════════════╪═══════════════  thread hop #5 (Notify + Acquire)
              │
 ╔════════════╪═══════════════════════════════════════════════════╗
 ║  TOKIO     ▼                                                   ║
 ║  ┌──────────────────────────────────────────────────────────┐  ║
 ║  │ Task: SRT egress handler                                  │  ║
 ║  │                                                          │  ║
 ║  │  reader.wait_for_data().await                            │  ║
 ║  │  while reader.pull():                                    │  ║
 ║  │    video/audio_payload_for_mux() (strip FLV if needed)   │  ║
 ║  │    dts_enforcer.enforce()                                │  ║
 ║  │    TsMuxer::mux_packet() (MPEG-TS mux, ~0.6µs/pkt)      │  ║
 ║  │    → out_queue.write(ts_bytes)                           │  ║
 ║  └────────────────────────────┬─────────────────────────────┘  ║
 ╚═══════════════════════════════╪════════════════════════════════╝
                                 │
  ═══════════════════════════════╪═══  thread hop #6 (MemoryQueue)
                            │          Mutex + Condvar
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

 Thread hops: 5
 Sync boundaries: 7 (2 lock-free rings, 2 MemoryQueues, 1 mpsc, 2 Notify wakeups)
 OS threads spawned: 2 (transcoder stage + sender)
```

### Data flow between rings and queues

```
                         ┌─────────────┐
                         │  MemoryQueue │ Mutex+Condvar
  SRT ingest ──write()──▶│  (MPEG-TS   │──read()──▶ FFmpeg demuxer
  handler task           │   bytes)     │            thread
                         └─────────────┘

                         ┌──────────────────────────────────────┐
                         │         Source RingBuffer             │
  FFmpeg demuxer         │  slots[4096] ArcSwap<MediaPacket>    │
  thread ──push()──▶     │  write_idx: Release                  │──pull()──▶ readers
                         │  notify: wake all                    │
                         └──────────────────────────────────────┘
                                         │
           ┌─────────────────────────────┼───────────────────┐
           ▼                             ▼                   ▼
     Transcode feeder              HLS segmenter       Recording feeder
     (tokio task)                  (tokio task)         (tokio task)
           │                             │                   │
           ▼                             │                   ▼
  ┌─────────────┐                        │          ┌─────────────┐
  │ MemoryQueue │                        │          │ MemoryQueue │
  │ (to encoder)│                        │          │ (to muxer)  │
  └──────┬──────┘                        │          └──────┬──────┘
         ▼                               │                 ▼
  FFmpeg encoder                         │          FFmpeg MKV muxer
  thread                                 │          thread
         │                               │                 │
         ▼                               ▼                 ▼
  ┌─────────────┐               ┌─────────────────┐  ┌──────────┐
  │ MemoryQueue │               │ HlsStore        │  │ File I/O │
  │ (encoded)   │               │ Mutex<VecDeque> │  │ (.mkv)   │
  └──────┬──────┘               │ → Axum handler  │  └──────────┘
         ▼                      └─────────────────┘
  Transcoder output
  reader thread
         │
         ▼
  ┌────────────────────┐
  │ Transcoded         │
  │ RingBuffer         │
  │ (shared by egress) │
  └─────────┬──────────┘
            │
    ┌───────┼───────┐
    ▼       ▼       ▼
  SRT     SRT     SRT
  egress  egress  egress
  #1      #2      #3
```

### Synchronization at each boundary

| Boundary | Mechanism | Blocking? |
|---|---|---|
| SRT accept → tokio handler | `mpsc::channel` | No (async recv) |
| Ingest handler → source RingBuffer | `push_batch()` (`ArcSwap` + `Release`) | No (lock-free) |
| Source ring → transcode feeder | `tokio::sync::Notify` + Acquire | No (async wait) |
| Feeder → transcoder | `MemoryQueue` (Mutex + Condvar) | Yes (Condvar wait) |
| Transcoder → transcoded ring | `ArcSwap` + Release | No (lock-free, direct push) |
| Transcoded ring → egress handler | `tokio::sync::Notify` + Acquire | No (async wait) |
| SRT egress task → SRT sender | `MemoryQueue` | Yes (Mutex + Condvar) |
| SRT TsMuxer → SRT sender | `MemoryQueue` (Mutex + Condvar) | Yes (Condvar wait) |

## Packet walk: SRT ingest → SRT egress (no transcoding)

When encoding is `source` (passthrough), no transcoder threads are spawned.
The egress reads directly from the source RingBuffer.

```
 INGRESS NIC
     │ UDP
     ▼
 Kernel → libsrt → SRT accept thread ──mpsc──▶ SRT ingest task
                                                    │
                                          srt_recv → MemoryQueue
                                                    │
                                          FFmpeg demuxer thread
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

 Thread hops: 5 (per egress path)
 OS threads spawned: 1 (demuxer) + 3×1 (sender) = 4
```

## Packet walk: HLS segmenter

```
 Source RingBuffer
     │
     │  Notify + Acquire
     ▼
 ╔════════════════════════════════════════════════════════════╗
 ║ TOKIO                                                      ║
 ║ ┌────────────────────────────────────────────────────────┐ ║
 ║ │ Task: HLS segmenter                                    │ ║
 ║ │   reader.pull()                                        │ ║
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

## Packet walk: MKV recording

```
 Source RingBuffer
     │
     │  Notify + Acquire
     ▼
 ╔══════════════════════════════════════╗
 ║ TOKIO                                ║
 ║ ┌──────────────────────────────────┐ ║
 ║ │ Task: recording feeder           │ ║
 ║ │   reader.pull()                  │ ║
 ║ │   queue.write(payload)           │ ║
 ║ └───────────────┬──────────────────┘ ║
 ╚═════════════════╪════════════════════╝
                   │
   ════════════════╪════  thread hop (MemoryQueue, Condvar)
                   ▼
 ┌──────────────────────────────────────┐
 │ OS Thread: MKV muxer                 │
 │   queue.read() → FFmpeg demux        │
 │   → FFmpeg mux to Matroska           │
 │   → file write (disk I/O)            │
 └──────────────────────────────────────┘

 Thread hops: 1
 OS threads spawned: 1
```

## Complete system diagram (all paths active)

```
                            ┌───────────────────────────────────────┐
                            │          INGRESS NIC                  │
                            └──────┬──────────────┬─────────────────┘
                                   │ TCP          │ UDP
                                   ▼              ▼
                            ┌──────────┐   ┌──────────────┐
                            │  Kernel  │   │   Kernel     │
                            │  :1935   │   │   :10080     │
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
                            ║  │  handler   │ │  handler     │  ║
                            ║  └─────┬──────┘ └──────┬───────┘  ║
                            ╚════════╪═══════════════╪══════════╝
                                     │               │
                                     │        SRT demuxer thread
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
                    │         encoder      │         MKV muxer
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

## libsrt internal threads

libsrt manages its own thread pool (opaque to the application):

- Sender threads: retransmission, ACKs, bandwidth probing
- Receiver threads: UDP recv, reordering, loss recovery

These are not controlled by restream. The application interacts via blocking
`srt_recv()` / `srt_send()` / `srt_accept()` calls.

Socket options set for all SRT sockets:

| Option | Value | Purpose |
|---|---|---|
| UDP buffer (kernel) | 8 MB | Kernel-level recv/send buffer |
| SRT buffer (internal) | 12 MB | libsrt internal buffer |
| Flow control window | 32768 packets | Congestion control |
| Latency | 250 ms | Dejitter + retransmit window |
| Loss max TTL | 256 | Reorder tolerance |

## Memory ordering (ring buffer hot path)

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

## Design rationale: why OS threads for FFmpeg

FFmpeg codec calls (`avcodec_decode_video2`, `avcodec_encode_video2`,
`av_interleaved_write_frame`) block indefinitely. Running them on a tokio
worker would stall all tasks on that thread. Explicit `std::thread::spawn`
keeps the async runtime responsive.

All FFmpeg threads use `catch_unwind(AssertUnwindSafe(…))` so that corrupt
streams or codec bugs log errors without crashing the process.

## Payload format tagging

`MediaPacket.format` is a `PayloadFormat` enum (`Flv` or `Raw`) set by the
producer and checked by each consumer:

| Producer | Format | Payload content |
|---|---|---|
| RTMP ingest | `Flv` | FLV-wrapped: 5-byte video header, 2-byte audio header |
| SRT ingest demuxer | `Raw` | Annex B (video), raw AAC (audio) from FFmpeg demux |
| Transcoder stage | `Raw` | Annex B / raw AAC from FFmpeg demux of input MPEG-TS |
| Rust MPEG-TS demuxer | `Raw` | Annex B / raw AAC extracted from PES |

Consumers use `format` to decide whether to strip FLV headers:

| Consumer | `Flv` action | `Raw` action |
|---|---|---|
| RTMP egress | Publish payload directly | Would need FLV re-wrap (not yet implemented) |
| SRT egress TsMuxer | Strip 5/2 byte FLV header, skip sequence headers | Pass through |
| HLS segmenter TsMuxer | Strip 5/2 byte FLV header, skip sequence headers | Pass through |
| Transcoder feeder | Strip FLV headers before muxing to input MPEG-TS | Pass through |
| Recording feeder | Passes raw bytes to FFmpeg MemoryQueue | Passes raw bytes |

This replaces the previous approach of guessing payload format from
`ingest.protocol`, which broke when a transcoded RingBuffer (always `Raw`)
was consumed by an egress that checked the original ingest protocol.

## Shared transcoding stages

Transcoding stages are keyed by `(pipeline_id, preset)`. Multiple egress
outputs requesting the same preset (e.g. three SRT outputs all at 720p) share
a single encoder thread and read from the same output RingBuffer. The thread
cost is per unique preset, not per output.
