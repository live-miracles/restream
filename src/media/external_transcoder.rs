//! External transcoder: shared pipeline stage using a subprocess FFmpeg.
//!
//! # Architecture
//!
//! The external transcoder is a **shared stage** in the media graph, not a
//! per-output process. One FFmpeg subprocess is spawned per (pipeline, preset)
//! pair. All egress outputs that request the same encoding on the same pipeline
//! read from the shared output ring buffer.
//!
//! ```text
//! source_ring
//!     │  (Reader + TsMuxer → MPEG-TS bytes)
//!     ▼
//! FFmpeg stdin  ──►  [scale + libx264 + …]  ──►  FFmpeg stdout (MPEG-TS)
//!                                                       │
//!                                           (TsDemuxer → MediaPackets)
//!                                                       │
//!                                                 output_ring  ◄── shared
//!                                                       │
//!                                    ┌─────────────────┼──────────────────┐
//!                                RTMP-out1          SRT-out1          HLS-out1
//! ```
//!
//! # Passthrough
//!
//! `source` / `custom` encodings never enter the transcoder stage. Egresses
//! for those encodings read directly from `source_ring`.
//!
//! # Backend selection
//!
//! By default every non-passthrough encoding uses this external backend.
//! Set `RESTREAM_USE_INTERNAL_TRANSCODER=1` to switch to the in-process FFmpeg
//! backend (`src/media/transcoder.rs`). The internal backend uses libavcodec
//! via Rust FFI; prefer the external backend until the FFI layer hardens.

use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::media::codec::{audio_for_ts_into, video_for_ts_into};
use crate::media::mpegts::{TsDemuxer, TsMuxer};
use crate::media::ring_buffer::{DtsEnforcer, MediaType, Reader, RingBuffer};

// ── FFmpeg arg builders ────────────────────────────────────────────────────

/// Build FFmpeg arguments for a **shared transcoder stage**.
///
/// Input  : MPEG-TS read from stdin (`-i -`)
/// Output : MPEG-TS written to stdout (`pipe:1`)
///
/// The returned args contain no destination URL; the caller reads stdout and
/// feeds the packets into `output_ring` for all consumers to share.
pub fn build_stage_ffmpeg_args(preset: &str) -> Vec<String> {
    // Strip the internal stage-key prefix ("video:720p" → "720p")
    let encoding = preset.strip_prefix("video:").unwrap_or(preset);

    let mut args = vec![
        "-nostdin".to_string(),
        "-hide_banner".to_string(),
        "-loglevel".to_string(),
        "info".to_string(),
        "-f".to_string(),
        "mpegts".to_string(),
        "-i".to_string(),
        "pipe:0".to_string(),
        "-map".to_string(),
        "0:v:0".to_string(),
        "-map".to_string(),
        "0:a?".to_string(),
    ];

    // ── video filter (scaling) ────────────────────────────────────────────
    // Named presets. Custom profiles with explicit width/height can extend this.
    match encoding {
        "480p" => args.extend(["-vf".to_string(), "scale=854:480".to_string()]),
        "720p" => args.extend(["-vf".to_string(), "scale=1280:720".to_string()]),
        "1080p" => args.extend(["-vf".to_string(), "scale=1920:1080".to_string()]),
        _ => {} // no scale for unknown presets; FFmpeg will encode as-is
    }

    // ── video codec ───────────────────────────────────────────────────────
    let is_passthrough = matches!(encoding, "" | "source" | "custom");
    if is_passthrough {
        args.extend(["-c:v".to_string(), "copy".to_string()]);
    } else {
        args.extend([
            "-c:v".to_string(),
            "libx264".to_string(),
            "-preset".to_string(),
            "veryfast".to_string(),
        ]);
    }

    // ── audio: always copy at this stage ─────────────────────────────────
    // Audio routing (atrack:/remap:/downmix:) is handled by a downstream
    // audio-filter stage, not here.
    args.extend(["-c:a".to_string(), "copy".to_string()]);

    // ── output: MPEG-TS to stdout ─────────────────────────────────────────
    args.extend(["-f".to_string(), "mpegts".to_string(), "pipe:1".to_string()]);

    args
}

// ── Shared stage entry point ───────────────────────────────────────────────

/// Run one external transcoder stage for `(pipeline_id, encoding)`.
///
/// Spawns an `ffmpeg` subprocess with stdin/stdout piped. Two concurrent tasks
/// manage the pipe ends:
///
/// * **stdin task** (runs in the caller's task): reads `input_buffer`, muxes
///   packets to MPEG-TS, writes to FFmpeg stdin.
/// * **stdout task** (separate Tokio task): reads FFmpeg stdout, feeds a
///   `TsDemuxer`, pushes demuxed `MediaPacket`s to `output_buffer`.
///
/// The stage shuts down when `cancel` fires or when the stdin/stdout pipe
/// closes. On exit the cancel token is triggered so the engine can clean up
/// the stage entry and restart it on the next reconciler cycle.
///
/// # Sharing
///
/// This function is called by `engine.get_or_create_transcoder` which ensures
/// only one stage exists per `(pipeline, encoding)` key. All egress consumers
/// receive an `Arc<RingBuffer>` pointing to the same `output_buffer`.
pub async fn start_external_transcoder_stage(
    pipeline_id: String,
    encoding: String,
    input_buffer: Arc<RingBuffer>,
    output_buffer: Arc<RingBuffer>,
    engine: Arc<crate::media::engine::MediaEngine>,
    cancel: CancellationToken,
    // Override the video codec used in the TsMuxer PMT.
    // Required when input_buffer is a transcoded ring whose codec differs from
    // the original ingest (e.g. hevc_to_h264 output → video:720p stage).
    input_codec_override: Option<String>,
) {
    let args = build_stage_ffmpeg_args(&encoding);
    println!(
        "[ext-transcoder] stage start  pipeline={} encoding={}",
        pipeline_id, encoding
    );

    let ffmpeg_bin = std::env::var("FFMPEG_BIN_PATH").unwrap_or_else(|_| "ffmpeg".to_string());
    let mut child = match Command::new(&ffmpeg_bin)
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "[ext-transcoder] failed to spawn ffmpeg ({}:{}): {}",
                pipeline_id, encoding, e
            );
            return;
        }
    };

    let mut stdin = child.stdin.take().expect("ffmpeg stdin");
    let stdout = child.stdout.take().expect("ffmpeg stdout");
    let stderr = child.stderr.take().expect("ffmpeg stderr");

    // ── stderr logger ──────────────────────────────────────────────────────
    let label = format!("{}:{}", pipeline_id, encoding);
    {
        let label = label.clone();
        let mut stderr = stderr;
        tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            let mut all = Vec::new();
            loop {
                match stderr.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => all.extend_from_slice(&buf[..n]),
                }
            }
            if !all.is_empty() {
                eprintln!(
                    "[ext-transcoder] ffmpeg stderr ({}): {}",
                    label,
                    String::from_utf8_lossy(&all).trim()
                );
            }
        });
    }

    // ── wait for ingest metadata (video codec, audio tracks) ──────────────
    let (video_meta, audio_tracks) = loop {
        if cancel.is_cancelled() {
            let _ = stdin.shutdown().await;
            let _ = child.kill().await;
            let _ = child.wait().await;
            return;
        }
        let result = {
            let ingests = engine.active_ingests.read().await;
            ingests.get(&pipeline_id).and_then(|i| {
                let video = i.video.clone()?;
                let mut tracks = i.audio_tracks.lock().unwrap().clone();
                if tracks.is_empty()
                    && let Some(a) = i.audio.clone()
                {
                    tracks.push(a);
                }
                Some((video, tracks))
            })
        };
        if let Some(meta) = result {
            break meta;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    };

    // Apply codec override: when the input ring comes from a hevc_to_h264 stage
    // the ingest metadata says "hevc" but packets are actually H.264 Annex B.
    // The TsMuxer PMT stream_type must match the actual bitstream so FFmpeg picks
    // the right decoder (0x1B for H.264, 0x24 for H.265).
    let video_meta = if let Some(ref oc) = input_codec_override {
        let mut vm = video_meta;
        vm.codec = oc.clone();
        vm
    } else {
        video_meta
    };

    // ── stdout task: demux MPEG-TS → output_ring ───────────────────────────
    // Mark output_ring as H.264: build_stage_ffmpeg_args always uses libx264.
    output_buffer.set_codec_hint("h264");
    {
        let out_ring = output_buffer.clone();
        let cancel_out = cancel.clone();
        let label_out = label.clone();
        let mut stdout = stdout;
        tokio::spawn(async move {
            let mut demuxer = TsDemuxer::new();
            let mut buf = vec![0u8; 65536];
            loop {
                tokio::select! {
                    _ = cancel_out.cancelled() => break,
                    result = stdout.read(&mut buf) => match result {
                        Ok(0) | Err(_) => {
                            eprintln!("[ext-transcoder] stdout closed ({})", label_out);
                            break;
                        }
                        Ok(n) => {
                            demuxer.feed(&buf[..n]);
                            let mut pkts = Vec::new();
                            demuxer.drain_into(&mut pkts);
                            for pkt in pkts {
                                out_ring.push(pkt);
                            }
                        }
                    }
                }
            }
            // Signal shutdown so the engine can clean up the stage entry
            cancel_out.cancel();
        });
    }

    // ── stdin task: source_ring → TsMuxer → FFmpeg stdin ─────────────────
    // Initialise SPS/PPS cache from the stored sequence header so the first
    // MPEG-TS segment is self-contained even when the reader joins mid-stream.
    let mut sps_pps_cache: Vec<u8> = {
        let (vsh, _) = engine.get_sequence_headers(&pipeline_id).await;
        if let Some(ref flv_sh) = vsh {
            if flv_sh.len() > 5 {
                let (nls, annexb) = crate::media::codec::parse_avcc_config(&flv_sh[5..]);
                let _ = nls; // nalu_len_size will be updated below on first packet
                annexb
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        }
    };
    let mut nalu_len_size = 4usize;

    let has_video = !video_meta.codec.is_empty();
    let num_streams = (has_video as usize) + audio_tracks.len();
    let mut muxer = TsMuxer::new(Some(&video_meta), &audio_tracks);
    let mut dts_enforcer = DtsEnforcer::new(num_streams);
    let mut reader = Reader::new(
        format!("ext-stage:{}:{}", pipeline_id, encoding),
        input_buffer,
    );
    let mut video_conv_buf = Vec::<u8>::new();
    let mut audio_conv_buf = Vec::<u8>::new();

    'outer: loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = reader.wait_for_data() => {
                let mut packets = Vec::with_capacity(32);
                if reader.pull_burst(&mut packets, 32).is_err() {
                    continue;
                }
                for pkt in packets {
                    let payload: &[u8] = match pkt.media_type {
                        MediaType::Video => match video_for_ts_into(
                            &pkt.payload,
                            pkt.format,
                            &mut nalu_len_size,
                            &mut sps_pps_cache,
                            &mut video_conv_buf,
                        ) {
                            Some(p) => p,
                            None => continue,
                        },
                        MediaType::Audio => {
                            let track = audio_tracks
                                .iter()
                                .find(|a| a.track_index == pkt.track_index)
                                .or(audio_tracks.first());
                            let (sr, ch) =
                                track.map(|a| (a.sample_rate, a.channels)).unwrap_or((48000, 1));
                            match audio_for_ts_into(&pkt.payload, pkt.format, sr, ch, &mut audio_conv_buf) {
                                Some(p) => p,
                                None => continue,
                            }
                        }
                    };

                    let stream_idx = match pkt.media_type {
                        MediaType::Video => 0,
                        MediaType::Audio => {
                            let vo = has_video as usize;
                            audio_tracks
                                .iter()
                                .position(|a| a.track_index == pkt.track_index)
                                .map(|i| i + vo)
                                .unwrap_or(0)
                        }
                    };

                    let (pts, dts) = dts_enforcer.enforce(stream_idx, pkt.pts, pkt.dts);
                    let ts = muxer.mux_packet(
                        pkt.media_type,
                        pkt.track_index,
                        pts,
                        dts,
                        pkt.is_keyframe,
                        payload,
                    );
                    if !ts.is_empty() && stdin.write_all(ts).await.is_err() {
                        eprintln!(
                            "[ext-transcoder] stdin write failed ({}:{}) — ffmpeg exited",
                            pipeline_id, encoding
                        );
                        break 'outer;
                    }
                }
            }
        }
    }

    let _ = stdin.shutdown().await;
    let _ = child.kill().await;
    let _ = child.wait().await;
    cancel.cancel();

    println!(
        "[ext-transcoder] stage exit   pipeline={} encoding={}",
        pipeline_id, encoding
    );
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_args_720p_reads_stdin_writes_stdout() {
        let args = build_stage_ffmpeg_args("720p");
        // reads from stdin
        assert!(args.iter().any(|a| a == "-i"));
        let i_pos = args.iter().position(|a| a == "-i").unwrap();
        assert_eq!(args[i_pos + 1], "pipe:0");
        // scale filter
        assert!(args.iter().any(|a| a == "-vf"));
        let vf_pos = args.iter().position(|a| a == "-vf").unwrap();
        assert!(args[vf_pos + 1].contains("1280"));
        // transcode, not copy
        let cv_pos = args.iter().position(|a| a == "-c:v").unwrap();
        assert_eq!(args[cv_pos + 1], "libx264");
        // writes to stdout
        assert!(args.last() == Some(&"pipe:1".to_string()));
    }

    #[test]
    fn stage_args_source_copies_video() {
        let args = build_stage_ffmpeg_args("source");
        let cv_pos = args.iter().position(|a| a == "-c:v").unwrap();
        assert_eq!(args[cv_pos + 1], "copy");
        // no scale filter
        assert!(!args.iter().any(|a| a == "-vf"));
        assert!(args.last() == Some(&"pipe:1".to_string()));
    }

    #[test]
    fn stage_args_video_prefix_stripped() {
        // "video:720p" (internal stage-key format) must produce same args as "720p"
        let a = build_stage_ffmpeg_args("video:720p");
        let b = build_stage_ffmpeg_args("720p");
        assert_eq!(a, b);
    }

    #[test]
    fn stage_args_audio_is_always_copied() {
        for preset in &["720p", "1080p", "source"] {
            let args = build_stage_ffmpeg_args(preset);
            let ca_pos = args.iter().position(|a| a == "-c:a").unwrap();
            assert_eq!(args[ca_pos + 1], "copy", "preset={preset}");
        }
    }
}
