//! Shared H.265→H.264 transcoder stage.
//!
//! Runs a single decode→encode pipeline per pipeline_id. All RTMP egresses
//! on the same source pipeline share one OS thread that decodes H.265 and
//! re-encodes H.264. Audio packets pass through unchanged.
//!
//! Architecture (same pattern as `transcoder.rs`):
//!
//!   tokio task:  source RingBuffer → TsMuxer → MemoryQueue
//!   std::thread: MemoryQueue → FFmpeg demux → decode H.265 → encode H.264 → output RingBuffer
//!
//! The output RingBuffer carries H.264 video (PayloadFormat::Raw) plus
//! passthrough audio — exactly what the RTMP egress reader expects.

use bytes::Bytes;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

use crate::domain::stage::StageKey;
use crate::media::avio::MemoryQueue;
use crate::media::engine::{AudioMeta, VideoMeta};
use crate::media::feeder::{PacketFeedConfig, TsPacketFeeder};
use crate::media::ring_buffer::{MediaPacket, MediaType, PayloadFormat, Reader, RingBuffer};

/// Zero-copy wrapper: holds an `ffmpeg_next::Packet` so `Bytes::from_owner`
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

async fn wait_for_h264_stage_metadata(
    engine: &Arc<crate::media::engine::MediaEngine>,
    pipeline_id: &str,
    input_buffer: &Arc<RingBuffer>,
    cancel_token: &CancellationToken,
) -> Option<(VideoMeta, std::sync::Arc<Vec<AudioMeta>>)> {
    loop {
        if cancel_token.is_cancelled() {
            return None;
        }

        let result = {
            let ingests = engine.ingests.active.read().await;
            ingests.get(pipeline_id).and_then(|ingest| {
                let mut video = ingest.video.clone()?;
                let input_codec = input_buffer.codec_hint_str();
                if !input_codec.is_empty() {
                    video.codec = input_codec.to_string();
                }
                if video.codec != "hevc" && video.codec != "h265" {
                    return None;
                }

                let tracks = if let Some(ring_tracks) = input_buffer.audio_tracks()
                    && !ring_tracks.is_empty()
                {
                    std::sync::Arc::new(ring_tracks.to_vec())
                } else {
                    let lock = ingest
                        .audio_tracks
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    if lock.is_empty()
                        && let Some(audio) = ingest.audio.clone()
                    {
                        std::sync::Arc::new(vec![audio])
                    } else {
                        std::sync::Arc::clone(&lock)
                    }
                };

                Some((video, tracks))
            })
        };

        if let Some(meta) = result {
            return Some(meta);
        }

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

/// Tokio task entry point for the shared H.265→H.264 transcoder.
///
/// 1. Waits for ingest metadata (video + audio tracks).
/// 2. Spawns a blocking OS thread for FFmpeg decode→encode.
/// 3. Forwards source RingBuffer packets to the MemoryQueue as MPEG-TS.
pub async fn start_h264_transcoder(
    pipeline_id: String,
    input_buffer: Arc<RingBuffer>,
    output_buffer: Arc<RingBuffer>,
    engine: Arc<crate::media::engine::MediaEngine>,
    cancel_token: CancellationToken,
    stage_key: StageKey,
) {
    let Some((video_meta, audio_tracks)) =
        wait_for_h264_stage_metadata(&engine, &pipeline_id, &input_buffer, &cancel_token).await
    else {
        engine
            .runtime
            .event_log
            .emit(crate::events::EventKind::StageStopped {
                pipeline_id: pipeline_id.clone(),
                encoding: stage_key.kind.to_string(),
            });
        return;
    };

    let stage_metrics = engine.get_or_create_stage_metrics(stage_key.clone()).await;

    let input_queue = Arc::new(MemoryQueue::new());
    engine
        .register_input_queue(stage_key.clone(), input_queue.clone())
        .await;

    // Spawn OS thread for FFmpeg decode→encode
    let iq_clone = input_queue.clone();
    let out_clone = output_buffer.clone();
    let cancel_clone = cancel_token.clone();
    let cancel_on_exit = cancel_token.clone();
    let pid = pipeline_id.clone();
    let handle = std::thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_ffmpeg_h264_stage(iq_clone, out_clone, cancel_clone, &pid)
        }));
        match result {
            Ok(Err(err)) => error!(pipeline_id = %pid, err, "FFmpeg H.264 stage failed"),
            Err(_) => error!("FFmpeg stage panicked for pipeline {pid}"),
            _ => {}
        }
        cancel_on_exit.cancel();
    });
    engine.register_os_thread(handle);

    // Forward source RingBuffer packets to MemoryQueue, muxed as MPEG-TS.
    let (video_sequence_header, _) = engine.get_sequence_headers(&pipeline_id).await;
    let mut feeder = TsPacketFeeder::new(
        Some(&video_meta),
        audio_tracks.clone(),
        PacketFeedConfig {
            video_sequence_header: video_sequence_header.as_ref().map(|v| v.to_vec()),
            ..PacketFeedConfig::default()
        },
    );
    let mut reader = Reader::new(format!("h264_tc:{}", pipeline_id), input_buffer);
    // Accumulation buffer: collect all muxed TS bytes for a burst, then
    // write them in a single queue.write() call (one lock acquisition per
    // burst instead of one per packet).
    let mut ts_batch: Vec<u8> = Vec::with_capacity(65536);
    let mut packets = Vec::with_capacity(32);

    loop {
        tokio::select! {
            _ = cancel_token.cancelled() => break,
            _ = reader.wait_for_data() => {
                // Clear both buffers at the top of each burst — defensive
                // guard so ts_batch never carries stale bytes if a future
                // continue path skips the end-of-arm clear (M6 fix).
                ts_batch.clear();
                packets.clear();
                if reader.pull_burst(&mut packets, 32).is_err() {
                    continue;
                }
                for pkt in &packets {
                    let in_bytes = pkt.payload.len() as u64;
                    if feeder.extend_ts_for_packet(pkt, &mut ts_batch) {
                        stage_metrics.record_in(in_bytes);
                    }
                }
                // One lock acquisition for the whole burst.
                if !ts_batch.is_empty()
                    && !input_queue.write_cancellable(&ts_batch, &cancel_token).await
                {
                    break;
                }
            }
        }
    }

    input_queue.close();
    engine.remove_input_queue(&stage_key).await;
    engine.remove_stage_metrics(&stage_key).await;
    engine
        .runtime
        .event_log
        .emit(crate::events::EventKind::StageStopped {
            pipeline_id: pipeline_id.clone(),
            encoding: stage_key.kind.to_string(),
        });
}

/// Blocking FFmpeg decode→encode loop, runs on a dedicated OS thread.
///
/// Demuxes MPEG-TS from `in_queue`, decodes H.265 video, encodes H.264,
/// and pushes packets to `out_ring`. Audio passes through unchanged.
fn run_ffmpeg_h264_stage(
    in_queue: Arc<MemoryQueue>,
    out_ring: Arc<RingBuffer>,
    cancel: CancellationToken,
    _pipeline_id: &str,
) -> Result<(), &'static str> {
    use crate::media::avio::CustomInput;
    use ffmpeg_next::format::Pixel;

    let mut custom = CustomInput::new(&*in_queue)?;
    let ictx = custom
        .input
        .as_mut()
        .ok_or("failed to get CustomInput context")?;

    // Identify streams
    let video_idx = ictx
        .streams()
        .find(|s| s.parameters().medium() == ffmpeg_next::media::Type::Video)
        .map(|s| s.index())
        .ok_or("no video stream")?;

    // Build stream metadata: (media_type, track_index) for each stream
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
    let dec_ctx = ffmpeg_next::codec::Context::from_parameters(dec_params)
        .map_err(|_| "decoder context error")?;
    let mut decoder = dec_ctx
        .decoder()
        .video()
        .map_err(|_| "decoder open error")?;

    let enc_codec = ffmpeg_next::codec::encoder::find(ffmpeg_next::codec::Id::H264)
        .ok_or("no H.264 encoder")?;

    // Build x264 encoder options: CRF mode for quality-based encoding
    // instead of fixed bitrate. CRF 23 is x264's default.

    let mut encoder: Option<ffmpeg_next::codec::encoder::video::Encoder> = None;
    let mut scaler: Option<ffmpeg_next::software::scaling::Context> = None;
    let mut enc_frame = ffmpeg_next::frame::Video::empty();
    let mut enc_pkt = ffmpeg_next::Packet::empty();
    let mut pts_counter: i64 = 0;
    let mut fps_den: i64 = 1;
    let mut fps_num: i64 = 30;

    for (stream, pkt) in ictx.packets() {
        if cancel.is_cancelled() {
            break;
        }

        let idx = stream.index();

        // Audio passthrough
        if stream.parameters().medium() == ffmpeg_next::media::Type::Audio {
            let Some(&Some((media_type, track_index))) = stream_meta.get(idx) else {
                continue;
            };
            let tb = stream.time_base();
            // Drop packets with AV_NOPTS_VALUE rather than substituting 0.
            // A pts of 0 on a stream running for hours would cause a massive
            // backward jump through DtsEnforcer, corrupting A/V sync (M7 fix).
            let Some(pts) = pkt.pts() else { continue };
            let dts_val = pkt.dts().unwrap_or(pts);
            let pts_ms = if tb.1 != 0 {
                // i128 avoids f64 precision loss for large pts values on long
                // streams (hours of 90 kHz timebase accumulate sub-ms drift).
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
                payload: Bytes::from_owner(OwnedFfmpegPacket(pkt)),
            });
            continue;
        }

        if stream.index() != video_idx {
            continue;
        }

        // Video: decode H.265 → encode H.264
        if decoder.send_packet(&pkt).is_err() {
            continue;
        }

        let mut dec_frame = ffmpeg_next::frame::Video::empty();
        while decoder.receive_frame(&mut dec_frame).is_ok() {
            // Lazy encoder + scaler init on first decoded frame
            if encoder.is_none() {
                let width = decoder.width();
                let height = decoder.height();
                let in_fmt = dec_frame.format();

                // Load transcode profile from DB (via runtime cache)
                let profile = crate::media::profiles::cache()
                    .blocking_read()
                    .get("h264")
                    .cloned()
                    .unwrap_or_default();

                // Resolve output dimensions: 0 = match source
                let out_w = if profile.width > 0 {
                    profile.width
                } else {
                    width
                };
                let out_h = if profile.height > 0 {
                    profile.height
                } else {
                    height
                };

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

                let fr = stream.avg_frame_rate();
                let (fn_, fd) = if fr.numerator() > 0 && fr.denominator() > 0 {
                    (fr.numerator(), fr.denominator())
                } else {
                    (30, 1)
                };
                fps_num = fn_ as i64;
                fps_den = fd as i64;

                // Allocate encoder context with the H.264 codec so codec_id
                // and codec_type are set correctly (avcodec_alloc_context3
                // with NULL leaves them unset, causing open to fail).
                // SAFETY: avcodec_alloc_context3 is an FFmpeg allocation
                // function. The `enc_codec` pointer was obtained from
                // avcodec_find_encoder_by_name (a valid codec descriptor
                // valid for the process lifetime). The returned AVCodecContext
                // pointer is either null (allocation failure, handled) or
                // a valid heap allocation owned by the caller.
                // Context::wrap takes ownership and manages deallocation.
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
                if profile.bitrate > 0 {
                    enc_video.set_bit_rate(profile.bitrate as usize);
                    if profile.max_bitrate > 0 {
                        enc_video.set_max_bit_rate(profile.max_bitrate as usize);
                    }
                }

                let mut opts = ffmpeg_next::Dictionary::new();
                opts.set("preset", &profile.preset);
                opts.set("tune", &profile.tune);
                if profile.bitrate == 0 {
                    opts.set("crf", &profile.crf.to_string());
                }

                info!(
                    "[h264-tc] encoder: {}x{} preset={} tune={} crf={} bitrate={}",
                    out_w, out_h, profile.preset, profile.tune, profile.crf, profile.bitrate
                );

                let opened = enc_video
                    .open_as_with(enc_codec, opts)
                    .map_err(|_| "failed to open encoder")?;

                scaler = Some(sw);
                encoder = Some(opened);
            }

            let Some(enc) = encoder.as_mut() else {
                continue;
            };
            let Some(sw) = scaler.as_mut() else { continue };

            if sw.run(&dec_frame, &mut enc_frame).is_err() {
                continue;
            }
            enc_frame.set_pts(Some(pts_counter));
            // Decoded frames may retain source I/P/B tags; clear them at the
            // transcode boundary so x264 uses this encoder's GOP/B-frame policy.
            enc_frame.set_kind(ffmpeg_next::util::picture::Type::None);
            pts_counter += 1;

            if enc.send_frame(&enc_frame).is_err() {
                continue;
            }
            while enc.receive_packet(&mut enc_pkt).is_ok() {
                let pts_ms = enc_pkt.pts().unwrap_or(0) * fps_den * 1000 / fps_num;
                // DTS can differ from PTS when B-frames are enabled: the encoder
                // returns the decode timestamp separately.  Setting dts=pts would
                // break B-frame reordering in downstream muxers (TS, MP4).
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
                    payload: Bytes::from_owner(OwnedFfmpegPacket(enc_pkt.clone())),
                });
            }
        }
    }

    // Flush remaining encoder
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
                payload: Bytes::from_owner(OwnedFfmpegPacket(enc_pkt.clone())),
            });
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::sync::Arc;

    fn extract_2v16a_hevc_ts_sample() -> Vec<u8> {
        let ffmpeg = crate::ffmpeg_extract::ensure_ffmpeg_extracted();
        let fixture = crate::test_fixtures::checked_in_fixture("media/colorbar-timer-2v16a.mp4")
            .expect("2v16a fixture should exist");
        let output = Command::new(ffmpeg)
            .args([
                "-v",
                "error",
                "-i",
                fixture.to_str().expect("utf-8 fixture path"),
                "-map",
                "0:v:1",
                "-map",
                "0:a",
                "-c",
                "copy",
                "-t",
                "1",
                "-f",
                "mpegts",
                "pipe:1",
            ])
            .output()
            .expect("spawn bundled ffmpeg for 2v16a HEVC sample extraction");
        assert!(
            output.status.success(),
            "ffmpeg 2v16a HEVC sample extraction failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !output.stdout.is_empty(),
            "2v16a HEVC TS sample should not be empty"
        );
        output.stdout
    }

    #[test]
    fn h264_transcoder_emits_packets_from_checked_in_hevc_fixture() {
        let fixture =
            crate::test_fixtures::canonical_h265_ts_fixture().unwrap_or_else(|e| panic!("{e}"));
        let fixture_bytes = std::fs::read(&fixture)
            .unwrap_or_else(|e| panic!("failed to read fixture {}: {e}", fixture.display()));

        let input_queue = Arc::new(MemoryQueue::new());
        input_queue.write_sync(&fixture_bytes);
        input_queue.close();

        let output_ring = Arc::new(RingBuffer::new(16_384));
        let cancel = CancellationToken::new();

        run_ffmpeg_h264_stage(input_queue, output_ring.clone(), cancel, "test-hevc-h264")
            .unwrap_or_else(|e| panic!("HEVC->H.264 stage failed on checked-in fixture: {e}"));

        let mut reader = Reader::new("test_h264_tc_output".to_string(), output_ring);
        let mut packets = Vec::new();
        while let Ok(Some(packet)) = reader.pull() {
            packets.push(packet);
        }

        assert!(
            !packets.is_empty(),
            "HEVC->H.264 stage should emit packets for the checked-in HEVC fixture"
        );
        assert!(
            packets
                .iter()
                .any(|packet| packet.media_type == MediaType::Video),
            "HEVC->H.264 stage should emit at least one video packet"
        );
        assert!(
            packets
                .iter()
                .any(|packet| packet.media_type == MediaType::Audio),
            "HEVC->H.264 stage should preserve audio packets"
        );
        assert!(
            packets
                .iter()
                .filter(|packet| packet.media_type == MediaType::Video)
                .all(|packet| {
                    packet.track_index == 0
                        && packet.format == PayloadFormat::Raw
                        && !packet.payload.is_empty()
                }),
            "transcoded video packets must remain non-empty raw track-0 packets"
        );
    }

    #[test]
    fn h264_transcoder_emits_packets_from_2v16a_hevc_stream() {
        let fixture_bytes = extract_2v16a_hevc_ts_sample();

        let input_queue = Arc::new(MemoryQueue::new());
        input_queue.write_sync(&fixture_bytes);
        input_queue.close();

        let output_ring = Arc::new(RingBuffer::new(16_384));
        let cancel = CancellationToken::new();

        run_ffmpeg_h264_stage(
            input_queue,
            output_ring.clone(),
            cancel,
            "test-2v16a-hevc-h264",
        )
        .unwrap_or_else(|e| panic!("HEVC->H.264 stage failed on 2v16a sample: {e}"));

        let mut reader = Reader::new("test_2v16a_h264_tc_output".to_string(), output_ring);
        let mut packets = Vec::new();
        while let Ok(Some(packet)) = reader.pull() {
            packets.push(packet);
        }

        assert!(
            !packets.is_empty(),
            "HEVC->H.264 stage should emit packets for the 2v16a HEVC sample"
        );
        assert!(
            packets
                .iter()
                .any(|packet| packet.media_type == MediaType::Video),
            "2v16a HEVC sample should produce transcoded video packets"
        );
        assert!(
            packets
                .iter()
                .any(|packet| packet.media_type == MediaType::Audio),
            "2v16a HEVC sample should preserve audio packets"
        );
    }

    #[tokio::test]
    async fn h264_stage_metadata_prefers_upstream_ring_tracks_and_codec_hint() {
        let engine = Arc::new(crate::media::engine::MediaEngine::new());
        engine
            .try_register_ingest("pipe-h264-stage-meta", "stream-key", "srt")
            .await
            .unwrap();

        let ingest_audio = vec![
            AudioMeta {
                codec: "aac".to_string(),
                sample_rate: 48000,
                channels: 2,
                channel_layout: None,
                track_index: 0,
                pid: Some(0x101),
                language: None,
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
                language: None,
                title: None,
                profile: None,
            },
        ];
        engine
            .update_ingest_meta(
                "pipe-h264-stage-meta",
                Some(VideoMeta {
                    codec: "hevc".to_string(),
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
                }),
                ingest_audio.first().cloned(),
                None,
            )
            .await;
        engine
            .update_ingest_audio_tracks("pipe-h264-stage-meta", ingest_audio)
            .await;

        let upstream_ring = Arc::new(RingBuffer::new(32));
        upstream_ring.set_codec_hint("hevc");
        upstream_ring.set_audio_tracks(vec![AudioMeta {
            codec: "aac".to_string(),
            sample_rate: 48000,
            channels: 2,
            channel_layout: None,
            track_index: 0,
            pid: Some(0x102),
            language: None,
            title: None,
            profile: None,
        }]);

        let cancel = CancellationToken::new();
        let (video, audio_tracks) =
            wait_for_h264_stage_metadata(&engine, "pipe-h264-stage-meta", &upstream_ring, &cancel)
                .await
                .expect("stage metadata");

        assert_eq!(video.codec, "hevc");
        assert_eq!(audio_tracks.len(), 1);
        assert_eq!(audio_tracks[0].track_index, 0);
        assert_eq!(audio_tracks[0].pid, Some(0x102));
    }

    #[test]
    #[ignore = "legacy in-process worker drains only on EOF; live codec-edge stages use external ffmpeg"]
    fn h264_transcoder_emits_live_video_before_input_eof() {
        let fixture_bytes = extract_2v16a_hevc_ts_sample();

        let input_queue = Arc::new(MemoryQueue::new());
        input_queue.write_sync(&fixture_bytes);

        let output_ring = Arc::new(RingBuffer::new(16_384));
        let mut reader = Reader::new_live(
            "test_live_2v16a_h264_tc_output".to_string(),
            output_ring.clone(),
        );
        let cancel = CancellationToken::new();
        let input_queue_for_thread = input_queue.clone();
        let output_ring_for_thread = output_ring.clone();
        let cancel_for_thread = cancel.clone();

        let handle = std::thread::spawn(move || {
            run_ffmpeg_h264_stage(
                input_queue_for_thread,
                output_ring_for_thread,
                cancel_for_thread,
                "test-live-2v16a-hevc-h264",
            )
        });

        std::thread::sleep(std::time::Duration::from_millis(750));

        let mut packets = Vec::new();
        while let Ok(Some(packet)) = reader.pull() {
            packets.push(packet);
        }

        cancel.cancel();
        input_queue.close();
        handle
            .join()
            .expect("live HEVC->H.264 stage thread should join")
            .unwrap_or_else(|e| panic!("live HEVC->H.264 stage failed on 2v16a sample: {e}"));

        assert!(
            packets
                .iter()
                .any(|packet| packet.media_type == MediaType::Video),
            "live HEVC->H.264 stage should emit video before EOF"
        );
        assert!(
            packets
                .iter()
                .any(|packet| packet.media_type == MediaType::Video && packet.is_keyframe),
            "live HEVC->H.264 stage should emit a keyframe before EOF for HLS preview"
        );
    }
}
