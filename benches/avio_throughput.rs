use criterion::{Criterion, criterion_group, criterion_main};
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
        b.iter(|| {
            let queue = Arc::new(MemoryQueue::new());
            let q_clone = queue.clone();
            let p_clone = payload.clone();

            let writer = thread::spawn(move || {
                for _ in 0..iterations {
                    q_clone.write(&p_clone);
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

            let _ = writer.join();
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

criterion_group!(benches, bench_avio_throughput);
criterion_main!(benches);
