# Architecture

Single Rust binary replacing Node.js + MediaMTX + spawned FFmpeg processes. All media transport, orchestration, state, and UI are in-process.

## System Shape

```text
Publisher (RTMP/SRT)
  │
  ▼
┌──────────────────────────────────────────────────────────┐
│                    restream binary                        │
│                                                          │
│  RTMP Server ─┐                                          │
│  SRT Server  ─┤─► RingBuffer ─┬─► RTMP Egress ──► CDN   │
│               │   (per pipe)  ├─► SRT Egress  ──► CDN    │
│               │               ├─► HLS Muxer  ──► disk   │
│               │               ├─► MKV Recorder ► disk   │
│               │               └─► Transcoder ──► RingBuf │
│                                                          │
│  Axum Web Server ──► REST API ──► SQLite (data.db)       │
│                  ──► Dashboard (embedded frontend)        │
│                  ──► SSE /health + /diagnostics           │
│                                                          │
│  Reconciler (1s) ──► output lifecycle + recording mgmt   │
└──────────────────────────────────────────────────────────┘
```

## Threading Model

```text
┌───────────────── tokio runtime (multi-threaded) ─────────────────┐
│  Axum web server          HTTP handlers, SSE streams             │
│  RTMP listener            per-connection async tasks             │
│  SRT accept loop          per-connection async tasks             │
│  Reconciler (1s tick)     output lifecycle + recording start/stop│
│  Egress tasks             RingBuffer reader → network send       │
└──────────────────────────────────────────────────────────────────┘

┌───────────── std::thread (OS threads, catch_unwind) ─────────────┐
│  FFmpeg demuxer           RTMP/SRT ingest → RingBuffer push      │
│  FFmpeg HLS muxer         MemoryQueue → HLS segments on disk     │
│  FFmpeg MKV muxer         MemoryQueue → .mkv recording file      │
│  FFmpeg transcoder        MemoryQueue → encode → MemoryQueue     │
└──────────────────────────────────────────────────────────────────┘
```

Tokio handles all network I/O and coordination. CPU-bound FFmpeg work runs on dedicated OS threads to avoid starving the async runtime. All `std::thread::spawn` calls are wrapped in `catch_unwind` so an FFmpeg panic logs an error instead of taking down the process.

## Packet Walk (Ingest → Egress)

```text
1. Publisher sends RTMP/SRT stream
2. Protocol handler (rtmp.rs/srt.rs) receives raw data
3. For RTMP: rml_rtmp parses FLV, emits VideoDataReceived/AudioDataReceived
   For SRT: raw MPEG-TS → MemoryQueue → FFmpeg demuxer (OS thread) → packets
4. MediaPacket { media_type, track_index, pts, dts, is_keyframe, payload }
5. RingBuffer.push() → ArcSwapOption.store() → Notify.notify_waiters()
6. N readers wake → ArcSwapOption.load_full() → Arc<MediaPacket> (zero-copy)
7. Each egress/muxer forwards the packet to its destination
```

## Ring Buffer Design

```text
Capacity: 4096 slots (at 30fps ≈ 136 seconds of buffering)

┌─────────────────────────────────────────────┐
│ Slot 0 │ Slot 1 │ Slot 2 │ ... │ Slot 4095 │  ← 64-byte aligned (cache line)
│ArcSwap │ArcSwap │ArcSwap │     │ ArcSwap   │
└─────────────────────────────────────────────┘
     ▲ write_idx (AtomicUsize, 64-byte aligned)
     ▲ last_keyframe_idx (AtomicUsize, O(1) fast-forward)
```

- **Single producer**: only the ingest thread calls `push()`, guaranteed by monotonic `write_idx`
- **Multi consumer**: readers call `load_full()` (lock-free, wait-free via `arc-swap`)
- **Overflow**: when a reader lags by ≥ capacity, it fast-forwards to `last_keyframe_idx` (O(1) atomic read)
- **False sharing prevention**: each slot is `#[repr(align(64))]` so concurrent readers on adjacent slots never stall each other

## In-Memory AVIO (MemoryQueue)

Replaces TCP loopback sockets for FFmpeg I/O. Data flows through a `VecDeque<u8>` protected by `Mutex` + `Condvar`.

- **Bulk reads**: `as_slices()` + `copy_from_slice` + `drain()` — single memcpy per read (was per-byte `pop_front()`)
- **Buffer size**: 32 KB (FFmpeg default) — 8x fewer callback invocations than the 4 KB buffer
- **Benchmark**: 173 µs for 1 MB transfer (2.3x faster than TCP loopback)

## Codec Support

| Codec | Ingest | Passthrough | Transcode |
|-------|--------|-------------|-----------|
| H.264 | RTMP, SRT | All egress | Yes |
| H.265/HEVC | SRT, Enhanced RTMP | All egress | Yes |
| AAC | RTMP, SRT | All egress | Yes |
| Multi-track audio | SRT | All egress | — |

RTMP keyframe detection uses FLV FrameType (`data[0] >> 4 == 1`), which is codec-agnostic. SRT demuxer maps all video and audio streams (not just "best") with per-track `track_index` for multi-track support.

## SIMD Optimizations

Runtime-dispatched: AVX-512 → AVX2 → SSE2 → scalar fallback.

| Operation | Size | Scalar | SIMD | Speedup |
|-----------|------|--------|------|---------|
| Sync byte scan | 1 KB | 257 ns | 15 ns | 17x |
| Sync byte scan | 64 KB | 16 µs | 894 ns | 18x |

## Scaling Target

- 50 concurrent ingests, each with its own `RingBuffer`
- Up to 500 egress readers on the hottest pipeline (Zipfian distribution)
- ArcSwap lock-free reads handle 500 concurrent readers without contention
- `mimalloc` allocator for reduced lock contention under multi-threaded load
- File descriptor limit raised to 65536 at startup via `setrlimit`

## Frontend Embedding

Static assets from `public/` are compiled into the binary via `rust-embed`. At runtime, the server tries disk first (for development hot-reload), then falls back to embedded assets (for production single-binary deployment). The SPA fallback serves `index.html` for all unmatched routes.

## Key Files

| File | Purpose |
|------|---------|
| `src/lib.rs` | App composition, reconciliation loop |
| `src/api.rs` | Axum router, REST handlers, embedded asset serving |
| `src/db.rs` | SQLite schema and queries |
| `src/diag.rs` | Streaming SSE diagnostics |
| `src/types.rs` | Domain types (Pipeline, Output, Job) |
| `src/media/engine.rs` | Central state: ingests, egresses, ring buffers |
| `src/media/ring_buffer.rs` | Lock-free SPMC ring buffer with ArcSwap |
| `src/media/avio.rs` | In-memory FFmpeg I/O (MemoryQueue + AVIO) |
| `src/media/rtmp.rs` | RTMP ingest/egress via rml_rtmp |
| `src/media/srt.rs` | SRT ingest/egress via libsrt FFI |
| `src/media/hls.rs` | HLS segment muxer |
| `src/media/recording.rs` | MKV recording muxer |
| `src/media/transcoder.rs` | In-process transcoder (H.264/H.265) |
| `src/media/simd.rs` | SIMD-accelerated memcpy and sync byte scan |
| `src/media/security.rs` | Ingest rate limiter |

## Dependencies

| Crate | Purpose |
|-------|---------|
| `tokio` | Async runtime |
| `axum` | HTTP framework |
| `sqlx` | SQLite driver |
| `ffmpeg-next` | FFmpeg bindings (libavformat/libavcodec 6.x) |
| `rml_rtmp` | RTMP protocol |
| `arc-swap` | Lock-free atomic Arc for ring buffer slots |
| `rust-embed` | Compile-time asset embedding |
| `sysinfo` | CPU/memory/disk/network monitoring |
| `mimalloc` | Performance allocator |
| `nix` | Unix socket stats (TCP_INFO) |
