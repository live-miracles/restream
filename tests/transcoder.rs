//! Correctness tests for `run_ffmpeg_transcoder_stage`.
//!
//! Uses the checked-in canonical MPEG-TS fixture and verifies that the
//! transcoder demuxes it and pushes MediaPackets to the output RingBuffer.

use proptest::prelude::*;
use restream::domain::stage::{StageKey, StageKind};
use restream::media::avio::MemoryQueue;
use restream::media::engine::VideoMeta;
use restream::media::engine::{AudioMeta, MediaEngine};
use restream::media::external_transcoder::build_stage_ffmpeg_args;
use restream::media::mpegts::{TsDemuxer, TsMuxer};
use restream::media::ring_buffer::{MediaType, PayloadFormat, Reader, RingBuffer};
use restream::media::transcoder::{run_ffmpeg_transcode_with_scale, run_ffmpeg_transcoder_stage};
use restream::media::{h264_transcoder, transcoder};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

static FFMPEG_EXTRACT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
static TEMP_ARTIFACT_COUNTER: AtomicU64 = AtomicU64::new(0);
static FFMPEG_TEST_LOGGING: std::sync::Once = std::sync::Once::new();

fn load_fixture() -> Vec<u8> {
    configure_ffmpeg_test_logging();
    let path =
        restream::test_fixtures::canonical_h264_ts_fixture().unwrap_or_else(|e| panic!("{e}"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("fixture missing at {}: {e}", path.display()))
}

fn load_primary_transport_packets(
    codec: &str,
) -> (
    restream::media::engine::VideoMeta,
    Vec<restream::media::engine::AudioMeta>,
    Vec<restream::media::ring_buffer::MediaPacket>,
) {
    restream::test_fixtures::primary_av_packets_for_codec(codec).unwrap_or_else(|e| panic!("{e}"))
}

fn configure_ffmpeg_test_logging() {
    FFMPEG_TEST_LOGGING.call_once(|| {
        ffmpeg_next::util::log::set_level(ffmpeg_next::util::log::Level::Warning);
    });
}

fn run_stage(
    fixture: &[u8],
    preset: &str,
) -> (
    Vec<std::sync::Arc<restream::media::ring_buffer::MediaPacket>>,
    bool,
) {
    let input = Arc::new(MemoryQueue::new());
    let output = Arc::new(RingBuffer::new(4096));
    {
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        rt.block_on(input.write(fixture));
    }
    input.close();

    let result =
        run_ffmpeg_transcoder_stage(input, output.clone(), preset, CancellationToken::new());

    let mut reader = Reader::new("test_transcoder".to_string(), output);
    let mut packets = Vec::new();
    while let Ok(Some(pkt)) = reader.pull() {
        packets.push(pkt);
    }

    (packets, result.is_ok())
}

fn run_external_stage_args(
    fixture: &[u8],
    preset: &str,
) -> Vec<restream::media::ring_buffer::MediaPacket> {
    let ffmpeg = {
        let _guard = FFMPEG_EXTRACT_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        restream::ffmpeg_extract::ensure_ffmpeg_extracted()
    };
    let temp_dir = temp_artifact_dir();
    let input_path = temp_dir.join("input.ts");
    let output_path = temp_dir.join("output.ts");
    std::fs::write(&input_path, fixture).expect("write input fixture");

    let mut args = build_stage_ffmpeg_args(preset, "h264");
    replace_arg_value(&mut args, "-i", input_path.to_string_lossy().as_ref());
    if let Some(last) = args.last_mut() {
        *last = output_path.to_string_lossy().to_string();
    } else {
        panic!("ffmpeg args missing output path");
    }

    let output = Command::new(ffmpeg)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn ffmpeg");
    assert!(
        output.status.success(),
        "ffmpeg stage failed for {preset}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = std::fs::read(&output_path).expect("read ffmpeg output");
    let _ = std::fs::remove_dir_all(&temp_dir);

    let mut demuxer = TsDemuxer::new();
    demuxer.feed(&stdout);
    let mut packets = Vec::new();
    demuxer.drain_into(&mut packets);
    packets
}

fn replace_arg_value(args: &mut [String], flag: &str, value: &str) {
    let position = args
        .iter()
        .position(|arg| arg == flag)
        .unwrap_or_else(|| panic!("missing ffmpeg arg flag {flag}"));
    let target = args
        .get_mut(position + 1)
        .unwrap_or_else(|| panic!("missing ffmpeg arg value for {flag}"));
    *target = value.to_string();
}

fn temp_artifact_dir() -> std::path::PathBuf {
    let suffix = TEMP_ARTIFACT_COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "restream-transcoder-test-{}-{suffix}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create temp artifact dir");
    dir
}

fn synthetic_video_only_ts(fixture: &[u8]) -> Vec<u8> {
    synthetic_video_only_ts_limited(fixture, usize::MAX)
}

fn synthetic_video_only_ts_limited(fixture: &[u8], max_video_packets: usize) -> Vec<u8> {
    let mut demuxer = TsDemuxer::new();
    demuxer.feed(fixture);
    let mut all_packets = Vec::new();
    demuxer.drain_into(&mut all_packets);

    let video_meta = VideoMeta {
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
    let mut muxer = TsMuxer::new(Some(&video_meta), &[]);
    let mut synthetic_ts = Vec::new();

    let mut video_count = 0usize;
    for pkt in all_packets
        .into_iter()
        .filter(|p| p.media_type == MediaType::Video)
    {
        if video_count >= max_video_packets {
            break;
        }
        video_count += 1;
        let ts_bytes = muxer.mux_packet(
            MediaType::Video,
            0,
            pkt.pts,
            pkt.dts,
            pkt.is_keyframe,
            &pkt.payload,
        );
        synthetic_ts.extend_from_slice(ts_bytes);
    }

    assert!(video_count > 0, "fixture must contain video packets");
    synthetic_ts
}

fn run_internal_scale_stage(
    synthetic_ts: &[u8],
    preset: &str,
) -> Vec<std::sync::Arc<restream::media::ring_buffer::MediaPacket>> {
    let input = Arc::new(MemoryQueue::new());
    let output = Arc::new(RingBuffer::new(4096));
    {
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        rt.block_on(input.write(synthetic_ts));
    }
    input.close();

    let result =
        run_ffmpeg_transcode_with_scale(input, output.clone(), preset, CancellationToken::new());
    assert!(
        result.is_ok(),
        "run_ffmpeg_transcode_with_scale failed for {preset}: {result:?}"
    );

    let mut reader = Reader::new(format!("test_transcode_scale_{preset}"), output);
    let mut output_packets = Vec::new();
    while let Ok(Some(pkt)) = reader.pull() {
        output_packets.push(pkt);
    }
    output_packets
}

fn feed_queue_with_chunk_pattern(
    input: &Arc<MemoryQueue>,
    payload: &[u8],
    chunk_pattern: &[usize],
) {
    assert!(!chunk_pattern.is_empty(), "chunk pattern must not be empty");

    let mut offset = 0usize;
    let mut pattern_index = 0usize;
    while offset < payload.len() {
        let chunk_len = chunk_pattern[pattern_index % chunk_pattern.len()].max(1);
        let end = (offset + chunk_len).min(payload.len());
        input.write_sync(&payload[offset..end]);
        offset = end;
        pattern_index += 1;
    }
}

fn assert_packets_have_monotonic_dts_per_stream(
    packets: &[std::sync::Arc<restream::media::ring_buffer::MediaPacket>],
) {
    let mut last_dts_by_stream = std::collections::HashMap::<(bool, u32), i64>::new();
    for (index, packet) in packets.iter().enumerate() {
        let stream_key = (packet.media_type == MediaType::Video, packet.track_index);
        if let Some(previous_dts) = last_dts_by_stream.get(&stream_key) {
            assert!(
                packet.dts >= *previous_dts,
                "dts regression at output packet {index} for {:?}/track {}: {} -> {}",
                packet.media_type,
                packet.track_index,
                previous_dts,
                packet.dts
            );
        }
        last_dts_by_stream.insert(stream_key, packet.dts);
    }
}

async fn collect_packets_with_deadline(
    reader: &mut Reader,
    min_packets: usize,
    timeout: Duration,
) -> Vec<std::sync::Arc<restream::media::ring_buffer::MediaPacket>> {
    let deadline = Instant::now() + timeout;
    let mut packets = Vec::new();
    while packets.len() < min_packets && Instant::now() < deadline {
        match reader.pull() {
            Ok(Some(packet)) => packets.push(packet),
            _ => tokio::time::sleep(Duration::from_millis(10)).await,
        }
    }
    packets
}

#[test]
fn source_passthrough_produces_output() {
    let fixture = load_fixture();
    let (packets, ok) = run_stage(&fixture, "source");
    assert!(ok, "transcoder stage failed for preset 'source'");
    assert!(
        !packets.is_empty(),
        "no packets produced for preset 'source'"
    );

    let video_count = packets
        .iter()
        .filter(|p| p.media_type == MediaType::Video)
        .count();
    let audio_count = packets
        .iter()
        .filter(|p| p.media_type == MediaType::Audio)
        .count();
    assert!(video_count > 0, "no video packets in output");
    assert!(audio_count > 0, "no audio packets in output");
}

#[test]
fn video_720p_preset_produces_output() {
    let fixture = load_fixture();
    let (packets, ok) = run_stage(&fixture, "video:720p");
    assert!(ok, "transcoder stage failed for preset 'video:720p'");
    assert!(
        !packets.is_empty(),
        "no packets produced for preset 'video:720p'"
    );

    let video_count = packets
        .iter()
        .filter(|p| p.media_type == MediaType::Video)
        .count();
    assert!(video_count > 0, "no video packets in 720p output");
}

#[test]
fn audio_routing_atrack_filters_correctly() {
    let fixture = load_fixture();
    // Single audio track in fixture, selecting track 0 should pass it through
    let (packets, ok) = run_stage(&fixture, "source+atrack:0");
    assert!(ok, "transcoder stage failed for preset 'source+atrack:0'");

    let audio_count = packets
        .iter()
        .filter(|p| p.media_type == MediaType::Audio)
        .count();
    assert!(audio_count > 0, "atrack:0 should include the audio track");

    // Selecting a non-existent track should produce no audio
    let (packets2, ok2) = run_stage(&fixture, "source+atrack:5");
    assert!(ok2, "transcoder stage failed for preset 'source+atrack:5'");

    let audio_count2 = packets2
        .iter()
        .filter(|p| p.media_type == MediaType::Audio)
        .count();
    assert_eq!(
        audio_count2, 0,
        "atrack:5 should exclude all audio (only 1 track in fixture)"
    );
}

#[test]
fn external_audio_remap_filter_produces_stereo_audio() {
    let fixture = load_fixture();
    let packets = run_external_stage_args(&fixture, "audio:remap:1:0:0:from:source");

    assert!(
        packets.iter().any(|p| p.media_type == MediaType::Audio),
        "remap filter should produce audio packets"
    );
    assert!(
        packets.iter().any(|p| p.media_type == MediaType::Video),
        "remap stage should copy video packets"
    );
}

#[test]
fn external_audio_downmix_filter_produces_stereo_audio() {
    let fixture = load_fixture();
    let packets = run_external_stage_args(&fixture, "audio:downmix:0:from:source");

    assert!(
        packets.iter().any(|p| p.media_type == MediaType::Audio),
        "downmix filter should produce audio packets"
    );
    assert!(
        packets.iter().any(|p| p.media_type == MediaType::Video),
        "downmix stage should copy video packets"
    );
}

#[test]
fn cancelled_token_stops_early() {
    let fixture = load_fixture();
    let input = Arc::new(MemoryQueue::new());
    let output = Arc::new(RingBuffer::new(4096));
    {
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        rt.block_on(input.write(&fixture));
    }
    input.close();

    let token = CancellationToken::new();
    token.cancel();

    let result = run_ffmpeg_transcoder_stage(input, output.clone(), "source", token);
    assert!(result.is_ok(), "cancelled transcoder should exit cleanly");
}

#[test]
fn internal_transcode_builtin_video_presets_produce_video() {
    let fixture = load_fixture();
    let synthetic_ts = synthetic_video_only_ts(&fixture);

    for preset in ["h264", "720p", "1080p"] {
        let output_packets = run_internal_scale_stage(&synthetic_ts, preset);

        assert!(
            !output_packets.is_empty(),
            "no packets produced by internal transcode preset {preset}"
        );
        assert!(
            output_packets.iter().any(|p| p.is_keyframe),
            "internal transcode preset {preset} should emit a keyframe"
        );
        for pkt in &output_packets {
            assert_eq!(
                pkt.media_type,
                MediaType::Video,
                "expected only video packets for preset {preset}"
            );
            assert_eq!(
                pkt.format,
                PayloadFormat::Raw,
                "expected raw encoded packets for preset {preset}"
            );
        }
    }
}

#[test]
fn internal_scale_stage_chunked_remux_input_preserves_video_timestamp_order() {
    let fixture = load_fixture();
    let synthetic_ts = synthetic_video_only_ts_limited(&fixture, 180);

    let input = Arc::new(MemoryQueue::new());
    let output = Arc::new(RingBuffer::new(4096));

    // Split writes across irregular boundaries to prove the in-process
    // demux/decode path is insensitive to queue chunking.
    feed_queue_with_chunk_pattern(&input, &synthetic_ts, &[7, 188, 31, 512, 93, 2048]);
    input.close();

    let result =
        run_ffmpeg_transcode_with_scale(input, output.clone(), "720p", CancellationToken::new());
    assert!(
        result.is_ok(),
        "run_ffmpeg_transcode_with_scale failed for chunked remux input: {result:?}"
    );

    let mut reader = Reader::new("internal_scale_chunked_remux".to_string(), output);
    let mut packets = Vec::new();
    while let Ok(Some(pkt)) = reader.pull() {
        packets.push(pkt);
    }

    assert!(
        !packets.is_empty(),
        "internal scale stage should emit packets"
    );
    assert!(
        packets
            .iter()
            .all(|packet| packet.media_type == MediaType::Video),
        "video-only remux input should emit only video packets"
    );
    assert_packets_have_monotonic_dts_per_stream(&packets);
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 6,
        max_shrink_iters: 0,
        .. ProptestConfig::default()
    })]

    #[test]
    fn prop_source_stage_chunked_input_preserves_per_stream_dts_order(
        chunk_pattern in prop::collection::vec(1usize..2048, 1..16)
    ) {
        let fixture = load_fixture();
        let input = Arc::new(MemoryQueue::new());
        let output = Arc::new(RingBuffer::new(4096));

        feed_queue_with_chunk_pattern(&input, &fixture, &chunk_pattern);
        input.close();

        let result = run_ffmpeg_transcoder_stage(
            input,
            output.clone(),
            "source",
            CancellationToken::new(),
        );
        prop_assert!(
            result.is_ok(),
            "source stage failed for chunk pattern {:?}: {:?}",
            chunk_pattern,
            result
        );

        let mut reader = Reader::new("prop_source_chunked_dts".to_string(), output);
        let mut packets = Vec::new();
        while let Ok(Some(pkt)) = reader.pull() {
            packets.push(pkt);
        }

        prop_assert!(!packets.is_empty(), "source stage produced no packets");
        assert_packets_have_monotonic_dts_per_stream(&packets);
        for packet in &packets {
            prop_assert!(packet.pts >= 0, "pts must remain non-negative");
            prop_assert!(packet.dts >= 0, "dts must remain non-negative");
        }
    }
}

#[tokio::test]
async fn replacement_video_stage_preserves_codec_hint_and_audio_tracks() {
    let engine = Arc::new(MediaEngine::new());
    let source = engine
        .get_or_create_pipeline("internal-replacement-meta")
        .await;
    source.set_codec_hint("hevc");
    source.set_audio_tracks(vec![
        AudioMeta {
            codec: "aac".to_string(),
            sample_rate: 48000,
            channels: 2,
            channel_layout: None,
            track_index: 0,
            pid: Some(0x101),
            language: Some("eng".to_string()),
            title: None,
            profile: None,
        },
        AudioMeta {
            codec: "aac".to_string(),
            sample_rate: 48000,
            channels: 2,
            channel_layout: None,
            track_index: 1,
            pid: Some(0x102),
            language: Some("spa".to_string()),
            title: None,
            profile: None,
        },
    ]);

    let stage_kind = StageKind::video_preset("720p");
    let first = engine
        .get_or_create_transcoder(
            "internal-replacement-meta",
            stage_kind.clone(),
            source.clone(),
            Some("hevc"),
        )
        .await;

    assert_eq!(
        first.codec_hint_str(),
        "hevc",
        "initial replacement candidate should inherit hevc codec hint"
    );
    let first_tracks = first
        .audio_tracks()
        .expect("initial stage should expose audio tracks")
        .to_vec();
    assert_eq!(first_tracks.len(), 2);
    assert_eq!(first_tracks[0].pid, Some(0x101));
    assert_eq!(first_tracks[1].pid, Some(0x102));

    // Simulate registry cancellation/replacement.
    engine
        .cleanup_pipeline_stages("internal-replacement-meta")
        .await;

    let replacement = engine
        .get_or_create_transcoder(
            "internal-replacement-meta",
            stage_kind,
            source,
            Some("hevc"),
        )
        .await;

    assert!(
        !Arc::ptr_eq(&first, &replacement),
        "replacement stage must allocate a new ring buffer after cancellation"
    );
    assert_eq!(
        replacement.codec_hint_str(),
        "hevc",
        "replacement stage must preserve codec hint metadata"
    );

    let replacement_tracks = replacement
        .audio_tracks()
        .expect("replacement stage should expose audio tracks")
        .to_vec();
    assert_eq!(replacement_tracks.len(), 2);
    assert_eq!(replacement_tracks[0].track_index, 0);
    assert_eq!(replacement_tracks[1].track_index, 1);
    assert_eq!(replacement_tracks[0].pid, Some(0x101));
    assert_eq!(replacement_tracks[1].pid, Some(0x102));

    // Stop the replacement stage task before test teardown.
    engine
        .cleanup_pipeline_stages("internal-replacement-meta")
        .await;
}

#[test]
fn rtmp_shaped_h264_packets_drive_source_stage() {
    configure_ffmpeg_test_logging();
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(async {
        let engine = Arc::new(MediaEngine::new());
        let source = engine.get_or_create_pipeline("rtmp-h264").await;
        let output = Arc::new(RingBuffer::new(4096));
        let cancel = CancellationToken::new();

        engine
            .try_register_ingest("rtmp-h264", "stream-key", "rtmp")
            .await
            .unwrap();

        let (video, audio_tracks, mut packets) = load_primary_transport_packets("h264");
        let (video_sh, audio_sh) = restream::test_fixtures::wrap_packets_for_rtmp_ingest(
            &video,
            &audio_tracks,
            &mut packets,
        );
        packets.truncate(100);
        let expected_packets = restream::test_fixtures::count_ts_feedable_packets(
            &video,
            &audio_tracks,
            &packets,
            video_sh.as_ref(),
        );

        engine
            .update_ingest_meta(
                "rtmp-h264",
                Some(video.clone()),
                audio_tracks.first().cloned(),
                None,
            )
            .await;
        engine
            .update_ingest_audio_tracks("rtmp-h264", audio_tracks.clone())
            .await;
        if let Some(vsh) = video_sh {
            engine.cache_sequence_header("rtmp-h264", true, vsh).await;
        }
        if let Some(ash) = audio_sh {
            engine.cache_sequence_header("rtmp-h264", false, ash).await;
        }

        for packet in packets.iter().take(10) {
            source.push(packet.clone());
        }

        let handle = tokio::spawn(transcoder::start_transcoder(
            "rtmp-h264".to_string(),
            "source".to_string(),
            source.clone(),
            output.clone(),
            engine.clone(),
            cancel.clone(),
            StageKey::new("rtmp-h264", StageKind::source()),
        ));

        tokio::time::sleep(Duration::from_millis(50)).await;

        for packet in packets.into_iter().skip(10) {
            source.push(packet);
        }

        let mut reader = Reader::new("rtmp_h264_stage".to_string(), output);
        let packets =
            collect_packets_with_deadline(&mut reader, expected_packets, Duration::from_secs(3))
                .await;

        cancel.cancel();
        let _ = handle.await;

        assert!(
            packets
                .iter()
                .any(|packet| packet.media_type == MediaType::Video),
            "expected video packets from RTMP-shaped h264 source stage"
        );
        assert!(
            packets
                .iter()
                .any(|packet| packet.media_type == MediaType::Audio),
            "expected audio packets from RTMP-shaped h264 source stage"
        );
    });
}

#[test]
fn rtmp_shaped_hevc_packets_drive_h264_edge_stage() {
    configure_ffmpeg_test_logging();
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(async {
        let engine = Arc::new(MediaEngine::new());
        let source = engine.get_or_create_pipeline("rtmp-hevc").await;
        let output = Arc::new(RingBuffer::new(4096));
        output.set_codec_hint("h264");
        let cancel = CancellationToken::new();

        engine
            .try_register_ingest("rtmp-hevc", "stream-key", "rtmp")
            .await
            .unwrap();

        let (video, audio_tracks, mut packets) = load_primary_transport_packets("h265");
        let (_video_sh, audio_sh) = restream::test_fixtures::wrap_packets_for_rtmp_ingest(
            &video,
            &audio_tracks,
            &mut packets,
        );
        packets.truncate(100);

        engine
            .update_ingest_meta(
                "rtmp-hevc",
                Some(video.clone()),
                audio_tracks.first().cloned(),
                None,
            )
            .await;
        engine
            .update_ingest_audio_tracks("rtmp-hevc", audio_tracks.clone())
            .await;
        if let Some(ash) = audio_sh {
            engine.cache_sequence_header("rtmp-hevc", false, ash).await;
        }

        for packet in packets.iter().take(10) {
            source.push(packet.clone());
        }

        let handle = tokio::spawn(h264_transcoder::start_h264_transcoder(
            "rtmp-hevc".to_string(),
            source.clone(),
            output.clone(),
            engine.clone(),
            cancel.clone(),
            StageKey::new(
                "rtmp-hevc",
                StageKind::codec_edge("hevc_to_h264", StageKind::source()),
            ),
        ));

        tokio::time::sleep(Duration::from_millis(50)).await;

        for packet in packets.into_iter().skip(10) {
            source.push(packet);
        }

        let mut reader = Reader::new("rtmp_hevc_h264_edge".to_string(), output.clone());
        let packets = collect_packets_with_deadline(&mut reader, 40, Duration::from_secs(5)).await;

        cancel.cancel();
        let _ = handle.await;

        assert_eq!(output.codec_hint_str(), "h264");
        assert!(
            packets
                .iter()
                .any(|packet| packet.media_type == MediaType::Video),
            "expected video packets from RTMP-shaped hevc h264 edge stage"
        );
        assert!(
            packets
                .iter()
                .any(|packet| packet.media_type == MediaType::Audio),
            "expected audio packets from RTMP-shaped hevc h264 edge stage"
        );
        assert!(
            packets
                .iter()
                .filter(|packet| packet.media_type == MediaType::Video)
                .all(|packet| packet.format == PayloadFormat::Raw),
            "expected raw H.264 packets out of the hevc_to_h264 edge stage"
        );
    });
}
