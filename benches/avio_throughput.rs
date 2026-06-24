use criterion::{BatchSize, Criterion, Throughput, black_box, criterion_group, criterion_main};
use restream::media::avio::MemoryQueue;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;

fn bench_avio_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("in_memory_vs_tcp_loopback");
    let chunk_size = 65536;
    let iterations = 16; // 1 MB total transfer per run
    let total_size = chunk_size * iterations;

    let payload = vec![0xCCu8; chunk_size];

    // Benchmark Custom MemoryQueue
    group.bench_function("memory_queue_throughput", |b| {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
        b.iter(|| {
            let queue = Arc::new(MemoryQueue::new());
            let q_clone = queue.clone();
            let p_clone = payload.clone();

            let writer = runtime.spawn(async move {
                for _ in 0..iterations {
                    q_clone.write(&p_clone).await;
                }
                q_clone.close();
            });

            let mut read_buf = vec![0u8; chunk_size];
            let mut bytes_read = 0;
            while bytes_read < total_size {
                let n = queue.read(&mut read_buf);
                if n == 0 {
                    break;
                }
                bytes_read += n;
            }

            runtime.block_on(async {
                let _ = writer.await;
            });
        })
    });

    // Benchmark TCP Loopback
    group.bench_function("tcp_loopback_throughput", |b| {
        // Bind local TCP listener
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        b.iter(|| {
            let p_clone = payload.clone();
            let writer = thread::spawn(move || {
                let mut stream = TcpStream::connect(format!("127.0.0.1:{}", port)).unwrap();
                for _ in 0..iterations {
                    let _ = stream.write_all(&p_clone);
                }
            });

            let (mut stream, _) = listener.accept().unwrap();
            let mut read_buf = vec![0u8; chunk_size];
            let mut bytes_read = 0;
            while bytes_read < total_size {
                let n = stream.read(&mut read_buf).unwrap();
                if n == 0 {
                    break;
                }
                bytes_read += n;
            }

            let _ = writer.join();
        })
    });

    group.finish();
}

/// Cost of `MemoryQueue::len()` — one Mutex lock/unlock + `VecDeque::len()`.
/// Both variants target < 50 ns; a value near the lock round-trip time (~20 ns)
/// confirms the call adds negligible overhead to any monitoring loop.
fn bench_memory_queue_len(c: &mut Criterion) {
    let mut group = c.benchmark_group("memory_queue/len");
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");

    group.bench_function("empty", |b| {
        let q = MemoryQueue::new();
        b.iter(|| black_box(q.len()))
    });

    group.bench_function("loaded_64k", |b| {
        let q = MemoryQueue::new();
        runtime.block_on(q.write(&vec![0u8; 65_536]));
        b.iter(|| black_box(q.len()))
    });

    group.finish();
}

/// Compares `write_batch` throughput for a single 1316-byte MPEG-TS packet
/// with and without a subsequent `len()` call.  Quantifies the cost of polling
/// queue depth on every write (worst-case diagnostic overhead).
fn bench_write_batch_overhead(c: &mut Criterion) {
    let mut group = c.benchmark_group("memory_queue/write_batch");
    group.throughput(Throughput::Bytes(1316));

    let chunk = vec![0u8; 1316];
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");

    group.bench_function("without_len", |b| {
        let c = chunk.clone();
        b.iter_batched(
            MemoryQueue::new,
            |q| {
                let _ = runtime.block_on(q.write_batch([c.as_slice()]));
            },
            BatchSize::SmallInput,
        )
    });

    group.bench_function("with_len", |b| {
        let c = chunk.clone();
        b.iter_batched(
            MemoryQueue::new,
            |q| {
                let _ = runtime.block_on(q.write_batch([c.as_slice()]));
                black_box(q.len())
            },
            BatchSize::SmallInput,
        )
    });

    group.finish();
}

criterion_group!(benches, bench_avio_throughput, bench_memory_queue_len, bench_write_batch_overhead);
criterion_main!(benches);
