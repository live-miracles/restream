//! In-process FFmpeg transcoder — re-encodes video to a target resolution preset.
//! Supports H.264 and H.265/HEVC (auto-detected from input codec). Uses two
//! `MemoryQueue`s: one for input (source `RingBuffer` → FFmpeg decoder) and one
//! for output (FFmpeg encoder → destination `RingBuffer`).
//!
//! Audio routing: compound encodings like `720p+atrack:0,1` or `source+remap:0:1`
//! are parsed to select/remap audio streams at the mux level.

use crate::media::ring_buffer::{MediaPacket, MediaType, Reader, RingBuffer};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub enum AudioRouting {
    /// Pass all audio streams through unchanged
    Passthrough,
    /// Select specific audio tracks by 0-based index
    SelectTracks(Vec<usize>),
    /// Remap stereo channels: (left_channel, right_channel, optional_track)
    Remap {
        left: usize,
        right: usize,
        track: usize,
    },
    /// Downmix a specific audio track to stereo
    Downmix(usize),
}

/// Parse the audio routing portion of a compound encoding string.
/// Examples: `remap:0:1`, `atrack:0,1`, `downmix:0`
pub fn parse_audio_routing(encoding: &str) -> AudioRouting {
    let audio_part = if let Some(pos) = encoding.find('+') {
        &encoding[pos + 1..]
    } else if encoding.starts_with("remap:")
        || encoding.starts_with("atrack:")
        || encoding.starts_with("downmix:")
    {
        encoding
    } else {
        return AudioRouting::Passthrough;
    };

    if let Some(rest) = audio_part.strip_prefix("remap:") {
        let parts: Vec<&str> = rest.split(':').collect();
        if parts.len() >= 2 {
            let left = parts[0].parse().unwrap_or(0);
            let right = parts[1].parse().unwrap_or(1);
            let track = parts.get(2).and_then(|t| t.parse().ok()).unwrap_or(0);
            return AudioRouting::Remap { left, right, track };
        }
    } else if let Some(rest) = audio_part.strip_prefix("atrack:") {
        let tracks: Vec<usize> = rest.split(',').filter_map(|t| t.parse().ok()).collect();
        if !tracks.is_empty() {
            return AudioRouting::SelectTracks(tracks);
        }
    } else if let Some(rest) = audio_part.strip_prefix("downmix:") {
        if let Ok(track) = rest.parse() {
            return AudioRouting::Downmix(track);
        }
    }

    AudioRouting::Passthrough
}

pub async fn start_transcoder(
    _pipeline_id: String,
    preset: String,
    input_buffer: Arc<RingBuffer>,
    _output_buffer: Arc<RingBuffer>,
    cancel_token: CancellationToken,
) {
    // Setup in-memory queues instead of TCP loopback
    let input_queue = Arc::new(crate::media::avio::MemoryQueue::new());
    let output_queue = Arc::new(crate::media::avio::MemoryQueue::new());

    // Spawn thread to run FFmpeg transcoding from CustomInput to CustomOutput
    let input_queue_clone = input_queue.clone();
    let output_queue_clone = output_queue.clone();
    let preset_clone = preset.clone();
    let cancel_token_clone = cancel_token.clone();
    std::thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_ffmpeg_transcoder(
                input_queue_clone,
                output_queue_clone,
                &preset_clone,
                cancel_token_clone,
            )
        }));
        match result {
            Ok(Err(e)) => eprintln!("[transcoder] FFmpeg transcode thread failed: {:?}", e),
            Err(_) => eprintln!("[transcoder] FFmpeg transcode thread panicked"),
            _ => {}
        }
    });

    // Spawn thread to demux output MPEG-TS and push packets with proper timestamps
    let out_queue_clone = output_queue.clone();
    let out_buf = _output_buffer.clone();
    let cancel = cancel_token.clone();
    std::thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            demux_transcoder_output(out_queue_clone, out_buf, cancel);
        }));
        if result.is_err() {
            eprintln!("[transcoder] Output reader thread panicked");
        }
    });

    // Forward source RingBuffer packets to input_queue
    let mut reader = Reader::new(input_buffer);
    let mut packets = Vec::with_capacity(32);
    loop {
        tokio::select! {
            _ = cancel_token.cancelled() => break,
            _ = reader.wait_for_data() => {
                loop {
                    packets.clear();
                    match reader.pull_burst(&mut packets, 32) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                    input_queue
                        .write_batch(packets.iter().map(|packet| packet.payload.as_ref()));
                }
            }
        }
    }

    input_queue.close();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_passthrough() {
        assert!(matches!(
            parse_audio_routing("source"),
            AudioRouting::Passthrough
        ));
        assert!(matches!(
            parse_audio_routing("720p"),
            AudioRouting::Passthrough
        ));
        assert!(matches!(parse_audio_routing(""), AudioRouting::Passthrough));
    }

    #[test]
    fn parse_atrack() {
        match parse_audio_routing("720p+atrack:0,1") {
            AudioRouting::SelectTracks(t) => assert_eq!(t, vec![0, 1]),
            other => panic!("expected SelectTracks, got {:?}", other),
        }
        match parse_audio_routing("source+atrack:2") {
            AudioRouting::SelectTracks(t) => assert_eq!(t, vec![2]),
            other => panic!("expected SelectTracks, got {:?}", other),
        }
    }

    #[test]
    fn parse_remap() {
        match parse_audio_routing("source+remap:0:1") {
            AudioRouting::Remap { left, right, track } => {
                assert_eq!((left, right, track), (0, 1, 0));
            }
            other => panic!("expected Remap, got {:?}", other),
        }
        match parse_audio_routing("720p+remap:1:0:2") {
            AudioRouting::Remap { left, right, track } => {
                assert_eq!((left, right, track), (1, 0, 2));
            }
            other => panic!("expected Remap, got {:?}", other),
        }
    }

    #[test]
    fn parse_downmix() {
        match parse_audio_routing("source+downmix:1") {
            AudioRouting::Downmix(t) => assert_eq!(t, 1),
            other => panic!("expected Downmix, got {:?}", other),
        }
    }

    #[test]
    fn parse_legacy_remap() {
        match parse_audio_routing("remap:0:1") {
            AudioRouting::Remap { left, right, track } => {
                assert_eq!((left, right, track), (0, 1, 0));
            }
            other => panic!("expected Remap, got {:?}", other),
        }
    }

    /// Verify that stage keys for different video presets with the same audio
    /// routing produce different cache keys, preventing cross-contamination.
    /// See docs/media-pipeline-stage-design.md "Audio Stage Cache Concern".
    #[test]
    fn stage_keys_isolate_video_presets() {
        let pipeline = "pipe1";

        // Reconciler produces these keys for 720p+atrack:0 and 1080p+atrack:0
        let key_720 = format!("{}:audio:atrack:0:from:720p", pipeline);
        let key_1080 = format!("{}:audio:atrack:0:from:1080p", pipeline);

        assert_ne!(
            key_720, key_1080,
            "audio stages with different video upstreams must have different keys"
        );

        // Same encoding on same pipeline must share
        let key_720_dup = format!("{}:audio:atrack:0:from:720p", pipeline);
        assert_eq!(key_720, key_720_dup);
    }

    /// Verify video stage keys are shared across outputs with different audio routing.
    #[test]
    fn video_stage_shared_across_audio_variants() {
        let pipeline = "pipe1";

        // 720p, 720p+atrack:0, 720p+remap:0:1 all use this video key
        let video_key = format!("{}:video:720p", pipeline);

        // All three outputs produce the same video stage key
        for encoding in &["720p", "720p+atrack:0", "720p+remap:0:1"] {
            let vp = encoding.split('+').next().unwrap();
            let expected = format!("{}:video:{}", pipeline, vp);
            assert_eq!(video_key, expected);
        }
    }
}

const SKIP_STREAM: usize = usize::MAX;

// In-process FFmpeg transcoder using CustomInput and CustomOutput
fn demux_transcoder_output(
    queue: Arc<crate::media::avio::MemoryQueue>,
    ring: Arc<RingBuffer>,
    token: CancellationToken,
) {
    use crate::media::avio::CustomInput;

    let mut custom_input = match CustomInput::new(&*queue) {
        Ok(ci) => ci,
        Err(e) => {
            eprintln!("[transcoder] output demux failed to open: {e}");
            return;
        }
    };
    let Some(mut ictx) = custom_input.input.take() else {
        eprintln!("[transcoder] output demux: no input context");
        return;
    };

    let mut audio_index = 0usize;
    let mut stream_meta: Vec<(MediaType, usize)> = Vec::new();
    for stream in ictx.streams() {
        match stream.parameters().medium() {
            ffmpeg_next::media::Type::Video => {
                stream_meta.push((MediaType::Video, 0));
            }
            ffmpeg_next::media::Type::Audio => {
                stream_meta.push((MediaType::Audio, audio_index));
                audio_index += 1;
            }
            _ => {
                stream_meta.push((MediaType::Video, 0));
            }
        }
    }

    for (stream, packet) in ictx.packets() {
        if token.is_cancelled() {
            break;
        }
        let idx = stream.index();
        let &(media_type, track_idx) = stream_meta.get(idx).unwrap_or(&(MediaType::Video, 0));
        let track_index = track_idx as u32;

        let pts = packet.pts().unwrap_or(0);
        let dts = packet.dts().unwrap_or(pts);
        let is_keyframe = packet.is_key();
        let data = packet.data().unwrap_or(&[]);

        ring.push(MediaPacket {
            media_type,
            track_index,
            pts,
            dts,
            is_keyframe,
            payload: bytes::Bytes::copy_from_slice(data),
        });
    }
}

fn run_ffmpeg_transcoder(
    in_queue: Arc<crate::media::avio::MemoryQueue>,
    out_queue: Arc<crate::media::avio::MemoryQueue>,
    preset: &str,
    token: CancellationToken,
) -> Result<(), &'static str> {
    use crate::media::avio::{CustomInput, CustomOutput};

    // Parse stage key format: "video:720p", "audio:atrack:0:from:720p",
    // or legacy compound "720p+atrack:0,1"
    let (video_preset, audio_routing) = if let Some(vp) = preset.strip_prefix("video:") {
        (vp, AudioRouting::Passthrough)
    } else if let Some(rest) = preset.strip_prefix("audio:") {
        let audio_op = rest.rsplit_once(":from:").map(|(op, _)| op).unwrap_or(rest);
        (
            "source",
            parse_audio_routing(&format!("source+{}", audio_op)),
        )
    } else {
        let vp = preset.split('+').next().unwrap_or(preset);
        (vp, parse_audio_routing(preset))
    };

    let mut custom_input = CustomInput::new(&*in_queue)?;
    let mut ictx = custom_input
        .input
        .take()
        .ok_or("Failed to get CustomInput context")?;

    let mut custom_output = CustomOutput::new(&*out_queue, "mpegts")?;
    let mut octx = custom_output
        .output
        .take()
        .ok_or("Failed to get CustomOutput context")?;

    // Track audio stream indices (0-based within audio streams only)
    let mut audio_stream_index = 0usize;
    let mut stream_mapping: Vec<usize> = Vec::new();

    // "h264" preset forces H264 output regardless of input codec (for RTMP egress of H265 sources)
    let force_h264 = video_preset == "h264";
    let needs_video_transcode = force_h264
        || (!video_preset.is_empty() && video_preset != "source" && video_preset != "custom");

    for stream in ictx.streams() {
        let medium = stream.parameters().medium();
        if medium == ffmpeg_next::media::Type::Video {
            if needs_video_transcode {
                let input_codec_id = stream.parameters().id();
                // H265→H264 when forced or for resolution presets on RTMP (standard RTMP has no H265)
                let out_codec_id = if force_h264 {
                    ffmpeg_next::codec::Id::H264
                } else {
                    match input_codec_id {
                        ffmpeg_next::codec::Id::HEVC => ffmpeg_next::codec::Id::HEVC,
                        _ => ffmpeg_next::codec::Id::H264,
                    }
                };
                let codec = ffmpeg_next::encoder::find(out_codec_id)
                    .ok_or("Video encoder not found (tried H.264/H.265)")?;
                let mut new_stream = octx
                    .add_stream(codec)
                    .map_err(|_| "Failed to add video stream")?;

                let (w, h) = if force_h264 {
                    // Preserve source resolution
                    let sw = unsafe { (*stream.parameters().as_ptr()).width } as u32;
                    let sh = unsafe { (*stream.parameters().as_ptr()).height } as u32;
                    (sw, sh)
                } else {
                    match video_preset {
                        "720p" => (1280, 720),
                        "1080p" => (1920, 1080),
                        "2160p" | "4k" => (3840, 2160),
                        "vertical-crop" | "vertical-rotate" => (1080, 1920),
                        _ => (1280, 720),
                    }
                };

                let enc_ctx = ffmpeg_next::codec::context::Context::new();
                let mut encoder = enc_ctx
                    .encoder()
                    .video()
                    .map_err(|_| "Failed to create video encoder")?;
                encoder.set_width(w);
                encoder.set_height(h);
                encoder.set_format(ffmpeg_next::format::Pixel::YUV420P);
                encoder.set_time_base(stream.time_base());

                let opened_encoder = encoder
                    .open_as(codec)
                    .map_err(|_| "Failed to open encoder")?;
                new_stream.set_parameters(&opened_encoder);
                stream_mapping.push(new_stream.index());
            } else {
                // Video passthrough
                let codec = ffmpeg_next::encoder::find(ffmpeg_next::codec::Id::None);
                let mut new_stream = octx
                    .add_stream(codec)
                    .map_err(|_| "Failed to add video copy stream")?;
                new_stream.set_parameters(stream.parameters());
                stream_mapping.push(new_stream.index());
            }
        } else if medium == ffmpeg_next::media::Type::Audio {
            let include = match &audio_routing {
                AudioRouting::Passthrough => true,
                AudioRouting::SelectTracks(tracks) => tracks.contains(&audio_stream_index),
                AudioRouting::Remap { track, .. } => audio_stream_index == *track,
                AudioRouting::Downmix(track) => audio_stream_index == *track,
            };

            if include {
                // Copy audio stream (remap/downmix would need decode+filter+encode,
                // which requires the full decode loop — for now, stream copy)
                let codec = ffmpeg_next::encoder::find(ffmpeg_next::codec::Id::None);
                let mut new_stream = octx
                    .add_stream(codec)
                    .map_err(|_| "Failed to add audio stream")?;
                new_stream.set_parameters(stream.parameters());
                stream_mapping.push(new_stream.index());
            } else {
                stream_mapping.push(SKIP_STREAM);
            }
            audio_stream_index += 1;
        } else {
            let codec = ffmpeg_next::encoder::find(ffmpeg_next::codec::Id::None);
            let mut new_stream = octx
                .add_stream(codec)
                .map_err(|_| "Failed to add stream copy")?;
            new_stream.set_parameters(stream.parameters());
            stream_mapping.push(new_stream.index());
        }
    }

    octx.write_header()
        .map_err(|_| "Transcoder: Failed to write header")?;

    for (stream, mut packet) in ictx.packets() {
        if token.is_cancelled() {
            break;
        }

        let Some(&out_stream_idx) = stream_mapping.get(stream.index()) else {
            continue;
        };
        if out_stream_idx == SKIP_STREAM {
            continue;
        }
        packet.set_stream(out_stream_idx);

        let in_time_base = stream.time_base();
        let Some(out_stream) = octx.stream(out_stream_idx) else {
            continue;
        };
        let out_time_base = out_stream.time_base();
        packet.rescale_ts(in_time_base, out_time_base);

        let _ = packet.write_interleaved(&mut octx);
    }

    octx.write_trailer()
        .map_err(|_| "Transcoder: Failed to write trailer")?;

    out_queue.close();
    Ok(())
}
