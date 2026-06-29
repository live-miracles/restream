//! **Decision benchmark** — run on demand, not in the routine bench loop.
//!
//! Compares `memchr` / `pulp` / `wide` / scalar for byte search and memcpy.
//! Per CLAUDE.md SIMD rules this is a one-time decision to pick an implementation,
//! not a continuous regression guard. The scalar oracle for the chosen path lives
//! in unit tests.
//!
//! Candidates:
//!   - `memchr`: specialized byte-search crate (portable, runtime dispatch)
//!   - `pulp`:   generic SIMD abstraction with runtime dispatch (AVX-512→AVX2→SSE2→scalar)
//!   - `wide`:   fixed-width SIMD types with compile-time dispatch
//!   - `std`:    copy_from_slice / iter::position (libc memcpy / scalar baseline)

use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};
use memchr::memchr;

// --- pulp: runtime-dispatched byte search ---

fn pulp_find_sync_byte(data: &[u8]) -> Option<usize> {
    use pulp::Simd;

    struct FindSync<'a>(&'a [u8]);

    impl pulp::WithSimd for FindSync<'_> {
        type Output = Option<usize>;

        #[inline(always)]
        fn with_simd<S: Simd>(self, simd: S) -> Self::Output {
            let data = self.0;
            let target = simd.splat_u8s(0x47);
            let (chunks, tail) = S::as_simd_u8s(data);

            let chunk_len = chunks.len() * S::U8_LANES;
            for (chunk_idx, &chunk) in chunks.iter().enumerate() {
                let mask = simd.equal_u8s(chunk, target);
                let first = simd.first_true_m8s(mask);
                if first < S::U8_LANES {
                    return Some(chunk_idx * S::U8_LANES + first);
                }
            }

            // Scalar tail
            for (i, &b) in tail.iter().enumerate() {
                if b == 0x47 {
                    return Some(chunk_len + i);
                }
            }

            None
        }
    }

    pulp::Arch::new().dispatch(FindSync(data))
}

// --- pulp: runtime-dispatched memcpy ---

fn pulp_copy(dst: &mut [u8], src: &[u8]) {
    use pulp::Simd;

    struct Copy<'a> {
        dst: &'a mut [u8],
        src: &'a [u8],
    }

    impl pulp::WithSimd for Copy<'_> {
        type Output = ();

        #[inline(always)]
        fn with_simd<S: Simd>(self, _simd: S) -> Self::Output {
            let (dst_chunks, dst_tail) = S::as_mut_simd_u8s(self.dst);
            let (src_chunks, src_tail) = S::as_simd_u8s(self.src);

            for (d, s) in dst_chunks.iter_mut().zip(src_chunks.iter()) {
                *d = *s;
            }

            dst_tail.copy_from_slice(src_tail);
        }
    }

    pulp::Arch::new().dispatch(Copy { dst, src });
}

// --- wide: compile-time dispatched byte search (u8x32 = 256-bit) ---

fn wide_find_sync_byte(data: &[u8]) -> Option<usize> {
    use wide::u8x32;

    let target = u8x32::splat(0x47);
    let chunks = data.chunks_exact(32);
    let remainder = chunks.remainder();

    for (chunk_idx, chunk) in chunks.enumerate() {
        let v = u8x32::from(<[u8; 32]>::try_from(chunk).unwrap());
        let cmp = v.simd_eq(target);
        let mask = cmp.to_bitmask();
        if mask != 0 {
            return Some(chunk_idx * 32 + mask.trailing_zeros() as usize);
        }
    }

    let base = data.len() - remainder.len();
    for (i, &b) in remainder.iter().enumerate() {
        if b == 0x47 {
            return Some(base + i);
        }
    }

    None
}

// --- wide: compile-time dispatched memcpy (u8x32 = 256-bit) ---

fn wide_copy(dst: &mut [u8], src: &[u8]) {
    use wide::u8x32;

    let mut dst_chunks = dst.chunks_exact_mut(32);
    let mut src_chunks = src.chunks_exact(32);

    for (d, s) in dst_chunks.by_ref().zip(src_chunks.by_ref()) {
        let v = u8x32::from(<[u8; 32]>::try_from(s).unwrap());
        let arr: [u8; 32] = v.into();
        d.copy_from_slice(&arr);
    }

    let dst_rem = dst_chunks.into_remainder();
    let src_rem = src_chunks.remainder();
    dst_rem.copy_from_slice(src_rem);
}

// ==========================================================================
// Benchmarks
// ==========================================================================

fn bench_sync_byte_alternatives(c: &mut Criterion) {
    let mut group = c.benchmark_group("sync_byte_alternatives");

    for &size in &[1024usize, 8192, 65536, 262144] {
        group.throughput(Throughput::Bytes(size as u64));

        let mut data_end = vec![0u8; size];
        data_end[size - 10] = 0x47;

        let mut data_start = vec![0u8; size];
        data_start[5] = 0x47;

        let data_miss = vec![0u8; size];

        // --- Worst case (near end) ---
        group.bench_with_input(BenchmarkId::new("memchr/near_end", size), &size, |b, _| {
            b.iter(|| memchr(0x47, &data_end))
        });
        group.bench_with_input(BenchmarkId::new("pulp/near_end", size), &size, |b, _| {
            b.iter(|| pulp_find_sync_byte(&data_end))
        });
        group.bench_with_input(BenchmarkId::new("wide/near_end", size), &size, |b, _| {
            b.iter(|| wide_find_sync_byte(&data_end))
        });
        group.bench_with_input(
            BenchmarkId::new("scalar_iter/near_end", size),
            &size,
            |b, _| b.iter(|| data_end.iter().position(|&b| b == 0x47)),
        );

        // --- Best case (near start) ---
        group.bench_with_input(
            BenchmarkId::new("memchr/near_start", size),
            &size,
            |b, _| b.iter(|| memchr(0x47, &data_start)),
        );
        group.bench_with_input(BenchmarkId::new("pulp/near_start", size), &size, |b, _| {
            b.iter(|| pulp_find_sync_byte(&data_start))
        });
        group.bench_with_input(BenchmarkId::new("wide/near_start", size), &size, |b, _| {
            b.iter(|| wide_find_sync_byte(&data_start))
        });

        // --- Miss (full scan) ---
        group.bench_with_input(BenchmarkId::new("memchr/miss", size), &size, |b, _| {
            b.iter(|| memchr(0x47, &data_miss))
        });
        group.bench_with_input(BenchmarkId::new("pulp/miss", size), &size, |b, _| {
            b.iter(|| pulp_find_sync_byte(&data_miss))
        });
        group.bench_with_input(BenchmarkId::new("wide/miss", size), &size, |b, _| {
            b.iter(|| wide_find_sync_byte(&data_miss))
        });
    }

    group.finish();
}

fn bench_memcpy_alternatives(c: &mut Criterion) {
    let mut group = c.benchmark_group("memcpy_alternatives");

    for &size in &[1024usize, 8192, 65536, 524288] {
        group.throughput(Throughput::Bytes(size as u64));

        let src = vec![0xABu8; size];
        let mut dst_std = vec![0u8; size];
        let mut dst_pulp = vec![0u8; size];
        let mut dst_wide = vec![0u8; size];

        group.bench_with_input(
            BenchmarkId::new("std_copy_from_slice", size),
            &size,
            |b, _| b.iter(|| dst_std.copy_from_slice(&src)),
        );

        group.bench_with_input(BenchmarkId::new("pulp", size), &size, |b, _| {
            b.iter(|| pulp_copy(&mut dst_pulp, &src))
        });

        group.bench_with_input(BenchmarkId::new("wide", size), &size, |b, _| {
            b.iter(|| wide_copy(&mut dst_wide, &src))
        });
    }

    group.finish();
}

fn bench_ts_mux_inhouse(c: &mut Criterion) {
    use restream::media::engine::{AudioMeta, VideoMeta};
    use restream::media::mpegts::TsMuxer;
    use restream::media::ring_buffer::MediaType;

    let video = VideoMeta {
        codec: "h264".to_string(),
        width: 1920,
        height: 1080,
        fps: 30.0,
        bw: None,
        pid: None,
        language: None,
        title: None,
        profile: None,
        level: None,
        pixel_format: None,
    };
    let audio = AudioMeta {
        codec: "aac".to_string(),
        sample_rate: 48000,
        channels: 2,
        channel_layout: None,
        track_index: 0,
        pid: None,
        language: None,
        title: None,
        profile: None,
    };

    let idr_payload = vec![0x00, 0x00, 0x00, 0x01, 0x65]
        .into_iter()
        .chain(std::iter::repeat_n(0xAA, 50_000))
        .collect::<Vec<_>>();
    let p_payload = vec![0x00, 0x00, 0x00, 0x01, 0x41]
        .into_iter()
        .chain(std::iter::repeat_n(0xBB, 5_000))
        .collect::<Vec<_>>();
    let audio_payload = vec![0xFF, 0xF1, 0x4C, 0x80, 0x04, 0x1F, 0xFC]
        .into_iter()
        .chain(std::iter::repeat_n(0xCC, 1_000))
        .collect::<Vec<_>>();

    let mut packets: Vec<(MediaType, u32, i64, bool, &[u8])> = Vec::new();
    for i in 0..30 {
        let pts = i * 33;
        let is_key = i == 0;
        let payload = if is_key { &idr_payload } else { &p_payload };
        packets.push((MediaType::Video, 0, pts, is_key, payload));
        packets.push((MediaType::Audio, 0, pts, false, &audio_payload));
    }

    let total_payload: usize = packets.iter().map(|(_, _, _, _, p)| p.len()).sum();

    let mut group = c.benchmark_group("ts_mux_inhouse");
    group.throughput(Throughput::Bytes(total_payload as u64));
    group.sample_size(50);

    group.bench_function("1s_30fps_1080p", |b| {
        b.iter(|| {
            let mut muxer = TsMuxer::new(Some(&video), std::slice::from_ref(&audio));
            let mut total = 0usize;
            for &(mt, ti, pts, key, payload) in &packets {
                total += muxer.mux_packet(mt, ti, pts, pts, key, payload).len();
            }
            total
        });
    });

    group.finish();
}

fn crc32_mpeg2_bit_at_a_time(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= (byte as u32) << 24;
        for _ in 0..8 {
            if crc & 0x8000_0000 != 0 {
                crc = (crc << 1) ^ 0x04C1_1DB7;
            } else {
                crc <<= 1;
            }
        }
    }
    crc
}

fn crc32_mpeg2_table_driven(data: &[u8]) -> u32 {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut table = [0u32; 256];
        for (i, slot) in table.iter_mut().enumerate() {
            let mut crc = (i as u32) << 24;
            for _ in 0..8 {
                if crc & 0x8000_0000 != 0 {
                    crc = (crc << 1) ^ 0x04C1_1DB7;
                } else {
                    crc <<= 1;
                }
            }
            *slot = crc;
        }
        table
    });

    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        let idx = (((crc >> 24) ^ (byte as u32)) & 0xFF) as usize;
        crc = (crc << 8) ^ table[idx];
    }
    crc
}

fn bench_crc32_alternatives(c: &mut Criterion) {
    let mut group = c.benchmark_group("crc32_mpeg2_alternatives");

    for &size in &[12usize, 188, 1024] {
        group.throughput(Throughput::Bytes(size as u64));
        let data = vec![0xABu8; size];

        group.bench_with_input(BenchmarkId::new("bit_at_a_time", size), &data, |b, d| {
            b.iter(|| black_box(crc32_mpeg2_bit_at_a_time(d)))
        });

        group.bench_with_input(BenchmarkId::new("table_driven", size), &data, |b, d| {
            b.iter(|| black_box(crc32_mpeg2_table_driven(d)))
        });

        // crc-fast (PCLMULQDQ) was benchmarked here during evaluation:
        //   188 B: 40.6 ns (4.31 GiB/s, 11.7× vs table)
        //   1024 B: 66.5 ns (14.3 GiB/s, 40.7× vs table)
        //   12 B: 25.8 ns (444 MiB/s, 0.4× vs table ← SIMD overhead dominates)
        // We chose table-driven for zero dependencies and faster tiny-input perf.
    }

    group.finish();
}

fn benches(c: &mut Criterion) {
    bench_sync_byte_alternatives(c);
    bench_memcpy_alternatives(c);
    bench_ts_mux_inhouse(c);
    bench_crc32_alternatives(c);
}

criterion_group!(alt_benches, benches);
criterion_main!(alt_benches);
