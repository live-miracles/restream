//! Benchmark: per-pipeline CPU cost of always-on HLS preview.
//!
//! Simulates realistic stream profiles (720p30, 1080p30, 1080p60, 4K30) at
//! real-world bitrates. Measures TsMuxer cost + segment accumulation + HlsStore
//! push — the full hot path of the rewritten HLS segmenter minus ring buffer reads.
//!
//! Reports wall-clock time per second of content so you can derive CPU%.

use bytes::BytesMut;
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use restream::media::engine::{AudioMeta, VideoMeta};
use restream::media::hls::HlsStore;
use restream::media::mpegts::TsMuxer;
use restream::media::ring_buffer::MediaType;
use std::sync::Arc;
use std::time::Instant;

struct StreamProfile {
    name: &'static str,
    width: u32,
    height: u32,
    fps: u32,
    video_bitrate_kbps: u32,
    audio_bitrate_kbps: u32,
    codec: &'static str,
}

const PROFILES: &[StreamProfile] = &[
    StreamProfile {
        name: "720p30_h264_3mbps",
        width: 1280,
        height: 720,
        fps: 30,
        video_bitrate_kbps: 3000,
        audio_bitrate_kbps: 128,
        codec: "h264",
    },
    StreamProfile {
        name: "1080p30_h264_5mbps",
        width: 1920,
        height: 1080,
        fps: 30,
        video_bitrate_kbps: 5000,
        audio_bitrate_kbps: 128,
        codec: "h264",
    },
    StreamProfile {
        name: "1080p60_h264_8mbps",
        width: 1920,
        height: 1080,
        fps: 60,
        video_bitrate_kbps: 8000,
        audio_bitrate_kbps: 192,
        codec: "h264",
    },
    StreamProfile {
        name: "4k30_hevc_15mbps",
        width: 3840,
        height: 2160,
        fps: 30,
        video_bitrate_kbps: 15000,
        audio_bitrate_kbps: 192,
        codec: "hevc",
    },
];

fn build_one_second(profile: &StreamProfile) -> Vec<(MediaType, u32, i64, bool, Vec<u8>)> {
    let video_bytes_per_sec = (profile.video_bitrate_kbps as usize) * 1000 / 8;
    let gop_frames = profile.fps.min(60) as usize; // 1 keyframe per second
    let idr_size = video_bytes_per_sec / 3; // keyframes are ~3x larger
    let p_size = (video_bytes_per_sec - idr_size) / (gop_frames - 1).max(1);
    let audio_frame_size = (profile.audio_bitrate_kbps as usize) * 1000 / 8 / 48; // ~48 AAC frames/s

    let idr_payload: Vec<u8> = [0x00, 0x00, 0x00, 0x01, 0x65]
        .into_iter()
        .chain(std::iter::repeat_n(0xAA, idr_size.saturating_sub(5)))
        .collect();
    let p_payload: Vec<u8> = [0x00, 0x00, 0x00, 0x01, 0x41]
        .into_iter()
        .chain(std::iter::repeat_n(0xBB, p_size.saturating_sub(5)))
        .collect();
    let audio_payload: Vec<u8> = [0xFF, 0xF1, 0x4C, 0x80, 0x04, 0x1F, 0xFC]
        .into_iter()
        .chain(std::iter::repeat_n(
            0xCC,
            audio_frame_size.saturating_sub(7),
        ))
        .collect();

    let frame_dur_ms = 1000 / profile.fps as i64;
    let mut packets = Vec::new();

    for i in 0..profile.fps as i64 {
        let pts = i * frame_dur_ms;
        let is_key = i == 0;
        let payload = if is_key {
            idr_payload.clone()
        } else {
            p_payload.clone()
        };
        packets.push((MediaType::Video, 0u32, pts, is_key, payload));

        // ~48 AAC frames/s → roughly 1-2 audio frames per video frame
        let audio_frames_this_tick = if i < 48 % profile.fps as i64 { 2 } else { 1 };
        for _ in 0..audio_frames_this_tick {
            packets.push((MediaType::Audio, 0u32, pts, false, audio_payload.clone()));
        }
    }

    packets
}

fn bench_hls_mux_cost(c: &mut Criterion) {
    let mut group = c.benchmark_group("hls_pipeline_cost");
    group.sample_size(50);

    for profile in PROFILES {
        let packets = build_one_second(profile);
        let total_input: usize = packets.iter().map(|p| p.4.len()).sum();

        let video = VideoMeta {
            codec: profile.codec.to_string(),
            width: profile.width,
            height: profile.height,
            fps: profile.fps as f64,
            bw: None,
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
            profile: None,
        };

        // Mux only (no segment storage) — isolates TsMuxer cost
        group.bench_with_input(
            BenchmarkId::new("mux_only", profile.name),
            &profile.name,
            |b, _| {
                b.iter(|| {
                    let mut muxer = TsMuxer::new(Some(&video), std::slice::from_ref(&audio));
                    let mut ts_bytes = 0usize;
                    for (mt, ti, pts, key, payload) in &packets {
                        ts_bytes += muxer.mux_packet(*mt, *ti, *pts, *pts, *key, payload).len();
                    }
                    ts_bytes
                });
            },
        );

        // Full path: mux + accumulate + segment push (simulates 6s segments)
        group.bench_with_input(
            BenchmarkId::new("mux_and_segment", profile.name),
            &profile.name,
            |b, _| {
                b.iter(|| {
                    let store = Arc::new(HlsStore::new());
                    let mut muxer = TsMuxer::new(Some(&video), std::slice::from_ref(&audio));
                    let mut accumulator = BytesMut::with_capacity(8 * 1024 * 1024);

                    // Simulate 6 seconds (one segment)
                    for sec in 0..6 {
                        for (mt, ti, pts, key, payload) in &packets {
                            let adjusted_pts = *pts + sec * 1000;
                            let ts = muxer.mux_packet(
                                *mt,
                                *ti,
                                adjusted_pts,
                                adjusted_pts,
                                *key,
                                payload,
                            );
                            accumulator.extend_from_slice(ts);
                        }
                    }

                    // Push segment
                    store.push_segment(6.0, accumulator.split().freeze());
                    store
                });
            },
        );

        eprintln!(
            "[{}] input: {} KiB/s video, {} KiB/s audio, {} packets/s",
            profile.name,
            profile.video_bitrate_kbps / 8,
            profile.audio_bitrate_kbps / 8,
            packets.len(),
        );
        eprintln!(
            "[{}] total input payload per second: {} KiB",
            profile.name,
            total_input / 1024,
        );
    }

    group.finish();
}

fn bench_hls_memory_cost(c: &mut Criterion) {
    let mut group = c.benchmark_group("hls_memory_cost");
    group.sample_size(20);

    for profile in PROFILES {
        let packets = build_one_second(profile);
        let video = VideoMeta {
            codec: profile.codec.to_string(),
            width: profile.width,
            height: profile.height,
            fps: profile.fps as f64,
            bw: None,
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
            profile: None,
        };

        // Measure steady-state memory: fill 10 segments (60s window)
        group.bench_with_input(
            BenchmarkId::new("steady_state_10seg", profile.name),
            &profile.name,
            |b, _| {
                b.iter_custom(|iters| {
                    let started = Instant::now();
                    for _ in 0..iters {
                        let store = Arc::new(HlsStore::new());
                        let mut muxer = TsMuxer::new(Some(&video), std::slice::from_ref(&audio));

                        for seg_idx in 0..10u64 {
                            let mut accumulator = BytesMut::with_capacity(8 * 1024 * 1024);
                            for sec in 0..6i64 {
                                for (mt, ti, pts, key, payload) in &packets {
                                    let t = *pts + (seg_idx as i64 * 6 + sec) * 1000;
                                    let ts = muxer.mux_packet(*mt, *ti, t, t, *key, payload);
                                    accumulator.extend_from_slice(ts);
                                }
                            }
                            store.push_segment(6.0, accumulator.freeze());
                        }
                        std::hint::black_box(&store);
                    }
                    started.elapsed()
                });
            },
        );

        // Report segment sizes
        {
            let mut muxer = TsMuxer::new(Some(&video), std::slice::from_ref(&audio));
            let mut accumulator = BytesMut::with_capacity(8 * 1024 * 1024);
            for sec in 0..6i64 {
                for (mt, ti, pts, key, payload) in &packets {
                    let t = *pts + sec * 1000;
                    let ts = muxer.mux_packet(*mt, *ti, t, t, *key, payload);
                    accumulator.extend_from_slice(ts);
                }
            }
            let seg_size = accumulator.len();
            eprintln!(
                "[{}] segment size (6s): {} KiB, 10-segment window: {} MiB",
                profile.name,
                seg_size / 1024,
                seg_size * 10 / (1024 * 1024),
            );
        }
    }

    group.finish();
}

fn benches(c: &mut Criterion) {
    bench_hls_mux_cost(c);
    bench_hls_memory_cost(c);
}

criterion_group!(hls_cost, benches);
criterion_main!(hls_cost);
