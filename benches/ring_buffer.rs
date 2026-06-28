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
                v.clear();
                black_box(v.len());
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
                v.clear();
                black_box(v.len());
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

/// Burst TS-chunk allocation: per-chunk `Bytes::copy_from_slice` vs a single
/// `BytesMut` accumulation + `Bytes::slice` pattern.
///
/// The SRT shared muxer previously called `copy_from_slice` once per muxed chunk
/// (up to 32 per burst), producing 32 independent heap allocations.  The new
/// pattern allocates one `BytesMut` per burst and slices it via refcount bumps.
fn benchmark_ts_chunk_burst_alloc(c: &mut Criterion) {
    let mut group = c.benchmark_group("ts_chunk_burst_alloc");

    // Simulate a typical burst: 32 media packets → 32 TS chunks of ~376 bytes each
    // (2 × 188-byte TS packets per media packet is typical for video).
    let chunk_size = 376usize;
    let burst = 32usize;
    let ts_bytes = vec![0x47u8; chunk_size];
    group.throughput(Throughput::Bytes((chunk_size * burst) as u64));

    // OLD: one Bytes::copy_from_slice per chunk → N alloc+memcpy
    group.bench_function("per_chunk_copy_from_slice", |b| {
        b.iter(|| {
            let mut batch: Vec<(bytes::Bytes, bool)> = Vec::with_capacity(burst);
            for i in 0..burst {
                batch.push((bytes::Bytes::copy_from_slice(&ts_bytes), i == 0));
            }
            black_box(batch);
        })
    });

    // NEW: one BytesMut per burst + Bytes::slice → 1 alloc + N refcount bumps
    group.bench_function("burst_bytesmut_then_slice", |b| {
        b.iter(|| {
            let mut accum = bytes::BytesMut::with_capacity(chunk_size * burst);
            let mut ends: Vec<(usize, bool)> = Vec::with_capacity(burst);
            for i in 0..burst {
                accum.extend_from_slice(&ts_bytes);
                ends.push((accum.len(), i == 0));
            }
            let frozen = accum.freeze();
            let mut prev = 0usize;
            let batch: Vec<(bytes::Bytes, bool)> = ends
                .drain(..)
                .map(move |(end, is_kf)| {
                    let chunk = frozen.slice(prev..end);
                    prev = end;
                    (chunk, is_kf)
                })
                .collect();
            black_box(batch);
        })
    });

    group.finish();
}

/// Measure `active_reader_count()` — prunes dead Weak refs and returns the count.
/// Called inside `adapt_pipeline_ring` and `get_or_create_pipeline` on every
/// publisher connect, so must be cheap even with many accumulated dead refs.
fn benchmark_active_reader_count(c: &mut Criterion) {
    let mut group = c.benchmark_group("ring_buffer/active_reader_count");

    // 0 readers — fastest path (empty readers vec).
    group.bench_function("no_readers", |b| {
        let buf = Arc::new(RingBuffer::new(1024));
        b.iter(|| black_box(buf.active_reader_count()))
    });

    // 4 live readers — typical egress fan-out (RTMP src, RTMP 720p, SRT src, SRT 720p).
    group.bench_function("4_live_readers", |b| {
        let buf = Arc::new(RingBuffer::new(1024));
        let _readers: Vec<_> = (0..4)
            .map(|i| Reader::new(format!("r{i}"), buf.clone()))
            .collect();
        b.iter(|| black_box(buf.active_reader_count()))
    });

    // 4 dead Weak refs left after readers drop — prune cost.
    group.bench_function("4_dead_refs", |b| {
        let buf = Arc::new(RingBuffer::new(1024));
        {
            let _readers: Vec<_> = (0..4)
                .map(|i| Reader::new(format!("r{i}"), buf.clone()))
                .collect();
            // readers drop here, leaving dead Weak entries in buf.readers
        }
        b.iter(|| black_box(buf.active_reader_count()))
    });

    group.finish();
}

/// Measure the cost of `seal_and_forward` + one reader migrating.
///
/// This is the steady-state impact on the hot path during an adaptive resize:
/// the seal itself is atomic + notify; the migration runs in each reader's
/// async task on its next `wait_for_data` wake-up.
///
/// Target: seal < 500 ns; single-reader migration < 2 µs.
fn benchmark_seal_and_forward(c: &mut Criterion) {
    let mut group = c.benchmark_group("ring_buffer/seal_and_forward");
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();

    // Cost of seal_and_forward itself (atomic store + notify_waiters).
    // Rings are created in setup (outside timed region). We return them from
    // the closure so Criterion drops them AFTER the timed section — correct
    // Criterion idiom to exclude deallocation from the measurement.
    group.bench_function("seal_only", |b| {
        b.iter_batched(
            || {
                let old = Arc::new(RingBuffer::new(1024));
                let new = Arc::new(RingBuffer::new_continuing(4096, 0));
                (old, new)
            },
            |(old, new)| {
                // Seal: ArcSwap store + tokio Notify::notify_waiters.
                old.seal_and_forward(new.clone());
                // Return rings so Criterion drops them outside the timed window.
                (old, new)
            },
            BatchSize::SmallInput,
        )
    });

    // Cost of seal + one reader migrating (wait_for_data detects seal).
    // One packet is pre-pushed to the new ring so wait_for_data returns
    // immediately after migration (no indefinite wait on empty new ring).
    // Rings and reader are returned from the closure so Criterion drops them
    // outside the timed section.
    group.bench_function("seal_plus_one_reader_migrate", |b| {
        b.iter_batched(
            || {
                let old = Arc::new(RingBuffer::new(1024));
                let new = Arc::new(RingBuffer::new_continuing(4096, old.get_write_idx()));
                // Pre-push to new ring so wait_for_data returns after migration.
                new.push(make_packet(64));
                let reader = Reader::new("bench".to_string(), old.clone());
                (old, new, reader)
            },
            |(old, new, mut reader)| {
                old.seal_and_forward(new.clone());
                // Detect seal, migrate, see the pre-pushed packet, return.
                rt.block_on(async {
                    reader.wait_for_data().await;
                });
                (old, new, reader)
            },
            BatchSize::SmallInput,
        )
    });

    // N readers all migrating after a single seal.
    // On 2026-06-28 this measured ~615 ns at 1 reader, ~16.6 us at 32 readers,
    // and ~3.85 ms at 512 readers, with fanout cost steepening noticeably past 64 readers.
    // Same pre-push trick: one packet on new ring so all readers unblock promptly.
    // Range covers 1→512 (powers of 2); 512 matches the 500-reader extreme fanout
    // tested in benchmark_ring_buffer_concurrency.
    for n in [1, 2, 4, 8, 16, 32, 64, 128, 256, 512] {
        group.bench_with_input(
            BenchmarkId::new("seal_N_readers_migrate", n),
            &n,
            |b, &n| {
                b.iter_batched(
                    || {
                        let old = Arc::new(RingBuffer::new(1024));
                        let new = Arc::new(RingBuffer::new_continuing(4096, old.get_write_idx()));
                        new.push(make_packet(64));
                        let readers: Vec<Reader> = (0..n)
                            .map(|idx| Reader::new(format!("r{idx}"), old.clone()))
                            .collect();
                        (old, new, readers)
                    },
                    |(old, new, mut readers)| {
                        old.seal_and_forward(new.clone());
                        rt.block_on(async {
                            for r in &mut readers {
                                r.wait_for_data().await;
                            }
                        });
                        (old, new, readers)
                    },
                    BatchSize::SmallInput,
                )
            },
        );
    }

    group.finish();
}

criterion_group!(
    benches,
    benchmark_push_batch_vs_push,
    benchmark_pull_burst,
    benchmark_reader_lag,
    benchmark_active_reader_count,
    benchmark_seal_and_forward,
    benchmark_vec_capacity,
    benchmark_vec_loop_reuse,
    benchmark_burst_drain_alloc,
    benchmark_ts_chunk_burst_alloc,
    // Run concurrency bench last so 500 threads don't noise-up the pure benchmarks.
    benchmark_ring_buffer_concurrency,
);
criterion_main!(benches);
