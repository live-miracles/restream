use bytes::Bytes;
use criterion::{
    BatchSize, BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main,
};
use restream::media::ring_buffer::{MediaPacket, MediaType, PayloadFormat, Reader, RingBuffer};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread;

fn make_packet(payload_bytes: usize) -> MediaPacket {
    MediaPacket {
        media_type: MediaType::Video,
        track_index: 0,
        pts: 0,
        dts: 0,
        is_keyframe: true,
        format: PayloadFormat::Raw,
        payload: Bytes::from(vec![0u8; payload_bytes]),
    }
}

/// Push with 500 live readers — measures contention cost of `notify_waiters`.
fn benchmark_ring_buffer_concurrency(c: &mut Criterion) {
    let buffer = Arc::new(RingBuffer::new(4096));

    let mut consumers = Vec::new();
    for _ in 0..500 {
        let buf_clone = buffer.clone();
        let handle = thread::spawn(move || {
            let mut reader = Reader::new("bench_ring_buffer".to_string(), buf_clone);
            let mut count = 0;
            while count < 100 {
                if let Ok(Some(_pkt)) = reader.pull() {
                    count += 1;
                } else {
                    thread::yield_now();
                }
            }
        });
        consumers.push(handle);
    }

    c.bench_function("ring_buffer_push_500_readers", |b| {
        b.iter(|| {
            buffer.push(make_packet(1024));
        })
    });

    for handle in consumers {
        let _ = handle.join();
    }
}

/// Pure producer throughput: `push_batch` vs individual `push` across burst sizes.
/// No readers — measures the ring write path in isolation.
fn benchmark_push_batch_vs_push(c: &mut Criterion) {
    let mut group = c.benchmark_group("ring_buffer/producer");

    // Use a shared pre-built payload so each iteration only pays for Arc refcount, not alloc.
    let pkt = make_packet(1316);

    for burst in [1usize, 4, 8, 16, 32] {
        group.throughput(Throughput::Elements(burst as u64));

        group.bench_with_input(
            BenchmarkId::new("push_one_at_a_time", burst),
            &burst,
            |b, &n| {
                let buf = RingBuffer::new(4096);
                b.iter(|| {
                    for _ in 0..n {
                        buf.push(pkt.clone());
                    }
                })
            },
        );

        group.bench_with_input(BenchmarkId::new("push_batch", burst), &burst, |b, &n| {
            let buf = RingBuffer::new(4096);
            b.iter(|| {
                black_box(buf.push_batch((0..n).map(|_| pkt.clone())));
            })
        });
    }

    group.finish();
}

/// Consumer hot path: `pull_burst` at different burst sizes.
/// Measures `ArcSwapOption::load_full` + Arc ref-count per packet.
fn benchmark_pull_burst(c: &mut Criterion) {
    let mut group = c.benchmark_group("ring_buffer/consumer");

    let pkt = make_packet(1316);

    for burst in [1usize, 4, 8, 16, 32] {
        group.throughput(Throughput::Elements(burst as u64));

        group.bench_with_input(BenchmarkId::new("pull_burst", burst), &burst, |b, &n| {
            // Fill a fresh ring buffer with enough packets for each iteration.
            // iter_custom lets us refill without measuring the refill cost.
            b.iter_custom(|iters| {
                use std::time::Instant;
                let buf = Arc::new(RingBuffer::new(4096));
                let mut reader = Reader::new("pull_burst_bench".to_string(), buf.clone());
                // Pre-fill: each iter drains n packets, fill 4× as buffer.
                let prefill = (n * iters as usize).min(4096);
                for _ in 0..prefill {
                    buf.push(pkt.clone());
                }
                let mut out = Vec::with_capacity(n);
                let started = Instant::now();
                for _ in 0..iters {
                    // Top up so reader always has n packets.
                    let r_idx = reader.info.read_idx.load(Ordering::Relaxed);
                    let available = buf.get_write_idx().saturating_sub(r_idx);
                    if available < n {
                        for _ in 0..(n - available) {
                            buf.push(pkt.clone());
                        }
                    }
                    out.clear();
                    black_box(reader.pull_burst(&mut out, n).ok());
                }
                started.elapsed()
            })
        });
    }

    group.finish();
}

/// Measure the overhead of `Reader::lag()` — one `Acquire` atomic load plus one
/// `saturating_sub`.  The call must be safe on hot paths; target latency < 5 ns.
fn benchmark_reader_lag(c: &mut Criterion) {
    let mut group = c.benchmark_group("ring_buffer/lag");

    let pkt = make_packet(1316);

    // lag = 0: reader is at the write cursor (fully caught up).
    group.bench_function("caught_up", |b| {
        let buf = Arc::new(RingBuffer::new(4096));
        let reader = Reader::new("bench_lag_caught_up".to_string(), buf.clone());
        b.iter(|| black_box(reader.lag()))
    });

    // lag = 1024: reader is 1024 slots behind, typical mid-burst scenario.
    // Create reader on empty buffer (read_idx = 0) then push 1024 packets so
    // write_idx = 1024 and lag() = 1024 throughout the measurement.
    group.bench_function("1024_behind", |b| {
        let buf = Arc::new(RingBuffer::new(4096));
        let reader = Reader::new("bench_lag_behind".to_string(), buf.clone());
        for _ in 0..1024 {
            buf.push(pkt.clone());
        }
        b.iter(|| black_box(reader.lag()))
    });

    // Validate that calling lag() after every pull_burst(32) does not regress
    // overall consumer throughput.  Use iter_batched so setup (pre-fill 32 packets)
    // is excluded from measurement; each iter measures one pull_burst(32) + lag().
    // Compare to `ring_buffer/consumer/pull_burst/32` to quantify overhead.
    group.bench_function("pull_burst_32_then_lag", |b| {
        b.iter_batched(
            || {
                let buf = Arc::new(RingBuffer::new(4096));
                let reader = Reader::new("bench_lag_pull".to_string(), buf.clone());
                for _ in 0..32 {
                    buf.push(pkt.clone());
                }
                (reader, Vec::with_capacity(32))
            },
            |(mut reader, mut out)| {
                out.clear();
                black_box(reader.pull_burst(&mut out, 32).ok());
                black_box(reader.lag())
            },
            BatchSize::SmallInput,
        )
    });

    group.finish();
}

/// Vec pre-allocation: Vec::new() vs Vec::with_capacity() for burst-sized
/// collections.  Our hot paths produce batches of (Bytes, bool) tuples (up
/// to 32) and byte buffers (up to 65536 bytes of muxed TS).  Pre-allocating
/// eliminates 3-5 reallocations on the first few bursts.
fn benchmark_vec_capacity(c: &mut Criterion) {
    let mut group = c.benchmark_group("vec_capacity");

    let payload = Bytes::from(vec![0u8; 1316]);
    let burst = 32usize;

    // (Bytes, bool) tuple batch — TsChunkRing consumers
    group.bench_with_input(
        BenchmarkId::new("tuple_push_new", burst),
        &burst,
        |b, &n| {
            b.iter(|| {
                let mut v: Vec<(Bytes, bool)> = Vec::new();
                for i in 0..n {
                    v.push((payload.clone(), i == 0));
                }
                black_box(v.clear());
            })
        },
    );

    group.bench_with_input(
        BenchmarkId::new("tuple_push_with_capacity", burst),
        &burst,
        |b, &n| {
            b.iter(|| {
                let mut v: Vec<(Bytes, bool)> = Vec::with_capacity(n);
                for i in 0..n {
                    v.push((payload.clone(), i == 0));
                }
                black_box(v.clear());
            })
        },
    );

    // u8 byte buffer — TS muxing hot loops
    let byte_cap = 65536usize;
    group.throughput(Throughput::Bytes(byte_cap as u64));

    group.bench_with_input(
        BenchmarkId::new("byte_extend_new", byte_cap),
        &byte_cap,
        |b, _| {
            b.iter(|| {
                let mut v: Vec<u8> = Vec::new();
                v.extend_from_slice(&vec![0x47u8; 1316]);
                black_box(&v);
                v.clear();
            })
        },
    );

    group.bench_with_input(
        BenchmarkId::new("byte_extend_with_capacity", byte_cap),
        &byte_cap,
        |b, _| {
            b.iter(|| {
                let mut v: Vec<u8> = Vec::with_capacity(65536);
                v.extend_from_slice(&vec![0x47u8; 1316]);
                black_box(&v);
                v.clear();
            })
        },
    );

    group.finish();
}

/// Vec allocation reuse: creating a fresh Vec inside each burst iteration
/// vs declaring outside the loop and reusing via drain().  Measured across
/// 100 burst cycles — the outer-declaration should retain the capacity
/// after the first burst and avoid the heap allocation on all subsequent
/// cycles.
fn benchmark_vec_loop_reuse(c: &mut Criterion) {
    let mut group = c.benchmark_group("vec_loop_reuse");

    let payload = Bytes::from(vec![0u8; 1316]);
    group.throughput(Throughput::Elements(100));

    // Fresh Vec each iteration (current pattern in rtmp/transcoder)
    group.bench_function("alloc_every_iter", |b| {
        b.iter(|| {
            for _ in 0..100 {
                let mut v: Vec<(Bytes, bool)> = Vec::with_capacity(32);
                for i in 0..32 {
                    v.push((payload.clone(), i == 0));
                }
                black_box(&v);
            }
        })
    });

    // Vec declared outside, reused with drain()
    group.bench_function("reuse_with_drain", |b| {
        b.iter_batched(
            || Vec::<(Bytes, bool)>::with_capacity(32),
            |mut v| {
                for _ in 0..100 {
                    for i in 0..32 {
                        v.push((payload.clone(), i == 0));
                    }
                    black_box(&v);
                    v.clear();
                }
            },
            BatchSize::SmallInput,
        )
    });

    // Vec declared inside, used with drain (keeps allocation within iter)
    group.bench_function("alloc_every_iter_drain", |b| {
        b.iter(|| {
            let mut v: Vec<(Bytes, bool)> = Vec::with_capacity(32);
            for _ in 0..100 {
                for i in 0..32 {
                    v.push((payload.clone(), i == 0));
                }
                black_box(&v);
                v.clear();
            }
        })
    });

    group.finish();
}

/// Benchmark the exact hot-loop pattern fixed in transcoder/h264_transcoder:
/// `Vec::with_capacity(32)` inside the burst arm vs hoisted + `.clear()`.
///
/// Uses `Arc<MediaPacket>` to match the real element type on the hot path.
/// The ring is pre-filled so each `pull_burst` call returns a full batch of 32.
fn benchmark_burst_drain_alloc(c: &mut Criterion) {
    let mut group = c.benchmark_group("burst_drain_alloc");
    group.throughput(Throughput::Elements(32));

    let ring = Arc::new(RingBuffer::new(4096));
    // Pre-fill the ring so pull_burst always has work to do.
    for i in 0..4096 {
        ring.push(make_packet(1316));
        // Update last_keyframe_idx so readers can join
        let _ = i;
    }

    // --- OLD: alloc inside the burst arm ---
    group.bench_function("alloc_per_burst", |b| {
        b.iter_batched(
            || {
                let r = Reader::new("bench_alloc".to_string(), ring.clone());
                // Refill ring for the reader
                for _ in 0..64 {
                    ring.push(make_packet(1316));
                }
                r
            },
            |mut reader| {
                // This is what the OLD transcoder/h264_transcoder code did:
                let mut packets = Vec::with_capacity(32); // ← was inside the arm
                let _ = reader.pull_burst(&mut packets, 32);
                for pkt in &packets {
                    black_box(pkt);
                }
                // Vec dropped here — malloc + free every burst
            },
            BatchSize::SmallInput,
        )
    });

    // --- NEW: hoisted Vec, .clear() per arm ---
    group.bench_function("hoisted_clear", |b| {
        b.iter_batched(
            || {
                let r = Reader::new("bench_hoisted".to_string(), ring.clone());
                for _ in 0..64 {
                    ring.push(make_packet(1316));
                }
                (r, Vec::<std::sync::Arc<MediaPacket>>::with_capacity(32))
            },
            |(mut reader, mut packets)| {
                // This is what the NEW code does:
                packets.clear(); // ← just zeroes len, no alloc
                let _ = reader.pull_burst(&mut packets, 32);
                for pkt in &packets {
                    black_box(pkt);
                }
                packets // return so vec lives until next iter_batched cycle
            },
            BatchSize::SmallInput,
        )
    });

    group.finish();
}

criterion_group!(
    benches,
    benchmark_push_batch_vs_push,
    benchmark_pull_burst,
    benchmark_reader_lag,
    benchmark_vec_capacity,
    benchmark_vec_loop_reuse,
    benchmark_burst_drain_alloc,
    // Run concurrency bench last so 500 threads don't noise-up the pure benchmarks.
    benchmark_ring_buffer_concurrency,
);
criterion_main!(benches);
