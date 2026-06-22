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

pub mod api;
pub mod db;
pub mod diag;
pub mod media;
pub mod runtime_info;
pub mod types;

use crate::media::engine::MediaEngine;
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex as TokioMutex;

pub struct ServerPorts {
    pub http: u16,
    pub rtmp: u16,
    pub srt: u16,
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

fn set_rlimit() {
    unsafe {
        let limit = libc::rlimit {
            rlim_cur: 65536,
            rlim_max: 65536,
        };
        if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) != 0 {
            eprintln!("[system] Failed to raise RLIMIT_NOFILE limit");
        } else {
            println!("[system] Successfully raised file descriptor limit to 65536");
        }
    }
}

pub async fn run_app() {
    // Elevate limits for high fd count (500+ egress streams)
    set_rlimit();

    // Initialize database
    let db_url = "sqlite:data.db?mode=rwc";
    let pool = SqlitePool::connect(db_url)
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
    let engine = Arc::new(MediaEngine::new());

    let ports = ServerPorts::from_env();

    let state = Arc::new(crate::api::AppState {
        db: pool.clone(),
        security: security.clone(),
        sessions,
        engine: engine.clone(),
        ports: crate::api::PortConfig {
            rtmp: ports.rtmp,
            srt: ports.srt,
        },
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
        if let Err(e) = axum::serve(listener, app).await {
            eprintln!("[web] Axum server error: {:?}", e);
        }
    });

    // Start RTMP server
    let db_clone = pool.clone();
    let security_clone = security.clone();
    let engine_clone = engine.clone();
    let rtmp_port = ports.rtmp;
    tokio::spawn(async move {
        crate::media::rtmp::start_rtmp_server_on(db_clone, security_clone, engine_clone, rtmp_port)
            .await;
    });

    // Start SRT server
    let srt_server = Arc::new(crate::media::srt::SrtServer::new(
        pool.clone(),
        engine.clone(),
    ));
    let srt_port = ports.srt;
    tokio::spawn(async move {
        srt_server.run(srt_port).await;
    });

    // Run reconciliation loop
    let last_failed: Arc<TokioMutex<HashMap<String, Instant>>> =
        Arc::new(TokioMutex::new(HashMap::new()));
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;

        let outputs = match db::list_outputs(&pool).await {
            Ok(o) => o,
            Err(_) => continue,
        };

        for output in outputs {
            let is_active = engine
                .egress_cancel_tokens
                .read()
                .await
                .contains_key(&output.id);
            let now_str = chrono::Utc::now().to_rfc3339();

            if output.desired_state == "running" && !is_active {
                // Check backoff if it failed recently
                let mut lf = last_failed.lock().await;
                if let Some(&failed_at) = lf.get(&output.id) {
                    if failed_at.elapsed() < Duration::from_secs(5) {
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

                // Two-stage transcoding: video transcode (shared) → audio filter (if needed)
                //
                // Stage 1 (video): keyed on video preset only.
                //   720p, 720p+atrack:0,1, 720p+remap:0:1 all share one 720p encoder.
                //   All audio streams are carried through (passthrough).
                //
                // Stage 2 (audio): keyed on video_preset + audio_routing.
                //   Cheap remux that copies video and selects/filters audio.
                //   Key includes upstream video preset to prevent cross-contamination:
                //   720p+atrack:0 and 1080p+atrack:0 must NOT share an audio stage.
                //
                // See docs/media-pipeline.md for rationale.
                let encoding = output.encoding.clone();
                let video_preset = encoding.split('+').next().unwrap_or("source");
                let audio_part = encoding.split('+').nth(1);

                // Standard RTMP does not support H.265 — auto-transcode to H.264
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
                let needs_h264_transcode = is_rtmp
                    && ingest_is_hevc
                    && (video_preset == "source" || video_preset.is_empty());

                let needs_video_transcode = needs_h264_transcode
                    || (!video_preset.is_empty()
                        && video_preset != "source"
                        && video_preset != "custom");

                // Stage 1: shared video transcode (or passthrough)
                let effective_preset = if needs_h264_transcode {
                    "h264"
                } else {
                    video_preset
                };
                let video_stage_key = if needs_video_transcode {
                    effective_preset
                } else {
                    "source"
                };
                let video_buf = if needs_video_transcode {
                    engine
                        .get_or_create_transcoder(
                            &output.pipeline_id,
                            &format!("video:{}", effective_preset),
                            source_buf.clone(),
                        )
                        .await
                } else {
                    source_buf
                };

                // Stage 2: audio filter (reads from video stage output)
                // Key includes upstream: "audio:atrack:0:from:720p" not just "atrack:0"
                let ring_buf = if let Some(audio) = audio_part {
                    if !audio.is_empty() {
                        let audio_key = format!("audio:{}:from:{}", audio, video_stage_key);
                        engine
                            .get_or_create_transcoder(&output.pipeline_id, &audio_key, video_buf)
                            .await
                    } else {
                        video_buf
                    }
                } else {
                    video_buf
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

                // Spawn the specific egress client
                let engine_c = engine.clone();
                let output_id_c = output.id.clone();
                let pipeline_id_c = output.pipeline_id.clone();
                let url_c = output.url.clone();
                let pool_c = pool.clone();
                let last_failed_c = last_failed.clone();

                tokio::spawn(async move {
                    if url_c.starts_with("rtmp://") {
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
                            url_c.clone(),
                            ring_buf,
                            engine_c.clone(),
                            cancel_token.clone(),
                        )
                        .await;
                    } else if url_c.starts_with("hls://") || url_c.starts_with("http://") || url_c.starts_with("https://") {
                        // HLS egress: use the shared segmenter, register as persistent consumer
                        let (store, already_running) =
                            engine_c.ensure_hls_segmenter(&pipeline_id_c).await;
                        if !already_running {
                            let hls_cancel =
                                engine_c.get_hls_cancel_token(&pipeline_id_c).await.unwrap();
                            let eng2 = engine_c.clone();
                            let pid2 = pipeline_id_c.clone();
                            let rb2 = ring_buf.clone();
                            tokio::spawn(async move {
                                crate::media::hls::start_hls_segmenter(
                                    pid2.clone(),
                                    store,
                                    rb2,
                                    eng2.clone(),
                                    hls_cancel,
                                )
                                .await;
                                eng2.shutdown_hls_segmenter(&pid2).await;
                            });
                        }
                        engine_c.add_hls_persistent_consumer(&pipeline_id_c).await;
                        cancel_token.cancelled().await;
                        engine_c
                            .remove_hls_persistent_consumer(&pipeline_id_c)
                            .await;
                    } else {
                        // Unsupported URL scheme fallback rejection.
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
                        last_failed_c
                            .lock()
                            .await
                            .insert(output_id_c, Instant::now());
                        return;
                    }

                    // On terminate, clean up and register failure if cancelled without operator intent
                    let is_cancelled = cancel_token.is_cancelled();
                    engine_c.unregister_egress(&output_id_c).await;

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
                        Some(&output.pipeline_id),
                        Some(&output_id_c),
                        "lifecycle.stop",
                        None,
                        &end_now,
                        &format!("[lifecycle] Output job exited with status: {}", job_status),
                    )
                    .await;

                    if !is_cancelled {
                        last_failed_c
                            .lock()
                            .await
                            .insert(output_id_c, Instant::now());
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

        // Reconcile recordings: auto-start/stop based on enabled flag and ingest state
        let pipelines = match db::list_pipelines(&pool).await {
            Ok(p) => p,
            Err(_) => continue,
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
                tokio::spawn(async move {
                    crate::media::recording::start_recording(
                        pipe_name,
                        pid.clone(),
                        "media".to_string(),
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

        // Sweep idle HLS segmenters: shut down if no consumers for 60s
        // or if ingest disconnected.
        let hls_ids: Vec<String> = engine.hls_consumers.read().await.keys().cloned().collect();
        for pid in hls_ids {
            let has_ingest = engine.active_ingests.read().await.contains_key(&pid);
            let idle = {
                let consumers = engine.hls_consumers.read().await;
                match consumers.get(&pid) {
                    Some(c) => !has_ingest || c.is_idle(60_000),
                    None => false,
                }
            };
            if idle {
                engine.shutdown_hls_segmenter(&pid).await;
            }
        }
    }
}
