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
//! │  TS recording writer      MemoryQueue → .ts recording file          │
//! │  FFmpeg transcoder        MemoryQueue → encode → MemoryQueue        │
//! └─────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! Tokio tasks handle all network I/O and coordination. CPU-bound FFmpeg work
//! runs on dedicated OS threads to avoid starving the async runtime. All
//! `std::thread::spawn` calls are wrapped in `catch_unwind` so an FFmpeg panic
//! (e.g., from a corrupt stream) logs an error instead of taking down the process.

// Prevent regression to raw print macros — use tracing macros instead.
// bin/test_harness.rs is exempt (test output is intentional).
#![cfg_attr(not(test), deny(clippy::print_stdout, clippy::print_stderr))]

#[cfg(any(feature = "mcp-http-backend", feature = "mcp-embedded"))]
pub mod agent_backends;
#[cfg(feature = "mcp-core")]
pub mod agent_core;
#[cfg(feature = "agent-execution")]
pub mod agent_execution;
#[cfg(feature = "mcp-core")]
pub mod agent_mcp;
#[cfg(feature = "agent-plane")]
pub mod agent_plane;
pub mod alerts;
pub mod api;
pub mod api_view_models;
pub mod application;
pub mod db;
pub mod diag;
pub mod domain;
pub mod events;
pub mod ffmpeg_extract;
pub mod logging;
pub mod media;
pub mod planner;
pub mod runtime_info;
pub mod test_fixtures;
pub mod types;

use crate::application::output_path::OutputPath;
use crate::application::reconcile::{
    OutputFailureWindow, OutputStartAction, OutputStopAction, RecordingAction,
    collect_needed_stage_keys, decide_output_start_action, decide_output_stop_action,
    decide_recording_action, next_output_retry_count,
};
use crate::domain::stage::StageKey;
use crate::media::engine::MediaEngine;
use futures_util::FutureExt as _;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex as TokioMutex;
use tracing::{error, info, warn};

#[cfg(restream_ffmpeg_needs_avcodec_close_shim)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn avcodec_close(
    _ctx: *mut ffmpeg_next::ffi::AVCodecContext,
) -> std::ffi::c_int {
    // FFmpeg 6+/libavcodec 60 removed this symbol. ffmpeg-next still calls it
    // from decoder drop, but Context::drop frees the codec context via
    // avcodec_free_context, so treating the legacy close step as a no-op keeps
    // linking compatible with newer libavcodec builds.
    0
}

pub struct ServerPorts {
    pub http: u16,
    pub rtmp: u16,
    pub srt: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeTuning {
    pub nofile_limit: u64,
    pub reconciler_interval_ms: u64,
    pub ingest_disconnect_grace_ms: u64,
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
            ingest_disconnect_grace_ms: 5_000,
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
            ingest_disconnect_grace_ms: env_u64(
                "RESTREAM_INGEST_DISCONNECT_GRACE_MS",
                defaults.ingest_disconnect_grace_ms,
            ),
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
        self.output_retry_policy().backoff_ms(retries)
    }

    fn output_retry_policy(&self) -> crate::application::reconcile::OutputRetryPolicy {
        crate::application::reconcile::OutputRetryPolicy {
            max_retries: self.output_max_retries,
            base_ms: self.output_retry_base_ms,
            max_ms: self.output_retry_max_ms,
        }
    }
}

fn next_output_job_id(output_id: &str) -> String {
    format!(
        "job_{output_id}_{}",
        crate::logging::next_correlation_id("attempt")
    )
}

fn persist_runtime_event(event: crate::events::Event) {
    use crate::events::EventKind;

    let seq = event.seq;
    match event.kind {
        EventKind::IngestConnected {
            pipeline_id,
            protocol,
            ..
        } => info!(
            pipeline_id = %pipeline_id,
            event_class = "lifecycle",
            event_type = "ingest.connected",
            protocol = %protocol,
            seq,
            "publisher connected",
        ),
        EventKind::IngestDisconnected {
            pipeline_id,
            protocol,
        } => info!(
            pipeline_id = %pipeline_id,
            event_class = "lifecycle",
            event_type = "ingest.disconnected",
            protocol = %protocol,
            seq,
            "publisher disconnected",
        ),
        EventKind::StageStarted {
            pipeline_id,
            encoding,
        } => info!(
            pipeline_id = %pipeline_id,
            event_class = "lifecycle",
            event_type = "stage.started",
            encoding = %encoding,
            seq,
            "stage started",
        ),
        EventKind::StageStopped {
            pipeline_id,
            encoding,
        } => info!(
            pipeline_id = %pipeline_id,
            event_class = "lifecycle",
            event_type = "stage.stopped",
            encoding = %encoding,
            seq,
            "stage stopped",
        ),
        EventKind::EgressStarted {
            pipeline_id,
            output_id,
        } => info!(
            pipeline_id = %pipeline_id,
            output_id = %output_id,
            event_class = "lifecycle",
            event_type = "egress.started",
            seq,
            "output started",
        ),
        EventKind::EgressStopped {
            pipeline_id,
            output_id,
        } => info!(
            pipeline_id = %pipeline_id,
            output_id = %output_id,
            event_class = "lifecycle",
            event_type = "egress.stopped",
            seq,
            "output stopped",
        ),
        EventKind::EgressFailed {
            pipeline_id,
            output_id,
            phase,
            error: error_message,
        } => error!(
            pipeline_id = %pipeline_id,
            output_id = %output_id,
            event_class = "lifecycle",
            event_type = "egress.failed",
            phase = %phase,
            error = %error_message,
            seq,
            "output failed",
        ),
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
            warn!("failed to raise RLIMIT_NOFILE limit");
        } else {
            info!(limit = limit.rlim_cur, "raised file descriptor limit");
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

    // Init logging before any other tasks so all subsequent tracing macros route
    // through the subscriber. Must be called after the DB pool is ready because
    // the DbLayer drain task needs it.
    let logging_handles = crate::logging::init(pool.clone());

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
    let meta_store = crate::application::ports::SqliteMetaStore::new(pool.clone());
    let srt_ingest_global =
        crate::application::srt_ingest::load_global_srt_ingest_config(&meta_store).await;
    let srt_ingest_pipelines = db::list_pipelines(&pool).await.unwrap_or_default();
    let srt_ingest_policy_store = Arc::new(crate::media::srt::SrtIngestPolicyStore::new(
        srt_ingest_global,
        &srt_ingest_pipelines,
    ));
    let sessions = Arc::new(tokio::sync::RwLock::new(std::collections::HashSet::new()));
    crate::api::initialize_auth(&pool, &sessions).await;
    crate::media::profiles::load_from_db(&pool).await;
    let engine = Arc::new(MediaEngine::new());
    let pipeline_lookup: Arc<dyn crate::application::ports::PipelineLookup> = Arc::new(
        crate::application::ports::SqlitePipelineLookup::new(pool.clone()),
    );
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    engine.set_event_sink(event_tx);
    {
        // Bridge engine state transitions into the persisted app log so the UI
        // can present one operator timeline across ingest, stage, and egress.
        tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                persist_runtime_event(event);
            }
        });
    }
    // Keep a clone of sessions for the reconciler's hourly prune tick.
    let sessions_for_reconciler = sessions.clone();

    let ports = ServerPorts::from_env();

    let media_dir = std::env::var("RESTREAM_MEDIA_DIR").unwrap_or_else(|_| "media".to_string());
    let reconciler_media_dir = media_dir.clone();
    let state = Arc::new(crate::api::AppState {
        db: pool.clone(),
        security: security.clone(),
        ingest_policy_store: srt_ingest_policy_store.clone(),
        sessions,
        engine: engine.clone(),
        ingest_disconnect_grace_ms: tuning.ingest_disconnect_grace_ms,
        ports: crate::api::PortConfig {
            rtmp: ports.rtmp,
            srt: ports.srt,
        },
        media_dir,
        alert_tracker: crate::alerts::AlertTracker::new(),
        log_broadcast: logging_handles.broadcast_tx.clone(),
        #[cfg(feature = "agent-execution")]
        agent_execution: Arc::new(crate::agent_execution::AgentExecutionStore::default()),
    });

    // Start Web Server
    let http_addr = format!("0.0.0.0:{}", ports.http);
    let app = crate::api::create_router(state);
    let listener = tokio::net::TcpListener::bind(&http_addr)
        .await
        .unwrap_or_else(|err| {
            panic!(
                "Failed to bind TCP listener on port {}: {}",
                ports.http, err
            )
        });
    info!(
        event_class = "lifecycle",
        event_type = "restream.http.ready",
        addr = %http_addr,
        "dashboard API server listening",
    );

    tokio::spawn(async move {
        if let Err(e) = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        {
            error!(err = ?e, "axum server error");
        }
    });

    // Start RTMP server — capture handle to detect early exit (M3).
    let security_clone = security.clone();
    let engine_clone = engine.clone();
    let pipeline_lookup_clone = pipeline_lookup.clone();
    let rtmp_port = ports.rtmp;
    let rtmp_handle = tokio::spawn(async move {
        crate::media::rtmp::start_rtmp_server_on(
            pipeline_lookup_clone,
            security_clone,
            engine_clone,
            rtmp_port,
        )
        .await;
        error!("RTMP server task exited unexpectedly");
    });

    // Start SRT server — pass security for rate limiting (H1).
    let srt_server = Arc::new(crate::media::srt::SrtServer::new(
        pipeline_lookup,
        engine.clone(),
        security.clone(),
        srt_ingest_policy_store,
    ));
    let srt_port = ports.srt;
    let srt_handle = tokio::spawn(async move {
        srt_server.run(srt_port).await;
        error!("SRT server task exited unexpectedly");
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
                        if let Err(e) = res { warn!(err = %e, "Ctrl+C error"); }
                    }
                    _ = sigterm.recv() => {}
                }
            }
            #[cfg(not(unix))]
            {
                if let Err(e) = tokio::signal::ctrl_c().await {
                    warn!(err = %e, "Ctrl+C error");
                }
            }
            info!(
                event_class = "lifecycle",
                event_type = "restream.shutdown.requested",
                "signal received — stopping reconciler",
            );
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
            let log_retention_days = std::env::var("RESTREAM_LOG_RETENTION_DAYS")
                .ok()
                .and_then(|v| v.parse::<i64>().ok())
                .unwrap_or(7);
            let _ = db::delete_app_logs_older_than(&pool, log_retention_days).await;
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
                    warn!(err = %e, "failed to list sessions for prune");
                }
            }
        }

        let outputs = match db::list_outputs(&pool).await {
            Ok(o) => o,
            Err(e) => {
                warn!(tick = reconciler_tick, err = %e, "DB error reading outputs");
                continue;
            }
        };

        for output in &outputs {
            let is_active = engine.has_active_egress(&output.id).await;
            if is_active || output.desired_state != "running" {
                engine.clear_egress_retry_state(&output.id).await;
            }
            let has_ingest = engine.has_active_ingest(&output.pipeline_id).await;
            let within_disconnect_grace = engine
                .has_recent_ingest_disconnect(
                    &output.pipeline_id,
                    tuning.ingest_disconnect_grace_ms,
                )
                .await;
            // This grace is only for brief upstream ingest flaps. It keeps
            // healthy egress sessions and shared stages alive while a publisher
            // reconnects, but it is not used for dead push destinations:
            // RTMP/SRT/HLS PUT destination loss still relies on retry/backoff.
            let effective_has_ingest = has_ingest || within_disconnect_grace;
            let now_str = chrono::Utc::now().to_rfc3339();
            let failure = {
                let lf = last_failed.lock().await;
                lf.get(&output.id)
                    .map(|(failed_at, retries)| OutputFailureWindow {
                        retries: *retries,
                        elapsed_ms: failed_at.elapsed().as_millis().min(u128::from(u64::MAX))
                            as u64,
                    })
            };

            match decide_output_start_action(
                &output.desired_state,
                is_active,
                effective_has_ingest,
                failure,
                tuning.output_retry_policy(),
            ) {
                OutputStartAction::NotApplicable => {}
                OutputStartAction::SkipNoIngest => {
                    engine.clear_egress_retry_state(&output.id).await;
                    continue;
                }
                OutputStartAction::MarkFailed => {
                    let mut lf = last_failed.lock().await;
                    lf.remove(&output.id);
                    drop(lf);
                    engine.clear_egress_retry_state(&output.id).await;
                    warn!(
                        correlation_id = %crate::logging::next_correlation_id("out"),
                        output_id = %output.id,
                        output_name = %output.name,
                        max_retries = tuning.output_max_retries,
                        "output exceeded max retries — marking failed",
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
                OutputStartAction::WaitRetry {
                    retries,
                    backoff_ms,
                    remaining_ms,
                } => {
                    engine
                        .update_egress_retry_state(&output.id, retries, backoff_ms, remaining_ms)
                        .await;
                    continue;
                }
                OutputStartAction::StartNow => {
                    let output_correlation_id = crate::logging::next_correlation_id("out");
                    info!(
                        correlation_id = %output_correlation_id,
                        output_id = %output.id,
                        output_name = %output.name,
                        pipeline_id = %output.pipeline_id,
                        event_class = "lifecycle",
                        event_type = "lifecycle.start",
                        "output job started",
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
                    let output_path = OutputPath::resolve(
                        output.pipeline_id.as_str(),
                        &output.encoding,
                        &output.url,
                    );
                    let ingest_video_codec = engine.ingest_video_codec(&output.pipeline_id).await;
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
                    // Pass the ingest codec as override so the video:preset ring is
                    // tagged with the correct codec hint for downstream audio stages
                    // and egress writers (source ring has no hint; active_ingests is
                    // the authoritative source).
                    let ingest_codec_override =
                        output_path.ingest_codec_override(ingest_video_codec.as_deref());

                    // Stage 1: video transcode from source ring (H.265 flows through
                    // directly; build_stage_ffmpeg_args picks libx265 vs libx264 from
                    // the input_codec_override passed into the stage).
                    let video_buf = if let Some(stage) = output_path.video_stage() {
                        engine
                            .get_or_create_transcoder(
                                &output.pipeline_id,
                                stage.kind,
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
                    let pre_h264_buf = if let Some(stage) = output_path.audio_stage() {
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
                    let ring_buf =
                        if output_path.needs_rtmp_h264_conv(ingest_video_codec.as_deref()) {
                            engine
                                .get_or_create_h264_transcoder(
                                    &output.pipeline_id,
                                    output_path.codec_edge_upstream_kind().clone(),
                                    pre_h264_buf,
                                )
                                .await
                        } else {
                            pre_h264_buf
                        };

                    // Register egress and get an attempt-scoped handle so stale
                    // workers cannot later scribble over a replacement session.
                    let registration = engine
                        .register_egress_attempt(&output.id, &output.pipeline_id, &output.url)
                        .await;

                    let job_id = next_output_job_id(&output.id);
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
                    let tuning_c = tuning;
                    let output_correlation_id_c = output_correlation_id.clone();
                    let registration_c = registration.clone();

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
                            let was_current = engine_c
                                .unregister_egress_if_current(&output_id_c, &registration_c)
                                .await;
                            let retry_backoff = if was_current {
                                let mut lf = last_failed_c.lock().await;
                                let retries = next_output_retry_count(
                                    lf.get(&output_id_c).map(|(_, retries)| *retries),
                                    false,
                                );
                                lf.insert(output_id_c.clone(), (Instant::now(), retries));
                                (retries < tuning_c.output_max_retries)
                                    .then_some((retries, tuning_c.output_backoff_ms(retries)))
                            } else {
                                None
                            };
                            if let Some((retries, backoff_ms)) = retry_backoff {
                                engine_c
                                    .update_egress_retry_state(
                                        &output_id_c,
                                        retries,
                                        backoff_ms,
                                        backoff_ms,
                                    )
                                    .await;
                            } else {
                                engine_c.clear_egress_retry_state(&output_id_c).await;
                            }
                            error!(
                                correlation_id = %output_correlation_id_c,
                                output_id = %output_id_c,
                                pipeline_id = %pipeline_id_c,
                                event_class = "lifecycle",
                                event_type = "egress.failed",
                                failure_reason = "unsupported_url_scheme",
                                url = %url_c,
                                "output rejected unsupported URL scheme",
                            );
                            return;
                        }

                        // Wrap the egress call in catch_unwind so a panic does not
                        // prevent the cleanup path below (unregister_egress, job-
                        // status update) from running.
                        let mut hls_persistent_registered = false;
                        let panicked = std::panic::AssertUnwindSafe(async {
                            if url_c.starts_with("rtmp://") || url_c.starts_with("rtmps://") {
                                crate::media::rtmp::start_rtmp_egress(
                                    output_id_c.clone(),
                                    pipeline_id_c.clone(),
                                    url_c.clone(),
                                    ring_buf,
                                    engine_c.clone(),
                                    registration_c.clone(),
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
                                    registration_c.clone(),
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
                                        warn!(
                                            correlation_id = %output_correlation_id_c,
                                            pipeline_id = %pipeline_id_c,
                                            output_id = %output_id_c,
                                            "HLS segmenter token missing — skipping"
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
                                hls_persistent_registered = true;
                                if url_c.starts_with("http://") || url_c.starts_with("https://") {
                                    crate::media::hls_upload::start_hls_put_upload(
                                        output_id_c.clone(),
                                        pipeline_id_c.clone(),
                                        url_c.clone(),
                                        store,
                                        engine_c.clone(),
                                        registration_c.clone(),
                                    )
                                    .await;
                                } else {
                                    engine_c
                                        .update_egress_phase_if_current(
                                            &output_id_c,
                                            &registration_c,
                                            "segmenting",
                                        )
                                        .await;
                                    registration_c.cancel_token.cancelled().await;
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
                            error!(
                                correlation_id = %output_correlation_id_c,
                                output_id = %output_id_c,
                                pipeline_id = %pipeline_id_c,
                                event_class = "lifecycle",
                                event_type = "egress.failed",
                                failure_reason = "panic",
                                "panic in egress task"
                            );
                        }

                        // Cleanup always runs — even after a panic in the egress fn.
                        let is_cancelled = registration_c.cancel_token.is_cancelled();
                        let had_progress = engine_c
                            .egress_has_recorded_progress_if_current(&output_id_c, &registration_c)
                            .await;
                        let was_current = engine_c
                            .unregister_egress_if_current(&output_id_c, &registration_c)
                            .await;
                        if hls_persistent_registered {
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

                        let retry_backoff = if was_current {
                            let mut lf = last_failed_c.lock().await;
                            if is_cancelled {
                                lf.remove(&output_id_c);
                            } else {
                                let retries = next_output_retry_count(
                                    lf.get(&output_id_c).map(|(_, retries)| *retries),
                                    had_progress,
                                );
                                lf.insert(output_id_c.clone(), (Instant::now(), retries));
                            }
                            let retry_backoff = if is_cancelled {
                                None
                            } else {
                                lf.get(&output_id_c).map(|(_, retries)| *retries).and_then(
                                    |retries| {
                                        (retries < tuning_c.output_max_retries).then_some((
                                            retries,
                                            tuning_c.output_backoff_ms(retries),
                                        ))
                                    },
                                )
                            };
                            drop(lf);
                            retry_backoff
                        } else {
                            None
                        };
                        if let Some((retries, backoff_ms)) = retry_backoff {
                            engine_c
                                .update_egress_retry_state(
                                    &output_id_c,
                                    retries,
                                    backoff_ms,
                                    backoff_ms,
                                )
                                .await;
                        } else {
                            engine_c.clear_egress_retry_state(&output_id_c).await;
                        }
                    });
                }
            }

            match decide_output_stop_action(&output.desired_state, is_active, effective_has_ingest)
            {
                OutputStopAction::KeepRunning => {}
                OutputStopAction::StopBecauseIngestLost => {
                    info!(
                        correlation_id = %crate::logging::next_correlation_id("out"),
                        output_id = %output.id,
                        output_name = %output.name,
                        pipeline_id = %output.pipeline_id,
                        event_class = "lifecycle",
                        event_type = "lifecycle.stop",
                        "output job stopped because ingest is no longer active",
                    );
                    engine.unregister_egress(&output.id).await;
                    engine.clear_egress_retry_state(&output.id).await;
                }
                OutputStopAction::StopRequested => {
                    info!(
                        correlation_id = %crate::logging::next_correlation_id("out"),
                        output_id = %output.id,
                        output_name = %output.name,
                        pipeline_id = %output.pipeline_id,
                        event_class = "lifecycle",
                        event_type = "lifecycle.stop",
                        "output job stopped",
                    );
                    engine.unregister_egress(&output.id).await;
                    engine.clear_egress_retry_state(&output.id).await;
                }
            }
        }

        // Clean up unused shared transcoder stages
        {
            let mut stage_inputs = Vec::new();
            for output in &outputs {
                let is_active = engine.has_active_egress(&output.id).await;
                let has_ingest = engine.has_active_ingest(&output.pipeline_id).await;
                let within_disconnect_grace = engine
                    .has_recent_ingest_disconnect(
                        &output.pipeline_id,
                        tuning.ingest_disconnect_grace_ms,
                    )
                    .await;
                let effective_has_ingest = has_ingest || within_disconnect_grace;
                let ingest_video_codec = engine.ingest_video_codec(&output.pipeline_id).await;
                stage_inputs.push(crate::application::reconcile::OutputStageSweepInput {
                    pipeline_id: output.pipeline_id.as_str(),
                    encoding: &output.encoding,
                    url: &output.url,
                    desired_state: &output.desired_state,
                    is_active,
                    effective_has_ingest,
                    ingest_video_codec,
                });
            }
            let needed_stages: std::collections::HashSet<StageKey> =
                collect_needed_stage_keys(stage_inputs);
            engine.sweep_unused_transcoder_stages(&needed_stages).await;
            engine.sweep_unused_stages().await;
        }

        // Reconcile recordings: auto-start/stop based on enabled flag and ingest state
        let pipelines = match db::list_pipelines(&pool).await {
            Ok(p) => p,
            Err(e) => {
                warn!(tick = reconciler_tick, err = %e, "DB error reading pipelines");
                continue;
            }
        };
        for pipeline in pipelines {
            let has_ingest = engine.has_active_ingest(&pipeline.id).await;
            let effective_has_ingest = has_ingest
                || engine
                    .has_recent_ingest_disconnect(&pipeline.id, tuning.ingest_disconnect_grace_ms)
                    .await;

            // Reconcile recordings
            let rec_key = format!("recording_enabled:{}", pipeline.id);
            let rec_enabled = db::get_meta(&pool, &rec_key)
                .await
                .ok()
                .flatten()
                .map(|v| v == "1")
                .unwrap_or(false);
            let rec_active = engine.is_recording_active(&pipeline.id).await;

            match decide_recording_action(rec_enabled, effective_has_ingest, rec_active) {
                RecordingAction::Keep => {}
                RecordingAction::Start => {
                    let ring_buf = engine.get_or_create_pipeline(&pipeline.id).await;
                    let cancel_token = engine.register_recording(&pipeline.id).await;
                    let engine_c = engine.clone();
                    let pid = pipeline.id.clone();
                    let pipe_name = pipeline.name.clone();
                    let input_source = pipeline.input_source.clone();
                    let engine_rec = engine_c.clone();
                    let media_dir_rec = reconciler_media_dir.clone();
                    let recording_settings =
                        crate::media::recording::load_recording_settings(&pool).await;
                    tokio::spawn(async move {
                        crate::media::recording::start_recording(
                            pipe_name,
                            pid.clone(),
                            input_source,
                            media_dir_rec,
                            recording_settings,
                            ring_buf,
                            engine_rec,
                            cancel_token,
                        )
                        .await;
                        engine_c.unregister_recording(&pid).await;
                    });
                }
                RecordingAction::Stop => {
                    engine.unregister_recording(&pipeline.id).await;
                }
            }
        }

        // Sweep idle HLS segmenters after the configured idle timeout
        // or if ingest disconnected.
        let hls_ids = engine.hls_pipeline_ids().await;
        for pid in hls_ids {
            if engine
                .has_recent_ingest_disconnect(&pid, tuning.ingest_disconnect_grace_ms)
                .await
            {
                continue;
            }
            if engine
                .should_shutdown_hls_segmenter(&pid, tuning.hls_idle_timeout_ms)
                .await
            {
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
    info!(
        event_class = "lifecycle",
        event_type = "restream.shutdown.started",
        "shutdown: cancelling all active tasks",
    );
    engine.cancel_all_active_tasks().await;
    engine.shutdown_all_hls_segmenters().await;

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
    info!(
        event_class = "lifecycle",
        event_type = "restream.shutdown.completed",
        "shutdown complete",
    );
}

fn normalize_sbom_for_repo_compare(sbom: &mut serde_json::Value) {
    if let Some(metadata) = sbom
        .get_mut("metadata")
        .and_then(|value| value.as_object_mut())
    {
        metadata.remove("timestamp");
    }
}

pub async fn emit_repo_sbom(path: &Path) -> Result<bool, String> {
    let (_, sbom) = crate::runtime_info::status_and_sbom(false);

    let mut normalized_new = sbom.clone();
    normalize_sbom_for_repo_compare(&mut normalized_new);
    if let Ok(existing) = std::fs::read_to_string(path)
        && let Ok(mut existing_json) = serde_json::from_str::<serde_json::Value>(&existing)
    {
        normalize_sbom_for_repo_compare(&mut existing_json);
        if existing_json == normalized_new {
            return Ok(false);
        }
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            format!(
                "failed to create SBOM directory {}: {error}",
                parent.display()
            )
        })?;
    }
    let bytes = serde_json::to_vec_pretty(&sbom)
        .map_err(|error| format!("failed to serialize SBOM JSON: {error}"))?;
    std::fs::write(
        path,
        format!("{}\n", String::from_utf8_lossy(&bytes)).as_bytes(),
    )
    .map_err(|error| format!("failed to write SBOM file {}: {error}", path.display()))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_sbom_compare_ignores_timestamp() {
        let mut left = serde_json::json!({
            "metadata": { "timestamp": "2026-06-28T01:00:00Z" },
            "components": [{ "name": "restream" }]
        });
        let mut right = serde_json::json!({
            "metadata": { "timestamp": "2026-06-29T01:00:00Z" },
            "components": [{ "name": "restream" }]
        });

        normalize_sbom_for_repo_compare(&mut left);
        normalize_sbom_for_repo_compare(&mut right);

        assert_eq!(left, right);
    }

    #[test]
    fn runtime_tuning_defaults_preserve_existing_operational_behavior() {
        let tuning = RuntimeTuning::default();

        assert_eq!(tuning.nofile_limit, 65_536);
        assert_eq!(tuning.reconciler_interval_ms, 1_000);
        assert_eq!(tuning.ingest_disconnect_grace_ms, 5_000);
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

    #[test]
    fn output_job_ids_are_unique_per_attempt() {
        let first = next_output_job_id("out-1");
        let second = next_output_job_id("out-1");

        assert!(first.starts_with("job_out-1_"));
        assert!(second.starts_with("job_out-1_"));
        assert_ne!(first, second);
    }
}
