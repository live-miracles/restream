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
//!                                                 output_ring  ─── shared
//!                                                       │
//!                                    ┌─────────────────┼──────────────────┐
//!                                RTMP-out1          SRT-out1          HLS-out1
//! ```
//!
//! # Passthrough
//!
//! `source` encodings never enter the transcoder stage. Legacy `custom`
//! output rows also fall through as passthrough, but output create/update now
//! rejects new custom output encodings until custom FFmpeg args are applied.
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
use tracing::{debug, error, info};

use crate::domain::audio_routing::{AudioRouting, parse_audio_routing};
use crate::domain::stage::StageKey;
use crate::media::feeder::{PacketFeedConfig, TsPacketFeeder};
use crate::media::mpegts::TsDemuxer;
use crate::media::pipe_metrics::PipeMetrics;
use crate::media::ring_buffer::{MediaType, Reader, RingBuffer};
use crate::media::{engine::AudioMeta, engine::VideoMeta};

/// Stdin writes or stdout reads exceeding this threshold are counted as stalls/idles.
/// 1 ms filters normal async scheduling jitter while catching real back-pressure.
const PIPE_STALL_THRESHOLD_US: u64 = 1_000;

use crate::media::timing;

// ── FFmpeg arg builders ────────────────────────────────────────────────────

/// Build FFmpeg arguments for a **shared transcoder stage**.
///
/// Input  : MPEG-TS read from stdin (`-i -`)
/// Output : MPEG-TS written to stdout (`pipe:1`)
///
/// `input_codec` selects the video encoder: `"hevc"` / `"h265"` → `libx265`,
/// anything else → `libx264`.  Pass the ingest codec so that H.265 sources
/// transcode to H.265 output (preserving codec across the preset stage)
/// and H.264 sources transcode to H.264 output.
fn build_stage_ffmpeg_args_inner(
    preset: &str,
    input_codec: &str,
    include_audio: bool,
) -> Vec<String> {
    // Strip the internal stage-key prefix ("video:720p" → "720p").
    // Audio stages receive the selected upstream video ring, so they copy video
    // while applying any channel-level audio filter.
    let encoding = if preset.starts_with("audio:") {
        "source"
    } else {
        preset.strip_prefix("video:").unwrap_or(preset)
    };
    let audio_routing = stage_audio_routing(preset);
    let profile = if matches!(encoding, "" | "source" | "custom") {
        None
    } else {
        crate::media::profiles::cache()
            .try_read()
            .ok()
            .and_then(|cache| cache.get(encoding).or_else(|| cache.get("h264")).cloned())
            .or_else(|| {
                crate::media::profiles::built_in_defaults()
                    .get(encoding)
                    .cloned()
            })
            .or_else(|| {
                crate::media::profiles::built_in_defaults()
                    .get("h264")
                    .cloned()
            })
    };

    let mut args = vec![
        "-nostdin".to_string(),
        "-hide_banner".to_string(),
        "-nostats".to_string(),
        "-loglevel".to_string(),
        "warning".to_string(),
        "-fflags".to_string(),
        "nobuffer".to_string(),
        "-flags".to_string(),
        "low_delay".to_string(),
        "-f".to_string(),
        "mpegts".to_string(),
        "-i".to_string(),
        "pipe:0".to_string(),
    ];

    if !include_audio {
        args.extend(["-map".to_string(), "0:v:0".to_string()]);
    } else if let Some(filter) = audio_filter_complex(&audio_routing) {
        args.extend(["-filter_complex".to_string(), filter]);
        args.extend(["-map".to_string(), "0:v:0?".to_string()]);
        args.extend(["-map".to_string(), "[aout]".to_string()]);
    } else {
        args.extend(["-map".to_string(), "0:v:0".to_string()]);
        args.extend(["-map".to_string(), "0:a?".to_string()]);
    }

    // ── video filter (scaling) ────────────────────────────────────────────
    if let Some(profile) = &profile
        && profile.width > 0
        && profile.height > 0
    {
        args.extend([
            "-vf".to_string(),
            format!("scale={}:{}", profile.width, profile.height),
        ]);
    }

    // ── video codec ───────────────────────────────────────────────────────
    let is_passthrough = matches!(encoding, "" | "source" | "custom");
    if is_passthrough {
        args.extend(["-c:v".to_string(), "copy".to_string()]);
    } else {
        // Preserve codec: H.265 source → libx265 (H.265 720p out),
        //                 H.264 source → libx264 (H.264 720p out).
        let encoder = if matches!(input_codec, "hevc" | "h265") {
            "libx265"
        } else {
            "libx264"
        };
        args.extend([
            "-c:v".to_string(),
            encoder.to_string(),
            "-preset".to_string(),
            profile
                .as_ref()
                .map(|profile| profile.preset.clone())
                .unwrap_or_else(|| "veryfast".to_string()),
        ]);
        if encoder == "libx265" {
            args.extend(["-x265-params".to_string(), "log-level=none".to_string()]);
        }
        if let Some(profile) = &profile {
            if !profile.tune.is_empty() {
                args.extend(["-tune".to_string(), profile.tune.clone()]);
            }
            args.extend(["-g".to_string(), profile.gop.to_string()]);
            args.extend(["-bf".to_string(), profile.bframes.to_string()]);
            if profile.bitrate > 0 {
                args.extend(["-b:v".to_string(), profile.bitrate.to_string()]);
                if profile.max_bitrate > 0 {
                    args.extend(["-maxrate".to_string(), profile.max_bitrate.to_string()]);
                    args.extend(["-bufsize".to_string(), profile.max_bitrate.to_string()]);
                }
            } else {
                args.extend(["-crf".to_string(), profile.crf.to_string()]);
            }
        }
    }

    // ── audio ────────────────────────────────────────────────────────────
    // atrack selection stays in the zero-copy audio router. Channel-level
    // remap/downmix stages arrive here and must decode/filter/re-encode audio.
    if !include_audio {
        // Video-only stages intentionally omit audio from the live pipe so FFmpeg
        // does not stall probing a high-track-count TS input when preview only
        // needs browser-safe video.
    } else if audio_routing.is_some() {
        args.extend([
            "-c:a".to_string(),
            "aac".to_string(),
            "-b:a".to_string(),
            "160k".to_string(),
            "-ac".to_string(),
            "2".to_string(),
        ]);
    } else {
        args.extend(["-c:a".to_string(), "copy".to_string()]);
    }

    // ── output: low-latency MPEG-TS to stdout ─────────────────────────────
    args.extend([
        "-mpegts_flags".to_string(),
        "resend_headers+pat_pmt_at_frames".to_string(),
        "-pes_payload_size".to_string(),
        "0".to_string(),
        "-omit_video_pes_length".to_string(),
        "0".to_string(),
        "-flush_packets".to_string(),
        "1".to_string(),
        "-muxdelay".to_string(),
        "0".to_string(),
        "-muxpreload".to_string(),
        "0".to_string(),
        "-f".to_string(),
        "mpegts".to_string(),
        "pipe:1".to_string(),
    ]);

    args
}

pub fn build_stage_ffmpeg_args(preset: &str, input_codec: &str) -> Vec<String> {
    build_stage_ffmpeg_args_inner(preset, input_codec, true)
}

pub fn build_stage_ffmpeg_video_only_args(preset: &str, input_codec: &str) -> Vec<String> {
    build_stage_ffmpeg_args_inner(preset, input_codec, false)
}

fn stage_audio_routing(preset: &str) -> Option<AudioRouting> {
    let operation = preset
        .strip_prefix("audio:")
        .and_then(|rest| rest.rsplit_once(":from:").map(|(op, _)| op))
        .map(str::to_string);

    let routing = if let Some(operation) = operation {
        parse_audio_routing(&format!("source+{operation}"))
    } else {
        parse_audio_routing(preset)
    };

    match routing {
        AudioRouting::Remap { .. } | AudioRouting::Downmix(_) => Some(routing),
        _ => None,
    }
}

fn audio_filter_complex(routing: &Option<AudioRouting>) -> Option<String> {
    match routing {
        Some(AudioRouting::Remap { left, right, track }) => Some(format!(
            "[0:a:{track}]pan=stereo|c0=c{left}|c1=c{right}[aout]"
        )),
        Some(AudioRouting::Downmix(track)) => {
            Some(format!("[0:a:{track}]aresample=out_chlayout=stereo[aout]"))
        }
        _ => None,
    }
}

async fn wait_for_stage_metadata(
    engine: &Arc<crate::media::engine::MediaEngine>,
    pipeline_id: &str,
    source_buffer: &Arc<RingBuffer>,
    include_audio: bool,
    input_codec_override: Option<&str>,
    cancel: &CancellationToken,
) -> Option<(VideoMeta, std::sync::Arc<Vec<AudioMeta>>)> {
    loop {
        if cancel.is_cancelled() {
            return None;
        }

        let ingest_result = {
            let ingests = engine.ingests.active.read().await;
            ingests.get(pipeline_id).and_then(|ingest| {
                let mut video = ingest.video.clone()?;
                if let Some(codec) = input_codec_override {
                    video.codec = codec.to_string();
                } else {
                    let hint = source_buffer.codec_hint_str();
                    if !hint.is_empty() {
                        video.codec = hint.to_string();
                    }
                }

                let audio_tracks = if include_audio {
                    source_buffer
                        .audio_tracks()
                        .filter(|tracks| !tracks.is_empty())
                        .map(|tracks| std::sync::Arc::new(tracks.to_vec()))
                        .unwrap_or_else(|| {
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
                        })
                } else {
                    std::sync::Arc::new(Vec::new())
                };

                Some((video, audio_tracks))
            })
        };

        if let Some(meta) = ingest_result {
            return Some(meta);
        }

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
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
#[allow(clippy::too_many_arguments)]
pub async fn start_external_transcoder_stage(
    pipeline_id: String,
    encoding: String,
    input_buffer: Arc<RingBuffer>,
    output_buffer: Arc<RingBuffer>,
    engine: Arc<crate::media::engine::MediaEngine>,
    cancel: CancellationToken,
    // Override the video codec used in the TsMuxer PMT, and selects the
    // encoder in build_stage_ffmpeg_args.  Pass "hevc" when the source ring
    // carries H.265 so the stage spawns libx265 and tags its output ring
    // correctly.  None defaults to H.264 (libx264).
    input_codec_override: Option<String>,
    stage_key: StageKey,
) {
    start_external_transcoder_stage_inner(
        pipeline_id,
        encoding,
        input_buffer,
        output_buffer,
        engine,
        cancel,
        input_codec_override,
        stage_key,
        true,
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
pub async fn start_external_transcoder_video_only_stage(
    pipeline_id: String,
    encoding: String,
    input_buffer: Arc<RingBuffer>,
    output_buffer: Arc<RingBuffer>,
    engine: Arc<crate::media::engine::MediaEngine>,
    cancel: CancellationToken,
    input_codec_override: Option<String>,
    stage_key: StageKey,
) {
    start_external_transcoder_stage_inner(
        pipeline_id,
        encoding,
        input_buffer,
        output_buffer,
        engine,
        cancel,
        input_codec_override,
        stage_key,
        false,
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
async fn start_external_transcoder_stage_inner(
    pipeline_id: String,
    encoding: String,
    input_buffer: Arc<RingBuffer>,
    output_buffer: Arc<RingBuffer>,
    engine: Arc<crate::media::engine::MediaEngine>,
    cancel: CancellationToken,
    input_codec_override: Option<String>,
    stage_key: StageKey,
    include_audio: bool,
) {
    let input_codec = input_codec_override.as_deref().unwrap_or("h264");
    let args = if include_audio {
        build_stage_ffmpeg_args(&encoding, input_codec)
    } else {
        build_stage_ffmpeg_video_only_args(&encoding, input_codec)
    };
    let correlation_id = crate::logging::next_correlation_id("stage");
    info!(
        correlation_id = %correlation_id,
        pipeline_id = %pipeline_id,
        stage_encoding = %encoding,
        stage_backend = "external_ffmpeg",
        include_audio,
        "[ext-transcoder] stage start  pipeline={} encoding={}",
        pipeline_id,
        encoding
    );

    let ffmpeg_bin = crate::ffmpeg_extract::ffmpeg_bin_path();
    let mut child = match Command::new(ffmpeg_bin)
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            error!(
                correlation_id = %correlation_id,
                pipeline_id = %pipeline_id,
                stage_encoding = %encoding,
                stage_backend = "external_ffmpeg",
                err = %e,
                "[ext-transcoder] failed to spawn ffmpeg ({}:{}): {}",
                pipeline_id,
                encoding,
                e
            );
            engine
                .runtime
                .event_log
                .emit(crate::events::EventKind::StageStopped {
                    pipeline_id: pipeline_id.clone(),
                    encoding: encoding.clone(),
                });
            return;
        }
    };

    // .take() returns None if the child exited between spawn() and here (rare but possible).
    // Use pattern matching rather than .expect() to avoid a panic in that race.
    let mut stdin = match child.stdin.take() {
        Some(s) => s,
        None => {
            error!(
                correlation_id = %correlation_id,
                pipeline_id = %pipeline_id,
                stage_encoding = %encoding,
                stage_backend = "external_ffmpeg",
                "[ext-transcoder] ffmpeg stdin unavailable ({}:{})",
                pipeline_id,
                encoding
            );
            let _ = child.kill().await;
            let _ = child.wait().await;
            cancel.cancel();
            engine
                .runtime
                .event_log
                .emit(crate::events::EventKind::StageStopped {
                    pipeline_id: pipeline_id.clone(),
                    encoding: encoding.clone(),
                });
            return;
        }
    };
    let stdout = match child.stdout.take() {
        Some(s) => s,
        None => {
            error!(
                correlation_id = %correlation_id,
                pipeline_id = %pipeline_id,
                stage_encoding = %encoding,
                stage_backend = "external_ffmpeg",
                "[ext-transcoder] ffmpeg stdout unavailable ({}:{})",
                pipeline_id,
                encoding
            );
            let _ = child.kill().await;
            let _ = child.wait().await;
            cancel.cancel();
            engine
                .runtime
                .event_log
                .emit(crate::events::EventKind::StageStopped {
                    pipeline_id: pipeline_id.clone(),
                    encoding: encoding.clone(),
                });
            return;
        }
    };
    let stderr = match child.stderr.take() {
        Some(s) => s,
        None => {
            error!(
                correlation_id = %correlation_id,
                pipeline_id = %pipeline_id,
                stage_encoding = %encoding,
                stage_backend = "external_ffmpeg",
                "[ext-transcoder] ffmpeg stderr unavailable ({}:{})",
                pipeline_id,
                encoding
            );
            let _ = child.kill().await;
            let _ = child.wait().await;
            cancel.cancel();
            engine
                .runtime
                .event_log
                .emit(crate::events::EventKind::StageStopped {
                    pipeline_id: pipeline_id.clone(),
                    encoding: encoding.clone(),
                });
            return;
        }
    };

    // ── stage metrics ─────────────────────────────────────────────────────
    let stage_metrics = engine.get_or_create_stage_metrics(stage_key.clone()).await;

    // ── pipe metrics ──────────────────────────────────────────────────────
    // Separate from stage_metrics: only subprocess-pipe stages have these.
    // Trigger TSC calibration eagerly (200 µs busy-wait, once per process).
    // Logs which path was chosen so operators can see it in the stage output.
    if !timing::calibrate() {
        info!(
            correlation_id = %correlation_id,
            pipeline_id = %pipeline_id,
            stage_encoding = %encoding,
            stage_backend = "external_ffmpeg",
            "[ext-transcoder] pipe timing: Instant fallback \
             (invariant TSC absent or calibration out of bounds)"
        );
    }
    let timing_clock = timing::clock();
    let pipe_metrics = Arc::new(PipeMetrics::default());
    engine
        .register_pipe_metrics(stage_key.clone(), pipe_metrics.clone())
        .await;

    // ── stderr logger ──────────────────────────────────────────────────────
    // Stream stderr line-by-line so progress lines are visible immediately.
    // Cap accumulation at 1 MB to avoid unbounded memory growth at
    // ~17 MB/hour (60fps × ~80 bytes/line of libx264 progress output).
    // Excess bytes are discarded; a truncation note is prepended on exit.
    const STDERR_CAP: usize = 1 << 20; // 1 MB
    let label = format!("{}:{}", pipeline_id, encoding);
    {
        let label = label.clone();
        let stderr_correlation_id = correlation_id.clone();
        let stderr_pipeline_id = pipeline_id.clone();
        let stderr_encoding = encoding.clone();
        let mut stderr = stderr;
        tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            let mut all: Vec<u8> = Vec::new();
            let mut truncated = false;
            loop {
                match stderr.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let chunk = &buf[..n];
                        let remaining = STDERR_CAP.saturating_sub(all.len());
                        if remaining > 0 {
                            all.extend_from_slice(&chunk[..n.min(remaining)]);
                        } else if !truncated {
                            truncated = true;
                            error!(
                                correlation_id = %stderr_correlation_id,
                                pipeline_id = %stderr_pipeline_id,
                                stage_encoding = %stderr_encoding,
                                stage_backend = "external_ffmpeg",
                                "[ext-transcoder] ffmpeg stderr ({}) truncated at 1 MB — \
                                 further output discarded",
                                label
                            );
                        }
                    }
                }
            }
            if !all.is_empty() {
                error!(
                    correlation_id = %stderr_correlation_id,
                    pipeline_id = %stderr_pipeline_id,
                    stage_encoding = %stderr_encoding,
                    stage_backend = "external_ffmpeg",
                    "[ext-transcoder] ffmpeg stderr ({}): {}",
                    label,
                    String::from_utf8_lossy(&all).trim()
                );
            }
        });
    }

    // ── wait for ingest metadata (video codec, audio tracks) ──────────────
    let Some((video_meta, audio_tracks)) = wait_for_stage_metadata(
        &engine,
        &pipeline_id,
        &input_buffer,
        include_audio,
        input_codec_override.as_deref(),
        &cancel,
    )
    .await
    else {
        let _ = stdin.shutdown().await;
        let _ = child.kill().await;
        let _ = child.wait().await;
        engine.remove_stage_metrics(&stage_key).await;
        engine.remove_pipe_metrics(&stage_key).await;
        engine
            .runtime
            .event_log
            .emit(crate::events::EventKind::StageStopped {
                pipeline_id: pipeline_id.clone(),
                encoding: encoding.clone(),
            });
        return;
    };

    // ── stdout task: demux MPEG-TS → output_ring ───────────────────────────
    // Codec hint is set synchronously by get_or_create_transcoder before this
    // task is spawned (OnceLock — set_codec_hint below is a no-op).
    // Keep it here as a defensive fallback in case the stage is ever called
    // outside the engine (e.g., tests).
    let output_codec = if matches!(input_codec, "hevc" | "h265") {
        "hevc"
    } else {
        "h264"
    };
    output_buffer.set_codec_hint(output_codec);
    if !include_audio {
        output_buffer.set_audio_tracks(Vec::new());
    }
    {
        let out_ring = output_buffer.clone();
        let cancel_out = cancel.clone();
        let label_out = label.clone();
        let out_stage_metrics = stage_metrics.clone();
        let out_pipe_metrics = pipe_metrics.clone();
        let out_timing_clock = timing_clock;
        let mut stdout = stdout;
        tokio::spawn(async move {
            let mut demuxer = TsDemuxer::new();
            let mut buf = vec![0u8; 65536];
            let mut pkts = Vec::with_capacity(32);
            loop {
                let t0 = out_timing_clock.now();
                let result = stdout.read(&mut buf).await;
                let idle_us = out_timing_clock.delta_us(t0);
                match result {
                    Ok(0) | Err(_) => {
                        debug!("stdout closed ({})", label_out);
                        break;
                    }
                    Ok(n) => {
                        if idle_us > PIPE_STALL_THRESHOLD_US {
                            out_pipe_metrics.record_idle(idle_us);
                        }
                        demuxer.feed(&buf[..n]);
                        demuxer.drain_into(&mut pkts);
                        for pkt in &pkts {
                            out_stage_metrics.record_out(pkt.payload.len() as u64);
                        }
                        out_ring.push_batch(pkts.drain(..));
                    }
                }
            }
            // Signal shutdown so the engine can clean up the stage entry
            cancel_out.cancel();
        });
    }

    // ── stdin task: source_ring → TsPacketFeeder → FFmpeg stdin ───────────
    let (video_sequence_header, _) = engine.get_sequence_headers(&pipeline_id).await;
    let video_meta_for_feeder = (!video_meta.codec.is_empty()).then_some(&video_meta);
    let mut feeder = TsPacketFeeder::new(
        video_meta_for_feeder,
        audio_tracks.clone(),
        PacketFeedConfig {
            video_sequence_header: video_sequence_header.as_ref().map(|v| v.to_vec()),
            ..PacketFeedConfig::default()
        },
    );
    let mut reader = Reader::new(
        format!("ext-stage:{}:{}", pipeline_id, encoding),
        input_buffer,
    );
    let mut ts_batch = Vec::<u8>::with_capacity(16 * 188);
    let mut packets = Vec::with_capacity(32);

    'outer: loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = reader.wait_for_data() => {
                packets.clear();
                if reader.pull_burst(&mut packets, 32).is_err() {
                    continue;
                }
                for pkt in packets.drain(..) {
                    if !include_audio && pkt.media_type == MediaType::Audio {
                        continue;
                    }
                    let in_bytes = pkt.payload.len() as u64;
                    ts_batch.clear();
                    if feeder.extend_ts_for_packet(&pkt, &mut ts_batch) {
                        let t0 = timing_clock.now();
                        if stdin.write_all(&ts_batch).await.is_err() {
                            error!(
                                correlation_id = %correlation_id,
                                pipeline_id = %pipeline_id,
                                stage_encoding = %encoding,
                                stage_backend = "external_ffmpeg",
                                "[ext-transcoder] stdin write failed ({}:{}) — ffmpeg exited",
                                pipeline_id,
                                encoding
                            );
                            break 'outer;
                        }
                        let write_us = timing_clock.delta_us(t0);
                        if write_us > PIPE_STALL_THRESHOLD_US {
                            pipe_metrics.record_stall(write_us);
                        }
                        stage_metrics.record_in(in_bytes);
                    }
                }
            }
        }
    }

    let _ = stdin.shutdown().await;
    let _ = child.kill().await;
    let _ = child.wait().await;
    cancel.cancel();

    engine.remove_stage_metrics(&stage_key).await;
    engine.remove_pipe_metrics(&stage_key).await;
    engine
        .runtime
        .event_log
        .emit(crate::events::EventKind::StageStopped {
            pipeline_id: pipeline_id.clone(),
            encoding: encoding.clone(),
        });

    info!(
        correlation_id = %correlation_id,
        pipeline_id = %pipeline_id,
        stage_encoding = %encoding,
        stage_backend = "external_ffmpeg",
        "[ext-transcoder] stage exit   pipeline={} encoding={}",
        pipeline_id,
        encoding
    );
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::stage::{StageKey, StageKind};
    use crate::media::engine::MediaEngine;
    use crate::media::feeder::{PacketFeedConfig, TsPacketFeeder};
    use crate::media::mpegts::TsDemuxer;
    use crate::media::ring_buffer::{MediaType, Reader};
    use tokio_util::sync::CancellationToken;

    fn extract_2v16a_hevc_ts_sample_for_duration(seconds: u32) -> Vec<u8> {
        let ffmpeg = crate::ffmpeg_extract::ensure_ffmpeg_extracted();
        let fixture = crate::test_fixtures::checked_in_fixture("media/colorbar-timer-2v16a.mp4")
            .expect("2v16a fixture should exist");
        let output = std::process::Command::new(ffmpeg)
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
                &seconds.to_string(),
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

    fn extract_2v16a_hevc_ts_sample() -> Vec<u8> {
        extract_2v16a_hevc_ts_sample_for_duration(1)
    }

    fn write_temp_ts_artifact(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "restream-external-transcoder-test-{}-{}",
            std::process::id(),
            name
        ));
        std::fs::create_dir_all(&dir).expect("create temp artifact dir");
        let path = dir.join("artifact.ts");
        std::fs::write(&path, bytes).expect("write temp TS artifact");
        path
    }

    #[test]
    fn stage_args_720p_reads_stdin_writes_stdout() {
        let args = build_stage_ffmpeg_args("720p", "h264");
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
        assert!(args.windows(2).any(|w| w == ["-flush_packets", "1"]));
        assert!(args.windows(2).any(|w| w == ["-muxdelay", "0"]));
        assert!(args.windows(2).any(|w| w == ["-muxpreload", "0"]));
        assert!(args.windows(2).any(|w| w == ["-pes_payload_size", "0"]));
        // writes to stdout
        assert!(args.last() == Some(&"pipe:1".to_string()));
    }

    #[test]
    fn stage_args_720p_hevc_uses_libx265() {
        for codec in &["hevc", "h265"] {
            let args = build_stage_ffmpeg_args("720p", codec);
            let cv_pos = args.iter().position(|a| a == "-c:v").unwrap();
            assert_eq!(args[cv_pos + 1], "libx265", "codec={codec}");
            assert!(args.last() == Some(&"pipe:1".to_string()));
        }
    }

    #[test]
    fn stage_args_custom_profile_uses_profile_settings() {
        {
            let mut cache = crate::media::profiles::cache().blocking_write();
            cache.insert(
                "square_test".to_string(),
                crate::domain::transcode_profile::TranscodeProfile {
                    preset: "superfast".to_string(),
                    tune: "zerolatency".to_string(),
                    crf: 21,
                    gop: 100,
                    bframes: 1,
                    bitrate: 1500000,
                    max_bitrate: 2000000,
                    width: 640,
                    height: 640,
                },
            );
        }

        let args = build_stage_ffmpeg_args("square_test", "h264");
        assert!(args.windows(2).any(|w| w == ["-vf", "scale=640:640"]));
        assert!(args.windows(2).any(|w| w == ["-preset", "superfast"]));
        assert!(args.windows(2).any(|w| w == ["-g", "100"]));
        assert!(args.windows(2).any(|w| w == ["-bf", "1"]));
        assert!(args.windows(2).any(|w| w == ["-b:v", "1500000"]));
        assert!(args.windows(2).any(|w| w == ["-maxrate", "2000000"]));
        assert!(!args.iter().any(|arg| arg == "-crf"));
    }

    #[test]
    fn stage_args_source_copies_video() {
        let args = build_stage_ffmpeg_args("source", "h264");
        let cv_pos = args.iter().position(|a| a == "-c:v").unwrap();
        assert_eq!(args[cv_pos + 1], "copy");
        // no scale filter
        assert!(!args.iter().any(|a| a == "-vf"));
        assert!(args.last() == Some(&"pipe:1".to_string()));
    }

    #[test]
    fn stage_args_h264_transcodes_without_scaling() {
        let args = build_stage_ffmpeg_args("h264", "h264");
        let cv_pos = args.iter().position(|a| a == "-c:v").unwrap();
        assert_eq!(args[cv_pos + 1], "libx264");
        assert!(!args.iter().any(|a| a == "-vf"));
    }

    #[test]
    fn stage_args_video_prefix_stripped() {
        // "video:720p" (internal stage-key format) must produce same args as "720p"
        let a = build_stage_ffmpeg_args("video:720p", "h264");
        let b = build_stage_ffmpeg_args("720p", "h264");
        assert_eq!(a, b);
    }

    #[test]
    fn stage_args_non_dsp_audio_is_copied() {
        for preset in &["720p", "1080p", "source"] {
            let args = build_stage_ffmpeg_args(preset, "h264");
            let ca_pos = args.iter().position(|a| a == "-c:a").unwrap();
            assert_eq!(args[ca_pos + 1], "copy", "preset={preset}");
        }
    }

    #[test]
    fn stage_args_remap_uses_pan_filter_and_audio_encode() {
        let args = build_stage_ffmpeg_args("audio:remap:1:0:2:from:720p", "h264");

        let filter_pos = args.iter().position(|a| a == "-filter_complex").unwrap();
        assert_eq!(args[filter_pos + 1], "[0:a:2]pan=stereo|c0=c1|c1=c0[aout]");
        assert!(args.windows(2).any(|w| w == ["-map", "0:v:0?"]));
        assert!(args.windows(2).any(|w| w == ["-map", "[aout]"]));
        let ca_pos = args.iter().position(|a| a == "-c:a").unwrap();
        assert_eq!(args[ca_pos + 1], "aac");
        assert!(args.windows(2).any(|w| w == ["-ac", "2"]));
        let cv_pos = args.iter().position(|a| a == "-c:v").unwrap();
        assert_eq!(args[cv_pos + 1], "copy");
    }

    #[test]
    fn stage_args_downmix_uses_stereo_resample_filter() {
        let args = build_stage_ffmpeg_args("audio:downmix:1:from:source", "h264");

        let filter_pos = args.iter().position(|a| a == "-filter_complex").unwrap();
        assert_eq!(
            args[filter_pos + 1],
            "[0:a:1]aresample=out_chlayout=stereo[aout]"
        );
        let ca_pos = args.iter().position(|a| a == "-c:a").unwrap();
        assert_eq!(args[ca_pos + 1], "aac");
        let cv_pos = args.iter().position(|a| a == "-c:v").unwrap();
        assert_eq!(args[cv_pos + 1], "copy");
    }

    #[test]
    fn stage_args_atrack_stays_packet_copy() {
        let args = build_stage_ffmpeg_args("audio:atrack:0:from:720p", "h264");

        assert!(!args.iter().any(|a| a == "-filter_complex"));
        let ca_pos = args.iter().position(|a| a == "-c:a").unwrap();
        assert_eq!(args[ca_pos + 1], "copy");
        let cv_pos = args.iter().position(|a| a == "-c:v").unwrap();
        assert_eq!(args[cv_pos + 1], "copy");
    }

    #[test]
    fn stage_args_empty_preset_copies_video_and_audio() {
        let args = build_stage_ffmpeg_args("", "h264");
        let cv_pos = args.iter().position(|a| a == "-c:v").unwrap();
        assert_eq!(args[cv_pos + 1], "copy");
        let ca_pos = args.iter().position(|a| a == "-c:a").unwrap();
        assert_eq!(args[ca_pos + 1], "copy");
    }

    #[test]
    fn stage_args_custom_preset_copies_video_and_audio() {
        let args = build_stage_ffmpeg_args("custom", "h264");
        let cv_pos = args.iter().position(|a| a == "-c:v").unwrap();
        assert_eq!(args[cv_pos + 1], "copy");
        let ca_pos = args.iter().position(|a| a == "-c:a").unwrap();
        assert_eq!(args[ca_pos + 1], "copy");
    }

    #[test]
    fn stage_audio_routing_remap_is_some() {
        let r = stage_audio_routing("audio:remap:0:1:0:from:source");
        assert!(r.is_some());
        assert!(matches!(r, Some(AudioRouting::Remap { .. })));
    }

    #[test]
    fn stage_audio_routing_downmix_is_some() {
        let r = stage_audio_routing("audio:downmix:0:from:source");
        assert!(r.is_some());
        assert!(matches!(r, Some(AudioRouting::Downmix(_))));
    }

    #[test]
    fn stage_audio_routing_atrack_returns_none() {
        let r = stage_audio_routing("audio:atrack:0:from:720p");
        assert!(r.is_none());
    }

    #[test]
    fn stage_audio_routing_video_preset_returns_none() {
        assert!(stage_audio_routing("720p").is_none());
        assert!(stage_audio_routing("source").is_none());
    }

    #[tokio::test]
    async fn stage_metadata_prefers_upstream_ring_tracks_and_codec_hint() {
        let engine = Arc::new(MediaEngine::new());
        engine
            .try_register_ingest("pipe-stage-meta", "stream-key", "srt")
            .await
            .unwrap();

        let ingest_audio = vec![
            crate::media::engine::AudioMeta {
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
            crate::media::engine::AudioMeta {
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
                "pipe-stage-meta",
                Some(crate::media::engine::VideoMeta {
                    codec: "hevc".to_string(),
                    width: 1920,
                    height: 1080,
                    fps: 30.0,
                    bw: None,
                    pid: Some(0x100),
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
            .update_ingest_audio_tracks("pipe-stage-meta", ingest_audio)
            .await;

        let upstream_ring = Arc::new(RingBuffer::new(1024));
        upstream_ring.set_codec_hint("h264");
        upstream_ring.set_audio_tracks(vec![crate::media::engine::AudioMeta {
            codec: "aac".to_string(),
            sample_rate: 48000,
            channels: 2,
            channel_layout: None,
            track_index: 0,
            pid: Some(0x101),
            language: None,
            title: None,
            profile: None,
        }]);

        let cancel = CancellationToken::new();
        let (video, audio_tracks) = wait_for_stage_metadata(
            &engine,
            "pipe-stage-meta",
            &upstream_ring,
            true,
            None,
            &cancel,
        )
        .await
        .expect("stage metadata");

        assert_eq!(video.codec, "h264");
        assert_eq!(audio_tracks.len(), 1);
        assert_eq!(audio_tracks[0].track_index, 0);
        assert_eq!(audio_tracks[0].pid, Some(0x101));
    }

    #[test]
    fn audio_filter_complex_remap_format() {
        let routing = Some(AudioRouting::Remap {
            left: 1,
            right: 0,
            track: 2,
        });
        let filter = audio_filter_complex(&routing).unwrap();
        assert_eq!(filter, "[0:a:2]pan=stereo|c0=c1|c1=c0[aout]");
    }

    #[test]
    fn audio_filter_complex_downmix_format() {
        let routing = Some(AudioRouting::Downmix(1));
        let filter = audio_filter_complex(&routing).unwrap();
        assert_eq!(filter, "[0:a:1]aresample=out_chlayout=stereo[aout]");
    }

    #[test]
    fn audio_filter_complex_none_for_no_routing() {
        assert!(audio_filter_complex(&None).is_none());
    }

    #[test]
    fn stage_args_profile_with_crf_when_bitrate_zero() {
        {
            let mut cache = crate::media::profiles::cache().blocking_write();
            cache.insert(
                "crf_test".to_string(),
                crate::domain::transcode_profile::TranscodeProfile {
                    preset: "veryfast".to_string(),
                    tune: String::new(),
                    crf: 28,
                    gop: 60,
                    bframes: 0,
                    bitrate: 0,
                    max_bitrate: 0,
                    width: 1280,
                    height: 720,
                },
            );
        }
        let args = build_stage_ffmpeg_args("crf_test", "h264");
        assert!(args.windows(2).any(|w| w == ["-crf", "28"]));
        assert!(!args.iter().any(|a| a == "-b:v"));
        assert!(!args.iter().any(|a| a == "-maxrate"));
    }

    #[test]
    fn stage_args_audio_stage_strips_prefix_and_copies_video() {
        // audio:atrack:0:from:720p → treated as "source" for video (copy)
        let args = build_stage_ffmpeg_args("audio:atrack:0:from:720p", "h264");
        let cv_pos = args.iter().position(|a| a == "-c:v").unwrap();
        assert_eq!(args[cv_pos + 1], "copy");
        // no scale filter
        assert!(!args.iter().any(|a| a == "-vf"));
    }

    // H4: verify that kill() + wait() on a child that has no stdin piped
    // completes without hanging or panicking. This is the exact pattern used
    // in the early-return error paths added by the H4 fix.
    #[tokio::test]
    async fn kill_and_wait_on_child_without_piped_stdin_does_not_hang() {
        // Spawn a process without piping stdin (simulates the race where
        // child.stdin.take() returns None after spawn).
        let mut child = Command::new("true") // exits immediately with code 0
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("failed to spawn 'true'");

        // stdin.take() returns None here (not piped) — the scenario the fix handles.
        assert!(child.stdin.take().is_none());

        // The fix: kill (no-op if already exited) then wait (reaps the child).
        // Must complete without blocking.
        let _ = child.kill().await;
        let status = child.wait().await.expect("wait must not fail");
        // 'true' exits 0; kill() on an already-exited process may produce a
        // non-zero status on some platforms — just assert we didn't hang.
        let _ = status;
    }

    #[tokio::test]
    #[ignore = "HEVC browser preview no longer transcodes directly from the 4K source ring"]
    async fn external_720p_stage_emits_live_packets_for_hevc_sample() {
        let ts_sample = extract_2v16a_hevc_ts_sample();
        let mut demuxer = TsDemuxer::new();
        let mut packets = Vec::new();
        for chunk in ts_sample.chunks(1316) {
            demuxer.feed(chunk);
            demuxer.drain_into(&mut packets);
        }
        demuxer.flush();
        demuxer.drain_into(&mut packets);

        let probe = demuxer.take_probe().expect("probe 2v16a HEVC sample");
        let video = probe.video.expect("sample should contain video");
        let audio_tracks = probe.audio_tracks;

        let engine = Arc::new(MediaEngine::new());
        engine
            .try_register_ingest("pipe-ext-preview", "stream-key", "srt")
            .await
            .unwrap();
        engine
            .update_ingest_meta(
                "pipe-ext-preview",
                Some(video),
                audio_tracks.first().cloned(),
                None,
            )
            .await;
        engine
            .update_ingest_audio_tracks("pipe-ext-preview", audio_tracks.clone())
            .await;

        let source_ring = Arc::new(RingBuffer::new(16_384));
        source_ring.set_codec_hint("hevc");
        source_ring.set_audio_tracks(audio_tracks);
        let output_ring = Arc::new(RingBuffer::new(16_384));
        let mut reader = Reader::new_live("test_ext_720p_output".to_string(), output_ring.clone());
        let cancel = CancellationToken::new();
        let stage_key = StageKey::new(
            "pipe-ext-preview",
            StageKind::codec_edge("hevc_to_h264", StageKind::source()),
        );

        tokio::spawn(start_external_transcoder_stage(
            "pipe-ext-preview".to_string(),
            "720p".to_string(),
            source_ring.clone(),
            output_ring,
            engine,
            cancel.clone(),
            None,
            stage_key,
        ));

        let ready_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if source_ring
                .reader_snapshots()
                .iter()
                .any(|snapshot| snapshot.name.contains("ext-stage:pipe-ext-preview:720p"))
            {
                break;
            }
            assert!(
                tokio::time::Instant::now() < ready_deadline,
                "external 720p stage reader did not attach to the source ring in time"
            );
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        source_ring.push_batch(packets.drain(..));
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;

        let mut output_packets = Vec::new();
        while let Ok(Some(packet)) = reader.pull() {
            output_packets.push(packet);
        }

        cancel.cancel();

        assert!(
            output_packets
                .iter()
                .any(|packet| packet.media_type == MediaType::Video),
            "external 720p HEVC preview stage should emit live video packets"
        );
        assert!(
            output_packets
                .iter()
                .any(|packet| packet.media_type == MediaType::Video && packet.is_keyframe),
            "external 720p HEVC preview stage should emit a live keyframe"
        );
    }

    #[tokio::test]
    #[ignore = "diagnostic: current live HEVC + 16 audio preview stage still stalls without EOF"]
    async fn chained_hevc_preview_stages_emit_live_h264_packets() {
        let ts_sample = extract_2v16a_hevc_ts_sample();
        let mut demuxer = TsDemuxer::new();
        let mut packets = Vec::new();
        for chunk in ts_sample.chunks(1316) {
            demuxer.feed(chunk);
            demuxer.drain_into(&mut packets);
        }
        demuxer.flush();
        demuxer.drain_into(&mut packets);

        let probe = demuxer.take_probe().expect("probe 2v16a HEVC sample");
        let video = probe.video.expect("sample should contain video");
        let audio_tracks = probe.audio_tracks;

        let engine = Arc::new(MediaEngine::new());
        engine
            .try_register_ingest("pipe-ext-preview-chain", "stream-key", "srt")
            .await
            .unwrap();
        engine
            .update_ingest_meta(
                "pipe-ext-preview-chain",
                Some(video),
                audio_tracks.first().cloned(),
                None,
            )
            .await;
        engine
            .update_ingest_audio_tracks("pipe-ext-preview-chain", audio_tracks.clone())
            .await;

        let source_ring = engine
            .get_or_create_pipeline("pipe-ext-preview-chain")
            .await;
        source_ring.set_codec_hint("hevc");
        source_ring.set_audio_tracks(audio_tracks);

        let hevc_preview_upstream = engine
            .get_or_create_transcoder(
                "pipe-ext-preview-chain",
                StageKind::video_preset("1080p"),
                source_ring.clone(),
                Some("hevc"),
            )
            .await;
        let h264_preview_ring = engine
            .get_or_create_h264_transcoder(
                "pipe-ext-preview-chain",
                StageKind::video_preset("1080p"),
                hevc_preview_upstream.clone(),
            )
            .await;
        let mut hevc_reader = Reader::new_live(
            "test_ext_preview_chain_mid".to_string(),
            hevc_preview_upstream.clone(),
        );
        let mut reader = Reader::new_live(
            "test_ext_preview_chain_output".to_string(),
            h264_preview_ring.clone(),
        );

        let ready_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            let source_attached = source_ring.reader_snapshots().iter().any(|snapshot| {
                snapshot
                    .name
                    .contains("ext-stage:pipe-ext-preview-chain:video:1080p")
            });
            let chained_attached =
                hevc_preview_upstream
                    .reader_snapshots()
                    .iter()
                    .any(|snapshot| {
                        snapshot
                            .name
                            .contains("ext-stage:pipe-ext-preview-chain:720p")
                    });
            if source_attached && chained_attached {
                break;
            }
            assert!(
                tokio::time::Instant::now() < ready_deadline,
                "preview chain readers did not both attach in time"
            );
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }

        source_ring.push_batch(packets.drain(..));
        let output_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(12);
        let mut hevc_packets = Vec::new();
        let mut output_packets = Vec::new();
        loop {
            while let Ok(Some(packet)) = hevc_reader.pull() {
                hevc_packets.push(packet);
            }
            while let Ok(Some(packet)) = reader.pull() {
                output_packets.push(packet);
            }
            if output_packets
                .iter()
                .any(|packet| packet.media_type == MediaType::Video)
            {
                break;
            }
            if tokio::time::Instant::now() >= output_deadline {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }

        assert_eq!(
            h264_preview_ring.codec_hint_str(),
            "h264",
            "preview codec-edge ring should advertise H.264 output"
        );
        assert!(
            hevc_packets
                .iter()
                .any(|packet| packet.media_type == MediaType::Video),
            "preview chain should first emit live HEVC packets from the 1080p stage"
        );
        assert!(
            output_packets
                .iter()
                .any(|packet| packet.media_type == MediaType::Video),
            "chained HEVC preview stages should emit live video packets"
        );
        assert!(
            output_packets
                .iter()
                .any(|packet| packet.media_type == MediaType::Video && packet.is_keyframe),
            "chained HEVC preview stages should emit a live keyframe"
        );
    }

    #[tokio::test]
    async fn external_720p_stage_emits_live_packets_for_h264_marker_fixture() {
        let path = crate::test_fixtures::av_marker_transport_fixture("h264", false)
            .expect("H.264 marker fixture");
        let file_bytes = std::fs::read(&path).expect("read H.264 marker fixture");
        let mut demuxer = TsDemuxer::new();
        let mut packets = Vec::new();
        for chunk in file_bytes.chunks(1316) {
            demuxer.feed(chunk);
            demuxer.drain_into(&mut packets);
        }
        demuxer.flush();
        demuxer.drain_into(&mut packets);

        let probe = demuxer.take_probe().expect("probe H.264 marker fixture");
        let video = probe.video.expect("marker fixture should contain video");
        let audio_tracks = probe.audio_tracks;

        let engine = Arc::new(MediaEngine::new());
        engine
            .try_register_ingest("pipe-ext-h264-marker", "stream-key", "file")
            .await
            .unwrap();
        engine
            .update_ingest_meta(
                "pipe-ext-h264-marker",
                Some(video),
                audio_tracks.first().cloned(),
                None,
            )
            .await;
        engine
            .update_ingest_audio_tracks("pipe-ext-h264-marker", audio_tracks.clone())
            .await;

        let source_ring = Arc::new(RingBuffer::new(16_384));
        source_ring.set_codec_hint("h264");
        source_ring.set_audio_tracks(audio_tracks);
        let output_ring = Arc::new(RingBuffer::new(16_384));
        let mut reader = Reader::new_live(
            "test_ext_h264_marker_output".to_string(),
            output_ring.clone(),
        );
        let cancel = CancellationToken::new();
        let stage_key = StageKey::new("pipe-ext-h264-marker", StageKind::video_preset("720p"));

        tokio::spawn(start_external_transcoder_stage(
            "pipe-ext-h264-marker".to_string(),
            "video:720p".to_string(),
            source_ring.clone(),
            output_ring,
            engine,
            cancel.clone(),
            None,
            stage_key,
        ));

        let ready_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if source_ring.reader_snapshots().iter().any(|snapshot| {
                snapshot
                    .name
                    .contains("ext-stage:pipe-ext-h264-marker:video:720p")
            }) {
                break;
            }
            assert!(
                tokio::time::Instant::now() < ready_deadline,
                "external H.264 marker stage reader did not attach in time"
            );
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }

        source_ring.push_batch(packets.drain(..));
        let output_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(8);
        let mut output_packets = Vec::new();
        loop {
            while let Ok(Some(packet)) = reader.pull() {
                output_packets.push(packet);
            }
            if output_packets
                .iter()
                .any(|packet| packet.media_type == MediaType::Video)
            {
                break;
            }
            if tokio::time::Instant::now() >= output_deadline {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }

        cancel.cancel();

        assert!(
            output_packets
                .iter()
                .any(|packet| packet.media_type == MediaType::Video),
            "external H.264 marker stage should emit live video packets"
        );
    }

    #[tokio::test]
    async fn external_720p_stage_emits_live_packets_for_single_audio_hevc_fixture() {
        let (video, audio_tracks, mut packets) =
            crate::test_fixtures::primary_av_packets_for_codec("h265")
                .expect("single-audio HEVC fixture");

        let engine = Arc::new(MediaEngine::new());
        engine
            .try_register_ingest("pipe-ext-hevc-single-audio", "stream-key", "srt")
            .await
            .unwrap();
        engine
            .update_ingest_meta(
                "pipe-ext-hevc-single-audio",
                Some(video),
                audio_tracks.first().cloned(),
                None,
            )
            .await;
        engine
            .update_ingest_audio_tracks("pipe-ext-hevc-single-audio", audio_tracks.clone())
            .await;

        let source_ring = Arc::new(RingBuffer::new(16_384));
        source_ring.set_codec_hint("hevc");
        source_ring.set_audio_tracks(audio_tracks);
        let output_ring = Arc::new(RingBuffer::new(16_384));
        let mut reader = Reader::new_live(
            "test_ext_720p_single_audio_output".to_string(),
            output_ring.clone(),
        );
        let cancel = CancellationToken::new();
        let stage_key = StageKey::new(
            "pipe-ext-hevc-single-audio",
            StageKind::codec_edge("hevc_to_h264", StageKind::source()),
        );

        tokio::spawn(start_external_transcoder_stage(
            "pipe-ext-hevc-single-audio".to_string(),
            "720p".to_string(),
            source_ring.clone(),
            output_ring,
            engine,
            cancel.clone(),
            None,
            stage_key,
        ));

        let ready_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if source_ring.reader_snapshots().iter().any(|snapshot| {
                snapshot
                    .name
                    .contains("ext-stage:pipe-ext-hevc-single-audio:720p")
            }) {
                break;
            }
            assert!(
                tokio::time::Instant::now() < ready_deadline,
                "external 720p single-audio HEVC stage reader did not attach in time"
            );
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }

        source_ring.push_batch(packets.drain(..));
        let output_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(6);
        let mut output_packets = Vec::new();
        loop {
            while let Ok(Some(packet)) = reader.pull() {
                output_packets.push(packet);
            }
            if output_packets
                .iter()
                .any(|packet| packet.media_type == MediaType::Video)
            {
                break;
            }
            if tokio::time::Instant::now() >= output_deadline {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }

        cancel.cancel();

        assert!(
            output_packets
                .iter()
                .any(|packet| packet.media_type == MediaType::Video),
            "external 720p single-audio HEVC stage should emit live video packets"
        );
    }

    #[tokio::test]
    #[ignore = "diagnostic: current live HEVC + 16 audio preview stage still stalls without EOF"]
    async fn external_720p_stage_emits_live_packets_for_2v16a_hevc_with_longer_input() {
        let ts_sample = extract_2v16a_hevc_ts_sample_for_duration(5);
        let mut demuxer = TsDemuxer::new();
        let mut packets = Vec::new();
        for chunk in ts_sample.chunks(1316) {
            demuxer.feed(chunk);
            demuxer.drain_into(&mut packets);
        }
        demuxer.flush();
        demuxer.drain_into(&mut packets);

        let probe = demuxer
            .take_probe()
            .expect("probe longer 2v16a HEVC sample");
        let video = probe.video.expect("sample should contain video");
        let audio_tracks = probe.audio_tracks;

        let engine = Arc::new(MediaEngine::new());
        engine
            .try_register_ingest("pipe-ext-preview-long", "stream-key", "srt")
            .await
            .unwrap();
        engine
            .update_ingest_meta(
                "pipe-ext-preview-long",
                Some(video),
                audio_tracks.first().cloned(),
                None,
            )
            .await;
        engine
            .update_ingest_audio_tracks("pipe-ext-preview-long", audio_tracks.clone())
            .await;

        let source_ring = Arc::new(RingBuffer::new(32_768));
        source_ring.set_codec_hint("hevc");
        source_ring.set_audio_tracks(audio_tracks);
        let output_ring = Arc::new(RingBuffer::new(32_768));
        let mut reader =
            Reader::new_live("test_ext_720p_long_output".to_string(), output_ring.clone());
        let cancel = CancellationToken::new();
        let stage_key = StageKey::new(
            "pipe-ext-preview-long",
            StageKind::codec_edge("hevc_to_h264", StageKind::source()),
        );

        tokio::spawn(start_external_transcoder_stage(
            "pipe-ext-preview-long".to_string(),
            "720p".to_string(),
            source_ring.clone(),
            output_ring,
            engine,
            cancel.clone(),
            None,
            stage_key,
        ));

        let ready_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if source_ring.reader_snapshots().iter().any(|snapshot| {
                snapshot
                    .name
                    .contains("ext-stage:pipe-ext-preview-long:720p")
            }) {
                break;
            }
            assert!(
                tokio::time::Instant::now() < ready_deadline,
                "external 720p long-input HEVC stage reader did not attach in time"
            );
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }

        source_ring.push_batch(packets.drain(..));
        let output_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
        let mut output_packets = Vec::new();
        loop {
            while let Ok(Some(packet)) = reader.pull() {
                output_packets.push(packet);
            }
            if output_packets
                .iter()
                .any(|packet| packet.media_type == MediaType::Video)
            {
                break;
            }
            if tokio::time::Instant::now() >= output_deadline {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }

        cancel.cancel();

        assert!(
            output_packets
                .iter()
                .any(|packet| packet.media_type == MediaType::Video),
            "external 720p HEVC preview stage should emit live video packets with longer 2v16a input"
        );
    }

    #[test]
    fn feeder_remuxed_single_audio_hevc_fixture_decodes_as_ts_file() {
        let (video, audio_tracks, packets) =
            crate::test_fixtures::primary_av_packets_for_codec("h265")
                .expect("single-audio HEVC fixture");
        let mut feeder = TsPacketFeeder::new(
            Some(&video),
            std::sync::Arc::new(audio_tracks),
            PacketFeedConfig::default(),
        );
        let mut ts_bytes = Vec::new();
        let mut packet_buf = Vec::new();

        for packet in &packets {
            packet_buf.clear();
            if feeder.extend_ts_for_packet(packet, &mut packet_buf) {
                ts_bytes.extend_from_slice(&packet_buf);
            }
        }

        assert!(
            !ts_bytes.is_empty(),
            "remuxed HEVC fixture should produce TS bytes"
        );

        let ts_path = write_temp_ts_artifact("hevc-feeder-remux", &ts_bytes);
        let output = std::process::Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-i",
                ts_path.to_str().expect("utf-8 ts path"),
                "-f",
                "null",
                "-",
            ])
            .output()
            .expect("spawn ffmpeg decode check");

        assert!(
            output.status.success(),
            "ffmpeg should decode feeder-remuxed HEVC TS: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn feeder_remuxed_single_audio_hevc_fixture_transcodes_as_file_input() {
        let (video, audio_tracks, packets) =
            crate::test_fixtures::primary_av_packets_for_codec("h265")
                .expect("single-audio HEVC fixture");
        let mut feeder = TsPacketFeeder::new(
            Some(&video),
            std::sync::Arc::new(audio_tracks),
            PacketFeedConfig::default(),
        );
        let mut ts_bytes = Vec::new();
        let mut packet_buf = Vec::new();

        for packet in &packets {
            packet_buf.clear();
            if feeder.extend_ts_for_packet(packet, &mut packet_buf) {
                ts_bytes.extend_from_slice(&packet_buf);
            }
        }

        let input_path = write_temp_ts_artifact("hevc-feeder-transcode-input", &ts_bytes);
        let output_path = input_path
            .parent()
            .expect("temp artifact dir")
            .join("output.ts");
        let ffmpeg = crate::ffmpeg_extract::ensure_ffmpeg_extracted();
        let mut args = build_stage_ffmpeg_args("720p", "h264");
        let input_pos = args
            .iter()
            .position(|arg| arg == "-i")
            .expect("stage args should contain input flag");
        args[input_pos + 1] = input_path.to_string_lossy().to_string();
        let last = args.last_mut().expect("stage args should contain output");
        *last = output_path.to_string_lossy().to_string();

        let output = std::process::Command::new(ffmpeg)
            .args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .output()
            .expect("spawn bundled ffmpeg transcode");

        assert!(
            output.status.success(),
            "ffmpeg should transcode feeder-remuxed HEVC TS file input: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            std::fs::metadata(&output_path)
                .map(|meta| meta.len() > 0)
                .unwrap_or(false),
            "file-based transcode should produce a non-empty TS output"
        );
    }
}
