# Rust Backend Rewrite — Status

Branch: `feat/rust-backend-rewrite`

## Overview

The Node.js + MediaMTX + spawned-FFmpeg architecture has been replaced by a single statically-linked Rust binary. MediaMTX is eliminated — RTMP/SRT ingest/egress, HLS segmenting, transcoding, and recording all happen in-process using FFmpeg libraries (`ffmpeg-next`) and a pure-Rust RTMP implementation (`rml_rtmp`).

| | Old (Node.js) | New (Rust) |
|-|---------------|------------|
| **Runtime** | Node 22 + MediaMTX + child FFmpeg processes | Single `restream` binary |
| **Binary size** | ~300 MB (node_modules + mediamtx + ffmpeg) | 6.2 MB (release, fat LTO, 1 codegen unit) |
| **Media transport** | MediaMTX (external process) | In-process RTMP/SRT servers |
| **FFmpeg** | Spawned child processes, TCP loopback I/O | Linked `libav*`, in-memory AVIO queues |
| **HLS preview** | MediaMTX writes segments to disk | In-memory `HlsStore`, zero disk I/O |
| **Allocator** | V8 GC | mimalloc (jemalloc-class performance) |
| **Backend lines** | 9,422 (TypeScript) | 6,396 (Rust, excl. tests/benches) |
| **Test lines** | — | 921 (unit) + 219 (integration) |

---

## Architecture

### Threading model

```
┌────────────────── tokio runtime (multi-threaded) ──────────────────┐
│  Axum web server          HTTP handlers, SSE diagnostics           │
│  RTMP listener            per-connection async tasks               │
│  SRT accept loop          per-connection async tasks               │
│  Reconciler (1 s tick)    output lifecycle + recording auto-start  │
│  Egress tasks             ring buffer reader → network send        │
└────────────────────────────────────────────────────────────────────┘

┌────────────── std::thread (OS threads, catch_unwind) ─────────────┐
│  FFmpeg demuxer           RTMP/SRT ingest → RingBuffer push        │
│  FFmpeg HLS muxer         MemoryQueue → in-memory HLS segments     │
│  FFmpeg MKV muxer         MemoryQueue → .mkv recording file        │
│  FFmpeg transcoder        MemoryQueue → encode → MemoryQueue       │
└────────────────────────────────────────────────────────────────────┘
```

All network I/O and coordination runs on tokio tasks. CPU-bound FFmpeg work runs on dedicated OS threads. Every `std::thread::spawn` call is wrapped in `catch_unwind` so a panic from a corrupt stream logs an error instead of crashing the process.

### Data flow

```
Publisher ─RTMP/SRT→ IngestServer ─MediaPacket→ RingBuffer ─Reader→ EgressClient ─RTMP/SRT→ Destination
                                                    │
                                                    ├─Reader→ HLS segmenter → HlsStore (in-memory)
                                                    ├─Reader→ MKV recorder → disk
                                                    └─Reader→ Transcoder → output RingBuffer
```

### Key data structures

**RingBuffer** (`src/media/ring_buffer.rs`) — lock-free single-producer multi-consumer packet fan-out. Each slot is `#[repr(align(64))]` (one cache line) to prevent false-sharing stalls. Write side uses a monotonic `AtomicUsize` index; readers call `ArcSwapOption::load_full()` concurrently without locking. Eliminates the per-slot `RwLock` contention that would bottleneck at 500+ concurrent egress readers. Overflow recovery jumps to the most recent keyframe via `last_keyframe_idx` (O(1) atomic read) to avoid decoding artifacts.

**MemoryQueue** (`src/media/avio.rs`) — replaces TCP loopback sockets from the Node.js backend. `VecDeque<u8>` protected by `Mutex` + `Condvar`, with bulk `as_slices()` + `drain()` reads. Backs custom `AVIOContext` callbacks (`read_packet_cb` / `write_packet_cb`) so FFmpeg reads/writes happen entirely in-process. AVIO buffer is 32 KB (FFmpeg default; 4 KB would cause ~8x more callback invocations per video frame).

**HlsStore** (`src/media/hls.rs`) — in-memory MPEG-TS segment buffer. Segments are `Bytes` in a `VecDeque` (max 10, ~6 s target duration). No disk I/O on the hot path. M3U8 playlist is generated on-the-fly from segment metadata. The segmenter splits on keyframe boundaries signaled via `AtomicBool` from the input feeder, with a minimum segment duration floor to avoid micro-segments.

**MediaEngine** (`src/media/engine.rs`) — central state registry. Owns all `HashMap`s for active ingests, egresses, ring buffers, HLS stores, recording cancel tokens. Byte counters use `AtomicU64` for lock-free updates from hot ingest/egress paths; `health_snapshot()` reads them atomically to build the JSON for `/health`.

### SIMD acceleration

`src/media/simd.rs` provides runtime-dispatched SIMD for two hot-path operations:

- **`optimized_copy`**: bulk memcpy using widest available vector registers (AVX-512 64B / AVX2 32B / SSE2 16B / scalar fallback).
- **`find_sync_byte`**: MPEG-TS 0x47 sync marker search. Benchmarked at 894 ns (AVX2) vs 16 µs (scalar) on a 64 KB buffer — 18x speedup.

Detection uses `is_x86_feature_detected!()` which caches the CPUID result after first call (no syscall on hot paths).

### Ingest security

`src/media/security.rs` — per-IP rate limiter protecting RTMP/SRT stream key brute-force attempts. Tracks failure timestamps in a sliding window, bans IPs that exceed the threshold. Configurable via `IngestSecurityConfig` (failure limit, window, ban duration, tracked IP limit). State is in-memory (`std::sync::RwLock<HashMap>`).

### Authentication

Scrypt password hashing (via `scrypt` crate). Session cookies (`HttpOnly; SameSite=Strict`), stored in-memory `HashSet<String>` protected by `TokioRwLock`. Sessions persisted to SQLite for crash recovery. Default password: `admin`.

### Reconciliation loop

`src/lib.rs` runs a 1-second tick reconciler that:

1. Reads all outputs from SQLite, compares `desired_state` against active egress tokens.
2. Starts outputs where `desired_state == "running"` but no egress is active (with 5 s backoff on recent failures). Routes to RTMP, SRT, or HLS egress based on URL scheme.
3. Stops outputs where `desired_state == "stopped"` but egress is still active.
4. Auto-starts/stops recordings based on `recording_enabled` meta flag and ingest presence.

### Static asset serving

Frontend files (`public/`) are compiled into the binary via `rust-embed`. Served by Axum with disk-first fallback for development hot-reload.

---

## Codebase layout

```
src/
  main.rs              8 lines — entry point
  lib.rs             312 lines — app composition, reconciler loop, RTMP/SRT server spawn
  api.rs           1,546 lines — Axum router, 33 routes, 50 handlers, auth, embedded assets
  db.rs              768 lines — SQLite schema + 40 query functions (sqlx)
  diag.rs            641 lines — native diagnostics runner, SSE streaming
  types.rs            79 lines — Pipeline, Output, Job, Ingest, JobLog, HistoryFilters
  media/
    engine.rs        395 lines — central state: ingests, egresses, ring buffers, HLS stores
    ring_buffer.rs   181 lines — lock-free SPMC ring buffer (ArcSwap, 64-byte aligned slots)
    avio.rs          284 lines — in-memory FFmpeg I/O (MemoryQueue replaces TCP loopback)
    rtmp.rs          647 lines — RTMP ingest server + egress client via rml_rtmp
    srt.rs           468 lines — SRT ingest server + egress client via libsrt FFI
    hls.rs           277 lines — in-memory HLS segmenter (keyframe-split MPEG-TS)
    recording.rs     139 lines — MKV recording muxer (auto-deletes <5 s recordings)
    transcoder.rs    211 lines — in-process H.264/H.265 transcoder
    simd.rs          198 lines — AVX-512/AVX2/SSE2 memcpy and sync byte scan
    security.rs      101 lines — ingest rate limiter (per-IP failure tracking + bans)

tests/
  db.rs              396 lines — 12 tests (pipeline/output/job/ingest/meta/session CRUD)
  api.rs             525 lines — 16 tests (auth, pipeline/output CRUD, config, HLS, etc.)

test/
  run-2x3.sh         219 lines — 2x3 integration test (bash, replaces old Node.js runner)
  artifacts/          session-2x3-manifest.json (2 pipelines × 3 outputs)

benches/              6 benchmarks (ring buffer, AVIO throughput, SIMD, packet remux, transcoder)

old/                  archived Node.js/TypeScript codebase (not built or tested)
public/               frontend (plain TS/ES modules, no framework, served by rust-embed)
```

### Dependencies

| Crate | Purpose |
|-------|---------|
| `axum` 0.7 | HTTP framework (macros feature) |
| `sqlx` 0.7 | Async SQLite (runtime-tokio-rustls) |
| `tokio` 1.35 | Async runtime (full features) |
| `ffmpeg-next` 6.0 | FFmpeg bindings (codec, filter, format, resampling, scaling) |
| `rml_rtmp` 0.8 | Pure-Rust RTMP protocol (client + server sessions) |
| `arc-swap` 1 | Lock-free `ArcSwapOption` for ring buffer slots |
| `bytes` 1.5 | Zero-copy byte buffers |
| `sysinfo` 0.30 | System metrics (CPU, memory, disk, network) |
| `rust-embed` 8 | Compile frontend assets into binary |
| `scrypt` 0.11 | Password hashing |
| `mimalloc` 0.1 | Global allocator |
| `tower-http` 0.5 | CORS, compression, static file serving |
| `tokio-util` 0.7 | `CancellationToken` for graceful shutdown |
| `chrono` 0.4 | Timestamps |
| `libc` 0.2 | RLIMIT, socket buffer sizing |
| `nix` 0.27 | Socket/sched/fs helpers |
| `core_affinity` 0.8 | CPU pinning (future use) |

Release profile: `opt-level = 3`, `lto = "fat"`, `codegen-units = 1`, `panic = "unwind"`.

---

## API parity

All REST endpoints from the Node.js backend are implemented. Routes that existed only to proxy MediaMTX were intentionally dropped.

### Implemented (30 routes)

| Area | Routes |
|------|--------|
| **Auth** | `POST /api/auth/login`, `POST /api/auth/logout`, `POST /api/auth/change-password` |
| **Config** | `GET /config`, `PATCH /config` |
| **Pipelines** | `GET /pipelines`, `POST /pipelines`, `POST /pipelines/:id`, `DELETE /pipelines/:id` |
| **Outputs** | `POST /pipelines/:id/outputs`, `POST /pipelines/:id/outputs/:id`, `DELETE /pipelines/:id/outputs/:id` |
| **Output control** | `POST /pipelines/:id/outputs/:id/start`, `POST /pipelines/:id/outputs/:id/stop` |
| **History** | `GET /pipelines/:id/outputs/:id/history`, `GET /pipelines/:id/history` |
| **Diagnostics** | `GET /pipelines/:id/diagnostics` (SSE) |
| **Recording** | `POST /pipelines/:id/recording/start`, `POST /pipelines/:id/recording/stop` |
| **Encodings** | `GET /encodings/custom`, `PUT /encodings/custom` |
| **Ingests** | `GET /api/ingests`, `POST /api/ingests`, `PUT /api/ingests/:id`, `DELETE /api/ingests/:id`, `POST /api/ingests/:id/start`, `POST /api/ingests/:id/stop` |
| **Media files** | `GET /api/media`, `DELETE /api/media/:filename` |
| **Preview** | `GET /preview/hls/:id` (M3U8 playlist), `GET /preview/hls/:id/:segment` (TS segment) |
| **Health** | `GET /health` (full pipeline/output/input state), `GET /healthz` (liveness probe) |
| **Metrics** | `GET /metrics/system` (CPU, memory, disk, network) |
| **Stream keys** | `GET /stream-keys` (20 keys with RTMP/SRT ingest URLs) |
| **Audio caps** | `GET /audio-caps` (platform audio capabilities) |

### Dropped (4 routes)

| Route | Reason |
|-------|--------|
| `POST /internal/mediamtx/auth` | MediaMTX removed — auth happens natively in RTMP/SRT handlers |
| `GET /api/status` | Was a proxy to MediaMTX status API |
| `GET /api/status/mediamtx-config` | Was a proxy to MediaMTX config API |
| Grafana proxy routes | Can be re-added if needed |

---

## RTMP implementation details

**Ingest** (`src/media/rtmp.rs` lines 57-250): TCP listener on `0.0.0.0:1935`. Per-connection flow:

1. RTMP handshake (C0/C1/S0/S1/S2) via `rml_rtmp::handshake`.
2. `ServerSession` processes publish requests — validates stream key against SQLite pipeline table.
3. Rate limiting: checks `IngestSecurityService` before accepting. Bans IP on repeated failures.
4. Socket tuning: `TCP_NODELAY` enabled, 8 MB `SO_RCVBUF`/`SO_SNDBUF` to absorb network bursts.
5. Video/audio data → `MediaPacket` pushed to pipeline `RingBuffer`. Keyframe detection via FLV FrameType byte (`(data[0] >> 4) == 1`).
6. On disconnect: `engine.unregister_ingest()` cancels the ingest token.

**Egress** (`src/media/rtmp.rs` lines 414-608): RTMP client connection to target URL. Flow:

1. Parse URL → `(host, port, app_name, stream_key)`.
2. Client handshake + `request_connection()` + `request_publishing()`.
3. `tokio::select!` loop: reads server acknowledgements while forwarding `RingBuffer` packets via `Reader`. Cancellable via `CancellationToken`.

## SRT implementation details

**Ingest** (`src/media/srt.rs`): Raw `libsrt` FFI bindings — `srt_startup`, `srt_create_socket`, `srt_bind`, `srt_listen`, `srt_accept`, `srt_recv`, `srt_send`, `srt_setsockopt`, etc. Listener on port 10080. Stream ID parsing for authentication (`publish:live/<stream_key>`). MPEG-TS data piped through `MemoryQueue` → FFmpeg demuxer on a dedicated OS thread. Publishes all video and audio streams (not just "best") into the RingBuffer with per-track indices for multi-track audio.

**Egress**: SRT client connecting to target URL, forwarding ring buffer packets. Same `CancellationToken` pattern as RTMP egress.

---

## Test coverage

### Unit tests — 28 total, all passing

**DB layer** (`tests/db.rs` — 12 tests):
- `pipeline_crud` — create, read, list, update, delete
- `update_nonexistent_pipeline_returns_none` — returns `None`, not error
- `output_crud` — create, read, list, update, set desired state, delete
- `cascade_delete_removes_outputs` — SQLite foreign key cascade
- `job_lifecycle` — create, get running, update status/exit_code, verify stopped
- `job_upsert_on_conflict` — second job replaces first for same pipeline+output
- `job_logs` — append + list by job ID and by output
- `ingest_crud` — create, list, update, delete
- `meta_operations` — get/set/overwrite key-value pairs
- `session_operations` — create, list, delete session tokens
- `reset_running_jobs` — marks all `running` jobs as `stopped` with `SIGKILL` signal
- `filtered_job_logs` — `HistoryFilters` with limit, order, prefix filtering

All DB tests use `SqlitePool::connect("sqlite::memory:")` — no disk, no external dependencies.

**API layer** (`tests/api.rs` — 16 tests):
- Auth: `healthz_no_auth`, `login_wrong_password`, `login_success_and_logout`, `unauthenticated_returns_401`, `change_password`
- Pipeline CRUD: `pipeline_crud_via_api` (create → list → update → delete)
- Output CRUD: `output_crud_via_api` (create → start → verify DB state → stop → delete)
- Config: `config_get_returns_structured_data`, `config_patch_server_name`
- Stream keys: `stream_keys_requires_auth`, `stream_keys_returns_array` (verifies 20 keys with RTMP/SRT URLs)
- Audio caps: `audio_caps_no_auth`
- Ingest CRUD: `ingest_crud_via_api` (create → list → delete)
- Custom encoding: `custom_encoding_roundtrip` (PUT → GET roundtrip)
- HLS: `hls_preview_no_stream_returns_404`, `hls_segment_bad_name_returns_400`

All API tests use `tower::ServiceExt::oneshot()` — zero network overhead. Each test constructs a fresh `AppState` with in-memory SQLite, `IngestSecurityService`, and `MediaEngine`.

### Benchmarks — 6

| Benchmark | What it measures |
|-----------|-----------------|
| `ring_buffer` | SPMC push/pull throughput under concurrent reader contention |
| `avio_throughput` | In-memory AVIO queue vs TCP loopback latency |
| `simd_memcpy` | AVX-512/AVX2/SSE2 memcpy vs `copy_from_slice` |
| `simd_search` | MPEG-TS sync byte scan across ISA tiers |
| `packet_remux` | Packet demux → remux roundtrip |
| `transcoder_throughput` | End-to-end transcode frame rate |

### Integration test

`test/run-2x3.sh` — bash script (replaces old Node.js `run-2x3.mjs`). Steps:

1. Verify prerequisites (ffmpeg, curl, jq, input file, manifest).
2. Wait for app reachability at `/healthz`.
3. Login with default password.
4. Create/ensure 2 pipelines with 3 outputs each from manifest.
5. Start 2 ffmpeg publishers (one RTMP, one SRT) using colorbar test input.
6. Start all 6 outputs via API.
7. Poll `/health` until all inputs are `on` and all outputs are `on` (timeout: 120 s).
8. Stop all outputs via API.
9. Verify all outputs reach `stopped` state (timeout: 60 s).
10. Cleanup: kill ffmpeg publishers, delete created resources.

---

## What's left

### Must-do before merge

- [ ] Run the full 2x3 integration test against the Rust binary with live media
- [ ] Clippy clean pass (`cargo clippy -- -W clippy::all`)
- [ ] CI pipeline updates — GitHub Actions currently runs `npm` commands; needs `cargo` equivalents
- [ ] Frontend verification — all dashboard features should work transparently (same REST API, same SSE format), but needs manual testing

### Nice-to-have / follow-up

- [ ] File-based ingest (`/api/ingests/:id/start` currently stubs `running: true` without spawning ffmpeg)
- [ ] SRT encryption/passphrase support
- [ ] Grafana proxy routes (if still needed)
- [ ] Deployment scripts (`old/scripts/`) — Rust equivalents or Dockerfile for the new binary
- [ ] Connection draining / graceful shutdown on SIGTERM
- [ ] Metrics export (Prometheus endpoint or structured logging)
- [ ] Delete `old/` directory once rewrite is validated in production
