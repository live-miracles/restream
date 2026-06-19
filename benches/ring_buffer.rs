use bytes::Bytes;
use criterion::{Criterion, criterion_group, criterion_main};
use restream::media::ring_buffer::{MediaPacket, MediaType, Reader, RingBuffer};
use std::sync::Arc;
use std::thread;

fn benchmark_ring_buffer_concurrency(c: &mut Criterion) {
    let buffer = Arc::new(RingBuffer::new(4096));

    // Spawn 500 consumer threads simulating client connections
    let mut consumers = Vec::new();
    for _ in 0..500 {
        let buf_clone = buffer.clone();
        let handle = thread::spawn(move || {
            let mut reader = Reader::new(buf_clone);
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
            let packet = MediaPacket {
                media_type: MediaType::Video,
                track_index: 0,
                pts: 0,
                dts: 0,
                is_keyframe: true,
                payload: Bytes::from(vec![0u8; 1024]),
            };
            buffer.push(packet);
        })
    });

    // Wait for consumers to complete
    for handle in consumers {
        let _ = handle.join();
    }
}

criterion_group!(benches, benchmark_ring_buffer_concurrency);
criterion_main!(benches);
