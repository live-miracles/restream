//! Benchmark: native HLS preview cost over representative profiles.
//!
//! Measures the current hot path only:
//! - MPEG-TS muxing via in-house `TsMuxer`
//! - segment accumulation into a 6-second buffer
//! - `HlsStore::push_segment` retention in the in-memory live window
//! - playlist + segment snapshot cost for an active window
//!
//! This intentionally avoids stale experimental fMP4 dependencies and stays
//! aligned with the shipping HLS implementation.

use bytes::{Bytes, BytesMut};
use criterion::{
    BatchSize, BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main,
};
use restream::media::engine::{AudioMeta, VideoMeta};
use restream::media::hls::{HlsConfig, HlsStore};
use restream::media::mpegts::TsMuxer;
use restream::media::ring_buffer::MediaType;

const SEGMENT_SECONDS: u32 = 6;
const WINDOW_SEGMENTS: usize = 10;

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
    let gop_frames = profile.fps.min(60) as usize;
    let idr_size = video_bytes_per_sec / 3;
    let p_size = (video_bytes_per_sec - idr_size) / (gop_frames - 1).max(1);
    let audio_frame_size = (profile.audio_bitrate_kbps as usize) * 1000 / 8 / 48;

    let video_key_nal = if profile.codec == "hevc" { 0x26 } else { 0x65 };
    let video_p_nal = if profile.codec == "hevc" { 0x02 } else { 0x41 };

    let idr_payload: Vec<u8> = [0x00, 0x00, 0x00, 0x01, video_key_nal]
        .into_iter()
        .chain(std::iter::repeat_n(0xAA, idr_size.saturating_sub(5)))
        .collect();
    let p_payload: Vec<u8> = [0x00, 0x00, 0x00, 0x01, video_p_nal]
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

        let audio_frames_this_tick = if i < 48 % profile.fps as i64 { 2 } else { 1 };
        for _ in 0..audio_frames_this_tick {
            packets.push((MediaType::Audio, 0u32, pts, false, audio_payload.clone()));
        }
    }

    packets
}

fn video_meta(profile: &StreamProfile) -> VideoMeta {
    VideoMeta {
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
    }
}

fn audio_tracks() -> [AudioMeta; 1] {
    [AudioMeta {
        codec: "aac".to_string(),
        sample_rate: 48_000,
        channels: 2,
        channel_layout: None,
        track_index: 0,
        pid: None,
        language: None,
        title: None,
        profile: None,
    }]
}

fn build_segment_bytes(profile: &StreamProfile, seconds: u32) -> Vec<u8> {
    let packets = build_one_second(profile);
    let video = video_meta(profile);
    let audio = audio_tracks();
    let mut muxer = TsMuxer::new(Some(&video), &audio);
    let mut segment = BytesMut::new();

    for second in 0..seconds as i64 {
        let offset_ms = second * 1000;
        for (media_type, track_index, pts_ms, is_keyframe, payload) in &packets {
            segment.extend_from_slice(muxer.mux_packet(
                *media_type,
                *track_index,
                *pts_ms + offset_ms,
                *pts_ms + offset_ms,
                *is_keyframe,
                payload,
            ));
        }
    }

    segment.freeze().to_vec()
}

fn bench_hls_cost(c: &mut Criterion) {
    let mut group = c.benchmark_group("hls_cost");
    group.sample_size(30);

    for profile in PROFILES {
        let packets = build_one_second(profile);
        let total_input_per_second: usize = packets.iter().map(|packet| packet.4.len()).sum();
        let segment = build_segment_bytes(profile, SEGMENT_SECONDS);
        let retained_window_bytes = segment.len() * WINDOW_SEGMENTS;
        let video = video_meta(profile);
        let audio = audio_tracks();

        group.throughput(Throughput::Bytes(total_input_per_second as u64));
        group.bench_with_input(
            BenchmarkId::new("mux_one_second", profile.name),
            profile,
            |b, profile| {
                b.iter(|| {
                    let mut muxer = TsMuxer::new(Some(&video), &audio);
                    let mut ts_bytes = 0usize;
                    for (media_type, track_index, pts_ms, is_keyframe, payload) in
                        &build_one_second(profile)
                    {
                        ts_bytes += muxer
                            .mux_packet(
                                *media_type,
                                *track_index,
                                *pts_ms,
                                *pts_ms,
                                *is_keyframe,
                                payload,
                            )
                            .len();
                    }
                    black_box(ts_bytes)
                });
            },
        );

        group.throughput(Throughput::Bytes(segment.len() as u64));
        group.bench_with_input(
            BenchmarkId::new("push_segment", profile.name),
            &segment,
            |b, segment| {
                b.iter_batched(
                    || {
                        HlsStore::with_config(HlsConfig {
                            max_segments: WINDOW_SEGMENTS,
                            ..HlsConfig::default()
                        })
                    },
                    |store| {
                        store.push_segment(SEGMENT_SECONDS as f64, Bytes::from(segment.clone()));
                        let stored = store.get_segment(0).expect("stored segment");
                        black_box(stored.len())
                    },
                    BatchSize::SmallInput,
                );
            },
        );

        group.bench_with_input(
            BenchmarkId::new("window_push_and_snapshot", profile.name),
            &segment,
            |b, segment| {
                b.iter_batched(
                    || {
                        let store = HlsStore::with_config(HlsConfig {
                            max_segments: WINDOW_SEGMENTS,
                            ..HlsConfig::default()
                        });
                        for _ in 0..(WINDOW_SEGMENTS - 1) {
                            store
                                .push_segment(SEGMENT_SECONDS as f64, Bytes::from(segment.clone()));
                        }
                        store
                    },
                    |store| {
                        store.push_segment(SEGMENT_SECONDS as f64, Bytes::from(segment.clone()));
                        let snapshot = store.snapshot().expect("snapshot");
                        let total_bytes: usize =
                            snapshot.segments.iter().map(|seg| seg.data.len()).sum();
                        black_box((snapshot.playlist.len(), total_bytes))
                    },
                    BatchSize::SmallInput,
                );
            },
        );

        eprintln!(
            "[hls_cost:{}] 6s segment={} KiB, retained {}-segment window={} MiB",
            profile.name,
            segment.len() / 1024,
            WINDOW_SEGMENTS,
            retained_window_bytes / (1024 * 1024),
        );
    }

    group.finish();
}

criterion_group!(hls_cost, bench_hls_cost);
criterion_main!(hls_cost);
