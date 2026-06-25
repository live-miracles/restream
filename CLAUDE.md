# CLAUDE.md

# Guidelines
- Always write correctness tests
- Hotpath code must be benchmarked before and after to justify changes to it
- Before committing bring docs up-to-date
- Group changes logically and commit incrementally
- Make sure not to club other agent's changes when stashing or committing

## Build & Test Commands
Standard cargo commands (`cargo build`, `cargo test`, `cargo fmt`, `cargo clippy`) work as expected.
- **Frontend Compile**: `npx tsc -p tsconfig.frontend.json` (Always edit `public/ts/` — never the generated `public/js/`).
- **Tailwind Build**: `npx tailwindcss -i public/input.css -o public/output.css`
- **Benchmarks**: `cargo bench --bench <name>` (run before and after any hotpath change).
- **Integration Tests**: `./test/run-integration.sh <mode>` — runs in a private loopback namespace by default (no port conflicts). Pass `--host` to run directly. Modes: `ramp` (8 configs, outputs added one-by-one, per-step RSS snapshots), `mixed-scale` (concurrent load; h264-srt anchor: HLS+smoke+lifecycle; h265-srt: TC_SPAWNS; multi-audio), `bonding` (SRT socket bonding, requires static build).

See [README.md § Development](README.md#development) for full dev setup, prerequisites, inner loop, and benchmark suite reference.

## Key Constraints
- **Ports**: Defaults are HTTP=3030, RTMP=1935, SRT=10080. Override via `RESTREAM_HTTP_PORT`, `RESTREAM_RTMP_PORT`, `RESTREAM_SRT_PORT` env vars.
- **Database**: SQLite at `data.db` by default. Override path via `RESTREAM_DB_PATH` (e.g. `RESTREAM_DB_PATH=/data/restream.db`). No migrations; schema created with `CREATE TABLE IF NOT EXISTS` at startup.
- **Media Directory**: Defaults to `media/`. Override via `RESTREAM_MEDIA_DIR` (e.g. `RESTREAM_MEDIA_DIR=/data/media`). Used for file-ingest sources and `.mkv` recordings.
- **HLS Segmenter**: In-memory storage only (`VecDeque<Bytes>` inside `HlsStore`), no disk I/O.
- **Frontend Assets**: Statically embedded in the binary via `rust-embed` (disk-first fallback in dev).

## Hotpath & Performance Guidelines
When modifying hotpath code (files under [src/media/](src/media/) or other performance-critical components), follow the principles in [high-performance-data-path.md](docs/high-performance-data-path.md):

1. **Always Benchmark Before & After**: Run the relevant Criterion benchmarks (`cargo bench --bench <bench_name>` under [benches/](benches/)) before and after changes.
2. **Burst-Oriented Design**: Change the unit of work from one packet, one lookup, and one wakeup to a bounded burst (`push_batch` / `pull_burst`).
3. **Direct Hot Handles**: Cache `Arc<RingBuffer>`, `Arc<AtomicU64>` byte counters, and other handles at connection setup. Never do registry map/lock lookups on the packet loop.
4. **Run-to-Completion**: Perform packet-local operations (parse, classify, normalize timestamps, account, publish) on the worker thread itself instead of spawning new tasks/channels.
5. **Zero-Copy Optimization**: Avoid payload copies. Transfer `Bytes`/`BytesMut` ownership or reuse vectors. Use `drain_into()` not `drain()` to retain vector allocations.
6. **Hoist Batch Buffers**: Declare burst-drain `Vec`s (packets, ts_batch, conv_buf) **before** the `loop {}`, call `.clear()` inside each arm. `Vec::with_capacity(N)` inside a loop re-allocates on every burst cycle — one alloc per ~8 ms at 30 fps.
7. **No Regressions**: Verify correctness gates (PTS/DTS ordering, keyframe alignment, format compliance via probes/ffprobe) after any change. Performance must not come at the cost of protocol correctness.

### SIMD / Vectorization
Vectorization opportunities exist in MPEG-TS sync-byte scanning, CRC32 computation, NALU start-code search, PES header parsing, timestamp extraction across packet bursts, and byte-pattern matching in codec probes.

Rules for adding vectorized code:
1. **Benchmark the scalar path first** with Criterion (`cargo bench`). Only add SIMD if the scalar version is a measured bottleneck. Never speculatively vectorize.
2. **Runtime width selection with scalar fallback**: Use `std::arch::is_x86_feature_detected!` (or equivalent) to pick the widest available path at startup or first call. Cache the function pointer / enum choice — never re-detect per packet. Provide a pure-scalar fallback that is always compiled and always reachable.
   ```rust
   // Example dispatch pattern
   type ScanFn = fn(&[u8]) -> Option<usize>;
   static SCAN: OnceLock<ScanFn> = OnceLock::new();
   fn pick_scan() -> ScanFn {
       #[cfg(target_arch = "x86_64")] {
           if is_x86_feature_detected!("avx2") { return scan_avx2; }
           if is_x86_feature_detected!("sse4.2") { return scan_sse42; }
       }
       scan_scalar
   }
   ```
3. **Target-feature gating**: Mark SIMD functions with `#[target_feature(enable = "...")]` and keep them in a dedicated module (e.g., `simd.rs` or `simd/`). Do not enable target features globally — it breaks the scalar fallback on older CPUs.
4. **Unsafe discipline**: SIMD intrinsics require `unsafe`. Keep the unsafe block minimal — just the intrinsic calls. Bounds-check slice access outside the unsafe block. Document the safety invariant (alignment, length) on each function.
5. **Width progression**: SSE2 (128-bit, x86-64 baseline) → AVX2 (256-bit) → AVX-512 (512-bit, only if benchmarks show a win). Older Intel CPUs throttle core frequency on AVX-512; recent chips (Sapphire Rapids+, Zen 4+) do not — but the wider width still only helps if the hot loop is memory-bandwidth or throughput-bound, not latency-bound. Benchmark on target hardware before committing to an AVX-512 path.
6. **Testing**: The scalar fallback is the test oracle. Add a property test or exhaustive small-input test that asserts `simd_fn(input) == scalar_fn(input)` for every width variant.

## Media Pipeline Rules
When modifying any code in [src/media/](src/media/), follow the pipeline design in [media-pipeline.md](docs/media-pipeline.md) and the architecture in [architecture.md](docs/architecture.md):

### Threading Model
- **Tokio tasks** own sockets, API handlers, timers, connection lifecycle, and inline demux/mux (RTMP ingest/egress, SRT ingest with TsDemuxer, SRT egress feed with TsMuxer, HLS segmenter).
- **Dedicated `std::thread`** for blocking FFmpeg work and blocking `srt_send()`: transcoder stages, recording muxer, SRT egress sender.
- **Never block the tokio runtime**: FFmpeg codec calls (`avcodec_decode_video2`, `avcodec_encode_video2`, `av_interleaved_write_frame`) and `srt_send()` must run on OS threads, not tokio tasks.
- **`catch_unwind(AssertUnwindSafe(…))`**: Wrap all FFmpeg/libsrt OS thread entry points to prevent panics from crashing the process.

### Packet Contract
- `MediaPacket.format` (`PayloadFormat::Flv` or `PayloadFormat::Raw`) is set by the producer and **must** be checked by every consumer. RTMP ingest produces `Flv`; SRT TsDemuxer, transcoder output, and native MPEG-TS demuxer produce `Raw`.
- Consumers must strip FLV headers (5-byte video, 2-byte audio) from `Flv` payloads before MPEG-TS muxing. `Raw` payloads pass through directly.
- Never replace PTS/DTS with wall-clock timestamps. Store and analyze media time and application time independently.
- RTMP video timestamps are DTS. The signed composition-time offset lives in the FLV payload: `PTS = DTS + offset`. Use `packet.dts` as the RTMP message timestamp for video; `packet.pts` for audio.

### Ring Buffer
- 4096-slot SPMC, lock-free via `ArcSwapOption`. Single-producer assumption (one ingest per pipeline).
- Use `push_batch()` / `pull_burst()` for burst publication and consumption (up to 32 packets).
- Overflow recovery: readers fast-forward to latest keyframe via `last_keyframe_idx`.
- Do not add per-packet logging, serialization, or channel sends on the ring hot path.

### Stage Sharing
- Video stages are shared by preset (e.g., one `720p` encoder for all `720p` outputs).
- Audio stages are keyed by **both** the audio operation **and** the upstream video stage (e.g., `audio:atrack:0:from:video:720p`). Never key audio stages by audio operation alone — this causes cross-contamination between presets.
- Task "active" state is cancellation-token presence, not worker health. A native thread can fail while its token remains active.

### Protocol Correctness
- Probe with the matching ingest protocol (RTMP ingest → RTMP probe, SRT ingest → SRT probe). Cross-protocol probing creates false positives.
- Emit only media streams: one video + intended audio tracks. Reject subtitles, private data, unknown stream types. The MPEG-TS remuxer must not guess codecs for unknown PIDs.
- SRT stream IDs: strip query parameters before database lookup. Accept all standard forms (`publish:live/<key>`, `#!::r=live/<key>,m=publish`, bare `<key>`, etc.).
- Bonded SRT: two independent sockets with matching StreamID are **not** a bond — reject as duplicate publishers. Only libsrt group connections (via `SRTO_GROUPCONNECT`) create bonds.
- A/V sync: every mux consumer must call `DtsEnforcer::enforce()` with `stream_idx` 0=video, 1…=audio by position in `audio_tracks` (not by `track_index` value). Never apply wall-clock offsets to ring-buffer timestamps. Regression test: `cargo test av_sync` (covers 48h drift-free, cross-stream isolation, RTMP wrap boundary).

### MemoryQueue / AVIO
- `MemoryQueue` (Mutex + Condvar) connects tokio tasks to blocking FFmpeg threads. Use `write_batch()` to amortize lock acquisition.
- FFmpeg AVIO callbacks run under the MemoryQueue lock. Keep callbacks minimal — no allocation, no logging.
- Retain `BytesMut` capacity across writes. Use `Bytes::from(Vec<u8>)` for zero-copy freeze.

### What NOT to Do on the Hot Path
- No per-packet allocation for diagnostics
- No locks, logging, serialization, or async channel sends
- No wall-clock or system calls per packet (except monotonic timestamp reads)
- No payload copies (use `Bytes` ref-counting)
- No unbounded event collection
- No diagnostic reader that changes the production pipeline
- No high-cardinality metrics per packet
