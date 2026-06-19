use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use restream::media::simd::find_sync_byte;

fn bench_sync_search(c: &mut Criterion) {
    let mut group = c.benchmark_group("sync_byte_search");
    for size in &[1024, 8192, 65536] {
        let mut data = vec![0x00u8; *size];
        // Place the sync byte near the end to force scanning most of the buffer
        let sync_idx = *size - 10;
        data[sync_idx] = 0x47;

        group.bench_with_input(BenchmarkId::new("std_search", size), size, |b, _s| {
            b.iter(|| {
                let pos = data.iter().position(|&b| b == 0x47);
                assert_eq!(pos, Some(sync_idx));
            })
        });

        group.bench_with_input(BenchmarkId::new("simd_search", size), size, |b, _s| {
            b.iter(|| {
                let pos = find_sync_byte(&data);
                assert_eq!(pos, Some(sync_idx));
            })
        });
    }
    group.finish();
}

criterion_group!(benches, bench_sync_search);
criterion_main!(benches);
