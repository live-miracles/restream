use bytes::Bytes;
use criterion::{Criterion, criterion_group, criterion_main};
use restream::media::ring_buffer::{MediaPacket, MediaType};

fn benchmark_packet_translation(c: &mut Criterion) {
    let payload = Bytes::from(vec![0u8; 1316]);
    let packet = MediaPacket {
        media_type: MediaType::Video,
        track_index: 0,
        pts: 12345,
        dts: 12345,
        is_keyframe: true,
        payload,
    };

    c.bench_function("packet_remux_flv_to_ts_mapping", |b| {
        b.iter(|| {
            // Simulate remuxing parsing logic and translation overhead
            let _mt = packet.media_type;
            let _pts = packet.pts;
            let _payload_len = packet.payload.len();
        })
    });
}

criterion_group!(benches, benchmark_packet_translation);
criterion_main!(benches);
