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

use crate::media::codec::{audio_for_ts_into, video_for_ts_into};
use crate::media::engine::PipeMetrics;
use crate::media::mpegts::{TsDemuxer, TsMuxer};
use crate::media::ring_buffer::{DtsEnforcer, MediaType, Reader, RingBuffer};
use crate::media::transcoder::{AudioRouting, parse_audio_routing};

/// Stdin writes or stdout reads exceeding this threshold are counted as stalls/idles.
/// 1 ms filters normal async scheduling jitter while catching real back-pressure.
const PIPE_STALL_THRESHOLD_US: u64 = 1_000;

// ── Fast timing ───────────────────────────────────────────────────────────────
// On x86_64 we prefer rdtsc (≈3 cycles) over Instant::now() (≈20-40 cycles via
// VDSO clock_gettime). Both are TSC-backed on Linux when TSC is the active
// clocksource, but rdtsc skips the VDSO calibration/scaling overhead.
//
// We validate before committing to rdtsc:
//   1. CPUID[0x80000007].EDX[8] — invariant TSC: rate is constant across
//      C-states and frequency scaling. Without this, the calibrated rate drifts.
//   2. Calibrated cycles/µs in [100, 10000] — sanity bounds (100 MHz to 10 GHz).
//      Values outside this range indicate preemption-skewed or implausibly short
//      calibration windows.
//   3. Minimum observed window of 50 µs — guards against timer granularity on
//      hypervisors where Instant ticks at coarse resolution.
//
// If any check fails, both now() and delta_us() fall back to Instant::now() so
// the caller sees no behaviour change — just slightly higher timing overhead.
// using_tsc() lets callers log which path is active.

pub mod tsc {
    use std::sync::OnceLock;
    use std::time::Instant;

    const MIN_CYCLES_PER_US: f64 = 100.0;    // 100 MHz — floor for any real CPU
    const MAX_CYCLES_PER_US: f64 = 10_000.0; // 10 GHz — ceiling beyond current hardware
    const MIN_WINDOW_US: f64 = 50.0;         // reject calibrations shorter than this

    enum Backend {
        Tsc(f64),   // cycles per microsecond, validated
        Instant,    // fallback: invariant TSC absent or calibration out of bounds
    }

    /// Opaque timestamp. Holds either TSC cycles or nanos since a fixed origin.
    /// Use only with the delta_us() from the same module — do not interpret directly.
    #[derive(Copy, Clone)]
    pub struct Timestamp(u64);

    static BACKEND: OnceLock<Backend> = OnceLock::new();
    static ORIGIN: OnceLock<Instant> = OnceLock::new();

    fn origin() -> Instant {
        *ORIGIN.get_or_init(Instant::now)
    }

    #[cfg(target_arch = "x86_64")]
    fn has_invariant_tsc() -> bool {
        // CPUID leaf 0x80000007 ("Advanced Power Management Information")
        // EDX bit 8 = invariant TSC.
        let r = unsafe { core::arch::x86_64::__cpuid(0x8000_0007) };
        (r.edx & (1 << 8)) != 0
    }

    #[cfg(not(target_arch = "x86_64"))]
    fn has_invariant_tsc() -> bool { false }

    fn backend() -> &'static Backend {
        BACKEND.get_or_init(|| {
            #[cfg(target_arch = "x86_64")]
            {
                if !has_invariant_tsc() {
                    return Backend::Instant;
                }

                let t0 = Instant::now();
                let c0 = unsafe { core::arch::x86_64::_rdtsc() };
                while t0.elapsed().as_micros() < 200 {
                    core::hint::spin_loop();
                }
                let elapsed_us = t0.elapsed().as_micros() as f64;
                let c1 = unsafe { core::arch::x86_64::_rdtsc() };

                if elapsed_us < MIN_WINDOW_US {
                    return Backend::Instant; // timer granularity too coarse
                }
                let cps = c1.saturating_sub(c0) as f64 / elapsed_us;
                if !(MIN_CYCLES_PER_US..=MAX_CYCLES_PER_US).contains(&cps) {
                    return Backend::Instant; // calibration out of sane bounds
                }
                Backend::Tsc(cps)
            }
            #[cfg(not(target_arch = "x86_64"))]
            {
                Backend::Instant
            }
        })
    }

    /// Trigger calibration eagerly. Call once at startup to amortise the
    /// 200 µs busy-wait before entering the hot path.
    /// Returns true if rdtsc is in use, false if falling back to Instant.
    pub fn calibrate() -> bool {
        matches!(backend(), Backend::Tsc(_))
    }

    /// True if rdtsc passed all checks; false means Instant fallback is active.
    #[inline]
    pub fn using_tsc() -> bool {
        matches!(backend(), Backend::Tsc(_))
    }

    #[inline(always)]
    pub fn now() -> Timestamp {
        match backend() {
            Backend::Tsc(_) => {
                #[cfg(target_arch = "x86_64")]
                return Timestamp(unsafe { core::arch::x86_64::_rdtsc() });
                #[cfg(not(target_arch = "x86_64"))]
                unreachable!()
            }
            Backend::Instant => {
                Timestamp(Instant::now().duration_since(origin()).as_nanos() as u64)
            }
        }
    }

    /// Microseconds since `start` was sampled with now().
    #[inline(always)]
    pub fn delta_us(start: Timestamp) -> u64 {
        match backend() {
            Backend::Tsc(cps) => {
                #[cfg(target_arch = "x86_64")]
                {
                    let now = unsafe { core::arch::x86_64::_rdtsc() };
                    (now.saturating_sub(start.0) as f64 / cps) as u64
                }
                #[cfg(not(target_arch = "x86_64"))]
                unreachable!()
            }
            Backend::Instant => {
                let now_ns = Instant::now().duration_since(origin()).as_nanos() as u64;
                now_ns.saturating_sub(start.0) / 1_000
            }
        }
    }

    // ── Validation logic (pure, exposed for testing) ──────────────────────

    /// Validate a raw cycles-per-µs value from calibration.
    /// Returns the value if sane, or None if it should trigger a fallback.
    pub fn validate_cps(cps: f64, window_us: f64) -> Option<f64> {
        if window_us < MIN_WINDOW_US {
            return None; // window too short — timer granularity coarse
        }
        if !(MIN_CYCLES_PER_US..=MAX_CYCLES_PER_US).contains(&cps) {
            return None; // out of sane bounds
        }
        Some(cps)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn validate_rejects_zero_window() {
            assert!(validate_cps(3000.0, 0.0).is_none());
            assert!(validate_cps(3000.0, 10.0).is_none()); // below MIN_WINDOW_US
            assert!(validate_cps(3000.0, 49.9).is_none());
        }

        #[test]
        fn validate_rejects_out_of_bounds_cps() {
            assert!(validate_cps(50.0, 200.0).is_none());    // below 100 MHz floor
            assert!(validate_cps(15_000.0, 200.0).is_none()); // above 10 GHz ceiling
            assert!(validate_cps(0.0, 200.0).is_none());
            assert!(validate_cps(-1.0, 200.0).is_none());
        }

        #[test]
        fn validate_accepts_sane_values() {
            // Typical desktop/server CPUs: 1–5 GHz
            assert!(validate_cps(1_000.0, 200.0).is_some()); // 1 GHz
            assert!(validate_cps(3_000.0, 200.0).is_some()); // 3 GHz
            assert!(validate_cps(5_000.0, 200.0).is_some()); // 5 GHz
            // Edge of valid range
            assert!(validate_cps(100.0, 50.0).is_some());
            assert!(validate_cps(10_000.0, 50.0).is_some());
        }

        #[test]
        fn delta_us_is_monotone() {
            // Regardless of backend, two consecutive delta_us calls should not
            // go backwards — start of zero gives current positive elapsed time.
            let t0 = now();
            let d = delta_us(t0);
            // We can't assert d > 0 (might be 0 on very fast systems), but
            // we can assert a second call doesn't underflow.
            let d2 = delta_us(t0);
            assert!(d2 >= d);
        }

        #[test]
        fn delta_us_measures_real_elapsed() {
            let t0 = now();
            std::thread::sleep(std::time::Duration::from_millis(5));
            let d = delta_us(t0);
            // Should see at least 3 ms (generous lower bound for loaded CI).
            assert!(d >= 3_000, "expected ≥ 3000 µs, got {} µs", d);
            // Should not see more than 500 ms (would indicate overflow or wrong units).
            assert!(d < 500_000, "expected < 500 000 µs, got {} µs", d);
        }

        #[cfg(target_arch = "x86_64")]
        #[test]
        fn invariant_tsc_check_does_not_panic() {
            // We can't assert the result (depends on CPU) but it must not crash.
            let _ = has_invariant_tsc();
        }
    }
}

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
pub fn build_stage_ffmpeg_args(preset: &str, input_codec: &str) -> Vec<String> {
    // Strip the internal stage-key prefix ("video:720p" → "720p").
    // Audio stages receive the selected upstream video ring, so they copy video
    // while applying any channel-level audio filter.
    let encoding = if preset.starts_with("audio:") {
        "source"
    } else {
        preset.strip_prefix("video:").unwrap_or(preset)
    };
    let audio_routing = stage_audio_routing(preset);

    let mut args = vec![
        "-nostdin".to_string(),
        "-hide_banner".to_string(),
        "-loglevel".to_string(),
        "info".to_string(),
        "-f".to_string(),
        "mpegts".to_string(),
        "-i".to_string(),
        "pipe:0".to_string(),
    ];

    if let Some(filter) = audio_filter_complex(&audio_routing) {
        args.extend(["-filter_complex".to_string(), filter]);
        args.extend(["-map".to_string(), "0:v:0?".to_string()]);
        args.extend(["-map".to_string(), "[aout]".to_string()]);
    } else {
        args.extend(["-map".to_string(), "0:v:0".to_string()]);
        args.extend(["-map".to_string(), "0:a?".to_string()]);
    }

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
            "veryfast".to_string(),
        ]);
    }

    // ── audio ────────────────────────────────────────────────────────────
    // atrack selection stays in the zero-copy audio router. Channel-level
    // remap/downmix stages arrive here and must decode/filter/re-encode audio.
    if audio_routing.is_some() {
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

    // ── output: MPEG-TS to stdout ─────────────────────────────────────────
    args.extend(["-f".to_string(), "mpegts".to_string(), "pipe:1".to_string()]);

    args
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
    // Override the video codec used in the TsMuxer PMT, and selects the
    // encoder in build_stage_ffmpeg_args.  Pass "hevc" when the source ring
    // carries H.265 so the stage spawns libx265 and tags its output ring
    // correctly.  None defaults to H.264 (libx264).
    input_codec_override: Option<String>,
) {
    let input_codec = input_codec_override.as_deref().unwrap_or("h264");
    let args = build_stage_ffmpeg_args(&encoding, input_codec);
    println!(
        "[ext-transcoder] stage start  pipeline={} encoding={}",
        pipeline_id, encoding
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
            eprintln!(
                "[ext-transcoder] failed to spawn ffmpeg ({}:{}): {}",
                pipeline_id, encoding, e
            );
            return;
        }
    };

    // .take() returns None if the child exited between spawn() and here (rare but possible).
    // Use pattern matching rather than .expect() to avoid a panic in that race.
    let mut stdin = match child.stdin.take() {
        Some(s) => s,
        None => {
            eprintln!(
                "[ext-transcoder] ffmpeg stdin unavailable ({}:{})",
                pipeline_id, encoding
            );
            let _ = child.kill().await;
            let _ = child.wait().await;
            cancel.cancel();
            return;
        }
    };
    let stdout = match child.stdout.take() {
        Some(s) => s,
        None => {
            eprintln!(
                "[ext-transcoder] ffmpeg stdout unavailable ({}:{})",
                pipeline_id, encoding
            );
            let _ = child.kill().await;
            let _ = child.wait().await;
            cancel.cancel();
            return;
        }
    };
    let stderr = match child.stderr.take() {
        Some(s) => s,
        None => {
            eprintln!(
                "[ext-transcoder] ffmpeg stderr unavailable ({}:{})",
                pipeline_id, encoding
            );
            let _ = child.kill().await;
            let _ = child.wait().await;
            cancel.cancel();
            return;
        }
    };

    // ── stage metrics ─────────────────────────────────────────────────────
    let stage_metrics = engine
        .get_or_create_stage_metrics(&pipeline_id, &encoding)
        .await;

    // ── pipe metrics ──────────────────────────────────────────────────────
    // Separate from stage_metrics: only subprocess-pipe stages have these.
    // Trigger TSC calibration eagerly (200 µs busy-wait, once per process).
    // Logs which path was chosen so operators can see it in the stage output.
    let using_tsc = tsc::calibrate();
    if !using_tsc {
        println!(
            "[ext-transcoder] pipe timing: Instant fallback \
             (invariant TSC absent or calibration out of bounds)"
        );
    }
    let pipe_metrics = Arc::new(PipeMetrics::default());
    engine
        .register_pipe_metrics(
            &format!("{}:{}", pipeline_id, encoding),
            pipe_metrics.clone(),
        )
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
                            eprintln!(
                                "[ext-transcoder] ffmpeg stderr ({}) truncated at 1 MB — \
                                 further output discarded",
                                label
                            );
                        }
                    }
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
                let lock = i.audio_tracks.lock().unwrap_or_else(|e| e.into_inner());
                let tracks = if lock.is_empty()
                    && let Some(a) = i.audio.clone()
                {
                    std::sync::Arc::new(vec![a])
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
    {
        let out_ring = output_buffer.clone();
        let cancel_out = cancel.clone();
        let label_out = label.clone();
        let out_stage_metrics = stage_metrics.clone();
        let out_pipe_metrics = pipe_metrics.clone();
        let mut stdout = stdout;
        tokio::spawn(async move {
            let mut demuxer = TsDemuxer::new();
            let mut buf = vec![0u8; 65536];
            let mut pkts = Vec::with_capacity(32);
            loop {
                let t0 = tsc::now();
                tokio::select! {
                    _ = cancel_out.cancelled() => break,
                    result = stdout.read(&mut buf) => {
                        let idle_us = tsc::delta_us(t0);
                        match result {
                            Ok(0) | Err(_) => {
                                eprintln!("[ext-transcoder] stdout closed ({})", label_out);
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
                    let in_bytes = pkt.payload.len() as u64;
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
                            match audio_tracks
                                .iter()
                                .position(|a| a.track_index == pkt.track_index)
                            {
                                Some(i) => i + vo,
                                None => continue, // unknown track — skip to avoid DTS corruption
                            }
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
                    if !ts.is_empty() {
                        let t0 = tsc::now();
                        if stdin.write_all(ts).await.is_err() {
                            eprintln!(
                                "[ext-transcoder] stdin write failed ({}:{}) — ffmpeg exited",
                                pipeline_id, encoding
                            );
                            break 'outer;
                        }
                        let write_us = tsc::delta_us(t0);
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

    engine
        .remove_stage_metrics(&pipeline_id, &encoding)
        .await;
    engine
        .remove_pipe_metrics(&format!("{}:{}", pipeline_id, encoding))
        .await;
    engine.event_log.emit(crate::events::EventKind::StageStopped {
        pipeline_id: pipeline_id.clone(),
        encoding: encoding.clone(),
    });

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
    fn stage_args_source_copies_video() {
        let args = build_stage_ffmpeg_args("source", "h264");
        let cv_pos = args.iter().position(|a| a == "-c:v").unwrap();
        assert_eq!(args[cv_pos + 1], "copy");
        // no scale filter
        assert!(!args.iter().any(|a| a == "-vf"));
        assert!(args.last() == Some(&"pipe:1".to_string()));
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
}
