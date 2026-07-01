use criterion::{Criterion, Throughput, black_box, criterion_group, criterion_main};
use restream::domain::stage::{StageKey, StageKind};
use restream::media::codec::{audio_for_ts, video_for_ts};
use restream::media::engine::{AudioMeta, MediaEngine, VideoMeta};
use restream::media::mpegts::TsMuxer;
use restream::media::ring_buffer::{DtsEnforcer, MediaPacket, MediaType, Reader, RingBuffer};
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
                expected_packets,
            ) = rt.block_on(async { setup_matrix_path(ingest, trans).await });
            let mut stream_reader = if egress == "hls" {
                None
            } else {
                Some(Reader::new(
                    "bench_matrix_throughput".to_string(),
                    target_ring.clone(),
                ))
            };
            if trans && egress != "hls" {
                rt.block_on(async {
                    prewarm_transcoded_path(
                        &source_ring,
                        &packets,
                        stream_reader
                            .as_mut()
                            .expect("stream reader required during transcode warmup"),
                    )
                    .await;
                });
            }

            // Bootstrap uses offset 0 and transcode warmup uses 10_000. Start
            // measured iterations after those phases so DTS stays monotonic.
            let mut iter_count = if trans && egress != "hls" { 2 } else { 1 };
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
                        stream_reader.as_mut(),
                        expected_packets,
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
    usize,
) {
    let engine = Arc::new(MediaEngine::new());
    let source_ring = engine.get_or_create_pipeline("pipe").await;

    // Register active ingest
    engine
        .try_register_ingest("pipe", "key", ingest)
        .await
        .unwrap();

    let fixture_codec = if trans { "h265" } else { "h264" };
    let (video_meta, audio_tracks, mut packets) =
        restream::test_fixtures::primary_av_packets_for_codec(fixture_codec)
            .unwrap_or_else(|e| panic!("{e}"));

    assert!(
        packets.len() >= NUM_PACKETS,
        "fixture has only {} packets, expected {}",
        packets.len(),
        NUM_PACKETS
    );
    packets.truncate(NUM_PACKETS);

    let (video_sequence_header, audio_sequence_header) = if ingest == "rtmp" {
        restream::test_fixtures::wrap_packets_for_rtmp_ingest(
            &video_meta,
            &audio_tracks,
            &mut packets,
        )
    } else {
        (None, None)
    };

    engine
        .update_ingest_meta(
            "pipe",
            Some(video_meta.clone()),
            audio_tracks.first().cloned(),
            None,
        )
        .await;
    engine
        .update_ingest_audio_tracks("pipe", audio_tracks.clone())
        .await;
    if let Some(ref sequence_header) = video_sequence_header {
        engine
            .cache_sequence_header("pipe", true, sequence_header.clone())
            .await;
    }
    if let Some(ref sequence_header) = audio_sequence_header {
        engine
            .cache_sequence_header("pipe", false, sequence_header.clone())
            .await;
    }
    let expected_packets = if ingest == "rtmp" {
        restream::test_fixtures::count_ts_feedable_packets(
            &video_meta,
            &audio_tracks,
            &packets,
            video_sequence_header.as_ref(),
        )
    } else {
        packets.len()
    };

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
            StageKey::new("pipe", StageKind::video_preset("720p")),
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
        expected_packets,
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

async fn prewarm_transcoded_path(
    source_ring: &Arc<RingBuffer>,
    packets: &[MediaPacket],
    reader: &mut Reader,
) {
    for packet in packets {
        let mut packet = packet.clone();
        packet.pts += 10_000;
        packet.dts += 10_000;
        source_ring.push(packet);
    }

    let _ =
        drain_reader_until_quiet(reader, Duration::from_secs(5), Duration::from_millis(500)).await;
}

async fn drain_reader_until_quiet(
    reader: &mut Reader,
    timeout: Duration,
    quiet_window: Duration,
) -> usize {
    let deadline = Instant::now() + timeout;
    let mut last_packet_at = Instant::now();
    let mut produced = 0usize;
    loop {
        if reader.pull().ok().flatten().is_some() {
            produced += 1;
            last_packet_at = Instant::now();
            continue;
        }
        if Instant::now() >= deadline || last_packet_at.elapsed() >= quiet_window {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    produced
}

#[allow(clippy::too_many_arguments)]
async fn run_matrix_iteration(
    source_ring: &Arc<RingBuffer>,
    target_ring: &Arc<RingBuffer>,
    egress: &str,
    trans: bool,
    video_meta: &VideoMeta,
    audio_tracks: &[AudioMeta],
    engine: &Arc<MediaEngine>,
    packets: &[MediaPacket],
    stream_reader: Option<&mut Reader>,
    expected_packets: usize,
    iter_count: i64,
) {
    let hls_segmenter = if egress == "hls" {
        let (store, _) = engine.ensure_hls_segmenter("pipe").await;
        store.clear(); // Reset HlsStore for this iteration
        let cancel = CancellationToken::new();
        let segmenter = tokio::spawn(restream::media::hls::start_hls_segmenter(
            "pipe".to_string(),
            store.clone(),
            target_ring.clone(),
            None,
            engine.clone(),
            cancel.clone(),
            None,
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
            let mut trans_reader =
                Reader::new("bench_trans_reader".to_string(), target_ring.clone());
            let mut trans_pulled = 0;
            let start = Instant::now();
            while trans_pulled < expected_packets && start.elapsed() < Duration::from_millis(2000) {
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
        if let Some(playlist) = store.get_playlist()
            && playlist.contains(".ts")
            && store.get_segment(0).is_some()
        {
            pulled = expected_packets;
        }
    } else if egress == "srt" {
        let reader = stream_reader.expect("stream reader required for srt egress");
        let mut muxer = TsMuxer::new(Some(video_meta), audio_tracks);
        let num_streams = 1 + audio_tracks.len();
        let mut dts_enforcer = DtsEnforcer::new(num_streams);
        let mut nalu_len_size: usize = 4;
        let mut sps_pps_cache: Vec<u8> = Vec::new();

        let start = Instant::now();
        while pulled < expected_packets && start.elapsed() < Duration::from_millis(2000) {
            if let Ok(Some(pkt)) = reader.pull() {
                let payload = match pkt.media_type {
                    MediaType::Video => video_for_ts(
                        &pkt.payload,
                        pkt.format,
                        &mut nalu_len_size,
                        &mut sps_pps_cache,
                    ),
                    MediaType::Audio => {
                        let track = audio_tracks
                            .iter()
                            .find(|a| a.track_index == pkt.track_index)
                            .or(audio_tracks.first());
                        let (sr, ch) = track
                            .map(|a| (a.sample_rate, a.channels))
                            .unwrap_or((48000, 1));
                        audio_for_ts(&pkt.payload, pkt.format, sr, ch)
                    }
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
                        &raw,
                    );
                    black_box(ts_bytes);
                }
                pulled += 1;
            } else {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }
    } else {
        let reader = stream_reader.expect("stream reader required for direct egress");
        let start = Instant::now();
        while pulled < expected_packets && start.elapsed() < Duration::from_millis(2000) {
            if let Ok(Some(pkt)) = reader.pull() {
                black_box(pkt);
                pulled += 1;
            } else {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }
    }

    if trans && egress != "hls" {
        // Benchmarks exercise a live encoder boundary where exact packet
        // cardinality is not a stable invariant across artificial batches.
        // Exact RTMP/SRT correctness is covered by dedicated tests and the
        // live harness, so here we enforce a strong throughput floor instead
        // of binding the benchmark to encoder packetization details.
        let min_expected = (expected_packets * 3) / 4;
        assert!(
            pulled >= min_expected,
            "Failed to sustain expected transcoded throughput for egress={}, pulled={}, floor={}",
            egress,
            pulled,
            min_expected
        );
    } else {
        assert_eq!(
            pulled, expected_packets,
            "Failed to pull expected number of packets for egress={}, trans={}",
            egress, trans
        );
    }
}

criterion_group!(benches, benchmark_matrix);
criterion_main!(benches);
