//! Benchmark: per-pipeline CPU cost of always-on HLS preview.
//!
//! Simulates realistic stream profiles (720p30, 1080p30, 1080p60, 4K30) at
//! real-world bitrates. Measures TsMuxer cost + segment accumulation + HlsStore
//! push — the full hot path of the rewritten HLS segmenter minus ring buffer reads.
//!
//! Reports wall-clock time per second of content so you can derive CPU%.

use bytes::BytesMut;
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use mse_fmp4::io::WriteTo;
use restream::media::engine::{AudioMeta, VideoMeta};
use restream::media::hls::HlsStore;
use restream::media::mpegts::TsMuxer;
use restream::media::ring_buffer::MediaType;
use std::io::Cursor;
use std::sync::Arc;
use std::time::Instant;

const TS_PACKET_SIZE: usize = 188;
const TS_SDT_PID: u16 = 0x0011;

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

fn build_h264_first_audio_only_segment(seconds: u32) -> Vec<u8> {
    assert_eq!(
        seconds, 6,
        "update the checked-in HLS benchmark fixture if the requested duration changes"
    );
    std::fs::read(
        restream::test_fixtures::checked_in_fixture("test/fixtures/hls-first-audio-only-6s.ts")
            .expect("checked-in HLS benchmark fixture"),
    )
    .expect("read checked-in HLS benchmark fixture")
}

fn convert_ts_segment_to_fmp4_sizes(ts_segment: &[u8]) -> (usize, usize) {
    let ts_reader =
        mpeg2ts::ts::TsPacketReader::new(Cursor::new(strip_ts_pid(ts_segment, TS_SDT_PID)));
    let (init_segment, media_segment) =
        mse_fmp4::mpeg2_ts::to_fmp4(ts_reader).expect("benchmark TS should convert to fMP4");

    let mut init_bytes = Vec::new();
    init_segment
        .write_to(&mut init_bytes)
        .expect("benchmark init segment serialization");

    let mut media_bytes = Vec::new();
    media_segment
        .write_to(&mut media_bytes)
        .expect("benchmark media segment serialization");

    (init_bytes.len(), media_bytes.len())
}

fn strip_ts_pid(ts_segment: &[u8], pid: u16) -> Vec<u8> {
    let mut filtered = Vec::with_capacity(ts_segment.len());
    let packets = ts_segment.chunks_exact(TS_PACKET_SIZE);
    let remainder = packets.remainder();
    for packet in packets {
        if packet.len() != TS_PACKET_SIZE || packet[0] != 0x47 {
            continue;
        }
        let packet_pid = (((packet[1] & 0x1F) as u16) << 8) | packet[2] as u16;
        if packet_pid != pid {
            filtered.extend_from_slice(packet);
        }
    }
    if !remainder.is_empty() {
        filtered.extend_from_slice(remainder);
    }
    filtered
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

fn bench_hls_fmp4_conversion(c: &mut Criterion) {
    let mut group = c.benchmark_group("hls_fmp4_conversion");
    group.sample_size(30);

    let ts_segment = build_h264_first_audio_only_segment(6);

    group.bench_function("h264_video1_audio0_ts_to_fmp4", |b| {
        b.iter(|| convert_ts_segment_to_fmp4_sizes(&ts_segment));
    });

    group.bench_function("h264_video1_audio0_ts_to_fmp4_store", |b| {
        b.iter(|| {
            let (init_len, media_len) = convert_ts_segment_to_fmp4_sizes(&ts_segment);
            let store = HlsStore::new();
            store.push_fmp4_segment(
                6.0,
                bytes::Bytes::from(vec![0u8; init_len]),
                bytes::Bytes::from(vec![0u8; media_len]),
            );
            store.snapshot().expect("snapshot")
        });
    });

    eprintln!(
        "[hls_fmp4_conversion] 6s H264 + first-audio-only TS segment: {} KiB",
        ts_segment.len() / 1024
    );

    group.finish();
}

fn benches(c: &mut Criterion) {
    bench_hls_mux_cost(c);
    bench_hls_memory_cost(c);
    bench_hls_fmp4_conversion(c);
}

criterion_group!(hls_cost, benches);
criterion_main!(hls_cost);
