# High-Performance Media Data Path

This document turns the media data-path audit into an incremental implementation
and measurement plan. The application should retain Tokio and the operating
system's TCP/SRT stacks while applying proven high-performance packet-processing
principles inside ingest, fan-out, packaging, and sender stages.

The governing rule is:

> Change the unit of work from one packet, one lookup, and one wakeup to a
> bounded burst owned by a stable worker.

No change should be accepted because it merely resembles a fast networking
framework. Every step must preserve protocol correctness and demonstrate an
improvement in production-shaped measurements.

## Progress Log

| Step | Status | Result |
|---|---|---|
| Baseline suite and roadmap | Complete in `e266608` | Added lookup, ring, fan-out, queue, and layout measurements |
| Clean benchmark builds | Complete in `205aae2` | Removed four test-harness warnings from benchmark compilation |
| Direct RTMP ingest handles | Complete in `5299db4` | Ring and byte-counter access fell from ~119 ns to ~7.3 ns, about 94% lower |
| Compact ring slots | Complete in `ad4ac9b` | Controlled pinned-CPU rerun: storage fell 256 KiB → 32 KiB, producer throughput was neutral, 32-packet consumer improved ~5.7%, and 500-reader fan-out improved ~9.6% |
| Burst ring primitives | Complete in `e0f33ac` | `push_batch()` improved 32-packet publication by ~15%; `pull_burst()` improved 8-packet consumption by ~17%. All 14 ring tests pass. Existing single-packet APIs remain the latency path |
| Burst adoption in internal stages | Complete in `95f2849` | HLS, recording, and transcoder feeders drain reusable 32-packet bursts. The primitive measured up to ~17% faster; module tests and all-target compilation pass |
| Bounded chunk queues | Pending | Not started |
| Batched AVIO queue writes | Complete in `10eaaf6` | One lock and notification per burst reduced the 32 × 1316-byte queue round trip from ~7.49 μs to ~3.84 μs, about 49% lower. Adopted by HLS, recording, and transcoder feeders |
| Shared package stages | Pending | Not started |
| Worker sharding and local pools | Pending | Not started |
| Batch metadata, prefetch, and vectorized-search refinement | Complete | TS resync and Annex B start-code scanning are vectorized using `memchr` (`find_annexb_start_codes` in `codec.rs` and `for_each_nal_raw` in `mpegts.rs`). Production TS resync uses `memchr` (64 KiB scan is ~631 ns). Annex B NAL scanning uses runtime-dispatched `memchr::memmem` at ~118.73 GiB/s, outperforming `wide` (~40.25 GiB/s) and `pulp` (~4.14 GiB/s) while avoiding complex custom target-feature configurations. |
| Zero-copy HLS segment finalization | Complete in `24fd309` | 8 MiB finalization fell from ~4.26 ms to ~347 ns by transferring `BytesMut` ownership, over 99.99% lower |
| Native shared HLS packaging | Complete in `a5d736f` | Replaced the FFmpeg queue plus two-OS-thread path with inline `TsMuxer`, a reusable accumulator, one shared segmenter per pipeline, demand-driven browser heartbeats, and persistent-output reference tracking |
| Native HLS cost benchmark | Complete in `a5d736f` | Mux-only cost is ~27–147 µs per second of content across 720p30 to 4K30 profiles; six-second mux/accumulate/store cost is ~0.23–2.75 ms. A twenty-segment window retains roughly twice the original ten-segment estimate |
| Lock-free stage telemetry | Complete through `c332c90` | Graph, health, and telemetry views now render through `engine_views.rs`, while transport/API call sites use `MediaEngine` façade helpers instead of reaching into nested registries directly. Benchmarked control-plane helper lookups stayed on par with or better than direct registry reads |
| Native MPEG-TS data-path audit | Complete in `abc558b` | New demux/mux path adds major opportunities in PID dispatch, reusable drains, PES construction, direct TS output, batch APIs, and SIMD-assisted resynchronization/NAL scanning |
| Reusable MPEG-TS demux drains | Complete in `a741faf` | Real 6.7 MB fixture replay improved from ~12.44 ms to ~11.36 ms, about 8.7%; all 15 MPEG-TS tests pass |
| Direct MPEG-TS PID dispatch | Complete in `a741faf` | 8192-entry PID table reduced pinned fixture replay from ~11.40 ms to ~8.54 ms, about 25.9%; throughput improved ~34.9% |
| Vectorized TS resync | Complete in `467e5c9` | 64 KiB corrupted prefix: portable `memchr` scan ~631 ns (~96.8 GiB/s), removed hand-written scanner ~907 ns, scalar scan ~16.4 µs. Production keeps stride verification after candidate search |
| Cumulative native demux | Complete in `ed40c91` | After all native MPEG-TS optimizations, 6.7 MB fixture replay runs in ~4.28 ms (1.45 GiB/s), down from original ~12.44 ms—65.6% lower end-to-end |
| Zero-copy PES muxer | Complete in `5eaedcd` | Stack-resident PES header + direct TS output eliminated payload copy and temp arrays; 6.7 MB mux time fell from ~880 µs to ~490 µs (45% faster, 6.8 → 12.3 GiB/s) |
| Native codec assembly | Complete in `8b03aad` | Matched pinned runs show the intended 4K HEVC decode/scale/H.264 encode chain at 2.49 s versus 5.45 s without FFmpeg x86 assembly, a 2.19× speedup; static setup now verifies assembly support |
| Cached SRT ingest byte counter | Complete in `4eb8ea6` | Cloned the `Arc<AtomicU64>` before the receive loop, replacing a per-receive `active_ingests.read().await` + HashMap lookup with a direct `fetch_add` |
| Cached egress byte counters | Complete in `a9c534f` | RTMP and SRT egress paths now cache `bytes_sent` counter before their send loops, replacing per-packet/batched `update_egress_bytes()` async lookups |
| Production transcoder-stage benchmark | Complete in `76d3969` | Replaced the fake `Bytes::clone()` benchmark with the exact FFmpeg `MemoryQueue` + custom-AVIO stage. Current `source` passthrough processes the 6.4 MiB fixture in ~26.8 ms (~238 MiB/s) |
| Actual decode/filter/encode transcoder | Missing | Resolution presets configure encoder metadata but currently remux original compressed packets; implement and benchmark the real decoder, scaler/filter graph, encoder, output demux, and ring publication path |
| Hoist burst-drain Vecs before loop | Complete | `transcoder.rs` and `h264_transcoder.rs` feeder loops fixed. Benchmark `burst_drain_alloc` (bench-dev, x86-64 Zen): `alloc_per_burst` ~2.79 µs vs `hoisted_clear` ~2.54 µs per 32-packet burst — **~9% faster**, ~250 ns saved per burst cycle. At 5 bursts/s per consumer the saving compounds across all egress stages (HLS, recording, SRT, transcoder feeders). |
| Custom AVIO teardown | Fixed in `76d3969` | Production-context benchmarking exposed a custom `AVIOContext` double-close. Contexts now remain owned by their wrappers, which detach `pb` before FFmpeg context destruction; repeated benchmark iterations complete cleanly |
| `MediaPacket` field reordering + `#[repr(C)]` | Complete | Without `#[repr(C)]`, rustc's greedy-alignment heuristic places `payload: Bytes` (32 bytes) first, pushing `media_type`/`is_keyframe`/`pts`/`dts` to ArcInner offsets 52–71, spanning two cache lines. With `#[repr(C)]` and the declared field order, all hot consumer fields (type dispatch, track routing, timestamps, payload ptr+len) land in cache line 0 (ArcInner bytes 0–63); only the Bytes Arc management fields land in cache line 1. `#[repr(u8)]` on `MediaType` and `PayloadFormat` guarantees 1-byte enum size. |
| `const` CRC-32/MPEG-2 table | Complete | Replaced `static OnceLock<[u32; 256]>` with `const CRC32_TABLE: [u32; 256]` computed at compile time. All operations (loops, bit shifts, conditionals) are valid in `const fn`. Eliminates the atomic acquire load on every PAT/PMT write (~500 ms interval). Table lives in `.rodata`, no first-call latency. |
| Sentinel `u8` for continuity counter and PMT version | Complete | `StreamInfo.continuity: Option<u8>` → `u8` with `CC_UNSET = u8::MAX` (valid CC values 0–15); `TsDemuxer.pmt_version: Option<u8>` → `u8` with `PMT_VER_UNSET = u8::MAX` (valid version 0–31). Removes the discriminant byte and the `Option`-unwrap branch on every TS packet processed. |
| BytesMut burst alloc in SRT shared muxer | Complete | `srt.rs` shared muxer replaced per-chunk `Bytes::copy_from_slice` (one `malloc+memcpy` per muxed packet) with a single `BytesMut::with_capacity(65536)` per burst, then `Bytes::slice()` for each chunk (refcount bump only, no malloc). Benchmark `ts_chunk_burst_alloc` (bench-dev, x86-64 Zen): `per_chunk_copy_from_slice` ~3.93 µs vs `burst_bytesmut_then_slice` ~2.23 µs per 32-chunk burst — **~43% faster**, ~1.7 µs saved per burst. |

## Per-Frame Allocation Audit (2026-06-23)

Measured allocations on the hot path per video frame (~1080p30, H.264, RTMP
ingest). "Warm" = after the first few frames when internal buffers have grown to
steady-state. All counts assume `PayloadFormat::Raw` egress (most common after
transcoder); FLV egress adds the AVCC→Annex B conversion Vecs.

### Ingest

| Stage | Per-packet allocs | Notes |
|---|---|---|
| RTMP socket → `rml_rtmp` | 1 (payload `Bytes`) | Library owns heap; `MediaPacket` borrows the ref |
| `ring.push(packet)` | 1 (`Arc<MediaPacket>` ~40 B) | Evicts old `Arc` on slot overwrite |
| **SRT → `TsDemuxer`** | **1 (frame copy)** | **Fixed (2026-06-23): was 3–8 (PES buf regrow)**. `flush_pes` now uses `Bytes::copy_from_slice` + `reset()` so the `Vec` capacity is retained across frames. |
| `push_batch` to ring | 1 `Arc` per packet | Same as RTMP |

**PES buffer fix detail**: `flush_pes` previously called `std::mem::take(&mut pes.buf)` which transferred the `Vec` to `Bytes::from()` (zero-copy) but left a zero-capacity `Vec` behind. The next frame restarted from capacity 0, triggering 3–8 `realloc` calls (doubling from 0→1→2→4→...→frame_size). For a 200 KB IDR that was ~8 reallocations per frame. The fix: `Bytes::copy_from_slice(&pes.buf)` (one allocation of exactly frame_size bytes) + `pes.reset()` (clears length, **preserves capacity**). Net: same 1 allocation per frame but 0 realloc cascades.

### Egress — per output per video frame

| Consumer | Format | Allocs | Notes |
|---|---|---|---|
| RTMP egress | Raw→FLV | 1 large (AVCC copy) + 2 small (NALU position Vecs) | `video_for_rtmp_into` → `annexb_to_avcc_into` → `split_annexb_nalus`. AVCC output is unavoidable (RTMP library needs to own it). 2 small Vecs ~48 B each. |
| RTMP egress | Flv→FLV | 0 | FLV passthrough: `payload.clone()` = `Arc` refcount only |
| SRT/HLS egress | Raw | 0 | `video_for_ts_into` returns `&payload` directly (zero-copy). `TsMuxer::output` pre-allocated, reused. |
| SRT/HLS egress | Flv→Raw | 2 small | `avcc_to_annexb_into` → `split_annexb_nalus`: 2 Vecs ~48 B each. Written into `video_conv_buf` (no extra alloc). |
| Recording | same as HLS | 2 small or 0 | |
| H264-transcoder feed | Flv→Raw | 0 | Migrated to `_into` variants (2026-06-23). |

### `annexb_to_avcc` scratch variant

`annexb_to_avcc_with_scratch(data, out, sc_scratch)` eliminates both small Vecs
by reusing a caller-provided `Vec<(usize,usize)>`. Benchmarked 2026-06-23:

| Input | `two_pass` | `with_scratch` |
|---|---|---|
| P-frame 8 KiB, 1 NALU | 2.73 µs | **1.80 µs (+34%)** |
| P-frame 30 KiB, 3 NALU | 9.83 µs | **8.95 µs (+9%)** |
| IDR 80 KiB, 1 NALU | **16.98 µs** | 24.07 µs (-42%) |

**Current production choice: `two_pass`** (wins for dominant large IDR case). Re-evaluate if workload shifts to many small NALUs.

### Unbounded allocation risks

| Structure | Bound | Location |
|---|---|---|
| `MemoryQueue::VecDeque<u8>` | Bounded to 2 MB (steady-state ≈ 1.5 MB at 50 Mbps/250 ms) | `src/media/avio.rs`; 2 per transcoder |
| `TsMuxer::output: Vec<u8>` | largest TS burst per frame ≈ `frame_size / 1316 × 188` bytes | per consumer; stabilises at IDR size |
| `PesAccumulator::buf` | `MAX_PES_BUFFER` constant in `mpegts.rs` | per stream per demuxer |
| `TsDemuxer::remainder` | `TS_PACKET_SIZE` = 188 bytes | per demuxer |
| `sps_pps_cache: Vec<u8>` | SPS+PPS size ≈ 50 bytes | per consumer |
| `HLS accumulator: BytesMut` | segment size ≈ bitrate × 6s ≈ 18 MB at 24 Mbps | shared across all HLS outputs per pipeline |
| `IngestSecurityService` HashMap | `tracked_ip_limit` (default 10 000 entries) | enforced since 2026-06-23 fix |

All structures are bounded. `MemoryQueue` is the largest steady-state allocation
and is proportional to stream bitrate × transcoder latency.

## Rust Zero-Cost Abstraction Patterns

These are the idioms that actually matter in this codebase, with the rule and the
anti-pattern side-by-side. Future code in `src/media/` must follow them.

### Rule 1 — Hoist burst-drain Vecs before the loop

A `Vec::with_capacity(N)` inside a `tokio::select!` arm allocates on every
burst cycle (~every 8 ms at 30 fps video). Hoist it before the `loop {}` and
call `.clear()` at the start of the arm.

```rust
// WRONG — new allocation per burst
loop {
    tokio::select! {
        _ = reader.wait_for_data() => {
            let mut packets = Vec::with_capacity(32); // ← alloc here
            reader.pull_burst(&mut packets, 32)?;
        }
    }
}

// CORRECT — one allocation, retained across bursts
let mut packets = Vec::with_capacity(32);
loop {
    tokio::select! {
        _ = reader.wait_for_data() => {
            packets.clear();                          // ← just zeroes len
            reader.pull_burst(&mut packets, 32)?;
        }
    }
}
```

The same rule applies to `ts_batch`, `video_conv_buf`, `audio_conv_buf`, and
every other scratch buffer used in a packet loop. A buffer declared outside the
loop retains its heap capacity indefinitely; a buffer declared inside re-triggers
the allocator on every burst.

**Measured (bench-dev, x86-64 Zen, `burst_drain_alloc` group, 32-packet burst):**

| Variant | Time per burst | Throughput |
|---|---|---|
| `Vec::with_capacity(32)` inside arm (old) | ~2.79 µs | ~11.5 Melem/s |
| Hoisted + `.clear()` (new) | ~2.54 µs | ~12.6 Melem/s |
| **Improvement** | **~9% faster, ~250 ns/burst** | |

**Files where this is done correctly**: `hls.rs`, `srt.rs` (play sender),
`recording.rs`.

### Rule 2 — Use `_into` codec variants with per-consumer scratch buffers

Every payload conversion function has a `_into` variant that writes into a
caller-provided `Vec<u8>` instead of returning a freshly allocated `Vec`:

| Allocating (avoid on hot path) | Zero-allocation (use this) |
|---|---|
| `video_for_ts(payload, fmt, ...)` → `Cow<[u8]>` | `video_for_ts_into(payload, fmt, ..., buf)` → `&[u8]` |
| `audio_for_ts(payload, fmt, ...)` → `Cow<[u8]>` | `audio_for_ts_into(payload, fmt, ..., buf)` → `&[u8]` |
| `avcc_to_annexb(data, nls)` → `Vec<u8>` | `avcc_to_annexb_into(data, nls, out)` |
| `annexb_to_avcc(data)` → `Vec<u8>` | `annexb_to_avcc_into(data, out)` |
| `video_for_rtmp(payload, kf)` → `Vec<u8>` | `video_for_rtmp_into(payload, kf, out)` |
| `audio_for_rtmp(payload)` → `Vec<u8>` | `audio_for_rtmp_into(payload, out)` |

Hold one `video_conv_buf` and one `audio_conv_buf` per consumer task, declared
before the loop. The `_into` variant clears the buffer and writes into it; on
the `Raw` passthrough path it returns the original slice directly (zero-copy).

### Rule 3 — `drain_into` over `drain` to retain `TsDemuxer` output capacity

`TsDemuxer::drain()` uses `std::mem::take`, which strips the internal output
`Vec`'s capacity on every call. `drain_into(&mut caller_vec)` uses
`Vec::append`, which transfers elements while leaving both vectors' allocations
intact.

```rust
// WRONG
let pkts = demuxer.drain();   // demuxer's Vec → capacity 0 next call

// CORRECT
demuxer.drain_into(&mut pkts); // demuxer keeps its allocation
```

`drain_into` is already the production API on all hot paths (SRT ingest,
external transcoder). Never introduce a call to `drain()` in a packet loop.

### Rule 4 — `Cow<'a, [u8]>` for conditional-allocation paths

Use `Cow<'a, [u8]>` when a function sometimes borrows and sometimes converts.
`Cow::Borrowed(slice)` is a zero-cost borrow; `Cow::Owned(vec)` signals an
allocation happened. This makes the fast path (Raw passthrough) pay nothing.

`video_for_ts` / `audio_for_ts` in `codec.rs` demonstrate this: the
`PayloadFormat::Raw + ADTS present` path returns `Cow::Borrowed` without
touching any allocator.

### Rule 5 — `OnceLock` for lazily-computed statics

One-time setup that would otherwise run per-packet (table generation, pattern
compilation, path resolution) belongs in a `static OnceLock<T>`. After the
first call the read path is a single atomic load.

Examples in this codebase:
- CRC-32/MPEG-2 lookup table — computed once, O(1) thereafter (`mpegts.rs`)
- `memchr::memmem::Finder` — needle pre-compiled once (`codec.rs`)
- `FFMPEG_BIN_PATH` — resolved once at startup (`ffmpeg_extract.rs`)

### Rule 6 — `Bytes::from_owner` for FFmpeg zero-copy publishing

`OwnedFfmpegPacket(ffmpeg_next::Packet)` wraps an `AVBufferRef`-backed FFmpeg
packet and implements `AsRef<[u8]>`. `Bytes::from_owner(OwnedFfmpegPacket(pkt))`
creates a `Bytes` that holds the FFmpeg refcount — no `memcpy` into a new
buffer. Drop of the last `Bytes` clone calls `av_packet_unref`.

Do not replace this with `Bytes::copy_from_slice(pkt.data())` unless the FFmpeg
buffer cannot be shared (e.g. the encoder reuses it immediately).

### Rule 7 — `#[repr(align(64))]` for writer-owned atomics

Producer-owned counters (`write_idx`, `last_keyframe_idx`) are wrapped in
`AlignedAtomicUsize { #[repr(align(64))] }` so each lands on its own cache
line. This eliminates false sharing between:
- the producer writing `write_idx`
- readers loading `write_idx`
- readers loading `last_keyframe_idx` on overflow recovery

Do not store writer-hot atomics alongside reader-hot or control-plane data in
the same struct without explicit alignment padding.

### Rule 8 — `#[inline]` on per-packet helpers

Functions called on every packet in a hot loop must carry `#[inline]` so they
are inlined in non-LTO builds (tests, benches with `bench-dev` profile). In
release with `lto = "fat"` the compiler inlines regardless, but explicit hints
improve profiler output and benchmark accuracy.

Apply `#[inline]` to: `video_for_ts_into`, `audio_for_ts_into`,
`avcc_to_annexb_into`, `annexb_to_avcc_into`, `video_for_rtmp_into`,
`find_ts_sync`, `h264_is_keyframe`, `h265_is_keyframe`, and any new function
that is called once per TS packet or media packet.

### Quick checklist for new hot-path code

- [ ] Batch `Vec`s declared **before** the `loop {}`
- [ ] `_into` codec variant used (not the `Cow`-returning version)
- [ ] No `drain()` — use `drain_into` or `Vec::append`
- [ ] No `Vec::with_capacity` or `String::from` inside packet loops
- [ ] Scratch buffers cleared with `.clear()`, not replaced
- [ ] New per-packet helper carries `#[inline]`
- [ ] No `Arc::clone` or `Bytes::clone` inside packet loops (use `Arc` handles cached before the loop)

## Current Baseline

Existing strengths:

- compressed payloads use reference-counted `Bytes`;
- source fan-out uses a single-producer, multi-consumer ring;
- global ring indexes are cache-line aligned;
- identical encoding stages can be shared;
- FFmpeg provides optimized native codec implementations;
- release builds use optimization, fat LTO, and one codegen unit.

The critical path remains predominantly packet-at-a-time:

```text
socket read
  -> protocol parse
  -> locked pipeline lookup
  -> locked counter lookup
  -> packet wrapper allocation
  -> ring push
  -> wake all waiters
  -> one reference-counted ring load per reader
  -> per-output package or mux
  -> socket write
```

| Area | Current behavior | Consequence |
|---|---|---|
| Pipeline lookup | RTMP calls `get_or_create_pipeline()` for each packet | write-locked hash-map access on the ingest path |
| Counters | packet-rate updates find their owner through async maps | control-plane registry access in the data plane |
| Ring publication | one allocation, release publication, and `notify_waiters()` per packet | allocator and scheduler work scales with source packet rate |
| Ring consumption | one index load, modulo, slot load, and reference increment per delivery | synchronization repeats across every output |
| Ring layout | each pointer slot is aligned to 64 bytes | 4096 slots require at least 256 KiB and one line per slot |
| AVIO bridge | mutex-protected `VecDeque<u8>` populated byte by byte | data is copied into and back out of an unbounded byte queue |
| SRT packaging | one MPEG-TS muxer and sender thread per output | packaging work and threads scale with destinations |
| HLS packaging | native muxing and segment storage are shared per pipeline | retained segment memory scales with bitrate and window size; slow consumers and idle cleanup still require validation |
| Worker placement | heavy threads have no ownership or affinity policy | migration and memory locality are uncontrolled |
| Vector search | production uses `memchr` for TS sync candidates | retain protocol verification and avoid custom architecture-specific code unless it beats the portable implementation |

## Target Shape

```text
control plane
  -> immutable hot handles and shared stage graph

socket workers
  -> read burst
  -> classify, timestamp, and account burst
  -> bounded source ring

shared workers
  -> unique video transforms
  -> late audio routing
  -> unique protocol packaging

package rings
  -> sharded destination senders
```

The control plane owns strings, hash maps, configuration, lifecycle, and
diagnostic objects. The data plane should operate on direct handles, integer
stage identifiers, bounded rings, compact metadata, and immutable payload
references.

## Optimization Areas

### Direct hot handles

Resolve a pipeline during authentication and retain its data-path state:

```rust
struct PipelineHotHandle {
    ring: Arc<RingBuffer>,
    bytes_received: Arc<AtomicU64>,
    keyframes: Arc<KeyframeTracker>,
    stream: Arc<StreamDescriptor>,
}
```

Apply the same pattern to outputs. Hash maps remain appropriate for setup,
health snapshots, and teardown. If a future worker handles unrelated pipelines
in one iteration, bulk lookup can then be evaluated; for the current
connection-owned flow, no lookup is better than a batched lookup.

### Bounded burst APIs

Introduce and benchmark:

```text
RingBuffer::push_batch()
Reader::pull_burst()
ChunkQueue::enqueue_batch()
ChunkQueue::dequeue_batch()
```

Initial packet counts:

```text
1, 4, 8, 16, 32, 64
```

Use both a count and a latency threshold. Start by testing a maximum of 32
packets with a 50–200 microsecond flush timer. Keyframes and queue pressure may
force earlier publication.

Batching should amortize:

- index acquisition and publication;
- queue synchronization;
- wakeups;
- timestamp and track classification;
- counter updates;
- package-stage calls.

### Run-to-completion for cheap work

An ingest worker should process a received burst locally:

```text
parse -> classify -> normalize timestamps -> account -> publish
```

Queue boundaries remain useful around expensive, shareable, or blocking work:
decode, encode, filtering, muxing, recording, and network backpressure. Cheap
packet-local operations should not each become a separate task or channel.

### Compact ring storage

Measure densely packed slots against the existing cache-line-per-slot layout.
Readers do not modify the slots, so aligning every slot does not prevent useful
reader false sharing.

A candidate layout is:

```rust
struct Slot {
    sequence: AtomicUsize,
    packet: ArcSwapOption<MediaPacket>,
}
```

Keep producer index, keyframe index, and notification state on separate cache
lines. The sequence protects readers from accepting a packet belonging to a
later wraparound generation.

### Bounded chunk queues

Replace the byte queue with an SPSC-oriented queue of immutable or pooled
chunks:

```rust
struct ChunkQueue {
    chunks: BoundedRing<Bytes>,
    read_offset: usize,
}
```

FFmpeg input callbacks consume across chunk boundaries. Output callbacks copy
their ephemeral buffer into pooled `BytesMut`, freeze it, and enqueue one
chunk. Expose capacity, occupancy, high-water mark, full events, and closure.

### Shared protocol packaging

Packaging should scale with unique media shape, not destination count:

```text
canonical packets
  -> one MPEG-TS package stage
  -> immutable 1316-byte chunk ring
  -> many SRT senders
```

Package identity must include upstream stage identity, codec shape, selected
tracks, timestamp policy, and mux options. RTMP should likewise investigate
sharing media-message bodies while retaining per-connection chunk-stream state.

### Stable workers and local pools

Long-lived workers should own:

- reusable packet-batch storage;
- local payload-buffer caches;
- counters periodically published to diagnostics;
- assigned pipelines or package stages.

Return buffers in batches. Derive size classes from recorded traffic rather
than guessing permanently. Pin only expensive demux, encode, mux, and fan-out
workers where measurements demonstrate a benefit; do not pin every socket task.

### Batch-oriented memory layout

Keep ergonomic packet objects at boundaries. Inside hot loops test an
array-of-structs-of-arrays representation:

```rust
struct PacketBatch<const N: usize> {
    pts: [i64; N],
    dts: [i64; N],
    tracks: [u16; N],
    flags: [u8; N],
    payloads: [Bytes; N],
    len: usize,
}
```

This can improve timestamp rescaling, track selection, keyframe classification,
and package planning. A sender-worker layout containing arrays of session
handles, reader indexes, pending byte counts, queue depths, and connection
states may produce a larger gain.

### Prefetch and vectorized search

Prefetch only inside real burst loops after compacting the layout. Candidate
data includes upcoming slots, payload headers, stream-map entries, and sharded
sender state. Test distances of one to four iterations and retain prefetch only
when cycles or cache stalls improve.

Use portable vectorized search at protocol edges:

- MPEG-TS sync and alignment;
- H.264/H.265 start-code scans;
- AAC ADTS sync;
- fixed-header classification.

Use a wide candidate scan followed by scalar protocol verification. Do not
replace ordinary memory copies or codec operations without production-shaped
evidence.

## Native MPEG-TS Opportunities

The new `mpegts.rs` path removes the FFmpeg demux thread and byte queue from SRT
ingest, then publishes completed packets with `push_batch()`. That is a strong
architectural improvement, but it also moves MPEG-TS parsing and muxing into the
application's hottest loops. Optimize it in the following order.

### P0: Retain output and accumulator capacity

`TsDemuxer::drain()` currently uses `std::mem::take(&mut self.output)`. The
demuxer therefore loses the preallocated output vector every time SRT drains
packets. Add an API such as:

```rust
fn drain_into(&mut self, output: &mut Vec<MediaPacket>)
```

Use `Vec::append` so the demuxer's vector retains its allocation and the SRT
loop reuses a caller-owned packet batch.

PES payload storage similarly loses its 16 KiB capacity after
`std::mem::take()`. Benchmark size-classed payload pools or a `BytesMut`
ownership-transfer design. `Bytes::from(Vec<u8>)` is already zero-copy, so the
remaining target is allocation reuse rather than another payload copy.

### P0: Constant-time PID dispatch

Every 188-byte TS packet currently searches `streams` with
`iter().position(...)`. Replace this with a PID-index table:

```text
pid_to_stream[8192] -> stream index or sentinel
```

An `i16` table occupies 16 KiB and removes a branchy linear scan from every TS
packet. Populate it when the PMT is parsed. Keep PAT and PMT PID checks before
the table lookup.

### P0: ~~Avoid constructing and copying a contiguous PES packet in the muxer~~ Done

PES header is now built on a `[u8; 19]` stack array. TS packets are written
directly into `self.output` via `resize` + slice mutation—no intermediate
`Vec<u8>` PES allocation, no full payload copy, no per-packet `[u8; 188]` temp
array. A `copy_pes_slices` helper walks the two logical slices (header +
original payload) without ever building a contiguous PES buffer.

**Result:** 6.7 MB fixture mux time dropped from ~880 µs to ~490 µs (45% faster,
throughput 6.8 → 12.3 GiB/s).

### P1: Native batch APIs

Add:

```text
TsDemuxer::feed_batch(chunks)
TsDemuxer::drain_into(packet_batch)
TsMuxer::mux_batch(media_packets)
```

The SRT receive loop should retain a reusable `Vec<MediaPacket>`, drain into it,
and call `RingBuffer::push_batch()` without allocating a new vector on every
receive. A mux batch can resolve stream mappings once, reserve aggregate output
capacity, and emit 1316-byte-aligned groups of seven TS packets for the sender.

### P1: Resynchronization and framing — complete

The demuxer uses `find_ts_sync()` with the runtime-dispatched `memchr`
implementation, then verifies `+188` and `+376` stride candidates scalarly. The
normal aligned path tests the expected sync byte directly and skips the scanner
entirely.

Measured on a 64 KiB corrupted prefix followed by aligned TS packets:

| Variant | Time | Throughput |
|---|---|---|
| Portable `memchr` sync scan | 631 ns | 96.8 GiB/s |
| Removed hand-written vector scanner | 907 ns | 67.3 GiB/s |
| Scalar sync scan (`iter().position()`) | 16.4 µs | 3.7 GiB/s |
| Full demuxer resync (vector search + stride verify + parse) | 1.31 µs | 46.6 GiB/s |

`memchr` is about 30% faster than the removed custom scanner and roughly 26×
faster than scalar search in this case. It also removes local unsafe
architecture-specific code while preserving runtime dispatch. The full
demuxer still performs scalar cadence verification before accepting a
candidate.

### P1: Annex-B start-code scanning — Complete

Vectorized Annex B NAL unit start-code scanning has been implemented directly in [src/media/codec.rs](file:///home/krsna1729/code/github/live-miracles/restream/src/media/codec.rs) using `memchr::memmem::Finder::new(&[0, 0, 1])` to locate start-code sequences at runtime.

#### Micro-Benchmark Comparison (8192-byte buffer):
- **memchr (AVX2/SSE2/scalar dispatch)**: **118.73 GiB/s**
- **wide (compile-time dispatch, 256-bit)**: **40.25 GiB/s**
- **pulp (runtime-dispatched SIMD abstraction)**: **4.14 GiB/s**

`memchr` provides the highest performance while automatically supporting multiple SIMD register widths on the target machine without custom target-feature configuration flags.

The vectorized scanner (`find_annexb_start_codes`) consumes arbitrary numbers of leading zeros backwards from the `00 00 01` signature to correctly match both 3-byte and 4-byte start codes. It is now called by:
1. `split_annexb_nalus` in `codec.rs` (used in conversions/sequence header synthesis).
2. `for_each_nal_raw` in `mpegts.rs` (used in MPEG-TS demux and keyframe detection).

### P2: ~~Stream lookup in the muxer~~ — not beneficial

Benchmarked a cached `video_stream_idx` + `audio_idx_by_track` lookup table
against the existing linear `.position()` search. With typical stream counts
(1 video + 1–16 audio), the linear scan is already branch-predicted and L1-hot.
The table lookup added indirection overhead and measured ~10% *slower*. Keeping
the simple linear search.

### P2: Timestamp and CRC helpers

Timestamp conversion currently uses floating point for the exact `90 kHz ->
milliseconds` conversion. Benchmark an integer implementation with explicit
negative-timestamp semantics.

PAT/PMT CRC uses a bit-at-a-time scalar loop. Tables or hardware acceleration
are possible, but PAT/PMT are emitted roughly every 500 ms and the sections are
small, so CRC work is not a significant SIMD target unless profiling proves
otherwise.

### Correctness and benchmark requirements

Before replacing the FFmpeg path or adopting the native muxer broadly, add:

- recorded H.264/H.265 plus multi-track AAC demux traces;
- aligned, split-packet, corrupted-prefix, and continuity-gap inputs;
- demux bytes/s, TS packets/s, media packets/s, allocations, and copied bytes;
- mux tests at small audio packets, ordinary P-frames, and 200–500 KiB I-frames;
- native demux versus FFmpeg demux throughput and output equivalence;
- scalar versus vectorized resync (done: `memchr` is roughly 26× faster on the 64 KiB prefix) and Annex-B scanning;
- packet-at-a-time versus batch demux/mux;
- output validation with `ffprobe` and the existing protocol probes.

Also remove the duplicate `try_build_probe(stream_idx, &payload)` invocation in
`flush_pes()`; it is mostly masked by the probe cache but is unnecessary work
and obscures the intended one-shot probe path.

## Opportunities From Other Recent Media Changes

### SRT native ingest

Moving SRT ingest from an FFmpeg thread plus `MemoryQueue` to `TsDemuxer`
removes a thread boundary and at least two byte-queue copies. Preserve that
advantage by avoiding new allocation and registry costs:

- ~~cache the ingest byte counter in the SRT connection handle instead of calling
  `update_ingest_bytes()` through an async map lookup for every receive~~ — done;
- ~~drain demux output into a reusable packet vector instead of returning a new
  vector from `TsDemuxer::drain()`~~ — done (`drain_into` adopted);
- ~~keep `push_batch()` at the demux-to-ring boundary~~ — done;
- ~~batch publish ingest packets to the ring buffer in the SRT ingest loop~~ — done;
- benchmark 1316-byte single-link receives separately from larger group-message
  receives;
- record allocations, copied bytes, TS packets/s, and media packets/s against
  the removed FFmpeg path.

### Egress and Transcoder loop burst consumption — Complete

All consumer loops have been migrated to burst consumption APIs and zero-allocation
codec helpers:
- **RTMP play/egress** (`rtmp.rs`): `pull_burst` 32; `video_for_rtmp_into` / `audio_for_rtmp_into` with per-egress scratch buffers.
- **SRT egress / play subscriber** (`srt.rs`): consume pre-muxed 1316-byte TS chunks in bursts from `TsChunkRing` directly, bypassing per-connection conversions and redundant `TsMuxer` instances.
- **HLS segmenter** (`hls.rs`): `pull_burst` 32; `video_for_ts_into` / `audio_for_ts_into` with scratch buffers.
- **Recording** (`recording.rs`): `pull_burst` 32; `video_for_ts_into` / `audio_for_ts_into` with scratch buffers.
- **Transcoder worker** (`transcoder.rs`): `pull_burst` 32.

All paths reuse the packet buffer vector and codec scratch buffers across loops.

### Transcoder output

The transcoder output demuxer copies every FFmpeg packet with
`Bytes::copy_from_slice()` and publishes one ring packet at a time. Opportunities:

- collect demuxed packets into a small vector and publish with `push_batch()`;
- investigate transferring or reference-counting FFmpeg `AVBufferRef` ownership
  before adding a custom payload pool;
- preserve stream identity and timestamps in the batch rather than emitting
  anonymous byte chunks;
- benchmark allocation count and copied bytes per output media packet.

### Native packaging and shared stages — Complete

Both HLS and SRT now utilize shared native packaging stages:
- **HLS:** Uses one shared native `TsMuxer` segmenter per source pipeline. Browser preview requests keep it alive through access heartbeats, persistent HLS outputs hold a reference, and the reconciler removes idle segmenters after 60 seconds.
- **SRT Egress and Play:** Share a single native `TsMuxer` task per pipeline+preset which feeds a shared `TsChunkRing` (SPMC lock-free package ring). Individual client loops consume pre-muxed 1316-byte packets directly from `TsChunkReader` and write to their bounded `MemoryQueue` buffers. This satisfies the high-performance shape:

```text
canonical packet burst
  -> one native MPEG-TS package stage per final media shape
  -> immutable 1316-byte package ring (TsChunkRing)
  -> many destination senders (SRT play and egress loops)
```

This design has been validated against `ffprobe` correctness checks, multi-track AAC, PCR/PTS/DTS monotone ordering, PAT/PMT cadence, and our end-to-end correctness protocol gates.

The HLS cost benchmark currently reports:

| Profile | Mux cost for 1 s content | Full 6 s segment | Ten-segment window |
|---|---:|---:|---:|
| 720p30 H.264, 3 Mbps | ~27 µs | ~0.23 ms | ~23 MiB |
| 1080p30 H.264, 5 Mbps | ~46 µs | ~0.41 ms | ~37 MiB |
| 1080p60 H.264, 8 Mbps | ~71 µs | ~0.66 ms | ~62 MiB |
| 4K30 HEVC, 15 Mbps | ~147 µs | ~2.75 ms | ~111 MiB |

These synthetic measurements isolate packaging and retained segment storage;
they do not include ring waits, socket delivery, browser behavior, or
production payload distributions.

### Thread and scheduler model

Recent transport and bonding work adds more long-lived socket and helper tasks.
Track:

- OS threads and Tokio tasks per pipeline and per destination;
- context switches and CPU migrations;
- whether package work scales with unique media shapes or output count;
- affinity experiments only for long-lived demux, mux, and encode workers;
- one slow destination versus the other readers in the same package fan-out.

## Baseline Benchmark

`high_performance_data_path` preserves current behavior as named baselines:

| Group | Measures |
|---|---|
| `data_path/control_plane_lookup` | locked pipeline registry lookup versus cached direct handle |
| `data_path/ingest_hot_handle` | packet-rate ring and byte-counter registry access versus cached handles |
| `data_path/ring_producer` | current publication loop at application burst sizes |
| `data_path/ring_consumer` | current pull loop at application burst sizes |
| `data_path/fanout_delivery` | slot and reference-count cost for 1–500 readers |
| `data_path/memory_queue` | byte-oriented AVIO queue round-trip throughput |
| `data_path/segment_finalize` | completed segment copy versus zero-copy ownership transfer |
| `data_path/mpegts_demux_drain` | real 6.7 MB fixture replay with disposable versus reusable output vectors |
| `data_path/mpegts_mux` | real 6.7 MB fixture re-mux throughput |
| `data_path/mpegts_resync` | `memchr` versus scalar sync scan, and full demuxer recovery from a 64 KiB corrupted prefix |
| `simd_alternatives` | Portable byte-search and copy alternatives used to decide whether custom architecture-specific routines are justified |
| `hls_cost` | Native HLS mux, accumulation, segment-store CPU cost, and retained HLS-window memory across representative profiles |
| `transcoder_runtime_stage` | Exact current FFmpeg `MemoryQueue` + custom-AVIO source passthrough stage over the full H.264 fixture; this is not labelled transcoding until decode/filter/encode is implemented |

It also prints `MediaPacket` and aligned-slot sizes.

Run everything:

```bash
scripts/resource-limit cargo bench --bench high_performance_data_path
```

Run one group:

```bash
scripts/resource-limit cargo bench --bench high_performance_data_path -- control_plane_lookup
scripts/resource-limit cargo bench --bench high_performance_data_path -- ingest_hot_handle
scripts/resource-limit cargo bench --bench high_performance_data_path -- ring_producer
scripts/resource-limit cargo bench --bench high_performance_data_path -- ring_consumer
scripts/resource-limit cargo bench --bench high_performance_data_path -- fanout_delivery
scripts/resource-limit cargo bench --bench high_performance_data_path -- memory_queue
scripts/resource-limit cargo bench --bench high_performance_data_path -- segment_finalize
```

### Initial local baseline

Short smoke measurements recorded on June 20, 2026 verify that the benchmark
seams work. These are not release claims; save a full Criterion baseline on the
target deployment hardware before implementation work.

| Case | Initial result |
|---|---|
| locked pipeline `get_or_create` | approximately 69 ns |
| cached ring-handle clone | approximately 12 ns |
| RTMP registry ring + counter access | approximately 119 ns |
| RTMP cached ring + counter access | approximately 7.3 ns, about 94% lower |
| current producer loop, 32 packets | approximately 5.00 microseconds, 6.40 million packets/s |
| `push_batch()`, 32 packets | approximately 4.40 microseconds versus 5.17 microseconds for repeated `push()`, about 15% lower |
| current consumer loop, 32 packets | approximately 776 ns, 41.3 million deliveries/s |
| `pull_burst()`, 8 packets | approximately 191 ns versus 229 ns for repeated `pull()`, about 17% lower |
| current fan-out, 500 readers × 32 packets | approximately 374 microseconds, 42.8 million deliveries/s |
| byte queue round trip, 32 × 1316-byte packets | approximately 9.20 microseconds, 4.26 GiB/s |
| batched byte queue round trip, 32 × 1316-byte packets | approximately 3.84 microseconds versus 7.49 microseconds for repeated writes, about 49% lower |
| `MediaPacket` layout | 56 bytes, 8-byte alignment |
| aligned ring slot layout | 64 bytes per slot; 4096 slots consume 256 KiB |
| compact ring slot layout | 8 bytes per slot; 4096 slots consume 32 KiB, 87.5% lower |
| HLS 8 MiB segment copy | approximately 4.26 milliseconds |
| HLS 8 MiB ownership transfer | approximately 347 nanoseconds, over 99.99% lower finalization time |
| MPEG-TS disposable output-vector replay | approximately 4.43 milliseconds for the 6.7 MB H.264 fixture (cumulative: PID dispatch + reusable drains + vectorized resync) |
| MPEG-TS reusable output-vector replay | approximately 4.28 milliseconds, about 3.4% lower than disposable |
| MPEG-TS direct PID-table replay | approximately 8.54 milliseconds versus 11.40 milliseconds linear lookup, about 25.9% lower |
| portable `memchr` scan, 64 KiB corrupted prefix | approximately 631 nanoseconds, 96.8 GiB/s |
| removed custom vector scan, 64 KiB corrupted prefix | approximately 907 nanoseconds, 67.3 GiB/s; `memchr` is about 30% faster |
| scalar sync scan, 64 KiB corrupted prefix | approximately 16.4 microseconds, 3.7 GiB/s; `memchr` is roughly 26× faster |
| full demuxer resync, 64 KiB corrupted prefix | approximately 1.31 microseconds, 46.6 GiB/s |
| MPEG-TS mux, 6.7 MB fixture | approximately 490 microseconds, 12.3 GiB/s (zero-copy PES; was ~880 µs before) |
| FFmpeg source passthrough stage, 6.4 MiB fixture | approximately 26.8 milliseconds, 238 MiB/s through production custom AVIO |
| Annex B NAL scanning (memchr::memmem, AVX2/SSE2/scalar) | approximately 118.73 GiB/s (selected at runtime, best performer) |
| Annex B NAL scanning (wide compile-time 256-bit SIMD) | approximately 40.25 GiB/s |
| Annex B NAL scanning (pulp runtime SIMD abstraction) | approximately 4.14 GiB/s |
| MPEG-TS muxing baseline (ts_mux_inhouse/1s_30fps_1080p) | approximately 11.85 µs (17.72 GiB/s for 1.55 MB payload) |
| burst drain, alloc per burst (old transcoder/h264_transcoder) | approximately 2.79 µs per 32-packet burst, 11.5 Melem/s |
| burst drain, hoisted + clear (new) | approximately 2.54 µs per 32-packet burst, 12.6 Melem/s — ~9% faster, ~250 ns saved per burst |

The lookup comparison supports moving registry resolution out of the packet
loop. The ring numbers measure in-memory steady-state delivery and deliberately
exclude sleeping-reader wakeups, socket work, packaging, and ring construction.
Those costs require the follow-up measurements listed below.

As optimized primitives are added, add them beside the immutable baseline:

```text
current_push_loop           push_batch
current_pull_loop           pull_burst
byte_vecdeque_round_trip    chunk_ring_round_trip
                            pooled_chunk_ring_round_trip
```

## Required Follow-Up Measurements

These need production seams or primitives that do not yet exist:

1. Sleeping-reader notifications: wakeups and p99 delivery latency.
2. Shared package stage versus one muxer per output.
3. Recorded RTMP and SRT packet-trace replay.
4. Worker-local pools versus allocator-backed copies.
5. Compact versus aligned slots under concurrent readers.
6. Sharded sender worker versus one task per destination.
7. Batch metadata timestamp and track-routing loops.
8. Prefetch-distance sweep on the winning compact layout.
9. Single-socket and multi-socket locality tests.

For release-mode harnesses collect:

```text
cycles and instructions
branches and branch misses
L1 and last-level cache misses
context switches and CPU migrations
allocations and allocated bytes
reference clone/drop rate
queue occupancy and high-water marks
wakeups
threads and Tokio tasks
RSS before, during, and after teardown
p50, p95, and p99 packet latency
```

Use realistic pre-demuxed traces in both realtime and saturation modes.

## Incremental Plan

### Step 0: Baselines and instrumentation

- Keep baseline benchmark names immutable.
- Add allocation and queue high-water instrumentation.
- Record CPU topology, compiler flags, FFmpeg version, and kernel.
- Save Criterion baselines before production changes.

### Step 1: Direct hot handles

- Resolve rings and counters once at authentication.
- Remove packet-rate engine-map access.
- Batch counter publication if direct atomics remain contended.

### Step 2: Burst ring APIs

- Add `push_batch()` and `pull_burst()`.
- Publish the write index once per burst.
- Coalesce notifications.
- Preserve overflow and keyframe recovery.

### Step 3: Compact ring layout

- Add generation validation.
- Pack read-mostly slots.
- Isolate only contended mutable indexes.

### Step 4: Bounded chunk queues

- Add chunk-based FFmpeg input/output queues.
- Instrument backpressure and occupancy.
- Compare ordinary and pooled chunks.

### Step 5: Shared package stages

- Establish a canonical packet contract.
- Cache package stages by upstream identity and final media shape.
- Fan immutable package chunks to destinations.

### Step 6: Worker sharding and pools

- Assign package and sender work to stable workers.
- Add local pools and batched counter publication.
- Test optional affinity and locality.

### Step 7: Layout, prefetch, and SIMD refinement

- Introduce `PacketBatch` only for demonstrated hot loops.
- Sweep prefetch distance.
- Integrate scanners only where they replace measured scalar work.

## Correctness Gates

Every step must retain:

- existing unit and integration tests;
- RTMP and SRT protocol probes;
- packet and byte counts;
- PTS/DTS ordering and B-frame behavior;
- keyframe startup and overflow recovery;
- audio-track identity;
- HLS playlist and segment ordering;
- recording validity;
- bounded queue behavior and clean teardown.

Throughput produced by invalid media is not an optimization result.
