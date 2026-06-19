use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use restream::media::simd::optimized_copy;

fn bench_memcpy(c: &mut Criterion) {
    let mut group = c.benchmark_group("memcpy_comparison");
    for size in &[1024, 8192, 65536, 524288] {
        let src = vec![0xABu8; *size];
        let mut dst_std = vec![0u8; *size];
        let mut dst_simd = vec![0u8; *size];

        group.bench_with_input(BenchmarkId::new("std_copy", size), size, |b, _s| {
            b.iter(|| {
                dst_std.copy_from_slice(&src);
            })
        });

        group.bench_with_input(BenchmarkId::new("simd_copy", size), size, |b, _s| {
            b.iter(|| {
                optimized_copy(&mut dst_simd, &src);
            })
        });
    }
    group.finish();
}

criterion_group!(benches, bench_memcpy);
criterion_main!(benches);
