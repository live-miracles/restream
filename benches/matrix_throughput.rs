use bytes::Bytes;
use criterion::{Criterion, Throughput, black_box, criterion_group, criterion_main};
use restream::media::engine::{AudioMeta, MediaEngine, VideoMeta};
use restream::media::mpegts::TsMuxer;
use restream::media::ring_buffer::{
    DtsEnforcer, MediaPacket, MediaType, PayloadFormat, Reader, RingBuffer,
};
use restream::media::srt::{audio_payload_for_mux, video_payload_for_mux};
use restream::media::transcoder::start_transcoder;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

const NUM_PACKETS: usize = 100;

fn benchmark_matrix(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();

    let mut group = c.benchmark_group("matrix_throughput");
    group.sample_size(10);
    group.throughput(Throughput::Elements(NUM_PACKETS as u64));

    let cases = [
        // (ingest, egress, transcoded)
        ("rtmp", "rtmp", false),
        ("rtmp", "rtmp", true),
        ("rtmp", "srt", false),
        ("rtmp", "srt", true),
        ("rtmp", "hls", false),
        ("rtmp", "hls", true),
        ("srt", "rtmp", false),
        ("srt", "rtmp", true),
        ("srt", "srt", false),
        ("srt", "srt", true),
        ("srt", "hls", false),
        ("srt", "hls", true),
    ];

    for (ingest, egress, trans) in cases {
        let name = format!(
            "{}_to_{}_{}",
            ingest,
            egress,
            if trans { "trans" } else { "direct" }
        );
        group.bench_function(&name, |b| {
            // Setup once per benchmark case
            let (
                engine,
                source_ring,
                target_ring,
                transcoder_cancel,
                transcoder_handle,
                video_meta,
                audio_tracks,
                packets,
            ) = rt.block_on(async { setup_matrix_path(ingest, trans).await });

            // Start iter_count at 1 to avoid overlap with the bootstrap packets (which use offset 0)
            let mut iter_count = 1i64;
            b.iter(|| {
                rt.block_on(async {
                    run_matrix_iteration(
                        &source_ring,
                        &target_ring,
                        egress,
                        trans,
                        &video_meta,
                        &audio_tracks,
                        &engine,
                        &packets,
                        iter_count,
                    )
                    .await;
                });
                iter_count += 1;
            });

            // Teardown once per benchmark case
            rt.block_on(async {
                teardown_matrix_path(transcoder_cancel, transcoder_handle).await;
            });
        });
    }
    group.finish();
}

async fn setup_matrix_path(
    ingest: &str,
    trans: bool,
) -> (
    Arc<MediaEngine>,
    Arc<RingBuffer>,
    Arc<RingBuffer>,
    Option<CancellationToken>,
    Option<tokio::task::JoinHandle<()>>,
    VideoMeta,
    Vec<AudioMeta>,
    Vec<MediaPacket>,
) {
    let engine = Arc::new(MediaEngine::new());
    let source_ring = engine.get_or_create_pipeline("pipe").await;

    // Register active ingest
    engine
        .try_register_ingest("pipe", "key", ingest)
        .await
        .unwrap();

    let fixture_name = if trans {
        "correctness-h265.ts"
    } else {
        "correctness-h264.ts"
    };
    let (video_meta, audio_tracks, mut packets) = load_fixture_packets(fixture_name, ingest);

    assert!(
        packets.len() >= NUM_PACKETS,
        "fixture has only {} packets, expected {}",
        packets.len(),
        NUM_PACKETS
    );
    packets.truncate(NUM_PACKETS);

    engine
        .update_ingest_meta("pipe", Some(video_meta.clone()), None, None)
        .await;
    engine
        .update_ingest_audio_tracks("pipe", audio_tracks.clone())
        .await;

    // Bootstrap: push first 10 packets to source_ring before spawning transcoder
    // so that the transcoder immediately gets stream headers and doesn't fail on open_input.
    for pkt in packets.iter().take(10) {
        source_ring.push(pkt.clone());
    }

    let (target_ring, transcoder_cancel, transcoder_handle) = if trans {
        let trans_ring = Arc::new(RingBuffer::new(4096));
        let cancel = CancellationToken::new();
        let handle = tokio::spawn(start_transcoder(
            "pipe".to_string(),
            "720p".to_string(),
            source_ring.clone(),
            trans_ring.clone(),
            engine.clone(),
            cancel.clone(),
        ));
        tokio::time::sleep(Duration::from_millis(50)).await;
        (trans_ring, Some(cancel), Some(handle))
    } else {
        (source_ring.clone(), None, None)
    };

    (
        engine,
        source_ring,
        target_ring,
        transcoder_cancel,
        transcoder_handle,
        video_meta,
        audio_tracks,
        packets,
    )
}

async fn teardown_matrix_path(
    transcoder_cancel: Option<CancellationToken>,
    transcoder_handle: Option<tokio::task::JoinHandle<()>>,
) {
    if let Some(cancel) = transcoder_cancel {
        cancel.cancel();
    }
    if let Some(handle) = transcoder_handle {
        let _ = handle.await;
    }
}

async fn run_matrix_iteration(
    source_ring: &Arc<RingBuffer>,
    target_ring: &Arc<RingBuffer>,
    egress: &str,
    trans: bool,
    video_meta: &VideoMeta,
    audio_tracks: &[AudioMeta],
    engine: &Arc<MediaEngine>,
    packets: &[MediaPacket],
    iter_count: i64,
) {
    // Create reader BEFORE pushing packets
    let mut reader = Reader::new("bench_matrix_throughput".to_string(), target_ring.clone());

    let hls_segmenter = if egress == "hls" {
        let (store, _) = engine.ensure_hls_segmenter("pipe").await;
        store.clear(); // Reset HlsStore for this iteration
        let cancel = CancellationToken::new();
        let segmenter = tokio::spawn(restream::media::hls::start_hls_segmenter(
            "pipe".to_string(),
            store.clone(),
            target_ring.clone(),
            engine.clone(),
            cancel.clone(),
        ));
        tokio::time::sleep(Duration::from_millis(10)).await;
        Some((store, cancel, segmenter))
    } else {
        None
    };

    // Pre-populate input with packets, offsetting timestamps dynamically to ensure strictly monotonic DTS/PTS across iterations.
    let time_offset_ms = iter_count * 20_000;
    for pkt in packets {
        let mut p = pkt.clone();
        p.pts += time_offset_ms;
        p.dts += time_offset_ms;
        source_ring.push(p);
    }

    let mut pulled = 0;

    if let Some((store, cancel, segmenter)) = hls_segmenter {
        if trans {
            let mut trans_reader = Reader::new("bench_trans_reader".to_string(), target_ring.clone());
            let mut trans_pulled = 0;
            let start = Instant::now();
            while trans_pulled < NUM_PACKETS && start.elapsed() < Duration::from_millis(2000) {
                if let Ok(Some(_)) = trans_reader.pull() {
                    trans_pulled += 1;
                } else {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;

        cancel.cancel();
        let _ = segmenter.await;
        if let Some(playlist) = store.get_playlist() {
            if playlist.contains(".ts") && store.get_segment(0).is_some() {
                pulled = NUM_PACKETS;
            }
        }
    } else if egress == "srt" {
        let mut muxer = TsMuxer::new(Some(video_meta), audio_tracks);
        let num_streams = 1 + audio_tracks.len();
        let mut dts_enforcer = DtsEnforcer::new(num_streams);

        let start = Instant::now();
        while pulled < NUM_PACKETS && start.elapsed() < Duration::from_millis(2000) {
            if let Ok(Some(pkt)) = reader.pull() {
                let is_flv = pkt.format == PayloadFormat::Flv;
                let payload = match pkt.media_type {
                    MediaType::Video => video_payload_for_mux(&pkt.payload, is_flv),
                    MediaType::Audio => audio_payload_for_mux(&pkt.payload, is_flv),
                };
                if let Some(raw) = payload {
                    let stream_idx = match pkt.media_type {
                        MediaType::Video => 0,
                        MediaType::Audio => audio_tracks
                            .iter()
                            .position(|a| a.track_index == pkt.track_index)
                            .map(|i| i + 1)
                            .unwrap_or(0),
                    };
                    let (pts, dts) = dts_enforcer.enforce(stream_idx, pkt.pts, pkt.dts);
                    let ts_bytes = muxer.mux_packet(
                        pkt.media_type,
                        pkt.track_index,
                        pts,
                        dts,
                        pkt.is_keyframe,
                        raw,
                    );
                    black_box(ts_bytes);
                }
                pulled += 1;
            } else {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }
    } else {
        let start = Instant::now();
        while pulled < NUM_PACKETS && start.elapsed() < Duration::from_millis(2000) {
            if let Ok(Some(pkt)) = reader.pull() {
                black_box(pkt);
                pulled += 1;
            } else {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }
    }

    assert_eq!(
        pulled, NUM_PACKETS,
        "Failed to pull expected number of packets for egress={}, trans={}",
        egress, trans
    );
}

fn load_fixture_packets(
    fixture_name: &str,
    ingest: &str,
) -> (VideoMeta, Vec<AudioMeta>, Vec<MediaPacket>) {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let path = std::path::Path::new(manifest_dir)
        .join("test/artifacts/latest")
        .join(fixture_name);
    let file_bytes = std::fs::read(&path)
        .unwrap_or_else(|e| panic!("failed to read fixture at {}: {}", path.display(), e));

    let mut demuxer = restream::media::mpegts::TsDemuxer::new();
    let mut all_packets = Vec::new();

    for chunk in file_bytes.chunks(1316) {
        demuxer.feed(chunk);
        demuxer.drain_into(&mut all_packets);
    }
    demuxer.flush();
    demuxer.drain_into(&mut all_packets);

    let mut probe = demuxer.take_probe().expect("failed to probe TS file");
    let video = probe.video.expect("missing video metadata");

    // Keep only the first audio track
    let mut audio_tracks: Vec<AudioMeta> = probe.audio_tracks.drain(..).take(1).collect();
    let keep_audio_track_index = audio_tracks.first().map(|a| a.track_index).unwrap_or(0);
    if let Some(a) = audio_tracks.first_mut() {
        a.track_index = 0;
    }

    // Filter packets: keep all video packets, and keep audio packets belonging to track 0
    let mut packets = Vec::new();
    for mut pkt in all_packets {
        if pkt.media_type == MediaType::Video {
            packets.push(pkt);
        } else if pkt.media_type == MediaType::Audio && pkt.track_index == keep_audio_track_index {
            // Re-map audio track index to 0
            pkt.track_index = 0;
            packets.push(pkt);
        }
    }

    // Wrap packets with FLV tags if ingest is RTMP
    if ingest == "rtmp" {
        for pkt in &mut packets {
            let is_video = pkt.media_type == MediaType::Video;
            let mut wrapped = Vec::with_capacity(pkt.payload.len() + 5);
            if is_video {
                let is_hevc = video.codec == "hevc" || video.codec == "h265";
                let tag_byte = if is_hevc {
                    if pkt.is_keyframe { 0x1c } else { 0x2c }
                } else {
                    if pkt.is_keyframe { 0x17 } else { 0x27 }
                };
                wrapped.extend_from_slice(&[tag_byte, 1, 0, 0, 0]);
            } else {
                wrapped.extend_from_slice(&[0xaf, 1]);
            }
            wrapped.extend_from_slice(&pkt.payload);
            pkt.payload = Bytes::from(wrapped);
            pkt.format = PayloadFormat::Flv;
        }
    }

    (video, audio_tracks, packets)
}

criterion_group!(benches, benchmark_matrix);
criterion_main!(benches);
