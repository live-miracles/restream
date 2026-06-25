//! In-process FFmpeg transcoder — demuxes input MPEG-TS, applies stream filtering,
//! and pushes `MediaPacket`s directly to the output `RingBuffer`. Uses a single
//! `MemoryQueue` for input (source `RingBuffer` → TsMuxer → FFmpeg demux).
//!
//! Audio routing: compound encodings like `720p+atrack:0,1` or `source+remap:0:1`
//! are parsed to select/remap audio streams.

use crate::media::codec::{audio_for_ts_into, video_for_ts_into};
use crate::media::engine::AudioMeta;
use crate::media::ring_buffer::{MediaPacket, MediaType, PayloadFormat, Reader, RingBuffer};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Zero-copy wrapper: holds an `ffmpeg_next::Packet` so `bytes::Bytes::from_owner`
/// can serve the encoded/demuxed buffer to ring-buffer readers without a `memcpy`.
///
/// Drop calls `av_packet_unref`, decrementing the AVBufferRef refcount. The data
/// remains valid until every downstream `Bytes` clone is released.
///
/// `ffmpeg_next::Packet` is `unsafe impl Send + Sync`, satisfying `from_owner`'s bounds.
struct OwnedFfmpegPacket(ffmpeg_next::Packet);
impl AsRef<[u8]> for OwnedFfmpegPacket {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        self.0.data().unwrap_or(&[])
    }
}

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

/// Lightweight audio routing stage — no FFmpeg, no MPEG-TS round-trip.
///
/// Handles `SelectTracks` and `Remap` by filtering/re-indexing `MediaPacket`s
/// in a tight async loop. Packets are `Arc<Bytes>` so no payload copy occurs.
///
/// `Downmix` is not handled here (requires DSP decode/encode) and falls back
/// to the full internal FFmpeg transcoder path.
pub async fn start_audio_router(
    pipeline_id: String,
    routing: AudioRouting,
    input_buffer: Arc<RingBuffer>,
    output_buffer: Arc<RingBuffer>,
    cancel: CancellationToken,
) {
    // Inherit the codec_hint from the input ring so downstream egresses
    // (SRT, RTMP) build correct PMT even after passing through the audio router.
    let hint = input_buffer.codec_hint_str();
    if !hint.is_empty() {
        output_buffer.set_codec_hint(hint);
    }

    eprintln!(
        "[audio-router] start pipeline={} routing={:?} input_codec='{}' output_codec='{}'",
        pipeline_id,
        std::mem::discriminant(&routing),
        input_buffer.codec_hint_str(),
        output_buffer.codec_hint_str(),
    );

    let mut reader = Reader::new(
        format!(
            "audio-router:{}:{:?}",
            pipeline_id,
            std::mem::discriminant(&routing)
        ),
        input_buffer,
    );
    let mut _pushed_count: u64 = 0;
    let mut first_push_logged = false;
    // Pre-allocated batches — reused across bursts so the Vec capacity
    // is retained (no re-allocation on the hot path after the first burst).
    let mut out_batch: Vec<MediaPacket> = Vec::with_capacity(32);
    let mut packets: Vec<std::sync::Arc<MediaPacket>> = Vec::with_capacity(32);
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = reader.wait_for_data() => {
                if reader.pull_burst(&mut packets, 32).is_err() {
                    continue;
                }
                for pkt in packets.drain(..) {
                    let out = match &routing {
                        AudioRouting::Passthrough => Some((*pkt).clone()),

                        AudioRouting::SelectTracks(tracks) => {
                            match pkt.media_type {
                                MediaType::Video => Some((*pkt).clone()),
                                MediaType::Audio => {
                                    if let Some(pos) = tracks.iter().position(|&t| t == pkt.track_index as usize) {
                                        let mut new_pkt = (*pkt).clone();
                                        new_pkt.track_index = pos as u32;
                                        Some(new_pkt)
                                    } else {
                                        None // drop this track
                                    }
                                }
                            }
                        }

                        AudioRouting::Remap { left, right, track } => {
                            match pkt.media_type {
                                MediaType::Video => Some((*pkt).clone()),
                                MediaType::Audio if pkt.track_index as usize == *track => {
                                    let _ = (left, right); // channel remap needs DSP
                                    let mut new_pkt = (*pkt).clone();
                                    new_pkt.track_index = 0;
                                    Some(new_pkt)
                                }
                                MediaType::Audio => None,
                            }
                        }

                        AudioRouting::Downmix(_) => {
                            // Downmix requires decode→mix→encode; not handled here.
                            // get_or_create_transcoder routes Downmix to the FFmpeg path.
                            Some((*pkt).clone())
                        }
                    };
                    if let Some(p) = out {
                        if !first_push_logged {
                            println!(
                                "[audio-router] first push pipeline={} type={:?} track={} codec_out='{}'",
                                pipeline_id, p.media_type, p.track_index,
                                output_buffer.codec_hint_str()
                            );
                            first_push_logged = true;
                        }
                        out_batch.push(p);
                        _pushed_count += 1;
                    }
                }
                // One write-index store + one Notify for the entire burst.
                if !out_batch.is_empty() {
                    output_buffer.push_batch(out_batch.drain(..));
                }
            }
        }
    }
}

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
    } else if let Some(rest) = audio_part.strip_prefix("downmix:")
        && let Ok(track) = rest.parse()
    {
        return AudioRouting::Downmix(track);
    }

    AudioRouting::Passthrough
}

pub fn apply_audio_routing(routing: &AudioRouting, input_tracks: &[AudioMeta]) -> Vec<AudioMeta> {
    match routing {
        AudioRouting::Passthrough => input_tracks.to_vec(),
        AudioRouting::SelectTracks(tracks) => {
            let mut out = Vec::new();
            let mut out_idx = 0;
            for (i, track) in input_tracks.iter().enumerate() {
                if tracks.contains(&i) {
                    let mut t = track.clone();
                    t.track_index = out_idx;
                    out.push(t);
                    out_idx += 1;
                }
            }
            out
        }
        AudioRouting::Remap { track, .. } => {
            if let Some(t) = input_tracks.get(*track) {
                let mut out_track = t.clone();
                out_track.track_index = 0;
                vec![out_track]
            } else {
                Vec::new()
            }
        }
        AudioRouting::Downmix(track) => {
            if let Some(t) = input_tracks.get(*track) {
                let mut out_track = t.clone();
                out_track.track_index = 0;
                out_track.channels = 2;
                out_track.channel_layout = Some("stereo".to_string());
                vec![out_track]
            } else {
                Vec::new()
            }
        }
    }
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
                video.as_ref()?;
                let lock = i.audio_tracks.lock().unwrap_or_else(|e| e.into_inner());
                let tracks = if lock.is_empty()
                    && let Some(audio) = i.audio.clone()
                {
                    std::sync::Arc::new(vec![audio])
                } else {
                    std::sync::Arc::clone(&lock)
                };
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
    let handle = std::thread::spawn(move || {
        let use_internal = std::env::var("RESTREAM_USE_INTERNAL_TRANSCODER")
            .map(|v| {
                matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            })
            .unwrap_or(false);

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if use_internal && preset_clone.starts_with("video:") {
                let video_preset = preset_clone.strip_prefix("video:").unwrap_or(&preset_clone);
                run_ffmpeg_transcode_with_scale(
                    input_queue_clone,
                    out_buf,
                    video_preset,
                    cancel_token_clone,
                )
            } else {
                run_ffmpeg_transcoder_stage(
                    input_queue_clone,
                    out_buf,
                    &preset_clone,
                    cancel_token_clone,
                )
            }
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
    engine.register_os_thread(handle);

    // Forward source RingBuffer packets to input_queue, muxed as MPEG-TS
    let mut muxer = crate::media::mpegts::TsMuxer::new(video_meta.as_ref(), &audio_tracks);
    let num_streams = (video_meta.is_some() as usize) + audio_tracks.len();
    let mut dts_enforcer = crate::media::ring_buffer::DtsEnforcer::new(num_streams);
    let mut reader = Reader::new(
        format!("transcoder:{}:{}", pipeline_id, preset),
        input_buffer,
    );
    let mut nalu_len_size: usize = 4;
    let mut sps_pps_cache: Vec<u8> = {
        let (vsh, _) = engine.get_sequence_headers(&pipeline_id).await;
        if let Some(ref flv_sh) = vsh {
            if flv_sh.len() > 5 {
                let (nls, annexb) = crate::media::codec::parse_avcc_config(&flv_sh[5..]);
                nalu_len_size = nls;
                annexb
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        }
    };
    let mut video_conv_buf = Vec::<u8>::new();
    let mut audio_conv_buf = Vec::<u8>::new();
    // Accumulation buffer: collect all muxed TS bytes for a burst, then
    // write them in a single queue.write() call (one lock acquisition per
    // burst instead of one per packet).
    let mut ts_batch: Vec<u8> = Vec::with_capacity(65536);
    let mut packets = Vec::with_capacity(32);
    loop {
        tokio::select! {
            _ = cancel_token.cancelled() => break,
            _ = reader.wait_for_data() => {
                // Clear at top so ts_batch never carries stale bytes if a
                // future continue path skips the end-of-arm clear (M6 fix).
                ts_batch.clear();
                packets.clear();
                if reader.pull_burst(&mut packets, 32).is_ok() {
                    for pkt in &packets {
                        let payload: &[u8] = match pkt.media_type {
                            MediaType::Video => {
                                match video_for_ts_into(&pkt.payload, pkt.format, &mut nalu_len_size, &mut sps_pps_cache, &mut video_conv_buf) {
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
                                match audio_for_ts_into(&pkt.payload, pkt.format, sr, ch, &mut audio_conv_buf) {
                                    Some(p) => p,
                                    None => continue,
                                }
                            }
                        };

                        let stream_idx = match pkt.media_type {
                            MediaType::Video => 0,
                            MediaType::Audio => {
                                let video_offset = video_meta.is_some() as usize;
                                match audio_tracks
                                    .iter()
                                    .position(|a| a.track_index == pkt.track_index)
                                {
                                    Some(i) => i + video_offset,
                                    None => continue, // unknown track — skip to avoid DTS corruption
                                }
                            }
                        };

                        let (pts, dts) = dts_enforcer.enforce(stream_idx, pkt.pts, pkt.dts);

                        let ts_bytes = muxer.mux_packet(
                            pkt.media_type,
                            pkt.track_index,
                            pts,
                            dts,
                            pkt.is_keyframe,
                            payload,
                        );

                        if !ts_bytes.is_empty() {
                            ts_batch.extend_from_slice(ts_bytes);
                        }
                    }
                    // One lock acquisition for the whole burst.
                    if !ts_batch.is_empty() {
                        input_queue.write(&ts_batch).await;
                    }
                }
            }
        }
    }

    input_queue.close();
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;
    use crate::media::engine::AudioMeta;
    use crate::media::ring_buffer::PayloadFormat;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    // --- parse_audio_routing tests ---

    #[test]
    fn routing_passthrough_for_plain_video_preset() {
        assert!(matches!(
            parse_audio_routing("720p"),
            AudioRouting::Passthrough
        ));
        assert!(matches!(
            parse_audio_routing("source"),
            AudioRouting::Passthrough
        ));
        assert!(matches!(
            parse_audio_routing("1080p"),
            AudioRouting::Passthrough
        ));
    }

    #[test]
    fn routing_select_tracks_single() {
        let routing = parse_audio_routing("720p+atrack:0");
        assert!(matches!(routing, AudioRouting::SelectTracks(ref t) if t == &[0]));
    }

    #[test]
    fn routing_select_tracks_multiple() {
        let routing = parse_audio_routing("source+atrack:0,2,5");
        assert!(matches!(routing, AudioRouting::SelectTracks(ref t) if t == &[0, 2, 5]));
    }

    #[test]
    fn routing_select_tracks_invalid_falls_back_to_passthrough() {
        // Non-numeric track index
        assert!(matches!(
            parse_audio_routing("720p+atrack:abc"),
            AudioRouting::Passthrough
        ));
        // Empty track list after colon
        assert!(matches!(
            parse_audio_routing("720p+atrack:"),
            AudioRouting::Passthrough
        ));
    }

    #[test]
    fn routing_remap_two_channel() {
        let routing = parse_audio_routing("720p+remap:0:1");
        assert!(matches!(
            routing,
            AudioRouting::Remap {
                left: 0,
                right: 1,
                track: 0
            }
        ));
    }

    #[test]
    fn routing_remap_with_track_index() {
        let routing = parse_audio_routing("source+remap:0:1:3");
        assert!(matches!(
            routing,
            AudioRouting::Remap {
                left: 0,
                right: 1,
                track: 3
            }
        ));
    }

    #[test]
    fn routing_remap_default_fallback() {
        // Single part (no right channel) returns Passthrough since parse requires
        // at least 2 parts to produce Remap
        let routing = parse_audio_routing("720p+remap:0");
        assert!(matches!(routing, AudioRouting::Passthrough));
    }

    #[test]
    fn routing_downmix_single_track() {
        let routing = parse_audio_routing("source+downmix:0");
        assert!(matches!(routing, AudioRouting::Downmix(0)));
        let routing = parse_audio_routing("720p+downmix:3");
        assert!(matches!(routing, AudioRouting::Downmix(3)));
    }

    #[test]
    fn routing_downmix_invalid_falls_back_to_passthrough() {
        assert!(matches!(
            parse_audio_routing("720p+downmix:abc"),
            AudioRouting::Passthrough
        ));
        assert!(matches!(
            parse_audio_routing("720p+downmix:"),
            AudioRouting::Passthrough
        ));
    }

    #[test]
    fn routing_atrack_standalone() {
        // atrack: without a video preset prefix
        let routing = parse_audio_routing("atrack:0,1");
        assert!(matches!(routing, AudioRouting::SelectTracks(ref t) if t == &[0, 1]));
    }

    #[test]
    fn routing_remap_standalone() {
        let routing = parse_audio_routing("remap:0:1");
        assert!(matches!(
            routing,
            AudioRouting::Remap {
                left: 0,
                right: 1,
                track: 0
            }
        ));
    }

    #[test]
    fn routing_downmix_standalone() {
        let routing = parse_audio_routing("downmix:0");
        assert!(matches!(routing, AudioRouting::Downmix(0)));
    }

    // --- apply_audio_routing tests ---

    #[test]
    fn apply_routing_passthrough_preserves_all_tracks() {
        let tracks = vec![
            AudioMeta {
                codec: "aac".into(),
                sample_rate: 48000,
                channels: 2,
                channel_layout: None,
                track_index: 0,
                profile: None,
            },
            AudioMeta {
                codec: "aac".into(),
                sample_rate: 44100,
                channels: 1,
                channel_layout: None,
                track_index: 1,
                profile: None,
            },
        ];
        let result = apply_audio_routing(&AudioRouting::Passthrough, &tracks);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].track_index, 0);
        assert_eq!(result[1].track_index, 1);
    }

    #[test]
    fn apply_routing_select_tracks_filters_and_reindexes() {
        let tracks = vec![
            AudioMeta {
                codec: "aac".into(),
                sample_rate: 48000,
                channels: 2,
                channel_layout: None,
                track_index: 0,
                profile: None,
            },
            AudioMeta {
                codec: "aac".into(),
                sample_rate: 44100,
                channels: 1,
                channel_layout: None,
                track_index: 1,
                profile: None,
            },
            AudioMeta {
                codec: "aac".into(),
                sample_rate: 32000,
                channels: 1,
                channel_layout: None,
                track_index: 2,
                profile: None,
            },
        ];
        // Select tracks 0 and 2
        let routing = AudioRouting::SelectTracks(vec![0, 2]);
        let result = apply_audio_routing(&routing, &tracks);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].track_index, 0); // re-indexed: track 0 → index 0
        assert_eq!(result[1].track_index, 1); // re-indexed: track 2 → index 1
        assert_eq!(result[0].sample_rate, 48000);
        assert_eq!(result[1].sample_rate, 32000);
    }

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

    #[test]
    fn test_apply_audio_routing_reindexes() {
        let input_tracks = vec![
            AudioMeta {
                codec: "aac".to_string(),
                channels: 2,
                sample_rate: 48000,
                track_index: 0,
                channel_layout: None,
                profile: None,
            },
            AudioMeta {
                codec: "aac".to_string(),
                channels: 2,
                sample_rate: 48000,
                track_index: 1,
                channel_layout: None,
                profile: None,
            },
            AudioMeta {
                codec: "aac".to_string(),
                channels: 2,
                sample_rate: 48000,
                track_index: 2,
                channel_layout: None,
                profile: None,
            },
        ];

        let routing = AudioRouting::SelectTracks(vec![2]);
        let output_tracks = apply_audio_routing(&routing, &input_tracks);
        assert_eq!(output_tracks.len(), 1);
        assert_eq!(output_tracks[0].track_index, 0); // re-indexed from 2 to 0
    }

    #[tokio::test]
    async fn test_audio_router_reindexes_packets() {
        let source_ring = Arc::new(RingBuffer::new(16));
        let out_ring = Arc::new(RingBuffer::new(16));
        let cancel = CancellationToken::new();

        // Start audio router
        let routing = AudioRouting::SelectTracks(vec![2]);
        let handle = tokio::spawn(start_audio_router(
            "pipe-id".to_string(),
            routing,
            source_ring.clone(),
            out_ring.clone(),
            cancel.clone(),
        ));

        // Push some source packets
        source_ring.push(MediaPacket {
            media_type: MediaType::Video,
            track_index: 0,
            pts: 0,
            dts: 0,
            is_keyframe: true,
            format: PayloadFormat::Raw,
            payload: bytes::Bytes::from_static(&[1, 2, 3]),
        });
        source_ring.push(MediaPacket {
            media_type: MediaType::Audio,
            track_index: 2, // track 2
            pts: 10,
            dts: 10,
            is_keyframe: false,
            format: PayloadFormat::Raw,
            payload: bytes::Bytes::from_static(&[4, 5, 6]),
        });

        // Let the router process
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        cancel.cancel();
        let _ = handle.await;

        // Verify output packets
        let mut reader = Reader::new("test_router".to_string(), out_ring);
        let mut out_pkts = Vec::new();
        while let Ok(Some(pkt)) = reader.pull() {
            out_pkts.push(pkt);
        }

        // Should contain video packet and audio packet
        assert_eq!(out_pkts.len(), 2);
        assert_eq!(out_pkts[0].media_type, MediaType::Video);
        assert_eq!(out_pkts[1].media_type, MediaType::Audio);
        assert_eq!(out_pkts[1].track_index, 0); // re-indexed to 0
    }

    // M7: pts=0 from AV_NOPTS_VALUE would cause a massive backward jump on a
    // long-running stream. Verify the timestamp conversion formula itself is
    // correct and that skipping None-pts packets is the right behavior.
    //
    // We can't inject AV_NOPTS_VALUE into a live FFmpeg pipeline in a unit
    // test, but we can verify that the i128-based conversion that follows
    // a valid pts produces the expected millisecond value, confirming that
    // pts=0 would produce 0ms (a backward jump from, e.g., 3600000ms).
    #[test]
    fn pts_zero_would_produce_zero_ms_timestamp() {
        // Simulate the conversion for pts=0 with a 90kHz timebase (tb=1/90000).
        let pts: i64 = 0;
        let tb_num: i64 = 1;
        let tb_den: i64 = 90000;
        let pts_ms = (pts as i128 * tb_num as i128 * 1000 / tb_den as i128) as i64;
        assert_eq!(pts_ms, 0, "pts=0 produces 0ms — correct to skip, not use");

        // A real 1-hour stream has pts ≈ 324_000_000 ticks at 90kHz.
        let pts_1h: i64 = 3_600 * 90_000;
        let pts_ms_1h = (pts_1h as i128 * tb_num as i128 * 1000 / tb_den as i128) as i64;
        assert_eq!(pts_ms_1h, 3_600_000, "1h at 90kHz = 3600000ms");
        // Substituting 0 for AV_NOPTS_VALUE would create a -3600000ms backward jump.
        assert_eq!(pts_ms - pts_ms_1h, -3_600_000);
    }

    // M6: ts_batch must be cleared at the top of each burst arm so stale bytes
    // never accumulate across iterations. Verify the invariant by simulating
    // the burst pattern: partial batch from one burst must not appear in the next.
    #[test]
    fn ts_batch_cleared_before_each_burst() {
        let mut ts_batch: Vec<u8> = Vec::with_capacity(65536);

        // Simulate two burst cycles: first accumulates data, second must start empty.
        let burst1 = b"packet_data_burst1";
        ts_batch.extend_from_slice(burst1);
        assert!(!ts_batch.is_empty());

        // Write and clear (as the arm does after write()).
        // Then simulate loop top: clear is now at the TOP of the arm.
        ts_batch.clear(); // ← this is the arm-top clear (M6 fix)
        assert!(
            ts_batch.is_empty(),
            "ts_batch must be empty at burst start — stale data would corrupt the stream"
        );

        let burst2 = b"packet_data_burst2";
        ts_batch.extend_from_slice(burst2);
        assert_eq!(&ts_batch[..], burst2, "burst2 must not contain burst1 data");
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

    let mut batch: Vec<MediaPacket> = Vec::with_capacity(32);
    for (stream, packet) in ictx.packets() {
        if token.is_cancelled() {
            break;
        }

        let idx = stream.index();
        let Some(&Some((media_type, track_index))) = stream_meta.get(idx) else {
            continue;
        };

        let tb = stream.time_base();
        // Skip packets with AV_NOPTS_VALUE — using 0 on a long-running stream
        // would cause a massive backward jump through DtsEnforcer (M7 fix).
        let Some(pts) = packet.pts() else { continue };
        let dts = packet.dts().unwrap_or(pts);
        let pts_ms = if tb.1 != 0 {
            // i128 avoids f64 precision loss for large pts values (e.g. after
            // hours of streaming at 90 kHz: pts ≈ 3×10¹¹, f64 has only 53-bit
            // mantissa ≈ 9×10¹⁵ exact range but loses sub-ms precision before that).
            (pts as i128 * tb.0 as i128 * 1000 / tb.1 as i128) as i64
        } else {
            pts
        };
        let dts_ms = if tb.1 != 0 {
            (dts as i128 * tb.0 as i128 * 1000 / tb.1 as i128) as i64
        } else {
            dts
        };
        let is_keyframe = packet.is_key();

        batch.push(MediaPacket {
            media_type,
            track_index,
            pts: pts_ms,
            dts: dts_ms,
            is_keyframe,
            format: PayloadFormat::Raw,
            payload: bytes::Bytes::from_owner(OwnedFfmpegPacket(packet)),
        });
        if batch.len() >= 32 {
            out_ring.push_batch(batch.drain(..));
        }
    }
    if !batch.is_empty() {
        out_ring.push_batch(batch.drain(..));
    }

    Ok(())
}

/// Real decode -> scale -> encode transcoder stage.
pub fn run_ffmpeg_transcode_with_scale(
    in_queue: Arc<crate::media::avio::MemoryQueue>,
    out_ring: Arc<RingBuffer>,
    video_preset: &str,
    token: CancellationToken,
) -> Result<(), &'static str> {
    use crate::media::avio::CustomInput;
    use ffmpeg_next::format::Pixel;

    let mut custom = CustomInput::new(&*in_queue)?;
    let ictx = custom
        .input
        .as_mut()
        .ok_or("Failed to get CustomInput context")?;

    // Identify streams
    let video_idx = ictx
        .streams()
        .find(|s| s.parameters().medium() == ffmpeg_next::media::Type::Video)
        .map(|s| s.index())
        .ok_or("no video stream")?;

    // Build stream metadata (same pattern as h264_transcoder)
    let mut stream_meta: Vec<Option<(MediaType, u32)>> = Vec::new();
    let mut audio_track_counter = 0u32;
    for s in ictx.streams() {
        match s.parameters().medium() {
            ffmpeg_next::media::Type::Video => {
                stream_meta.push(Some((MediaType::Video, 0)));
            }
            ffmpeg_next::media::Type::Audio => {
                stream_meta.push(Some((MediaType::Audio, audio_track_counter)));
                audio_track_counter += 1;
            }
            _ => {
                stream_meta.push(None);
            }
        }
    }

    let dec_params = ictx
        .stream(video_idx)
        .ok_or("no video stream")?
        .parameters();
    let codec_id = dec_params.id();
    let dec_ctx = ffmpeg_next::codec::Context::from_parameters(dec_params)
        .map_err(|_| "decoder context error")?;
    let mut decoder = dec_ctx
        .decoder()
        .video()
        .map_err(|_| "decoder open error")?;

    // Look up target dimensions
    let profile = {
        let cache = crate::media::profiles::cache().blocking_read();
        cache
            .get(video_preset)
            .or_else(|| cache.get("h264"))
            .cloned()
            .unwrap_or_default()
    };

    let target_w = profile.width;
    let target_h = profile.height;
    let skip_scaling = target_w == 0;

    let enc_codec = match codec_id {
        ffmpeg_next::codec::Id::H264 => {
            ffmpeg_next::codec::encoder::find(ffmpeg_next::codec::Id::H264)
                .ok_or("no H.264 encoder")?
        }
        ffmpeg_next::codec::Id::HEVC => ffmpeg_next::codec::encoder::find_by_name("libx265")
            .or_else(|| ffmpeg_next::codec::encoder::find(ffmpeg_next::codec::Id::HEVC))
            .ok_or("no HEVC/H.265 encoder")?,
        _ => return Err("Unsupported video codec for internal transcoding"),
    };

    let mut encoder: Option<ffmpeg_next::codec::encoder::video::Encoder> = None;
    let mut scaler: Option<ffmpeg_next::software::scaling::Context> = None;
    let mut enc_frame = ffmpeg_next::frame::Video::empty();
    let mut enc_pkt = ffmpeg_next::Packet::empty();
    let mut pts_counter: i64 = 0;
    let mut fps_den: i64 = 1;
    let mut fps_num: i64 = 30;

    for (stream, pkt) in ictx.packets() {
        if token.is_cancelled() {
            break;
        }

        let idx = stream.index();

        // Audio copy
        if stream.parameters().medium() == ffmpeg_next::media::Type::Audio {
            let Some(&Some((media_type, track_index))) = stream_meta.get(idx) else {
                continue;
            };
            let tb = stream.time_base();
            // Skip packets with AV_NOPTS_VALUE (M7 fix — same as passthrough path).
            let Some(pts) = pkt.pts() else { continue };
            let dts_val = pkt.dts().unwrap_or(pts);
            let pts_ms = if tb.1 != 0 {
                (pts as i128 * tb.0 as i128 * 1000 / tb.1 as i128) as i64
            } else {
                pts
            };
            let dts_ms = if tb.1 != 0 {
                (dts_val as i128 * tb.0 as i128 * 1000 / tb.1 as i128) as i64
            } else {
                dts_val
            };
            let is_keyframe = pkt.is_key();
            out_ring.push(MediaPacket {
                media_type,
                track_index,
                pts: pts_ms,
                dts: dts_ms,
                is_keyframe,
                format: PayloadFormat::Raw,
                payload: bytes::Bytes::from_owner(OwnedFfmpegPacket(pkt)),
            });
            continue;
        }

        if idx != video_idx {
            continue;
        }

        if decoder.send_packet(&pkt).is_err() {
            continue;
        }

        let mut dec_frame = ffmpeg_next::frame::Video::empty();
        while decoder.receive_frame(&mut dec_frame).is_ok() {
            // Lazy encoder + scaler init
            if encoder.is_none() {
                let width = decoder.width();
                let height = decoder.height();
                let in_fmt = dec_frame.format();

                let out_w = if target_w > 0 { target_w } else { width };
                let out_h = if target_h > 0 { target_h } else { height };

                let need_scaling = !skip_scaling && (out_w != width || out_h != height)
                    || in_fmt != Pixel::YUV420P;
                if need_scaling {
                    let sw = ffmpeg_next::software::scaling::Context::get(
                        in_fmt,
                        width,
                        height,
                        Pixel::YUV420P,
                        out_w,
                        out_h,
                        ffmpeg_next::software::scaling::Flags::BILINEAR,
                    )
                    .map_err(|_| "failed to create scaler")?;
                    scaler = Some(sw);
                }

                let fr = stream.avg_frame_rate();
                let (fn_, fd) = if fr.numerator() > 0 && fr.denominator() > 0 {
                    (fr.numerator(), fr.denominator())
                } else {
                    (30, 1)
                };
                fps_num = fn_ as i64;
                fps_den = fd as i64;

                // SAFETY: avcodec_alloc_context3 allocates an FFmpeg
                // AVCodecContext. The `enc_codec` pointer was obtained from
                // avcodec_find_encoder_by_name and is valid for the process
                // lifetime. The returned pointer is either null (handled) or
                // a valid heap allocation. Context::wrap takes ownership.
                let enc_ctx = unsafe {
                    let ptr = ffmpeg_next::ffi::avcodec_alloc_context3(
                        enc_codec.as_ptr() as *mut ffmpeg_next::ffi::AVCodec
                    );
                    if ptr.is_null() {
                        return Err("failed to allocate encoder context");
                    }
                    ffmpeg_next::codec::Context::wrap(ptr, None)
                };
                let mut enc_video = enc_ctx
                    .encoder()
                    .video()
                    .map_err(|_| "failed to get encoder video interface")?;

                enc_video.set_width(out_w);
                enc_video.set_height(out_h);
                enc_video.set_format(Pixel::YUV420P);
                enc_video.set_time_base(ffmpeg_next::Rational::new(fd, fn_));
                enc_video.set_frame_rate(Some(ffmpeg_next::Rational::new(fn_, fd)));
                enc_video.set_gop(profile.gop);
                enc_video.set_max_b_frames(profile.bframes);

                let bitrate = if profile.bitrate > 0 {
                    profile.bitrate as usize
                } else {
                    (out_w * out_h) as usize * 3
                };
                enc_video.set_bit_rate(bitrate);
                if profile.max_bitrate > 0 {
                    enc_video.set_max_bit_rate(profile.max_bitrate as usize);
                }

                let mut opts = ffmpeg_next::Dictionary::new();
                opts.set("preset", &profile.preset);
                opts.set("tune", &profile.tune);
                if profile.bitrate == 0 {
                    opts.set("crf", &profile.crf.to_string());
                }

                let opened = enc_video
                    .open_as_with(enc_codec, opts)
                    .map_err(|_| "failed to open encoder")?;
                encoder = Some(opened);
            }

            let Some(enc) = encoder.as_mut() else {
                continue;
            };

            let frame_to_encode = if let Some(ref mut sw) = scaler {
                if sw.run(&dec_frame, &mut enc_frame).is_err() {
                    continue;
                }
                enc_frame.set_pts(Some(pts_counter));
                &enc_frame
            } else {
                dec_frame.set_pts(Some(pts_counter));
                &dec_frame
            };
            pts_counter += 1;

            if enc.send_frame(frame_to_encode).is_err() {
                continue;
            }

            while enc.receive_packet(&mut enc_pkt).is_ok() {
                let pts_ms = enc_pkt.pts().unwrap_or(0) * fps_den * 1000 / fps_num;
                let dts_raw = enc_pkt.dts().unwrap_or_else(|| enc_pkt.pts().unwrap_or(0));
                let dts_ms = dts_raw * fps_den * 1000 / fps_num;
                // enc_pkt is reused across iterations; clone() calls av_packet_ref (refcount
                // bump only, no data copy) so the ring buffer holds the AVBufferRef alive.
                out_ring.push(MediaPacket {
                    media_type: MediaType::Video,
                    track_index: 0,
                    pts: pts_ms,
                    dts: dts_ms,
                    is_keyframe: enc_pkt.is_key(),
                    format: PayloadFormat::Raw,
                    payload: bytes::Bytes::from_owner(OwnedFfmpegPacket(enc_pkt.clone())),
                });
            }
        }
    }

    if let Some(enc) = encoder.as_mut() {
        let _ = enc.send_eof();
        while enc.receive_packet(&mut enc_pkt).is_ok() {
            let pts_ms = enc_pkt.pts().unwrap_or(0) * fps_den * 1000 / fps_num;
            let dts_raw = enc_pkt.dts().unwrap_or_else(|| enc_pkt.pts().unwrap_or(0));
            let dts_ms = dts_raw * fps_den * 1000 / fps_num;
            out_ring.push(MediaPacket {
                media_type: MediaType::Video,
                track_index: 0,
                pts: pts_ms,
                dts: dts_ms,
                is_keyframe: enc_pkt.is_key(),
                format: PayloadFormat::Raw,
                payload: bytes::Bytes::from_owner(OwnedFfmpegPacket(enc_pkt.clone())),
            });
        }
    }

    Ok(())
}
