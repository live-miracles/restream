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
| Compact ring slots | Accepted, commit pending | Controlled pinned-CPU rerun: storage fell 256 KiB → 32 KiB, producer throughput was neutral, 32-packet consumer improved ~5.7%, and 500-reader fan-out improved ~9.6% |
| Burst ring primitives | Complete in `e0f33ac` | `push_batch()` improved 32-packet publication by ~15%; `pull_burst()` improved 8-packet consumption by ~17%. All 14 ring tests pass. Existing single-packet APIs remain the latency path |
| Burst adoption in internal stages | Complete in `95f2849` | HLS, recording, and transcoder feeders drain reusable 32-packet bursts. The primitive measured up to ~17% faster; module tests and all-target compilation pass |
| Bounded chunk queues | Pending | Not started |
| Batched AVIO queue writes | Complete, commit pending | One lock and notification per burst reduced the 32 × 1316-byte queue round trip from ~7.49 μs to ~3.84 μs, about 49% lower. Adopted by HLS, recording, and transcoder feeders |
| Shared package stages | Pending | Not started |
| Worker sharding and local pools | Pending | Not started |
| Batch metadata, prefetch, and SIMD refinement | Pending | Not started |
| Zero-copy HLS segment finalization | Complete, commit pending | 8 MiB finalization fell from ~4.26 ms to ~347 ns by transferring `BytesMut` ownership, over 99.99% lower |
| Native MPEG-TS data-path audit | Complete | New demux/mux path adds major opportunities in PID dispatch, reusable drains, PES construction, direct TS output, batch APIs, and SIMD-assisted resynchronization/NAL scanning |

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
| HLS finalization | segment accumulator is copied into a new `Bytes` | an extra multi-megabyte copy per segment |
| Worker placement | heavy threads have no ownership or affinity policy | migration and memory locality are uncontrolled |
| SIMD | custom routines are disconnected from production | no demonstrated end-to-end benefit |

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

### Prefetch and SIMD

Prefetch only inside real burst loops after compacting the layout. Candidate
data includes upcoming slots, payload headers, stream-map entries, and sharded
sender state. Test distances of one to four iterations and retain prefetch only
when cycles or cache stalls improve.

Use SIMD at protocol edges:

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

### P0: Avoid constructing and copying a contiguous PES packet in the muxer

`TsMuxer::mux_packet()` currently:

1. allocates a `Vec` for PES;
2. copies the complete codec payload into it;
3. copies PES slices into a temporary 188-byte TS array;
4. copies each TS array into the output vector.

Packetize a small stack-resident PES header and the original payload as two
logical slices. Write TS packets directly into reserved output-vector space.
This removes the full payload copy, the per-packet PES allocation, and one
188-byte copy per TS packet.

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

### P1: Resynchronization and framing

The current demuxer advances one byte at a time until it sees `0x47`. A valid
candidate should be verified at offsets `+188` and `+376` where available.
Extend the existing SIMD scanner to return candidate sync positions, followed
by scalar stride verification:

```text
wide 0x47 scan -> verify 188-byte cadence -> resume packet loop
```

Do not run the scanner while input remains correctly aligned; the normal path
should test the expected sync byte and advance directly.

### P1: Annex-B start-code scanning

H.264/H.265 keyframe and probe parsing use scalar searches for three- and
four-byte start codes. Add a vectorized zero-byte candidate scanner and verify
`00 00 01` / `00 00 00 01` scalarly. Benchmark realistic P-frames and large
I-frames separately. Skip the scan when the TS random-access flag already
establishes keyframe status.

### P2: Stream lookup in the muxer

`TsMuxer::mux_packet()` linearly searches stream metadata for every media
packet. Cache the video stream index and an audio-track-index lookup table when
constructing the muxer. The expected stream count is small, so this is lower
priority than TS-packet PID dispatch and payload-copy elimination.

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
- scalar versus SIMD resync and Annex-B scanning;
- packet-at-a-time versus batch demux/mux;
- output validation with `ffprobe` and the existing protocol probes.

Also remove the duplicate `try_build_probe(stream_idx, &payload)` invocation in
`flush_pes()`; it is mostly masked by the probe cache but is unnecessary work
and obscures the intended one-shot probe path.

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

It also prints `MediaPacket` and aligned-slot sizes.

Run everything:

```bash
cargo bench --bench high_performance_data_path
```

Run one group:

```bash
cargo bench --bench high_performance_data_path -- control_plane_lookup
cargo bench --bench high_performance_data_path -- ingest_hot_handle
cargo bench --bench high_performance_data_path -- ring_producer
cargo bench --bench high_performance_data_path -- ring_consumer
cargo bench --bench high_performance_data_path -- fanout_delivery
cargo bench --bench high_performance_data_path -- memory_queue
cargo bench --bench high_performance_data_path -- segment_finalize
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
| HLS 8 MiB segment copy | approximately 4.26 milliseconds |
| HLS 8 MiB ownership transfer | approximately 347 nanoseconds, over 99.99% lower finalization time |

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
