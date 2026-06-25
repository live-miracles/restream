//! Benchmarks for the shared TS packet feeder introduced in Phase 1.
//!
//! Run:
//!   cargo bench --bench stage_feeder --profile bench-dev

use bytes::Bytes;
use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};
use restream::media::engine::{AudioMeta, VideoMeta};
use restream::media::feeder::{PacketFeedConfig, TsPacketFeeder};
use restream::media::ring_buffer::{MediaPacket, MediaType, PayloadFormat};
use std::sync::Arc;

fn video_meta() -> VideoMeta {
    VideoMeta {
        codec: "h264".to_string(),
        width: 1920,
        height: 1080,
        fps: 30.0,
        bw: None,
        profile: None,
        level: None,
        pixel_format: None,
    }
}

fn audio_tracks() -> Arc<Vec<AudioMeta>> {
    Arc::new(vec![AudioMeta {
        codec: "aac".to_string(),
        sample_rate: 48_000,
        channels: 2,
        channel_layout: None,
        track_index: 0,
        profile: None,
    }])
}

fn raw_h264_packet(payload_bytes: usize, pts: i64) -> MediaPacket {
    let mut payload = Vec::with_capacity(payload_bytes);
    payload.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x65]);
    payload.extend(std::iter::repeat_n(0x88, payload_bytes.saturating_sub(5)));
    MediaPacket {
        media_type: MediaType::Video,
        format: PayloadFormat::Raw,
        is_keyframe: pts == 0,
        track_index: 0,
        pts,
        dts: pts,
        payload: Bytes::from(payload),
    }
}

fn raw_aac_packet(payload_bytes: usize, pts: i64) -> MediaPacket {
    MediaPacket {
        media_type: MediaType::Audio,
        format: PayloadFormat::Raw,
        is_keyframe: false,
        track_index: 0,
        pts,
        dts: pts,
        payload: Bytes::from(vec![0x11; payload_bytes]),
    }
}

fn bench_single_packet_feed(c: &mut Criterion) {
    let video = video_meta();
    let audio_tracks = audio_tracks();
    let mut group = c.benchmark_group("stage_feeder/single_packet");

    for (label, packet) in [
        ("video_raw_h264_8k", raw_h264_packet(8 * 1024, 0)),
        ("audio_raw_aac_200b", raw_aac_packet(200, 20)),
    ] {
        group.throughput(Throughput::Bytes(packet.payload.len() as u64));
        group.bench_with_input(BenchmarkId::from_parameter(label), &packet, |b, packet| {
            let mut feeder = TsPacketFeeder::new(
                Some(&video),
                audio_tracks.clone(),
                PacketFeedConfig::default(),
            );
            let mut output = Vec::with_capacity(16 * 188);
            b.iter(|| {
                output.clear();
                black_box(feeder.extend_ts_for_packet(black_box(packet), &mut output));
                black_box(output.len());
            });
        });
    }

    group.finish();
}

fn bench_burst_feed(c: &mut Criterion) {
    let video = video_meta();
    let audio_tracks = audio_tracks();
    let packets: Vec<MediaPacket> = (0..30)
        .flat_map(|i| {
            [
                raw_h264_packet(8 * 1024, i * 33),
                raw_aac_packet(200, i * 33),
            ]
        })
        .collect();
    let bytes_per_burst = packets.iter().map(|p| p.payload.len() as u64).sum();
    let mut group = c.benchmark_group("stage_feeder/burst");

    group.throughput(Throughput::Bytes(bytes_per_burst));
    group.bench_function("30_video_30_audio_packets", |b| {
        b.iter(|| {
            let mut feeder = TsPacketFeeder::new(
                Some(&video),
                audio_tracks.clone(),
                PacketFeedConfig::default(),
            );
            let mut output = Vec::with_capacity(512 * 188);
            for packet in &packets {
                black_box(feeder.extend_ts_for_packet(black_box(packet), &mut output));
            }
            black_box(output.len());
        });
    });

    group.finish();
}

criterion_group!(benches, bench_single_packet_feed, bench_burst_feed);
criterion_main!(benches);
