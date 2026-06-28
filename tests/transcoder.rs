//! Correctness tests for `run_ffmpeg_transcoder_stage`.
//!
//! Generates a small MPEG-TS fixture via ffmpeg CLI and verifies that the
//! transcoder demuxes it and pushes MediaPackets to the output RingBuffer.

use restream::media::avio::MemoryQueue;
use restream::media::engine::VideoMeta;
use restream::media::external_transcoder::build_stage_ffmpeg_args;
use restream::media::mpegts::{TsDemuxer, TsMuxer};
use restream::media::ring_buffer::{MediaType, PayloadFormat, Reader, RingBuffer};
use restream::media::transcoder::{run_ffmpeg_transcode_with_scale, run_ffmpeg_transcoder_stage};
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

static FFMPEG_EXTRACT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn load_fixture() -> Vec<u8> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/test/artifacts/test-h264.ts");
    match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(_) => {
            eprintln!("skipping transcoder fixture test: missing test-h264.ts");
            Vec::new()
        }
    }
}

fn fixture_or_skip() -> Option<Vec<u8>> {
    let fixture = load_fixture();
    if fixture.is_empty() {
        None
    } else {
        Some(fixture)
    }
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
    let args = build_stage_ffmpeg_args(preset, "h264");
    let mut child = Command::new(ffmpeg)
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ffmpeg");

    child
        .stdin
        .as_mut()
        .expect("ffmpeg stdin")
        .write_all(fixture)
        .expect("write fixture to ffmpeg");
    drop(child.stdin.take());

    let output = child.wait_with_output().expect("wait ffmpeg");
    assert!(
        output.status.success(),
        "ffmpeg stage failed for {preset}: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let mut demuxer = TsDemuxer::new();
    demuxer.feed(&output.stdout);
    let mut packets = Vec::new();
    demuxer.drain_into(&mut packets);
    packets
}

fn synthetic_video_only_ts(fixture: &[u8]) -> Vec<u8> {
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

#[test]
fn source_passthrough_produces_output() {
    let Some(fixture) = fixture_or_skip() else {
        return;
    };
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
    let Some(fixture) = fixture_or_skip() else {
        return;
    };
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
    let Some(fixture) = fixture_or_skip() else {
        return;
    };
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
    let Some(fixture) = fixture_or_skip() else {
        return;
    };
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
    let Some(fixture) = fixture_or_skip() else {
        return;
    };
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
    let Some(fixture) = fixture_or_skip() else {
        return;
    };
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
    let Some(fixture) = fixture_or_skip() else {
        return;
    };
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
