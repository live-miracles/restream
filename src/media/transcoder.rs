//! In-process FFmpeg transcoder — demuxes input MPEG-TS, applies stream filtering,
//! and pushes `MediaPacket`s directly to the output `RingBuffer`. Uses a single
//! `MemoryQueue` for input (source `RingBuffer` → TsMuxer → FFmpeg demux).
//!
//! Audio routing: compound encodings like `720p+atrack:0,1` or `source+remap:0:1`
//! are parsed to select/remap audio streams.

use crate::media::codec::{audio_for_ts, video_for_ts};
use crate::media::ring_buffer::{MediaPacket, MediaType, PayloadFormat, Reader, RingBuffer};
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
    pipeline_id: String,
    preset: String,
    input_buffer: Arc<RingBuffer>,
    output_buffer: Arc<RingBuffer>,
    engine: Arc<crate::media::engine::MediaEngine>,
    cancel_token: CancellationToken,
) {
    // Wait for ingest metadata before starting the transcoder
    let (video_meta, audio_tracks) = loop {
        if cancel_token.is_cancelled() {
            return;
        }
        let result = {
            let ingests = engine.active_ingests.read().await;
            ingests.get(&pipeline_id).and_then(|i| {
                let video = i.video.clone();
                if video.is_none() {
                    return None;
                }
                let mut tracks = i.audio_tracks.lock().unwrap().clone();
                if tracks.is_empty() {
                    if let Some(audio) = i.audio.clone() {
                        tracks.push(audio);
                    }
                }
                Some((video, tracks))
            })
        };
        if let Some(meta) = result {
            break meta;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    };

    let input_queue = Arc::new(crate::media::avio::MemoryQueue::new());

    // Spawn thread to run FFmpeg processing: demux input MPEG-TS, push packets
    // directly to the output RingBuffer (no output mux/demux round-trip).
    let input_queue_clone = input_queue.clone();
    let preset_clone = preset.clone();
    let cancel_token_clone = cancel_token.clone();
    let cancel_on_exit = cancel_token.clone();
    let pipeline_id_clone = pipeline_id.clone();
    let out_buf = output_buffer.clone();
    std::thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_ffmpeg_transcoder_stage(
                input_queue_clone,
                out_buf,
                &preset_clone,
                cancel_token_clone,
            )
        }));
        match result {
            Ok(Err(e)) => eprintln!(
                "[transcoder] FFmpeg transcode thread failed for {} ({}): {:?}",
                pipeline_id_clone, preset_clone, e
            ),
            Err(_) => eprintln!(
                "[transcoder] FFmpeg transcode thread panicked for {} ({})",
                pipeline_id_clone, preset_clone
            ),
            _ => {}
        }
        cancel_on_exit.cancel();
    });

    // Forward source RingBuffer packets to input_queue, muxed as MPEG-TS
    let mut muxer = crate::media::mpegts::TsMuxer::new(video_meta.as_ref(), &audio_tracks);
    let num_streams = (video_meta.is_some() as usize) + audio_tracks.len();
    let mut dts_enforcer = crate::media::ring_buffer::DtsEnforcer::new(num_streams);
    let mut reader = Reader::new(format!("transcoder:{}:{}", pipeline_id, preset), input_buffer);
    let mut nalu_len_size: usize = 4;
    loop {
        tokio::select! {
            _ = cancel_token.cancelled() => break,
            _ = reader.wait_for_data() => {
                let mut packets = Vec::with_capacity(32);
                if reader.pull_burst(&mut packets, 32).is_ok() {
                    for pkt in packets {
                        let payload = match pkt.media_type {
                            MediaType::Video => {
                                match video_for_ts(&pkt.payload, pkt.format, &mut nalu_len_size) {
                                    Some(p) => p,
                                    None => continue,
                                }
                            }
                            MediaType::Audio => {
                                let track = audio_tracks
                                    .iter()
                                    .find(|a| a.track_index == pkt.track_index)
                                    .or(audio_tracks.first());
                                let (sr, ch) = track
                                    .map(|a| (a.sample_rate, a.channels))
                                    .unwrap_or((48000, 1));
                                match audio_for_ts(&pkt.payload, pkt.format, sr, ch) {
                                    Some(p) => p,
                                    None => continue,
                                }
                            }
                        };

                        let stream_idx = match pkt.media_type {
                            MediaType::Video => 0,
                            MediaType::Audio => {
                                let video_offset = video_meta.is_some() as usize;
                                audio_tracks
                                    .iter()
                                    .position(|a| a.track_index == pkt.track_index)
                                    .map(|i| i + video_offset)
                                    .unwrap_or(0)
                            }
                        };

                        let (pts, dts) = dts_enforcer.enforce(stream_idx, pkt.pts, pkt.dts);

                        let ts_bytes = muxer.mux_packet(
                            pkt.media_type,
                            pkt.track_index,
                            pts,
                            dts,
                            pkt.is_keyframe,
                            &payload,
                        );

                        if !ts_bytes.is_empty() {
                            input_queue.write(&ts_bytes);
                        }
                    }
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
    /// See docs/media-pipeline.md "Audio Stage Cache Concern".
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

/// Execute the FFmpeg-backed processing stage used by `start_transcoder`.
///
/// Demuxes input MPEG-TS from `in_queue`, applies stream filtering (audio
/// routing), and pushes `MediaPacket`s directly to the output `RingBuffer`.
/// No output muxer or demux thread needed.
#[doc(hidden)]
pub fn run_ffmpeg_transcoder_stage(
    in_queue: Arc<crate::media::avio::MemoryQueue>,
    out_ring: Arc<RingBuffer>,
    preset: &str,
    token: CancellationToken,
) -> Result<(), &'static str> {
    use crate::media::avio::CustomInput;

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
    let ictx = custom_input
        .input
        .as_mut()
        .ok_or("Failed to get CustomInput context")?;

    let mut audio_stream_index = 0usize;
    let mut audio_out_index = 0u32;
    let mut stream_meta: Vec<Option<(MediaType, u32)>> = Vec::new();

    let _force_h264 = video_preset == "h264";

    for stream in ictx.streams() {
        let medium = stream.parameters().medium();
        if medium == ffmpeg_next::media::Type::Video {
            stream_meta.push(Some((MediaType::Video, 0)));
        } else if medium == ffmpeg_next::media::Type::Audio {
            let include = match &audio_routing {
                AudioRouting::Passthrough => true,
                AudioRouting::SelectTracks(tracks) => tracks.contains(&audio_stream_index),
                AudioRouting::Remap { track, .. } => audio_stream_index == *track,
                AudioRouting::Downmix(track) => audio_stream_index == *track,
            };
            if include {
                stream_meta.push(Some((MediaType::Audio, audio_out_index)));
                audio_out_index += 1;
            } else {
                stream_meta.push(None);
            }
            audio_stream_index += 1;
        } else {
            stream_meta.push(None);
        }
    }

    for (stream, packet) in ictx.packets() {
        if token.is_cancelled() {
            break;
        }

        let idx = stream.index();
        let Some(&Some((media_type, track_index))) = stream_meta.get(idx) else {
            continue;
        };

        let tb = stream.time_base();
        let pts = packet.pts().unwrap_or(0);
        let dts = packet.dts().unwrap_or(pts);
        let pts_ms = if tb.1 != 0 {
            (pts as f64 * tb.0 as f64 / tb.1 as f64 * 1000.0) as i64
        } else {
            pts
        };
        let dts_ms = if tb.1 != 0 {
            (dts as f64 * tb.0 as f64 / tb.1 as f64 * 1000.0) as i64
        } else {
            dts
        };
        let data = packet.data().unwrap_or(&[]);

        out_ring.push(MediaPacket {
            media_type,
            track_index,
            pts: pts_ms,
            dts: dts_ms,
            is_keyframe: packet.is_key(),
            format: PayloadFormat::Raw,
            payload: bytes::Bytes::copy_from_slice(data),
        });
    }

    Ok(())
}
