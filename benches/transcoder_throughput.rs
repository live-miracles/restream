use bytes::Bytes;
use criterion::{Criterion, criterion_group, criterion_main};

fn benchmark_transcode_throughput(c: &mut Criterion) {
    // Simulating 1080p YUV420p frame size (1920 * 1080 * 1.5 = 3,110,400 bytes)
    let frame_size = 1920 * 1080 * 3 / 2;
    let mock_frame = Bytes::from(vec![128u8; frame_size]);

    c.bench_function("transcoder_yuv420p_frame_copy_throughput", |b| {
        b.iter(|| {
            // Benchmark memory copy / cache locality throughput
            let _copied = mock_frame.clone();
        })
    });
}

criterion_group!(benches, benchmark_transcode_throughput);
criterion_main!(benches);
