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

use crate::media::avio::MemoryQueue;
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
) {
    // Wait for ingest metadata before starting
    let (video_meta, audio_tracks) = loop {
        if cancel_token.is_cancelled() {
            return;
        }
        let result = {
            let ingests = engine.active_ingests.read().await;
            ingests.get(&pipeline_id).and_then(|i| {
                let video = i.video.clone()?;
                if video.codec != "hevc" && video.codec != "h265" {
                    return None;
                }
                let mut tracks = i
                    .audio_tracks
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                if tracks.is_empty()
                    && let Some(audio) = i.audio.clone()
                {
                    tracks.push(audio);
                }
                Some((video, tracks))
            })
        };
        if let Some(meta) = result {
            break meta;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    };

    let input_queue = Arc::new(MemoryQueue::new());

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
        if result.is_err() {
            eprintln!("[h264-tc] FFmpeg stage panicked for pipeline {pid}");
        }
        cancel_on_exit.cancel();
    });
    engine.register_os_thread(handle);

    // Forward source RingBuffer packets to MemoryQueue, muxed as MPEG-TS
    let mut muxer = crate::media::mpegts::TsMuxer::new(Some(&video_meta), &audio_tracks);
    let num_streams = 1 + audio_tracks.len();
    let mut dts = crate::media::ring_buffer::DtsEnforcer::new(num_streams);
    let mut reader = Reader::new(format!("h264_tc:{}", pipeline_id), input_buffer);
    let mut nalu_len = 4usize;
    let mut sps_cache = Vec::new();
    let mut video_conv_buf = Vec::<u8>::new();
    let mut audio_conv_buf = Vec::<u8>::new();
    // Accumulation buffer: collect all muxed TS bytes for a burst, then
    // write them in a single queue.write() call (one lock acquisition per
    // burst instead of one per packet).
    let mut ts_batch: Vec<u8> = Vec::new();

    loop {
        tokio::select! {
            _ = cancel_token.cancelled() => break,
            _ = reader.wait_for_data() => {
                let mut packets = Vec::with_capacity(32);
                if reader.pull_burst(&mut packets, 32).is_err() {
                    continue;
                }
                for pkt in packets {
                    let payload: &[u8] = match pkt.media_type {
                        MediaType::Video => {
                            match crate::media::codec::video_for_ts_into(
                                &pkt.payload, pkt.format,
                                &mut nalu_len, &mut sps_cache, &mut video_conv_buf,
                            ) {
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
                            match crate::media::codec::audio_for_ts_into(
                                &pkt.payload, pkt.format, sr, ch, &mut audio_conv_buf,
                            ) {
                                Some(p) => p,
                                None => continue,
                            }
                        }
                    };

                    let stream_idx = match pkt.media_type {
                        MediaType::Video => 0,
                        MediaType::Audio => {
                            match audio_tracks
                                .iter()
                                .position(|a| a.track_index == pkt.track_index)
                            {
                                Some(i) => i + 1,
                                None => continue, // unknown track — skip to avoid DTS corruption
                            }
                        }
                    };

                    let (pts, dts_val) = dts.enforce(stream_idx, pkt.pts, pkt.dts);
                    let ts_bytes = muxer.mux_packet(
                        pkt.media_type, pkt.track_index,
                        pts, dts_val, pkt.is_keyframe, payload,
                    );
                    if !ts_bytes.is_empty() {
                        ts_batch.extend_from_slice(ts_bytes);
                    }
                }
                // One lock acquisition for the whole burst.
                if !ts_batch.is_empty() {
                    input_queue.write(&ts_batch).await;
                    ts_batch.clear();
                }
            }
        }
    }

    input_queue.close();
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
) {
    use crate::media::avio::CustomInput;
    use ffmpeg_next::format::Pixel;

    let mut custom = match CustomInput::new(&*in_queue) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[h264-tc] CustomInput: {e}");
            return;
        }
    };
    let ictx = match custom.input.as_mut() {
        Some(i) => i,
        None => return,
    };

    // Identify streams
    let video_idx = match ictx
        .streams()
        .find(|s| s.parameters().medium() == ffmpeg_next::media::Type::Video)
        .map(|s| s.index())
    {
        Some(i) => i,
        None => {
            eprintln!("[h264-tc] no video stream");
            return;
        }
    };

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

    let dec_params = match ictx.stream(video_idx) {
        Some(s) => s.parameters(),
        None => return,
    };
    let dec_ctx = match ffmpeg_next::codec::Context::from_parameters(dec_params) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[h264-tc] decoder context: {e}");
            return;
        }
    };
    let mut decoder = match dec_ctx.decoder().video() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("[h264-tc] decoder open: {e}");
            return;
        }
    };

    let enc_codec = match ffmpeg_next::codec::encoder::find(ffmpeg_next::codec::Id::H264) {
        Some(c) => c,
        None => {
            eprintln!("[h264-tc] no H.264 encoder");
            return;
        }
    };

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
            let pts = pkt.pts().unwrap_or(0);
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

                let sw = match ffmpeg_next::software::scaling::Context::get(
                    in_fmt,
                    width,
                    height,
                    Pixel::YUV420P,
                    out_w,
                    out_h,
                    ffmpeg_next::software::scaling::Flags::BILINEAR,
                ) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("[h264-tc] scaler: {e}");
                        return;
                    }
                };

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
                let enc_ctx = unsafe {
                    let ptr = ffmpeg_next::ffi::avcodec_alloc_context3(
                        enc_codec.as_ptr() as *mut ffmpeg_next::ffi::AVCodec
                    );
                    if ptr.is_null() {
                        eprintln!("[h264-tc] failed to allocate encoder context");
                        return;
                    }
                    ffmpeg_next::codec::Context::wrap(ptr, None)
                };
                let mut enc_video = match enc_ctx.encoder().video() {
                    Ok(e) => e,
                    Err(e) => {
                        eprintln!("[h264-tc] encoder ctx: {e}");
                        return;
                    }
                };

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

                println!(
                    "[h264-tc] encoder: {}x{} preset={} tune={} crf={} bitrate={}",
                    out_w, out_h, profile.preset, profile.tune, profile.crf, profile.bitrate
                );

                let opened = match enc_video.open_as_with(enc_codec, opts) {
                    Ok(e) => e,
                    Err(e) => {
                        eprintln!("[h264-tc] encoder open: {e}");
                        return;
                    }
                };

                scaler = Some(sw);
                encoder = Some(opened);
            }

            let enc = encoder.as_mut().unwrap();
            let sw = scaler.as_mut().unwrap();

            if sw.run(&dec_frame, &mut enc_frame).is_err() {
                continue;
            }
            enc_frame.set_pts(Some(pts_counter));
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
}
