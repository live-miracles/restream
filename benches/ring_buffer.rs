use bytes::Bytes;
use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};
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

criterion_group!(
    benches,
    benchmark_push_batch_vs_push,
    benchmark_pull_burst,
    // Run concurrency bench last so 500 threads don't noise-up the pure benchmarks.
    benchmark_ring_buffer_concurrency,
);
criterion_main!(benches);
