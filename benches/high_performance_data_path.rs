use bytes::{Bytes, BytesMut};
use criterion::{
    BatchSize, BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main,
};
use memchr::memchr;
use restream::media::avio::MemoryQueue;
use restream::media::engine::MediaEngine;
use restream::media::engine::{AudioMeta, VideoMeta};
use restream::media::mpegts::{TsDemuxer, TsMuxer};
use restream::media::ring_buffer::{
    MediaPacket, MediaType, PayloadFormat, Reader, RingBuffer, RingSlot,
};
use std::mem::{align_of, size_of};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

const PACKET_BYTES: usize = 1316;
const RING_CAPACITY: usize = 4096;

fn packet(sequence: usize, payload: &Bytes) -> MediaPacket {
    MediaPacket {
        media_type: if sequence % 3 == 0 {
            MediaType::Audio
        } else {
            MediaType::Video
        },
        track_index: 0,
        pts: sequence as i64 * 20,
        dts: sequence as i64 * 20,
        is_keyframe: sequence % 60 == 0,
        format: PayloadFormat::Raw,
        payload: payload.clone(),
    }
}

fn print_layout_baseline() {
    eprintln!(
        "data-path layout: MediaPacket={}B align={}B, RingSlot={}B align={}B, \
         {} slots={}KiB",
        size_of::<MediaPacket>(),
        align_of::<MediaPacket>(),
        size_of::<RingSlot>(),
        align_of::<RingSlot>(),
        RING_CAPACITY,
        size_of::<RingSlot>() * RING_CAPACITY / 1024,
    );
}

fn bench_control_plane_lookup(c: &mut Criterion) {
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let engine = Arc::new(MediaEngine::new());
    let cached = runtime.block_on(engine.get_or_create_pipeline("data-path-bench"));
    let mut group = c.benchmark_group("data_path/control_plane_lookup");

    group.bench_function("locked_hashmap_get_or_create", |b| {
        b.iter_custom(|iterations| {
            runtime.block_on(async {
                let started = Instant::now();
                for _ in 0..iterations {
                    black_box(engine.get_or_create_pipeline("data-path-bench").await);
                }
                started.elapsed()
            })
        });
    });

    group.bench_function("cached_hot_handle_clone", |b| {
        b.iter(|| black_box(cached.clone()));
    });

    group.finish();
}

fn bench_ingest_hot_handle(c: &mut Criterion) {
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let engine = Arc::new(MediaEngine::new());
    runtime
        .block_on(engine.try_register_ingest("hot-ingest-bench", "key", "rtmp"))
        .expect("register benchmark ingest");
    let cached_ring = runtime.block_on(engine.get_or_create_pipeline("hot-ingest-bench"));
    let cached_counter = runtime.block_on(async {
        engine.active_ingests.read().await["hot-ingest-bench"]
            .bytes_received
            .clone()
    });
    let mut group = c.benchmark_group("data_path/ingest_hot_handle");

    group.bench_function("registry_ring_and_counter", |b| {
        b.iter_custom(|iterations| {
            runtime.block_on(async {
                let started = Instant::now();
                for _ in 0..iterations {
                    let ring = engine.get_or_create_pipeline("hot-ingest-bench").await;
                    engine.update_ingest_bytes("hot-ingest-bench", 1316).await;
                    black_box(ring);
                }
                started.elapsed()
            })
        });
    });

    group.bench_function("cached_ring_and_counter", |b| {
        b.iter(|| {
            cached_counter.fetch_add(1316, Ordering::Relaxed);
            black_box(&cached_ring);
        });
    });

    group.finish();
}

fn bench_ring_producer(c: &mut Criterion) {
    let payload = Bytes::from(vec![0x47; PACKET_BYTES]);
    let mut group = c.benchmark_group("data_path/ring_producer");

    for burst in [1usize, 4, 8, 16, 32, 64] {
        group.throughput(Throughput::Elements(burst as u64));
        group.bench_with_input(
            BenchmarkId::new("current_push_loop", burst),
            &burst,
            |b, &burst| {
                let ring = RingBuffer::new(RING_CAPACITY);
                let mut sequence = 0usize;
                b.iter(|| {
                    for _ in 0..burst {
                        ring.push(packet(sequence, &payload));
                        sequence = sequence.wrapping_add(1);
                    }
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("push_batch", burst),
            &burst,
            |b, &burst| {
                let ring = RingBuffer::new(RING_CAPACITY);
                let mut sequence = 0usize;
                b.iter(|| {
                    let start = sequence;
                    sequence = sequence.wrapping_add(burst);
                    black_box(ring.push_batch(
                        (start..start + burst).map(|sequence| packet(sequence, &payload)),
                    ));
                });
            },
        );
    }

    group.finish();
}

fn bench_ring_consumer(c: &mut Criterion) {
    let payload = Bytes::from(vec![0x47; PACKET_BYTES]);
    let mut group = c.benchmark_group("data_path/ring_consumer");

    for burst in [1usize, 4, 8, 16, 32, 64] {
        group.throughput(Throughput::Elements(burst as u64));
        group.bench_with_input(
            BenchmarkId::new("current_pull_loop", burst),
            &burst,
            |b, &burst| {
                b.iter_custom(|iterations| {
                    let mut remaining = iterations;
                    let mut elapsed = Duration::ZERO;
                    let mut sequence = 0usize;

                    while remaining > 0 {
                        let chunk = remaining.min(64) as usize;
                        let ring = Arc::new(RingBuffer::new(chunk * burst + 1));
                        let mut reader = Reader::new("bench_data_path_1".to_string(), ring.clone());
                        for _ in 0..chunk * burst {
                            ring.push(packet(sequence, &payload));
                            sequence = sequence.wrapping_add(1);
                        }

                        let started = Instant::now();
                        let mut packets = 0usize;
                        let mut bytes = 0usize;
                        let mut checksum = 0i64;
                        while packets < chunk * burst {
                            match reader.pull() {
                                Ok(Some(packet)) => {
                                    packets += 1;
                                    bytes += packet.payload.len();
                                    checksum = checksum.wrapping_add(packet.pts ^ packet.dts);
                                }
                                Ok(None) => break,
                                Err(error) => panic!("{error}"),
                            }
                        }
                        elapsed += started.elapsed();
                        black_box((packets, bytes, checksum));
                        remaining -= chunk as u64;
                    }

                    elapsed
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("pull_burst", burst),
            &burst,
            |b, &burst| {
                b.iter_custom(|iterations| {
                    let mut remaining = iterations;
                    let mut elapsed = Duration::ZERO;
                    let mut sequence = 0usize;

                    while remaining > 0 {
                        let chunk = remaining.min(64) as usize;
                        let ring = Arc::new(RingBuffer::new(chunk * burst + 1));
                        let mut reader = Reader::new("bench_data_path_2".to_string(), ring.clone());
                        ring.push_batch((0..chunk * burst).map(|_| {
                            let value = packet(sequence, &payload);
                            sequence = sequence.wrapping_add(1);
                            value
                        }));
                        let mut packets = Vec::with_capacity(burst);

                        let started = Instant::now();
                        let mut received = 0usize;
                        let mut bytes = 0usize;
                        let mut checksum = 0i64;
                        for _ in 0..chunk {
                            packets.clear();
                            let loaded = reader
                                .pull_burst(&mut packets, burst)
                                .expect("reader overflow");
                            received += loaded;
                            for packet in &packets {
                                bytes += packet.payload.len();
                                checksum = checksum.wrapping_add(packet.pts ^ packet.dts);
                            }
                        }
                        elapsed += started.elapsed();
                        black_box((received, bytes, checksum));
                        remaining -= chunk as u64;
                    }

                    elapsed
                });
            },
        );
    }

    group.finish();
}

fn bench_fanout_delivery(c: &mut Criterion) {
    let payload = Bytes::from(vec![0x47; PACKET_BYTES]);
    let mut group = c.benchmark_group("data_path/fanout_delivery");
    group.sample_size(20);

    for readers in [1usize, 32, 128, 500] {
        for burst in [1usize, 32] {
            let deliveries = readers * burst;
            group.throughput(Throughput::Elements(deliveries as u64));
            group.bench_with_input(
                BenchmarkId::new(format!("readers_{readers}"), burst),
                &(readers, burst),
                |b, &(readers, burst)| {
                    b.iter_custom(|iterations| {
                        let mut remaining = iterations;
                        let mut elapsed = Duration::ZERO;
                        let mut sequence = 0usize;

                        while remaining > 0 {
                            let chunk = remaining.min(4) as usize;
                            let ring = Arc::new(RingBuffer::new(chunk * burst + 1));
                            let mut consumers = (0..readers)
                                .map(|i| Reader::new(format!("bench_data_path_multi_{}", i), ring.clone()))
                                .collect::<Vec<_>>();
                            for _ in 0..chunk * burst {
                                ring.push(packet(sequence, &payload));
                                sequence = sequence.wrapping_add(1);
                            }

                            let started = Instant::now();
                            let mut delivered = 0usize;
                            let mut checksum = 0i64;
                            for consumer in &mut consumers {
                                for _ in 0..chunk * burst {
                                    let packet = consumer
                                        .pull()
                                        .expect("reader overflow")
                                        .expect("missing packet");
                                    delivered += 1;
                                    checksum = checksum
                                        .wrapping_add(packet.pts)
                                        .wrapping_add(packet.payload.len() as i64);
                                }
                            }
                            elapsed += started.elapsed();
                            black_box((delivered, checksum));
                            remaining -= chunk as u64;
                        }

                        elapsed
                    });
                },
            );
        }
    }

    group.finish();
}

fn bench_memory_queue(c: &mut Criterion) {
    let packet = vec![0x47u8; PACKET_BYTES];
    let mut group = c.benchmark_group("data_path/memory_queue");

    for burst in [1usize, 4, 8, 16, 32, 64] {
        let total_bytes = PACKET_BYTES * burst;
        group.throughput(Throughput::Bytes(total_bytes as u64));
        group.bench_with_input(
            BenchmarkId::new("byte_vecdeque_round_trip", burst),
            &burst,
            |b, &burst| {
                b.iter_batched(
                    MemoryQueue::new,
                    |queue| {
                        for _ in 0..burst {
                            queue.write(&packet);
                        }
                        let mut output = vec![0u8; total_bytes];
                        let mut offset = 0usize;
                        while offset < output.len() {
                            let read = queue.read(&mut output[offset..]);
                            if read == 0 {
                                break;
                            }
                            offset += read;
                        }
                        black_box((queue, output, offset));
                    },
                    BatchSize::SmallInput,
                );
            },
        );

        group.bench_with_input(
            BenchmarkId::new("byte_vecdeque_batch_round_trip", burst),
            &burst,
            |b, &burst| {
                b.iter_batched(
                    MemoryQueue::new,
                    |queue| {
                        black_box(queue.write_batch(std::iter::repeat_n(packet.as_slice(), burst)));
                        let mut output = vec![0u8; total_bytes];
                        let mut offset = 0usize;
                        while offset < output.len() {
                            let read = queue.read(&mut output[offset..]);
                            if read == 0 {
                                break;
                            }
                            offset += read;
                        }
                        black_box((queue, output, offset));
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }

    group.finish();
}

fn bench_segment_finalize(c: &mut Criterion) {
    let mut group = c.benchmark_group("data_path/segment_finalize");
    group.sample_size(20);

    for size in [2usize * 1024 * 1024, 8 * 1024 * 1024] {
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::new("copy_from_slice", size),
            &size,
            |b, &size| {
                b.iter_batched(
                    || vec![0x47u8; size],
                    |data| black_box(Bytes::copy_from_slice(&data)),
                    BatchSize::LargeInput,
                );
            },
        );
        group.bench_with_input(
            BenchmarkId::new("split_and_freeze", size),
            &size,
            |b, &size| {
                b.iter_batched(
                    || {
                        let mut data = BytesMut::with_capacity(size);
                        data.resize(size, 0x47);
                        data
                    },
                    |mut data| black_box(data.split().freeze()),
                    BatchSize::LargeInput,
                );
            },
        );
    }

    group.finish();
}

fn bench_mpegts_demux_drain(c: &mut Criterion) {
    let fixture_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/test/artifacts/latest/correctness-h264.ts"
    );
    let Ok(fixture) = std::fs::read(fixture_path) else {
        eprintln!("skipping MPEG-TS drain benchmark: fixture not found at {fixture_path}");
        return;
    };

    let mut group = c.benchmark_group("data_path/mpegts_demux_drain");
    group.sample_size(10);
    group.throughput(Throughput::Bytes(fixture.len() as u64));

    group.bench_function("take_output_vector", |b| {
        b.iter(|| {
            let mut demuxer = TsDemuxer::new();
            let mut packets = 0usize;
            for chunk in fixture.chunks(1316) {
                demuxer.feed(chunk);
                packets += demuxer.drain().len();
            }
            demuxer.flush();
            packets += demuxer.drain().len();
            black_box(packets);
        });
    });

    group.bench_function("reuse_output_vector", |b| {
        b.iter(|| {
            let mut demuxer = TsDemuxer::new();
            let mut output = Vec::with_capacity(16);
            let mut packets = 0usize;
            for chunk in fixture.chunks(1316) {
                demuxer.feed(chunk);
                packets += demuxer.drain_into(&mut output);
                output.clear();
            }
            demuxer.flush();
            packets += demuxer.drain_into(&mut output);
            black_box(packets);
        });
    });

    group.finish();
}

fn bench_mpegts_resync(c: &mut Criterion) {
    let fixture_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/test/artifacts/latest/correctness-h264.ts"
    );
    let Ok(fixture) = std::fs::read(fixture_path) else {
        eprintln!("skipping MPEG-TS resync benchmark: fixture not found at {fixture_path}");
        return;
    };

    let prefix_len = 64 * 1024;
    let mut input = vec![0u8; prefix_len];
    input.extend_from_slice(&fixture[..fixture.len().min(1316)]);

    let mut group = c.benchmark_group("data_path/mpegts_resync");
    group.throughput(Throughput::Bytes(prefix_len as u64));

    group.bench_function("memchr_sync_scan", |b| {
        b.iter(|| black_box(memchr(0x47, black_box(&input))))
    });

    group.bench_function("scalar_sync_scan", |b| {
        b.iter(|| black_box(black_box(&input).iter().position(|&b| b == 0x47)))
    });

    group.bench_function("corrupt_64k_prefix", |b| {
        b.iter(|| {
            let mut demuxer = TsDemuxer::new();
            demuxer.feed(black_box(&input));
            black_box(demuxer.has_streams());
        });
    });
    group.finish();
}

fn bench_mpegts_mux(c: &mut Criterion) {
    let fixture_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/test/artifacts/latest/correctness-h264.ts"
    );
    let Ok(fixture) = std::fs::read(fixture_path) else {
        eprintln!("skipping MPEG-TS mux benchmark: fixture not found at {fixture_path}");
        return;
    };

    let mut demuxer = TsDemuxer::new();
    demuxer.feed(&fixture);
    demuxer.flush();
    let packets = demuxer.drain();
    let probe = demuxer.take_probe();
    if packets.is_empty() {
        eprintln!("skipping MPEG-TS mux benchmark: no packets decoded from fixture");
        return;
    }

    let video = probe.as_ref().and_then(|p| {
        p.video.as_ref().map(|v| VideoMeta {
            codec: v.codec.clone(),
            width: v.width,
            height: v.height,
            fps: v.fps,
            bw: None,
            profile: None,
            level: None,
            pixel_format: None,
        })
    });
    let audio_tracks: Vec<AudioMeta> = probe
        .as_ref()
        .map(|p| {
            p.audio_tracks
                .iter()
                .map(|a| AudioMeta {
                    codec: a.codec.clone(),
                    sample_rate: a.sample_rate,
                    channels: a.channels,
                    channel_layout: None,
                    track_index: a.track_index,
                })
                .collect()
        })
        .unwrap_or_default();

    let total_payload: usize = packets.iter().map(|p| p.payload.len()).sum();

    let mut group = c.benchmark_group("data_path/mpegts_mux");
    group.sample_size(10);
    group.throughput(Throughput::Bytes(total_payload as u64));

    group.bench_function("mux_all_packets", |b| {
        b.iter(|| {
            let mut muxer = TsMuxer::new(video.as_ref(), &audio_tracks);
            let mut total_bytes = 0usize;
            for pkt in &packets {
                let ts = muxer.mux_packet(
                    pkt.media_type,
                    pkt.track_index,
                    pkt.pts,
                    pkt.dts,
                    pkt.is_keyframe,
                    &pkt.payload,
                );
                total_bytes += ts.len();
            }
            black_box(total_bytes)
        });
    });

    group.finish();
}

fn benches(c: &mut Criterion) {
    print_layout_baseline();
    bench_control_plane_lookup(c);
    bench_ingest_hot_handle(c);
    bench_ring_producer(c);
    bench_ring_consumer(c);
    bench_fanout_delivery(c);
    bench_memory_queue(c);
    bench_segment_finalize(c);
    bench_mpegts_demux_drain(c);
    bench_mpegts_mux(c);
    bench_mpegts_resync(c);
}

criterion_group!(data_path_benches, benches);
criterion_main!(data_path_benches);
