//! # Threading Model
//!
//! ```text
//! ┌─────────────────── tokio runtime (multi-threaded) ───────────────────┐
//! │  Axum web server          HTTP handlers, SSE streams                │
//! │  RTMP listener            per-connection async tasks                │
//! │  SRT accept loop          per-connection async tasks                │
//! │  Reconciler (1s tick)     output lifecycle + recording auto-start   │
//! │  Egress tasks             ring buffer reader → network send         │
//! │  HLS segmenter            TsMuxer → segment accumulator → HlsStore │
//! └─────────────────────────────────────────────────────────────────────┘
//!
//! ┌─────────────── std::thread (OS threads, catch_unwind) ──────────────┐
//! │  FFmpeg demuxer           RTMP/SRT ingest → RingBuffer push         │
//! │  FFmpeg MKV muxer         MemoryQueue → .mkv recording file         │
//! │  FFmpeg transcoder        MemoryQueue → encode → MemoryQueue        │
//! └─────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! Tokio tasks handle all network I/O and coordination. CPU-bound FFmpeg work
//! runs on dedicated OS threads to avoid starving the async runtime. All
//! `std::thread::spawn` calls are wrapped in `catch_unwind` so an FFmpeg panic
//! (e.g., from a corrupt stream) logs an error instead of taking down the process.

pub mod alerts;
pub mod api;
pub mod db;
pub mod diag;
pub mod domain;
pub mod events;
pub mod ffmpeg_extract;
pub mod media;
pub mod planner;
pub mod runtime_info;
pub mod types;

use crate::domain::stage::{EncodingStagePlan, StageKey};
use crate::media::engine::MediaEngine;
use futures_util::FutureExt as _;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex as TokioMutex;

pub struct ServerPorts {
    pub http: u16,
    pub rtmp: u16,
    pub srt: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeTuning {
    pub nofile_limit: u64,
    pub reconciler_interval_ms: u64,
    pub output_max_retries: u32,
    pub output_retry_base_ms: u64,
    pub output_retry_max_ms: u64,
    pub hls_idle_timeout_ms: u64,
}

impl ServerPorts {
    pub fn from_env() -> Self {
        Self {
            http: std::env::var("RESTREAM_HTTP_PORT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(3030),
            rtmp: std::env::var("RESTREAM_RTMP_PORT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(1935),
            srt: std::env::var("RESTREAM_SRT_PORT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(10080),
        }
    }
}

impl Default for RuntimeTuning {
    fn default() -> Self {
        Self {
            nofile_limit: 65_536,
            reconciler_interval_ms: 1_000,
            output_max_retries: 10,
            output_retry_base_ms: 5_000,
            output_retry_max_ms: 300_000,
            hls_idle_timeout_ms: 60_000,
        }
    }
}

impl RuntimeTuning {
    pub fn from_env() -> Self {
        let defaults = Self::default();
        Self {
            nofile_limit: env_u64("RESTREAM_NOFILE_LIMIT", defaults.nofile_limit).max(1),
            reconciler_interval_ms: env_u64(
                "RESTREAM_RECONCILE_INTERVAL_MS",
                defaults.reconciler_interval_ms,
            )
            .max(100),
            output_max_retries: env_u32("RESTREAM_OUTPUT_MAX_RETRIES", defaults.output_max_retries),
            output_retry_base_ms: env_u64(
                "RESTREAM_OUTPUT_RETRY_BASE_MS",
                defaults.output_retry_base_ms,
            )
            .max(1),
            output_retry_max_ms: env_u64(
                "RESTREAM_OUTPUT_RETRY_MAX_MS",
                defaults.output_retry_max_ms,
            )
            .max(1),
            hls_idle_timeout_ms: env_u64(
                "RESTREAM_HLS_IDLE_TIMEOUT_MS",
                defaults.hls_idle_timeout_ms,
            )
            .max(1),
        }
    }

    fn session_prune_every_ticks(&self) -> u64 {
        let ticks = 3_600_000u64.div_ceil(self.reconciler_interval_ms);
        ticks.max(1)
    }

    fn output_backoff_ms(&self, retries: u32) -> u64 {
        let shift = retries.min(16);
        let multiplier = 1u64.checked_shl(shift).unwrap_or(u64::MAX);
        self.output_retry_base_ms
            .saturating_mul(multiplier)
            .min(self.output_retry_max_ms)
    }
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn env_u32(name: &str, default: u32) -> u32 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

pub fn env_flag_enabled(name: &str) -> bool {
    std::env::var(name)
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

fn set_rlimit(limit: u64) {
    // SAFETY: setrlimit is a POSIX system call. The rlimit struct is stack-
    // allocated with valid values; no pointer aliasing concerns. Called once
    // at startup before any file descriptors are opened, so raising the limit
    // cannot interfere with other operations.
    unsafe {
        let limit = libc::rlimit {
            rlim_cur: limit,
            rlim_max: limit,
        };
        if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) != 0 {
            eprintln!("[system] Failed to raise RLIMIT_NOFILE limit");
        } else {
            println!(
                "[system] Successfully raised file descriptor limit to {}",
                limit.rlim_cur
            );
        }
    }
}

pub async fn run_app() {
    let tuning = RuntimeTuning::from_env();

    // Elevate limits for high fd count (500+ egress streams)
    set_rlimit(tuning.nofile_limit);

    // Initialize database — use create_pool() so per-connection PRAGMAs
    // (busy_timeout, synchronous, cache_size, …) apply to every pooled connection.
    let db_url = std::env::var("RESTREAM_DB_PATH")
        .map(|p| format!("sqlite:{}?mode=rwc", p))
        .unwrap_or_else(|_| "sqlite:data.db?mode=rwc".to_string());
    let pool = db::create_pool(&db_url)
        .await
        .expect("Failed to connect to SQLite database");

    db::setup_database_schema(&pool)
        .await
        .expect("Failed to set up SQLite schema");

    let now_rfc = chrono::Utc::now().to_rfc3339();
    db::reset_running_jobs(&pool, &now_rfc)
        .await
        .expect("Failed to reset stale running jobs");

    // Initialize services
    let config_str = db::get_meta(&pool, "ingest_security_config")
        .await
        .unwrap_or(None);
    let sec_config = if let Some(s) = config_str {
        serde_json::from_str::<crate::types::IngestSecurityConfig>(&s)
            .unwrap_or(crate::media::security::DEFAULT_INGEST_SECURITY_CONFIG)
    } else {
        crate::media::security::DEFAULT_INGEST_SECURITY_CONFIG
    };
    let security = Arc::new(crate::media::security::IngestSecurityService::new(
        sec_config,
    ));
    let sessions = Arc::new(tokio::sync::RwLock::new(std::collections::HashSet::new()));
    crate::api::initialize_auth(&pool, &sessions).await;
    crate::media::profiles::load_from_db(&pool).await;
    let engine = Arc::new(MediaEngine::new());
    // Keep a clone of sessions for the reconciler's hourly prune tick.
    let sessions_for_reconciler = sessions.clone();

    let ports = ServerPorts::from_env();

    let media_dir = std::env::var("RESTREAM_MEDIA_DIR").unwrap_or_else(|_| "media".to_string());
    let reconciler_media_dir = media_dir.clone();
    let state = Arc::new(crate::api::AppState {
        db: pool.clone(),
        security: security.clone(),
        sessions,
        engine: engine.clone(),
        ports: crate::api::PortConfig {
            rtmp: ports.rtmp,
            srt: ports.srt,
        },
        media_dir,
        alert_tracker: crate::alerts::AlertTracker::new(),
    });

    // Start Web Server
    let http_addr = format!("0.0.0.0:{}", ports.http);
    let app = crate::api::create_router(state);
    let listener = tokio::net::TcpListener::bind(&http_addr)
        .await
        .unwrap_or_else(|_| panic!("Failed to bind TCP listener on port {}", ports.http));
    println!(
        "[web] Dashboard API server listening on http://{}",
        http_addr
    );

    tokio::spawn(async move {
        if let Err(e) = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        {
            eprintln!("[web] Axum server error: {:?}", e);
        }
    });

    // Start RTMP server — capture handle to detect early exit (M3).
    let db_clone = pool.clone();
    let security_clone = security.clone();
    let engine_clone = engine.clone();
    let rtmp_port = ports.rtmp;
    let rtmp_handle = tokio::spawn(async move {
        crate::media::rtmp::start_rtmp_server_on(db_clone, security_clone, engine_clone, rtmp_port)
            .await;
        eprintln!("[rtmp] Server task exited unexpectedly");
    });

    // Start SRT server — pass security for rate limiting (H1).
    let srt_server = Arc::new(crate::media::srt::SrtServer::new(
        pool.clone(),
        engine.clone(),
        security.clone(),
    ));
    let srt_port = ports.srt;
    let srt_handle = tokio::spawn(async move {
        srt_server.run(srt_port).await;
        eprintln!("[srt] Server task exited unexpectedly");
    });

    // ── Graceful shutdown ────────────────────────────────────────────────────
    // A single CancellationToken is shared between the signal watcher and the
    // reconciler loop.  On Ctrl+C or SIGTERM the token fires and the reconciler
    // loop breaks, after which we cancel all active egress tasks and exit.
    let shutdown = tokio_util::sync::CancellationToken::new();
    {
        let shutdown_c = shutdown.clone();
        tokio::spawn(async move {
            #[cfg(unix)]
            {
                let mut sigterm =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                        .expect("Failed to install SIGTERM handler");
                tokio::select! {
                    res = tokio::signal::ctrl_c() => {
                        if let Err(e) = res { eprintln!("[shutdown] Ctrl+C error: {e}"); }
                    }
                    _ = sigterm.recv() => {}
                }
            }
            #[cfg(not(unix))]
            {
                if let Err(e) = tokio::signal::ctrl_c().await {
                    eprintln!("[shutdown] Ctrl+C error: {e}");
                }
            }
            println!("[shutdown] Signal received — stopping reconciler");
            shutdown_c.cancel();
        });
    }

    // Run reconciliation loop
    let last_failed: Arc<TokioMutex<HashMap<String, (Instant, u32)>>> =
        Arc::new(TokioMutex::new(HashMap::new()));
    let mut reconciler_tick: u64 = 0;
    let session_prune_every_ticks = tuning.session_prune_every_ticks();
    loop {
        // Wait one reconciler interval OR until a shutdown signal fires.
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = tokio::time::sleep(Duration::from_millis(tuning.reconciler_interval_ms)) => {}
        }
        reconciler_tick += 1;

        // Hourly session prune, adjusted for the configured reconciler interval.
        // Removes in-memory sessions whose DB token has expired (older than
        // 30 days). Prevents the HashSet and sessions table from growing
        // indefinitely across months of uptime.
        if reconciler_tick.is_multiple_of(session_prune_every_ticks) {
            // DB prune
            let _ = db::prune_expired_sessions(&pool, 30 * 24 * 60 * 60 * 1000).await;
            // In-memory prune: remove tokens that no longer exist in DB.
            // Skip the retain if the DB call fails — an empty result would
            // incorrectly log out every active session.
            match db::list_sessions(&pool).await {
                Ok(live_tokens) => {
                    let live_set: std::collections::HashSet<String> =
                        live_tokens.into_iter().collect();
                    sessions_for_reconciler
                        .write()
                        .await
                        .retain(|t| live_set.contains(t));
                }
                Err(e) => {
                    eprintln!("[reconciler] Failed to list sessions for prune: {e}");
                }
            }
        }

        let outputs = match db::list_outputs(&pool).await {
            Ok(o) => o,
            Err(e) => {
                eprintln!(
                    "[reconciler] DB error reading outputs (tick {}): {}",
                    reconciler_tick, e
                );
                continue;
            }
        };

        for output in &outputs {
            let is_active = engine
                .egress_cancel_tokens
                .read()
                .await
                .contains_key(&output.id);
            let now_str = chrono::Utc::now().to_rfc3339();

            if output.desired_state == "running" && !is_active {
                // Check backoff / max-retries for recently-failed outputs
                let mut lf = last_failed.lock().await;
                if let Some(&(failed_at, retries)) = lf.get(&output.id) {
                    if retries >= tuning.output_max_retries {
                        lf.remove(&output.id);
                        drop(lf);
                        eprintln!(
                            "[reconciler] Output {} ({}) exceeded {} retries — marking failed",
                            output.name, output.id, tuning.output_max_retries
                        );
                        let _ = db::set_output_desired_state(
                            &pool,
                            &output.pipeline_id,
                            &output.id,
                            "failed",
                        )
                        .await;
                        continue;
                    }
                    let backoff_ms = tuning.output_backoff_ms(retries);
                    if failed_at.elapsed() < Duration::from_millis(backoff_ms) {
                        drop(lf);
                        continue; // Wait for backoff
                    }
                }
                lf.remove(&output.id);
                drop(lf);

                println!(
                    "[reconciler] Starting output job: {} ({})",
                    output.name, output.id
                );

                // Get source pipeline ring buffer
                let source_buf = engine.get_or_create_pipeline(&output.pipeline_id).await;

                // ── Ring-buffer routing ────────────────────────────────────────────
                //
                // Passthrough (source/custom): read directly from source_ring.
                // Transcoded  (e.g. 720p):     route through a shared transcoder
                //   stage that produces its own output_ring.  All egresses for the
                //   same (pipeline, preset) share one stage process and one ring.
                //
                // Stage graph:
                //   source_ring
                //     │  [if video preset]          video:preset shared stage
                //     │  [if audio routing suffix]  audio filter shared stage
                //     │  [if RTMP + H.265 ingest]   hevc_to_h264:from:<upstream>
                //     ↓
                //   ring_buf   ←── egress reads from here
                //
                // Transcoder backend: external by default (subprocess FFmpeg,
                // stdin→stdout).  Set RESTREAM_USE_INTERNAL_TRANSCODER=1 for the
                // in-process libavcodec path.  See docs/media-pipeline.md.
                let stage_plan =
                    EncodingStagePlan::from_encoding(output.pipeline_id.as_str(), &output.encoding);

                let is_rtmp =
                    output.url.starts_with("rtmp://") || output.url.starts_with("rtmps://");
                let ingest_is_hevc = {
                    let ingests = engine.active_ingests.read().await;
                    ingests
                        .get(&output.pipeline_id)
                        .and_then(|i| i.video.as_ref())
                        .map(|v| v.codec == "hevc" || v.codec == "h265")
                        .unwrap_or(false)
                };
                // Stage graph (new design — H.265→H.264 conversion at the output
                // edge, not up-front):
                //
                //   source_ring (H.265 or H.264)
                //     │  [Stage 1, if preset] video:NNNp   ← preserves input codec
                //     │  [Stage 2, if audio]  audio filter ← shared by RTMP + SRT
                //     │  [Stage 3, RTMP only] hevc_to_h264 ← keyed by upstream
                //     ↓
                //   ring_buf  ←── egress reads from here
                //
                // SRT outputs receive native H.265 at the target resolution.
                // RTMP outputs get one shared hevc_to_h264 per (pipeline, preset)
                // applied after the video+audio stages, avoiding a redundant
                // source-resolution encode pass.
                let video_stage = stage_plan.video_stage();
                // H.265→H.264 is only needed at the RTMP output edge.
                let needs_rtmp_h264_conv = ingest_is_hevc && is_rtmp;
                // Pass the ingest codec as override so the video:preset ring is
                // tagged with the correct codec hint for downstream audio stages
                // and egress writers (source ring has no hint; active_ingests is
                // the authoritative source).
                let ingest_codec_override = if ingest_is_hevc { Some("hevc") } else { None };

                // Stage 1: video transcode from source ring (H.265 flows through
                // directly; build_stage_ffmpeg_args picks libx265 vs libx264 from
                // the input_codec_override passed into the stage).
                let video_buf = if let Some(stage) = &video_stage {
                    engine
                        .get_or_create_transcoder(
                            &output.pipeline_id,
                            stage.kind.clone(),
                            source_buf.clone(),
                            ingest_codec_override,
                        )
                        .await
                } else {
                    source_buf.clone()
                };

                // Stage 2: optional audio filter (keyed on video stage to prevent
                // cross-contamination between presets). Shared between RTMP and SRT
                // egresses on the same preset.
                let pre_h264_buf = if let Some(stage) = stage_plan.audio_stage() {
                    engine
                        .get_or_create_transcoder(
                            &output.pipeline_id,
                            stage.kind,
                            video_buf.clone(),
                            None,
                        )
                        .await
                } else {
                    video_buf.clone()
                };

                // Stage 3: H.265→H.264 conversion for RTMP only (applied after
                // audio routing so the converter sees the selected audio tracks).
                // Keyed by terminal_kind so RTMP-passthrough and RTMP-720p each
                // get their own converter, and all RTMP egresses on the same preset
                // share it.
                let ring_buf = if needs_rtmp_h264_conv {
                    engine
                        .get_or_create_h264_transcoder(
                            &output.pipeline_id,
                            stage_plan.terminal_kind().clone(),
                            pre_h264_buf,
                        )
                        .await
                } else {
                    pre_h264_buf
                };

                // Register egress and get token
                let cancel_token = engine
                    .register_egress(&output.id, &output.pipeline_id, &output.url)
                    .await;

                let job_id = format!("job_{}", output.id);
                let _ = db::create_job(
                    &pool,
                    &job_id,
                    &output.pipeline_id,
                    &output.id,
                    None,
                    "running",
                    &now_str,
                )
                .await;

                let engine_c = engine.clone();
                let _ = db::append_job_log(
                    &pool,
                    Some(&job_id),
                    Some(&output.pipeline_id),
                    Some(&output.id),
                    "lifecycle.start",
                    None,
                    &now_str,
                    "[lifecycle] Output job started",
                )
                .await;

                // Spawn the specific egress client.
                // ring_buf already points to the correct ring:
                //   • passthrough  → source_ring
                //   • transcoded   → shared transcoder stage output_ring
                // The egress client is protocol-agnostic w.r.t. this choice.
                let output_id_c = output.id.clone();
                let pipeline_id_c = output.pipeline_id.clone();
                let encoding_c = output.encoding.clone();
                let url_c = output.url.clone();
                let pool_c = pool.clone();
                let last_failed_c = last_failed.clone();

                tokio::spawn(async move {
                    // Unsupported URL: reject immediately before the panic-safe
                    // block so its early `return` exits the entire spawn task.
                    let is_supported = url_c.starts_with("rtmp://")
                        || url_c.starts_with("rtmps://")
                        || url_c.starts_with("srt://")
                        || url_c.starts_with("hls://")
                        || url_c.starts_with("http://")
                        || url_c.starts_with("https://");
                    if !is_supported {
                        let end_now = chrono::Utc::now().to_rfc3339();
                        let _ = db::update_job(
                            &pool_c,
                            &job_id,
                            None,
                            Some("failed"),
                            Some(&end_now),
                            Some(0),
                            None,
                        )
                        .await;
                        let _ = db::append_job_log(
                            &pool_c,
                            Some(&job_id),
                            Some(&pipeline_id_c),
                            Some(&output_id_c),
                            "lifecycle.error",
                            None,
                            &end_now,
                            &format!("[lifecycle] Unsupported URL scheme: {}", url_c),
                        )
                        .await;
                        engine_c.unregister_egress(&output_id_c).await;
                        {
                            let mut lf = last_failed_c.lock().await;
                            let retries = lf.get(&output_id_c).map(|(_, r)| r + 1).unwrap_or(1);
                            lf.insert(output_id_c, (Instant::now(), retries));
                        }
                        return;
                    }

                    // Wrap the egress call in catch_unwind so a panic does not
                    // prevent the cleanup path below (unregister_egress, job-
                    // status update) from running.
                    let panicked = std::panic::AssertUnwindSafe(async {
                        if url_c.starts_with("rtmp://") || url_c.starts_with("rtmps://") {
                            crate::media::rtmp::start_rtmp_egress(
                                output_id_c.clone(),
                                pipeline_id_c.clone(),
                                url_c.clone(),
                                ring_buf,
                                engine_c.clone(),
                                cancel_token.clone(),
                            )
                            .await;
                        } else if url_c.starts_with("srt://") {
                            crate::media::srt::start_srt_egress(
                                output_id_c.clone(),
                                pipeline_id_c.clone(),
                                encoding_c,
                                url_c.clone(),
                                ring_buf,
                                engine_c.clone(),
                                cancel_token.clone(),
                            )
                            .await;
                        } else if url_c.starts_with("hls://")
                            || url_c.starts_with("http://")
                            || url_c.starts_with("https://")
                        {
                            // HLS egress: use the shared segmenter, register as persistent consumer
                            let (store, already_running) =
                                engine_c.ensure_hls_segmenter(&pipeline_id_c).await;
                            if !already_running {
                                let Some(hls_cancel) =
                                    engine_c.get_hls_cancel_token(&pipeline_id_c).await
                                else {
                                    eprintln!(
                                        "[reconciler] HLS segmenter token missing for {} — skipping",
                                        pipeline_id_c
                                    );
                                    return;
                                };
                                let eng2 = engine_c.clone();
                                let pid2 = pipeline_id_c.clone();
                                let rb2 = ring_buf.clone();
                                let store_for_segmenter = store.clone();
                                tokio::spawn(async move {
                                    crate::media::hls::start_hls_segmenter(
                                        pid2.clone(),
                                        store_for_segmenter,
                                        rb2,
                                        eng2.clone(),
                                        hls_cancel,
                                    )
                                    .await;
                                    eng2.shutdown_hls_segmenter(&pid2).await;
                                });
                            }
                            engine_c.add_hls_persistent_consumer(&pipeline_id_c).await;
                            if url_c.starts_with("http://") || url_c.starts_with("https://") {
                                crate::media::hls_upload::start_hls_put_upload(
                                    output_id_c.clone(),
                                    pipeline_id_c.clone(),
                                    url_c.clone(),
                                    store,
                                    engine_c.clone(),
                                    cancel_token.clone(),
                                )
                                .await;
                            } else {
                                cancel_token.cancelled().await;
                            }
                            // remove_hls_persistent_consumer is intentionally called
                            // in the always-runs cleanup section below (outside the
                            // catch_unwind) so a panic between add and here cannot
                            // permanently leak the refcount.
                        }
                    })
                    .catch_unwind()
                    .await
                    .is_err();

                    if panicked {
                        eprintln!(
                            "[egress] Panic in egress task for output {} (pipeline {})",
                            output_id_c, pipeline_id_c
                        );
                    }

                    // Cleanup always runs — even after a panic in the egress fn.
                    let is_cancelled = cancel_token.is_cancelled();
                    engine_c.unregister_egress(&output_id_c).await;
                    // Remove the HLS persistent consumer refcount unconditionally
                    // for HLS outputs. The add happened inside the catch_unwind
                    // above; by moving the remove here we ensure it runs even
                    // when the egress future panics after add but before its own
                    // remove (which was guarded only by a cancellation wait).
                    if url_c.starts_with("hls://")
                        || url_c.starts_with("http://")
                        || url_c.starts_with("https://")
                    {
                        engine_c
                            .remove_hls_persistent_consumer(&pipeline_id_c)
                            .await;
                    }

                    let end_now = chrono::Utc::now().to_rfc3339();
                    let job_status = if is_cancelled { "stopped" } else { "failed" };
                    let _ = db::update_job(
                        &pool_c,
                        &job_id,
                        None,
                        Some(job_status),
                        Some(&end_now),
                        Some(0),
                        None,
                    )
                    .await;
                    let _ = db::append_job_log(
                        &pool_c,
                        Some(&job_id),
                        Some(&pipeline_id_c),
                        Some(&output_id_c),
                        "lifecycle.stop",
                        None,
                        &end_now,
                        &format!("[lifecycle] Output job exited with status: {}", job_status),
                    )
                    .await;

                    if !is_cancelled {
                        let mut lf = last_failed_c.lock().await;
                        let retries = lf.get(&output_id_c).map(|(_, r)| r + 1).unwrap_or(1);
                        lf.insert(output_id_c, (Instant::now(), retries));
                    }
                });
            } else if output.desired_state == "stopped" && is_active {
                println!(
                    "[reconciler] Stopping output job: {} ({})",
                    output.name, output.id
                );
                engine.unregister_egress(&output.id).await;
            }
        }

        // Clean up unused shared transcoder stages
        {
            let mut needed_stages: std::collections::HashSet<StageKey> =
                std::collections::HashSet::new();
            let ingests = engine.active_ingests.read().await;
            let egress_tokens = engine.egress_cancel_tokens.read().await;
            for output in &outputs {
                let is_active = egress_tokens.contains_key(&output.id);
                if is_active || output.desired_state == "running" {
                    let is_rtmp =
                        output.url.starts_with("rtmp://") || output.url.starts_with("rtmps://");
                    let ingest_is_hevc = ingests
                        .get(&output.pipeline_id)
                        .and_then(|i| i.video.as_ref())
                        .map(|v| v.codec == "hevc" || v.codec == "h265")
                        .unwrap_or(false);

                    let stage_plan = EncodingStagePlan::from_encoding(
                        output.pipeline_id.as_str(),
                        &output.encoding,
                    );
                    let needs_rtmp_h264_conv = ingest_is_hevc && is_rtmp;

                    if let Some(stage) = stage_plan.video_stage() {
                        needed_stages.insert(stage);
                    }
                    if let Some(stage) = stage_plan.audio_stage() {
                        needed_stages.insert(stage);
                    }
                    if needs_rtmp_h264_conv {
                        needed_stages.insert(stage_plan.codec_edge_stage("hevc_to_h264"));
                    }
                }
            }
            engine.sweep_unused_transcoder_stages(&needed_stages).await;
            engine.sweep_unused_stages().await;
        }

        // Reconcile recordings: auto-start/stop based on enabled flag and ingest state
        let pipelines = match db::list_pipelines(&pool).await {
            Ok(p) => p,
            Err(e) => {
                eprintln!(
                    "[reconciler] DB error reading pipelines (tick {}): {}",
                    reconciler_tick, e
                );
                continue;
            }
        };
        for pipeline in pipelines {
            let has_ingest = engine
                .active_ingests
                .read()
                .await
                .contains_key(&pipeline.id);

            // Reconcile recordings
            let rec_key = format!("recording_enabled:{}", pipeline.id);
            let rec_enabled = db::get_meta(&pool, &rec_key)
                .await
                .ok()
                .flatten()
                .map(|v| v == "1")
                .unwrap_or(false);
            let rec_active = engine.is_recording_active(&pipeline.id).await;

            if rec_enabled && has_ingest && !rec_active {
                let ring_buf = engine.get_or_create_pipeline(&pipeline.id).await;
                let cancel_token = engine.register_recording(&pipeline.id).await;
                let engine_c = engine.clone();
                let pid = pipeline.id.clone();
                let pipe_name = pipeline.name.clone();
                let engine_rec = engine_c.clone();
                let media_dir_rec = reconciler_media_dir.clone();
                tokio::spawn(async move {
                    crate::media::recording::start_recording(
                        pipe_name,
                        pid.clone(),
                        media_dir_rec,
                        ring_buf,
                        engine_rec,
                        cancel_token,
                    )
                    .await;
                    engine_c.unregister_recording(&pid).await;
                });
            } else if rec_active && (!rec_enabled || !has_ingest) {
                engine.unregister_recording(&pipeline.id).await;
            }
        }

        // Sweep idle HLS segmenters after the configured idle timeout
        // or if ingest disconnected.
        let hls_ids: Vec<String> = engine.hls_consumers.read().await.keys().cloned().collect();
        for pid in hls_ids {
            let has_ingest = engine.active_ingests.read().await.contains_key(&pid);
            let idle = {
                let consumers = engine.hls_consumers.read().await;
                match consumers.get(&pid) {
                    Some(c) => !has_ingest || c.is_idle(tuning.hls_idle_timeout_ms),
                    None => false,
                }
            };
            if idle {
                engine.shutdown_hls_segmenter(&pid).await;
            }
        }

        // Periodically reap exited file-ingest subprocesses
        engine.reap_file_ingests().await;
    }

    // ── Graceful shutdown cleanup ────────────────────────────────────────────
    // Cancel ALL active tasks (egress, ingest, recording) and shut down HLS
    // segmenters.  Previously only egress tokens were cancelled, leaving ingest
    // OS threads, recording FFmpeg threads, and HLS segmenters alive until the
    // runtime forcibly dropped them.
    println!("[shutdown] Cancelling all active tasks...");
    {
        let egress = engine.egress_cancel_tokens.read().await;
        for token in egress.values() {
            token.cancel();
        }
    }
    {
        let ingests = engine.ingest_cancel_tokens.read().await;
        for token in ingests.values() {
            token.cancel();
        }
    }
    {
        let recs = engine.recording_cancel_tokens.read().await;
        for token in recs.values() {
            token.cancel();
        }
    }
    // Shut down all HLS segmenters (stops their internal tasks/timers).
    let hls_ids: Vec<String> = engine.hls_consumers.read().await.keys().cloned().collect();
    for pid in hls_ids {
        engine.shutdown_hls_segmenter(&pid).await;
    }

    // Give all tasks a moment to run their cleanup paths before we close the DB.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Join all registered OS threads (FFmpeg transcoders, SRT accept/sender threads).
    // By now cancellation tokens have fired and MemoryQueues are being closed, so
    // most threads should exit within milliseconds.  spawn_blocking lets us join
    // without blocking the tokio thread-pool.
    let handles = engine.drain_os_thread_handles();
    if !handles.is_empty() {
        tokio::task::spawn_blocking(move || {
            for h in handles {
                let _ = h.join(); // returns Err only if the thread panicked; already logged
            }
        })
        .await
        .ok();
    }

    // Close the SQLite pool so WAL is checkpointed into the main DB file.
    // Must be called after all DB-writing tasks have been cancelled above.
    pool.close().await;

    // Abort RTMP task (it has no graceful-shutdown path; aborting is safe here).
    rtmp_handle.abort();

    // Await the SRT server task: dropping tx (via drain_os_thread_handles above)
    // unblocks run()'s accept loop; waiting here ensures _server_sock_guard Drop
    // fires and srt_close(server_sock) completes before srt_cleanup() below.
    let _ = srt_handle.await;

    // Tear down libsrt global state AFTER all SRT sockets are closed.
    // This must come after join_os_threads() above, which guarantees all
    // SRT sender threads have exited and called srt_close() on their sockets.
    crate::media::srt::teardown_srt();
    println!("[shutdown] Done");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_tuning_defaults_preserve_existing_operational_behavior() {
        let tuning = RuntimeTuning::default();

        assert_eq!(tuning.nofile_limit, 65_536);
        assert_eq!(tuning.reconciler_interval_ms, 1_000);
        assert_eq!(tuning.session_prune_every_ticks(), 3_600);
        assert_eq!(tuning.output_max_retries, 10);
        assert_eq!(tuning.output_backoff_ms(1), 10_000);
        assert_eq!(tuning.output_backoff_ms(6), 300_000);
        assert_eq!(tuning.hls_idle_timeout_ms, 60_000);
    }

    #[test]
    fn runtime_tuning_prune_cadence_tracks_reconciler_interval() {
        let tuning = RuntimeTuning {
            reconciler_interval_ms: 250,
            ..RuntimeTuning::default()
        };

        assert_eq!(tuning.session_prune_every_ticks(), 14_400);
    }
}
