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
    input.write(fixture);
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
    input.write(&fixture);
    input.close();

    let token = CancellationToken::new();
    token.cancel();

    let result = run_ffmpeg_transcoder_stage(input, output.clone(), "source", token);
    assert!(result.is_ok(), "cancelled transcoder should exit cleanly");
}
