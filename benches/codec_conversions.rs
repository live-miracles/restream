//! Benchmarks for the payload format conversion hot path.
//!
//! Every function that runs per-packet on the media data path is measured here:
//!   avcc_to_annexb  — FLV ingest → TS muxer (per video frame, O(NALUs))
//!   annexb_to_avcc  — Ring buffer → RTMP egress (per video frame, O(NALUs))
//!   video_for_ts    — FLV and Raw variants (HLS/transcoder/recording consumers)
//!   video_for_rtmp  — Ring buffer → RTMP egress (alloc + AVCC wrap)
//!   audio_for_ts    — Raw AAC: ADTS passthrough + ADTS synthesis
//!   audio_for_rtmp  — ADTS strip + FLV 2-byte header
//!   packet_to_bytes — copy_from_slice vs Bytes::from_owner for encoded output packets
//!
//! Sizes are chosen to match real-world 1080p H.264 streams:
//!   IDR frame  ≈ 80–200 KiB (single NALU after split_annexb)
//!   P-frame    ≈ 8–30 KiB   (1–4 NALUs)
//!   Audio AAC  ≈ 200–400 B  (one frame)

use bytes::Bytes;
use criterion::{
    BatchSize, BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main,
};
use restream::media::codec::{
    annexb_to_avcc, annexb_to_avcc_with_scratch, audio_for_rtmp, audio_for_ts, avcc_to_annexb,
    video_for_rtmp, video_for_ts,
};
use restream::media::ring_buffer::PayloadFormat;

// ---------------------------------------------------------------------------
// OwnedVec: local stand-in for OwnedFfmpegPacket in codec/packet_to_bytes bench.
// The real implementation wraps `ffmpeg_next::Packet`; the overhead pattern is
// identical — one Box<dyn> allocation replacing one alloc + memcpy.
// ---------------------------------------------------------------------------
struct OwnedVec(Vec<u8>);
impl AsRef<[u8]> for OwnedVec {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

// ---------------------------------------------------------------------------
// Test data helpers
// ---------------------------------------------------------------------------

/// Build a 4-byte-length-prefixed AVCC buffer with `num_nalus` each of `nalu_size` bytes.
fn make_avcc(num_nalus: usize, nalu_size: usize) -> Vec<u8> {
    let mut buf = Vec::with_capacity(num_nalus * (4 + nalu_size));
    for i in 0..num_nalus {
        let len = nalu_size as u32;
        buf.extend_from_slice(&len.to_be_bytes());
        // NAL type byte: 0x01 = non-IDR slice (not SPS/PPS/AUD which are filtered)
        buf.push(if i == 0 { 0x65 } else { 0x41 }); // 0x65=IDR, 0x41=P-slice
        buf.extend(std::iter::repeat(0xBBu8).take(nalu_size - 1));
    }
    buf
}

/// Build an Annex B buffer with `num_nalus` each of `nalu_size` bytes.
/// First NALU is IDR (0x65), rest are P-slices (0x41).
fn make_annexb(num_nalus: usize, nalu_size: usize) -> Vec<u8> {
    let mut buf = Vec::with_capacity(num_nalus * (4 + nalu_size));
    for i in 0..num_nalus {
        buf.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        buf.push(if i == 0 { 0x65 } else { 0x41 });
        buf.extend(std::iter::repeat(0xBBu8).take(nalu_size - 1));
    }
    buf
}

/// Build a 5-byte FLV video tag header + AVCC payload (as produced by RTMP ingest).
fn make_flv_video(num_nalus: usize, nalu_size: usize, is_keyframe: bool) -> Vec<u8> {
    let tag = if is_keyframe { 0x17u8 } else { 0x27u8 };
    let mut buf = vec![tag, 1, 0, 0, 0]; // 5-byte FLV header, packet_type=1 (data)
    buf.extend(make_avcc(num_nalus, nalu_size));
    buf
}

/// 7-byte ADTS-framed AAC frame (typical 128kbps audio).
fn make_adts_audio(size: usize) -> Vec<u8> {
    // ADTS sync word + header: 0xFFF1 = MPEG-4 AAC-LC, no CRC
    let frame_len = size + 7;
    let mut h = Vec::with_capacity(7 + size);
    h.push(0xFF);
    h.push(0xF1);
    h.push(0x50); // AAC-LC, 48 kHz
    h.push(0x80);
    // 13-bit frame_length in bits 30..18: (frame_len << 3) >> 8 upper, lower
    h.push(((frame_len << 3) >> 8) as u8);
    h.push((frame_len << 3) as u8);
    h.push(0xFC);
    h.extend(std::iter::repeat(0xAA_u8).take(size));
    h
}

/// 2-byte FLV audio header + raw AAC payload (as produced by RTMP ingest).
fn make_flv_audio(size: usize) -> Vec<u8> {
    let mut buf = vec![0xAF, 1]; // FLV audio tag: AAC, data packet
    buf.extend(std::iter::repeat(0xCC_u8).take(size));
    buf
}

// ---------------------------------------------------------------------------
// avcc_to_annexb  (FLV ingest → TS muxer per video frame)
// ---------------------------------------------------------------------------

fn bench_avcc_to_annexb(c: &mut Criterion) {
    let mut group = c.benchmark_group("codec/avcc_to_annexb");

    for (label, num_nalus, nalu_size) in [
        ("p_frame_8k_1nalu", 1, 8 * 1024),
        ("p_frame_30k_3nalu", 3, 10 * 1024),
        ("idr_80k_1nalu", 1, 80 * 1024),
        ("idr_200k_1nalu", 1, 200 * 1024),
    ] {
        let avcc = make_avcc(num_nalus, nalu_size);
        group.throughput(Throughput::Bytes(avcc.len() as u64));
        group.bench_with_input(BenchmarkId::from_parameter(label), &avcc, |b, data| {
            b.iter(|| black_box(avcc_to_annexb(data, 4)))
        });
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// annexb_to_avcc  (ring buffer → RTMP egress per video frame)
// Two variants benchmarked for hardware-specific comparison:
//   two_pass:     split_annexb_nalus (2 Vec allocs/call; faster for IDR frames)
//   with_scratch: reuses caller-provided positions Vec (0 allocs after warmup;
//                 faster for small/multi-NALU frames)
//
// 2026-06-23 Zen results: two_pass wins IDR 80k (+42%); with_scratch wins
// P-frame 8k (+34%) and 3-NALU 30k (+9%). Production uses two_pass.
// Re-run on target hardware before switching: cargo bench --bench codec_conversions
// ---------------------------------------------------------------------------

fn bench_annexb_to_avcc(c: &mut Criterion) {
    let mut group = c.benchmark_group("codec/annexb_to_avcc");

    for (label, num_nalus, nalu_size) in [
        ("p_frame_8k_1nalu", 1, 8 * 1024),
        ("p_frame_30k_3nalu", 3, 10 * 1024),
        ("idr_80k_1nalu", 1, 80 * 1024),
    ] {
        let annexb = make_annexb(num_nalus, nalu_size);
        group.throughput(Throughput::Bytes(annexb.len() as u64));

        // Current two-pass implementation (chosen on 2026-06-23 Zen benchmarks)
        group.bench_with_input(BenchmarkId::new("two_pass", label), &annexb, |b, data| {
            b.iter(|| black_box(annexb_to_avcc(data)))
        });

        // Scratch-buffer variant: 0 Vec allocs per call after warmup
        group.bench_with_input(
            BenchmarkId::new("with_scratch", label),
            &annexb,
            |b, data| {
                let mut out = Vec::with_capacity(data.len());
                let mut sc = Vec::new();
                b.iter(|| {
                    out.clear();
                    black_box(annexb_to_avcc_with_scratch(data, &mut out, &mut sc));
                })
            },
        );
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// video_for_ts  — FLV (RTMP ingest consumers: HLS, transcoder, recording)
// ---------------------------------------------------------------------------

fn bench_video_for_ts(c: &mut Criterion) {
    let mut group = c.benchmark_group("codec/video_for_ts");

    // FLV P-frame: stripped + AVCC→Annex B
    let flv_p = make_flv_video(1, 8 * 1024, false);
    group.throughput(Throughput::Bytes(flv_p.len() as u64));
    group.bench_function("flv_pframe_8k", |b| {
        b.iter_batched(
            || (0usize, Vec::new()),
            |(mut nls, mut cache)| {
                black_box(video_for_ts(
                    &flv_p,
                    PayloadFormat::Flv,
                    &mut nls,
                    &mut cache,
                ))
            },
            BatchSize::SmallInput,
        )
    });

    // FLV IDR-frame: stripped + SPS/PPS prepend + AVCC→Annex B (keyframe path)
    // Simulate a populated SPS/PPS cache by using a seq header first
    let flv_idr = make_flv_video(1, 80 * 1024, true);
    group.throughput(Throughput::Bytes(flv_idr.len() as u64));
    group.bench_function("flv_idr_80k_with_spspps", |b| {
        // Pre-populated SPS/PPS cache (~30 bytes typical)
        let sps_pps: Vec<u8> = vec![
            0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0xC0, 0x1F, 0x00, 0x00, 0x00, 0x01, 0x68, 0xCE,
            0x38, 0x80,
        ];
        b.iter_batched(
            || (4usize, sps_pps.clone()),
            |(mut nls, mut cache)| {
                black_box(video_for_ts(
                    &flv_idr,
                    PayloadFormat::Flv,
                    &mut nls,
                    &mut cache,
                ))
            },
            BatchSize::SmallInput,
        )
    });

    // Raw Annex B (SRT ingest / transcoder output): zero-copy borrowed return
    let raw_p = make_annexb(1, 8 * 1024);
    group.throughput(Throughput::Bytes(raw_p.len() as u64));
    group.bench_function("raw_pframe_8k_borrowed", |b| {
        b.iter_batched(
            || (0usize, Vec::new()),
            |(mut nls, mut cache)| {
                black_box(video_for_ts(
                    &raw_p,
                    PayloadFormat::Raw,
                    &mut nls,
                    &mut cache,
                ))
            },
            BatchSize::SmallInput,
        )
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// video_for_rtmp  (ring buffer → RTMP egress: Annex B → FLV/AVCC)
// ---------------------------------------------------------------------------

fn bench_video_for_rtmp(c: &mut Criterion) {
    let mut group = c.benchmark_group("codec/video_for_rtmp");

    for (label, num_nalus, nalu_size, is_key) in [
        ("pframe_8k", 1, 8 * 1024, false),
        ("pframe_30k_3nalu", 3, 10 * 1024, false),
        ("idr_80k", 1, 80 * 1024, true),
    ] {
        let annexb = make_annexb(num_nalus, nalu_size);
        group.throughput(Throughput::Bytes(annexb.len() as u64));
        group.bench_with_input(BenchmarkId::from_parameter(label), &annexb, |b, data| {
            b.iter(|| black_box(video_for_rtmp(data, is_key)))
        });
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// audio_for_ts  — Raw ADTS passthrough and ADTS synthesis
// ---------------------------------------------------------------------------

fn bench_audio_for_ts(c: &mut Criterion) {
    let mut group = c.benchmark_group("codec/audio_for_ts");

    // ADTS passthrough (SRT ingest): borrowed, no alloc
    let adts = make_adts_audio(200);
    group.throughput(Throughput::Bytes(adts.len() as u64));
    group.bench_function("raw_adts_passthrough_207b", |b| {
        b.iter(|| black_box(audio_for_ts(&adts, PayloadFormat::Raw, 48000, 2)))
    });

    // FLV AAC: strip 2-byte header + prepend 7-byte ADTS
    let flv_audio = make_flv_audio(200);
    group.throughput(Throughput::Bytes(flv_audio.len() as u64));
    group.bench_function("flv_aac_strip_adts_wrap_200b", |b| {
        b.iter(|| black_box(audio_for_ts(&flv_audio, PayloadFormat::Flv, 48000, 2)))
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// audio_for_rtmp  (ring buffer → RTMP egress: ADTS strip + FLV header)
// ---------------------------------------------------------------------------

fn bench_audio_for_rtmp(c: &mut Criterion) {
    let mut group = c.benchmark_group("codec/audio_for_rtmp");

    let adts = make_adts_audio(200);
    group.throughput(Throughput::Bytes(adts.len() as u64));
    group.bench_function("adts_to_flv_207b", |b| {
        b.iter(|| black_box(audio_for_rtmp(&adts)))
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// packet_to_bytes  (transcoder output: copy_from_slice vs Bytes::from_owner)
// ---------------------------------------------------------------------------

/// Benchmark `Bytes::copy_from_slice` (current) vs `Bytes::from_owner` (proposed)
/// for wrapping FFmpeg encoded output packets into ring-buffer payloads.
///
/// `OwnedVec` stands in for `OwnedFfmpegPacket(ffmpeg_next::Packet)`. The
/// allocation pattern is identical: one `Box<dyn>` in `from_owner` replaces
/// one `alloc + memcpy` in `copy_from_slice`. For video I-frames (80–200 KB)
/// the saving is substantial; for audio (~200 B) it is negligible.
fn bench_packet_to_bytes(c: &mut Criterion) {
    let mut group = c.benchmark_group("codec/packet_to_bytes");

    // Real encoded frame sizes at 1080p30 H.264 ~3 Mbps:
    //   audio AAC   ≈ 200 B  (1 frame / 23 ms)
    //   P-frame     ≈ 8 KB   (inter, no keyframe)
    //   I-frame     ≈ 80 KB  (IDR, ~3× per second)
    //   large IDR   ≈ 200 KB (scene change / initial)
    let sizes: &[(usize, &str)] = &[
        (200, "200B_audio"),
        (8 * 1024, "8KB_pframe"),
        (80 * 1024, "80KB_iframe"),
        (200 * 1024, "200KB_iframe"),
    ];

    for &(size, label) in sizes {
        let data = vec![0xBBu8; size];
        group.throughput(Throughput::Bytes(size as u64));

        // Baseline: alloc a new buffer + memcpy (current approach in all 7 sites)
        group.bench_with_input(
            BenchmarkId::new("copy_from_slice", label),
            &data,
            |b, data| b.iter(|| black_box(Bytes::copy_from_slice(data))),
        );

        // Proposed: move ownership into a Box<dyn>, zero memcpy.
        // In production, `OwnedVec` is replaced by `OwnedFfmpegPacket(packet)` where
        // `packet` is already owned (demux loop) or by `OwnedFfmpegPacket(enc_pkt.clone())`
        // where `.clone()` calls `av_packet_ref()` — a refcount bump, not a data copy.
        group.bench_with_input(BenchmarkId::new("from_owner", label), &size, |b, &size| {
            b.iter_batched(
                || vec![0xBBu8; size],
                |v| black_box(Bytes::from_owner(OwnedVec(v))),
                BatchSize::SmallInput,
            )
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_avcc_to_annexb,
    bench_annexb_to_avcc,
    bench_video_for_ts,
    bench_video_for_rtmp,
    bench_audio_for_ts,
    bench_audio_for_rtmp,
    bench_packet_to_bytes,
);
criterion_main!(benches);
