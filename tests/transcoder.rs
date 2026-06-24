//! Correctness tests for `run_ffmpeg_transcoder_stage`.
//!
//! Generates a small MPEG-TS fixture via ffmpeg CLI and verifies that the
//! transcoder demuxes it and pushes MediaPackets to the output RingBuffer.

use restream::media::avio::MemoryQueue;
use restream::media::ring_buffer::{MediaType, Reader, RingBuffer};
use restream::media::transcoder::run_ffmpeg_transcoder_stage;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

fn load_fixture() -> Vec<u8> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/test/artifacts/test-h264.ts");
    std::fs::read(path).unwrap_or_else(|e| panic!("fixture missing at {path}: {e}"))
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
fn transcode_with_scale_synthetic_video_only() {
    use restream::media::transcoder::run_ffmpeg_transcode_with_scale;
    use restream::media::mpegts::{TsDemuxer, TsMuxer};
    use restream::media::engine::VideoMeta;
    use restream::media::ring_buffer::PayloadFormat;

    let fixture = load_fixture();

    // 1. Demux video packets from the fixture
    let mut demuxer = TsDemuxer::new();
    demuxer.feed(&fixture);
    let mut all_packets = Vec::new();
    demuxer.drain_into(&mut all_packets);

    let video_packets: Vec<_> = all_packets
        .into_iter()
        .filter(|p| p.media_type == MediaType::Video)
        .collect();

    assert!(!video_packets.is_empty(), "Fixture must contain video packets");

    // 2. Mux video-only packets to generate a synthetic video-only MPEG-TS stream
    let video_meta = VideoMeta {
        codec: "h264".to_string(),
        width: 1920,
        height: 1080,
        fps: 30.0,
        bw: None,
        profile: None,
        level: None,
        pixel_format: None,
    };

    let mut muxer = TsMuxer::new(Some(&video_meta), &[]);
    let mut synthetic_ts = Vec::new();

    for pkt in video_packets {
        // Mux packet using its properties
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

    // 3. Write synthetic stream to MemoryQueue
    let input = Arc::new(MemoryQueue::new());
    let output = Arc::new(RingBuffer::new(4096));
    {
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        rt.block_on(input.write(&synthetic_ts));
    }
    input.close();

    // 4. Run the decode -> scale -> encode loop with "720p" preset
    let result = run_ffmpeg_transcode_with_scale(
        input,
        output.clone(),
        "720p",
        CancellationToken::new(),
    );

    assert!(result.is_ok(), "run_ffmpeg_transcode_with_scale failed: {:?}", result);

    // 5. Assert output packets arrive and contain only video
    let mut reader = Reader::new("test_transcode_scale".to_string(), output);
    let mut output_packets = Vec::new();
    while let Ok(Some(pkt)) = reader.pull() {
        output_packets.push(pkt);
    }

    assert!(!output_packets.is_empty(), "No packets produced by transcode with scale");
    for pkt in &output_packets {
        assert_eq!(pkt.media_type, MediaType::Video, "Expected only video packets in output");
    }

    // The output format should be Raw
    assert_eq!(output_packets[0].format, PayloadFormat::Raw);
}
