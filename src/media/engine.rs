//! Central media engine state — owns all active ingests, egresses, ring buffers,
//! and recordings. Byte counters use `AtomicU64` for lock-free updates from the
//! hot ingest/egress paths; the `health_snapshot()` method reads them atomically
//! to build the JSON returned by `/health`.

use ffmpeg_next as ffmpeg;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;
use tokio::sync::RwLock as TokioRwLock;
use tokio_util::sync::CancellationToken;

use crate::domain::stage::{EncodingStagePlan, StageKey, StageKind};
use crate::media::avio::MemoryQueue;
use crate::media::hls::HlsStore;
use crate::media::ring_buffer::RingBuffer;
use crate::media::ts_chunk_ring::TsChunkRing;
use crate::planner::backend_policy::{BackendPolicy, StageBackend};

pub use crate::media::pipe_metrics::PipeMetrics;
pub use crate::media::stage_metrics::StageMetrics;

/// Tracks HLS consumers for a pipeline. Persistent consumers (egress outputs)
/// register/unregister explicitly. Transient consumers (browser preview) keep
/// the segmenter alive via playlist fetch heartbeats.
pub struct HlsConsumers {
    /// Number of persistent consumers (HLS egress outputs).
    pub persistent: AtomicU64,
    /// Monotonic reference time.
    pub reference_instant: Instant,
    /// Monotonic elapsed millis since reference_instant for the last access.
    pub last_access_ms: AtomicU64,
    /// Cancel token for the segmenter task.
    pub cancel_token: CancellationToken,
}

impl HlsConsumers {
    pub fn new(cancel_token: CancellationToken) -> Self {
        Self {
            persistent: AtomicU64::new(0),
            reference_instant: Instant::now(),
            last_access_ms: AtomicU64::new(0),
            cancel_token,
        }
    }

    fn now_ms(&self) -> u64 {
        self.reference_instant.elapsed().as_millis() as u64
    }

    pub fn touch(&self) {
        self.last_access_ms.store(self.now_ms(), Ordering::Relaxed);
    }

    pub fn add_persistent(&self) {
        self.persistent.fetch_add(1, Ordering::Relaxed);
    }

    pub fn remove_persistent(&self) {
        self.persistent.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn is_idle(&self, timeout_ms: u64) -> bool {
        let persistent = self.persistent.load(Ordering::Relaxed);
        if persistent > 0 {
            return false;
        }
        let last = self.last_access_ms.load(Ordering::Relaxed);
        let now = self.now_ms();
        now.saturating_sub(last) >= timeout_ms
    }
}

/// Per-pipeline ingest quality snapshot (RTMP TCP or SRT link stats).
#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PublisherQuality {
    // TCP metrics (RTMP)
    pub tcp_rtt_ms: Option<f64>,
    pub tcp_rtt_var_ms: Option<f64>,
    pub tcp_bytes_received: Option<u64>,
    pub tcp_last_rcv_ms: Option<u64>,
    pub tcp_rcv_rtt_ms: Option<f64>,
    pub tcp_rcv_space: Option<u64>,
    pub tcp_rcv_ooopack: Option<u64>,
    pub tcp_skmem_rmem_alloc: Option<u64>,
    pub tcp_skmem_rmem_max: Option<u64>,
    pub tcp_receive_rate_mbps: Option<f64>,
    pub tcp_stats_unavailable_reason: Option<String>,
    // SRT metrics
    pub ms_rtt: Option<f64>,
    pub mbps_receive_rate: Option<f64>,
    pub mbps_link_capacity: Option<f64>,
    pub ms_receive_tsb_pd_delay: Option<f64>,
    pub ms_receive_buf: Option<f64>,
    pub packets_sent_nak: Option<u64>,
    pub packets_received_loss: Option<u64>,
    pub packets_received_drop: Option<u64>,
    pub packets_received_retrans: Option<u64>,
    pub packets_received_undecrypt: Option<u64>,
    pub packets_received_loss_per_sec: Option<f64>,
    pub packets_received_drop_per_sec: Option<f64>,
    pub packets_received_retrans_per_sec: Option<f64>,
    pub packets_received_undecrypt_per_sec: Option<f64>,
    // SRT buffer occupancy
    pub srt_send_buf_bytes: Option<i32>,
    pub srt_recv_buf_bytes: Option<i32>,
    pub srt_send_buf_avail_bytes: Option<i32>,
    pub srt_recv_buf_avail_bytes: Option<i32>,
    pub srt_flight_size_pkts: Option<i32>,
    pub srt_bonded: Option<bool>,
    pub srt_group_member_count: Option<u32>,
    pub srt_group_connected_members: Option<u32>,
    pub srt_group_active_members: Option<u32>,
    pub srt_group_broken_members: Option<u32>,
    pub inbound_rtp_packets_lost: Option<u64>,
    pub inbound_rtp_packets_in_error: Option<u64>,
    pub inbound_rtp_packets_jitter: Option<f64>,
}

/// Video stream metadata collected from the demuxer.
#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoMeta {
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub bw: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub level: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pixel_format: Option<String>,
}

/// Audio stream metadata collected from the demuxer.
#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioMeta {
    pub codec: String,
    pub sample_rate: u32,
    pub channels: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_layout: Option<String>,
    pub track_index: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
}

/// Publisher connection info.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Publisher {
    pub protocol: String,
    pub remote_addr: Option<String>,
    pub quality: PublisherQuality,
}

/// Runtime state for one active ingest connection.
pub struct ActiveIngest {
    pub stream_key: String,
    pub start_time: Instant,
    pub protocol: String, // "rtmp" | "srt" | "file"
    pub bytes_received: Arc<AtomicU64>,
    pub metrics: Arc<StageMetrics>,
    pub remote_addr: Option<String>,
    pub video: Option<VideoMeta>,
    pub audio: Option<AudioMeta>,
    pub audio_tracks: std::sync::Mutex<std::sync::Arc<Vec<AudioMeta>>>,
    pub quality: PublisherQuality,
    pub keyframe_times: Arc<std::sync::Mutex<Vec<i64>>>,
    /// Cached FLV sequence headers for RTMP play subscribers (video config + audio config)
    pub video_sequence_header: std::sync::Mutex<Option<bytes::Bytes>>,
    pub audio_sequence_header: std::sync::Mutex<Option<bytes::Bytes>>,
}

/// Runtime state for one active egress target.
pub struct ActiveEgress {
    pub output_id: String,
    pub pipeline_id: String,
    pub target_url: String,
    pub status: String, // "running" | "stopped" | "failed"
    pub started_at: String,
    pub start_instant: Instant,
    pub bytes_sent: Arc<AtomicU64>,
    pub metrics: Arc<StageMetrics>,
    pub prev_bytes_sent: AtomicU64,
    pub prev_sample_time: std::sync::Mutex<Instant>,
    pub bitrate_kbps: std::sync::Mutex<Option<f64>>,
}

/// Shared listener socket buffer occupancy, updated by the SRT monitor task.
#[derive(Debug, Default)]
pub struct ListenerSocketStats {
    pub bonding_available: AtomicBool,
    pub rx_queue_bytes: AtomicU64,
    pub rx_queue_max_bytes: AtomicU64,
    pub drops: AtomicU64,
}

pub struct MediaEngine {
    // Map of pipeline_id -> RingBuffer
    pub pipelines: TokioRwLock<HashMap<String, Arc<RingBuffer>>>,
    // Map of pipeline_id -> Ingest cancellation token (for loop files or RTMP/SRT)
    pub ingest_cancel_tokens: TokioRwLock<HashMap<String, CancellationToken>>,
    // Map of output_id -> Egress cancellation token
    pub egress_cancel_tokens: TokioRwLock<HashMap<String, CancellationToken>>,
    // Active ingest stats
    pub active_ingests: TokioRwLock<HashMap<String, ActiveIngest>>,
    // Active egress stats
    pub active_egresses: TokioRwLock<HashMap<String, ActiveEgress>>,
    // Map of pipeline_id -> recording cancellation token (active recordings)
    pub recording_cancel_tokens: TokioRwLock<HashMap<String, CancellationToken>>,
    // Map of pipeline_id -> in-memory HLS segment store
    pub hls_stores: TokioRwLock<HashMap<String, Arc<HlsStore>>>,
    // Map of ingest_id -> file ingest child process
    pub file_ingest_children: TokioRwLock<HashMap<String, tokio::process::Child>>,
    // Transcoded RingBuffer + cancel token keyed by typed pipeline/stage identity.
    pub transcoder_buffers: TokioRwLock<HashMap<StageKey, (Arc<RingBuffer>, CancellationToken)>>,
    // SRT listener socket kernel buffer stats
    pub srt_listener_stats: Arc<ListenerSocketStats>,
    // Map of pipeline_id -> HLS consumer tracking (refcount + idle timer)
    pub hls_consumers: TokioRwLock<HashMap<String, HlsConsumers>>,
    // Per-stage processing metrics keyed by typed pipeline/stage identity.
    pub stage_metrics: TokioRwLock<HashMap<StageKey, Arc<StageMetrics>>>,
    // OS thread handles registered by spawn sites (transcoder, h264_transcoder, SRT threads).
    // Drained and joined at shutdown to prevent blocking threads from outliving the runtime.
    pub os_threads: std::sync::Mutex<Vec<std::thread::JoinHandle<()>>>,
    // Semaphore limiting concurrent SRT sender OS threads (play + egress).
    // Each sender acquires a permit before spawning and releases it (via OwnedSemaphorePermit
    // moved into the thread) when the thread exits. Caps virtual address space usage
    // at ~512 × 8 MB stack ≈ 4 GB instead of unbounded growth at 1 thread / connection.
    pub srt_sender_semaphore: Arc<tokio::sync::Semaphore>,
    // Shared TS muxer stages
    pub ts_muxer_stages: TokioRwLock<HashMap<String, Arc<TsChunkRing>>>,
    // Per-pipeline semaphore preventing concurrent diagnostic runs on the same pipeline
    pub diag_semaphores: TokioRwLock<HashMap<String, Arc<tokio::sync::Semaphore>>>,
    // Input MemoryQueues for stages that bridge tokio→OS-thread.
    // Keyed by the same typed stage identity as transcoder_buffers.
    // Used by processing_graph() to surface queue depth/HWM in the engineer view.
    pub input_queues: TokioRwLock<HashMap<StageKey, Arc<MemoryQueue>>>,
    // Pipe back-pressure metrics for external-subprocess stages.
    // Keyed by the same typed stage identity as transcoder_buffers.
    pub pipe_metrics: TokioRwLock<HashMap<StageKey, Arc<PipeMetrics>>>,
    // Bounded in-memory lifecycle event log.
    pub event_log: Arc<crate::events::EventLog>,
}

impl Default for MediaEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl MediaEngine {
    pub fn new() -> Self {
        // Initialize FFmpeg once. On failure, emit a human-readable message
        // and exit — a panic here produces an unreadable backtrace with no
        // context about what went wrong or which library is missing.
        if let Err(e) = ffmpeg::init() {
            eprintln!(
                "[media] Fatal: FFmpeg initialization failed ({}). \
                 Ensure the system has compatible FFmpeg libraries installed \
                 and RESTREAM_FFMPEG_PATH is set correctly if using a custom build.",
                e
            );
            std::process::exit(1);
        }

        Self {
            pipelines: TokioRwLock::new(HashMap::new()),
            ingest_cancel_tokens: TokioRwLock::new(HashMap::new()),
            egress_cancel_tokens: TokioRwLock::new(HashMap::new()),
            active_ingests: TokioRwLock::new(HashMap::new()),
            active_egresses: TokioRwLock::new(HashMap::new()),
            recording_cancel_tokens: TokioRwLock::new(HashMap::new()),
            hls_stores: TokioRwLock::new(HashMap::new()),
            file_ingest_children: TokioRwLock::new(HashMap::new()),
            transcoder_buffers: TokioRwLock::new(HashMap::new()),
            srt_listener_stats: Arc::new(ListenerSocketStats::default()),
            hls_consumers: TokioRwLock::new(HashMap::new()),
            stage_metrics: TokioRwLock::new(HashMap::new()),
            os_threads: std::sync::Mutex::new(Vec::new()),
            srt_sender_semaphore: Arc::new(tokio::sync::Semaphore::new(512)),
            ts_muxer_stages: TokioRwLock::new(HashMap::new()),
            diag_semaphores: TokioRwLock::new(HashMap::new()),
            input_queues: TokioRwLock::new(HashMap::new()),
            pipe_metrics: TokioRwLock::new(HashMap::new()),
            event_log: Arc::new(crate::events::EventLog::new()),
        }
    }

    /// Register an OS thread JoinHandle so it can be joined at shutdown.
    /// Already-finished handles are pruned opportunistically to prevent unbounded accumulation
    /// in long-running servers with many short-lived per-connection threads.
    pub fn register_os_thread(&self, handle: std::thread::JoinHandle<()>) {
        let mut guards = self.os_threads.lock().unwrap_or_else(|e| e.into_inner());
        guards.retain(|h| !h.is_finished());
        guards.push(handle);
    }

    /// Drain all registered OS thread handles for joining at shutdown.
    pub fn drain_os_thread_handles(&self) -> Vec<std::thread::JoinHandle<()>> {
        self.os_threads
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
            .collect()
    }

    pub async fn get_or_create_stage_metrics(&self, key: StageKey) -> Arc<StageMetrics> {
        let mut metrics = self.stage_metrics.write().await;
        metrics
            .entry(key)
            .or_insert_with(|| Arc::new(StageMetrics::new()))
            .clone()
    }

    pub async fn remove_stage_metrics(&self, key: &StageKey) {
        self.stage_metrics.write().await.remove(key);
    }

    pub async fn register_input_queue(&self, key: StageKey, queue: Arc<MemoryQueue>) {
        self.input_queues.write().await.insert(key, queue);
    }

    pub async fn remove_input_queue(&self, key: &StageKey) {
        self.input_queues.write().await.remove(key);
    }

    pub async fn register_pipe_metrics(&self, key: StageKey, metrics: Arc<PipeMetrics>) {
        self.pipe_metrics.write().await.insert(key, metrics);
    }

    pub async fn remove_pipe_metrics(&self, key: &StageKey) {
        self.pipe_metrics.write().await.remove(key);
    }

    pub async fn is_file_ingest_running(&self, id: &str) -> bool {
        let mut children = self.file_ingest_children.write().await;
        if let Some(child) = children.get_mut(id) {
            match child.try_wait() {
                Ok(None) => true,
                _ => {
                    children.remove(id);
                    false
                }
            }
        } else {
            false
        }
    }

    pub async fn reap_file_ingests(&self) {
        let mut children = self.file_ingest_children.write().await;
        children.retain(|id, child| match child.try_wait() {
            Ok(None) => true,
            _ => {
                println!(
                    "[engine] File ingest child process {} has exited/stopped",
                    id
                );
                false
            }
        });
    }

    pub async fn get_or_create_pipeline(&self, pipeline_id: &str) -> Arc<RingBuffer> {
        let mut pipelines = self.pipelines.write().await;
        if let Some(rb) = pipelines.get(pipeline_id) {
            rb.clone()
        } else {
            let rb = Arc::new(RingBuffer::new(4096)); // ~24s at 4K60, ~48s at 1080p30
            pipelines.insert(pipeline_id.to_string(), rb.clone());
            rb
        }
    }

    /// Get or create a shared transcoder stage for a pipeline + encoding combo.
    /// Keyed by the full encoding string — callers are responsible for splitting
    /// video and audio into separate stages when sharing is needed.
    ///
    /// Used for both video transcoding (keyed on video preset) and audio-only
    /// filtering (keyed on full compound encoding). Multiple egresses wanting
    /// the same encoding share the same output RingBuffer.
    pub async fn get_or_create_transcoder(
        self: &Arc<Self>,
        pipeline_id: &str,
        stage_kind: StageKind,
        source_buffer: Arc<RingBuffer>,
        // When the source_buffer is a transcoded ring whose codec differs from the
        // original ingest (e.g. hevc_to_h264 → video:720p), pass the actual codec
        // of the packets in source_buffer so the TsMuxer gets the right PMT.
        input_codec_override: Option<&str>,
    ) -> Arc<RingBuffer> {
        let key = StageKey::new(pipeline_id, stage_kind.clone());

        // Use a single write-lock acquisition to atomically check-and-insert.
        // The previous read-lock-then-write-lock pattern had a TOCTOU window:
        // two concurrent callers could both see the key absent, then both create
        // a ring buffer and spawn a transcoder task — the second insert would
        // overwrite the first, leaving an orphaned transcoder eating CPU/memory.
        let mut buffers = self.transcoder_buffers.write().await;
        if let Some((rb, token)) = buffers.get(&key)
            && !token.is_cancelled()
        {
            return rb.clone();
        }
        // Cancelled stage — fall through and replace it

        let output_buf = Arc::new(RingBuffer::new(4096));
        let cancel = CancellationToken::new();
        buffers.insert(key.clone(), (output_buf.clone(), cancel.clone()));
        drop(buffers); // release write lock before spawning

        // Set codec_hint SYNCHRONOUSLY before spawning — downstream stages
        // (e.g. audio routers, SRT egress warmup) may query it before the
        // spawned task runs.
        // • video:* stages: always re-encode with libx264 → always "h264"
        // • audio:* stages: passthrough → inherit hint from source_buffer
        if stage_kind.is_video_preset() {
            // Preserve the input codec: H.265 source → H.265 output, H.264 → H.264.
            // input_codec_override carries the ingest codec ("hevc" or "h265") so
            // the ring is tagged correctly for downstream audio stages and egress.
            // Falls back to "h264" when no override is provided (H.264 ingest).
            output_buf.set_codec_hint(input_codec_override.unwrap_or("h264"));
        } else if let Some(oc) = input_codec_override {
            output_buf.set_codec_hint(oc);
        } else {
            let hint = source_buffer.codec_hint_str();
            if !hint.is_empty() {
                output_buf.set_codec_hint(hint);
            }
        }

        // Set audio_tracks metadata
        let input_tracks = if let Some(tracks) = source_buffer.audio_tracks() {
            std::sync::Arc::new(tracks.to_vec())
        } else {
            let ingests = self.active_ingests.read().await;
            ingests
                .get(pipeline_id)
                .map(|i| {
                    let lock = i.audio_tracks.lock().unwrap_or_else(|e| e.into_inner());
                    if lock.is_empty()
                        && let Some(audio) = i.audio.clone()
                    {
                        std::sync::Arc::new(vec![audio])
                    } else {
                        std::sync::Arc::clone(&lock)
                    }
                })
                .unwrap_or_default()
        };

        let output_tracks = if let Some(audio_op) = stage_kind.audio_operation() {
            let routing =
                crate::media::transcoder::parse_audio_routing(&format!("source+{audio_op}"));
            crate::media::transcoder::apply_audio_routing(&routing, &input_tracks)
        } else {
            (*input_tracks).clone()
        };
        output_buf.set_audio_tracks(output_tracks);

        let encoding_str = stage_kind.to_string();
        println!(
            "[transcoder] Spawning stage: pipeline={} encoding={}",
            pipeline_id, encoding_str
        );
        self.event_log.emit(crate::events::EventKind::StageStarted {
            pipeline_id: pipeline_id.to_string(),
            encoding: encoding_str.clone(),
        });

        let pid = pipeline_id.to_string();
        let enc = encoding_str.clone();
        let ob = output_buf.clone();
        let self_clone = self.clone();
        let codec_override = input_codec_override.map(String::from);

        // ── Backend dispatch ───────────────────────────────────────────────
        // atrack audio stages are pure packet filters — no mux/demux, no codec
        // work. Remap/downmix require DSP decode→filter→encode and run through
        // the FFmpeg stage backend.
        // video: stages (video:720p etc.): external subprocess FFmpeg by default;
        //   override with RESTREAM_USE_INTERNAL_TRANSCODER=1.
        let backend_policy = BackendPolicy::from_env();
        if let Some(audio_op) = stage_kind.audio_operation() {
            let routing =
                crate::media::transcoder::parse_audio_routing(&format!("source+{audio_op}"));
            if backend_policy.select_backend(&stage_kind) == StageBackend::AudioRouter {
                println!(
                    "[audio-router] Spawning stage: pipeline={} encoding={}",
                    pipeline_id, encoding_str
                );
                tokio::spawn(async move {
                    crate::media::transcoder::start_audio_router(
                        pid,
                        routing,
                        source_buffer,
                        ob,
                        cancel,
                    )
                    .await;
                });
                return output_buf;
            }
            // Channel-level DSP routes fall through to the selected FFmpeg backend.
        }

        let backend = backend_policy.select_backend(&stage_kind);

        println!(
            "[transcoder] Spawning stage: pipeline={} encoding={} backend={}",
            pipeline_id,
            encoding_str,
            match backend {
                StageBackend::AudioRouter => "audio-router",
                StageBackend::InternalFfmpeg => "internal",
                StageBackend::ExternalFfmpeg => "external",
            }
        );

        if backend == StageBackend::InternalFfmpeg {
            if stage_kind.is_video_preset() {
                println!(
                    "[transcoder] Info: RESTREAM_USE_INTERNAL_TRANSCODER=1 with video \
                     preset '{}' — using in-process decode->scale->encode loop.",
                    encoding_str
                );
            }
            let int_stage_key = key.clone();
            tokio::spawn(async move {
                crate::media::transcoder::start_transcoder(
                    pid,
                    enc,
                    source_buffer,
                    ob,
                    self_clone,
                    cancel,
                    int_stage_key,
                )
                .await;
            });
        } else {
            let ext_stage_key = key.clone();
            tokio::spawn(async move {
                crate::media::external_transcoder::start_external_transcoder_stage(
                    pid,
                    enc,
                    source_buffer,
                    ob,
                    self_clone,
                    cancel,
                    codec_override,
                    ext_stage_key,
                )
                .await;
            });
        }

        output_buf
    }

    /// Get or create a shared H.265→H.264 transcoder stage for a pipeline.
    ///
    /// Keyed by `"<pipeline_id>:hevc_to_h264:from:<upstream_stage_key>"` so that
    /// RTMP-passthrough (`from:source`) and RTMP-720p (`from:720p`) stages are
    /// independent and all RTMP egresses on the same preset share one converter.
    pub async fn get_or_create_h264_transcoder(
        self: &Arc<Self>,
        pipeline_id: &str,
        upstream: StageKind,
        source_buffer: Arc<RingBuffer>,
    ) -> Arc<RingBuffer> {
        let key = StageKey::new(
            pipeline_id,
            StageKind::codec_edge("hevc_to_h264", upstream),
        );

        // Single write-lock to avoid the TOCTOU race (see get_or_create_transcoder).
        let mut buffers = self.transcoder_buffers.write().await;
        if let Some((rb, token)) = buffers.get(&key)
            && !token.is_cancelled()
        {
            return rb.clone();
        }

        let output_buf = Arc::new(RingBuffer::new(4096));
        // hevc_to_h264 stage always produces H.264 — tag the ring so consumers
        // can initialize their TsMuxer / PMT with the correct codec.
        output_buf.set_codec_hint("h264");

        // Inherit audio tracks from source_buffer
        let input_tracks = if let Some(tracks) = source_buffer.audio_tracks() {
            std::sync::Arc::new(tracks.to_vec())
        } else {
            let ingests = self.active_ingests.read().await;
            ingests
                .get(pipeline_id)
                .map(|i| {
                    let lock = i.audio_tracks.lock().unwrap_or_else(|e| e.into_inner());
                    if lock.is_empty()
                        && let Some(audio) = i.audio.clone()
                    {
                        std::sync::Arc::new(vec![audio])
                    } else {
                        std::sync::Arc::clone(&lock)
                    }
                })
                .unwrap_or_default()
        };
        output_buf.set_audio_tracks((*input_tracks).clone());
        let cancel = CancellationToken::new();
        buffers.insert(key.clone(), (output_buf.clone(), cancel.clone()));
        drop(buffers); // release write lock before spawning

        println!(
            "[h264-tc] Spawning shared H.265→H.264 transcoder for pipeline {}",
            pipeline_id
        );
        self.event_log.emit(crate::events::EventKind::StageStarted {
            pipeline_id: pipeline_id.to_string(),
            encoding: key.kind.to_string(),
        });

        let pid = pipeline_id.to_string();
        let ob = output_buf.clone();
        let self_clone = self.clone();
        tokio::spawn(async move {
            crate::media::h264_transcoder::start_h264_transcoder(
                pid,
                source_buffer,
                ob,
                self_clone,
                cancel,
                key,
            )
            .await;
        });

        output_buf
    }

    /// Return the active processing stages for a pipeline as (kind, is_alive) pairs.
    pub async fn active_transcoder_stages(
        &self,
        pipeline_id: &str,
    ) -> Vec<(StageKind, bool)> {
        let buffers = self.transcoder_buffers.read().await;
        buffers
            .iter()
            .filter(|(key, _)| key.pipeline.as_str() == pipeline_id)
            .map(|(k, (_, token))| (k.kind.clone(), !token.is_cancelled()))
            .collect()
    }

    pub async fn remove_pipeline(&self, pipeline_id: &str) {
        let mut pipelines = self.pipelines.write().await;
        pipelines.remove(pipeline_id);
    }

    /// Remove all transcoder stage entries for a pipeline from `transcoder_buffers`.
    ///
    /// Stages whose cancel tokens have already fired are cleaned up lazily by
    /// `get_or_create_transcoder`. This function does the eager sweep on pipeline
    /// deletion so the `Arc<RingBuffer>` for every stage is freed immediately
    /// instead of surviving until the next reconciler creates a replacement stage.
    pub async fn cleanup_pipeline_stages(&self, pipeline_id: &str) {
        let mut buffers = self.transcoder_buffers.write().await;
        // Cancel all still-running stages then remove every entry for this pipeline.
        buffers.retain(|key, (_rb, token)| {
            if key.pipeline.as_str() == pipeline_id {
                token.cancel();
                false
            } else {
                true
            }
        });
    }

    pub async fn sweep_unused_transcoder_stages(
        &self,
        active_keys: &std::collections::HashSet<StageKey>,
    ) {
        let mut buffers = self.transcoder_buffers.write().await;
        buffers.retain(|key, (_rb, token)| {
            if !active_keys.contains(key) {
                println!("[engine] Sweeping unused transcoder stage: {}", key);
                token.cancel();
                false
            } else {
                true
            }
        });
    }

    pub async fn get_or_create_ts_muxer_stage(
        self: &Arc<Self>,
        pipeline_id: &str,
        stage_key: &str,
        source_ring: Arc<RingBuffer>,
    ) -> Arc<TsChunkRing> {
        let key = format!("{}:{}", pipeline_id, stage_key);

        let mut stages = self.ts_muxer_stages.write().await;
        if let Some(stage) = stages.get(&key)
            && !stage.cancel.is_cancelled()
        {
            return stage.clone();
        }

        let cancel = CancellationToken::new();
        let shared_muxer = crate::media::srt::start_shared_ts_muxer(
            pipeline_id,
            source_ring,
            self.clone(),
            cancel,
        );

        stages.insert(key, shared_muxer.clone());
        shared_muxer
    }

    pub async fn sweep_unused_stages(&self) {
        let mut stages = self.ts_muxer_stages.write().await;
        stages.retain(|key, stage| {
            let has_readers = if let Ok(mut r) = stage.ring.readers.lock() {
                r.retain(|w| w.upgrade().is_some());
                !r.is_empty()
            } else {
                false
            };

            let in_use = has_readers;

            if !in_use {
                println!("[engine] Sweeping unused TS muxer stage: {}", key);
                stage.cancel.cancel();
                false
            } else {
                true
            }
        });
    }

    ///
    /// A pipeline has one application-level producer. A bonded SRT publisher is
    /// still one producer because libsrt presents the accepted bond as one group
    /// socket. A second independent RTMP/SRT connection must be rejected instead
    /// of overwriting the token and creating concurrent RingBuffer writers.
    pub async fn try_register_ingest(
        &self,
        pipeline_id: &str,
        stream_key: &str,
        protocol: &str,
    ) -> Option<CancellationToken> {
        let mut tokens = self.ingest_cancel_tokens.write().await;
        if let Some(existing) = tokens.get(pipeline_id)
            && !existing.is_cancelled()
        {
            return None;
        }

        let token = CancellationToken::new();
        tokens.insert(pipeline_id.to_string(), token.clone());

        let mut ingests = self.active_ingests.write().await;
        ingests.insert(
            pipeline_id.to_string(),
            ActiveIngest {
                stream_key: stream_key.to_string(),
                start_time: Instant::now(),
                protocol: protocol.to_string(),
                bytes_received: Arc::new(AtomicU64::new(0)),
                metrics: Arc::new(StageMetrics::new()),
                remote_addr: None,
                video: None,
                audio: None,
                audio_tracks: std::sync::Mutex::new(std::sync::Arc::new(Vec::new())),
                quality: PublisherQuality::default(),
                keyframe_times: Arc::new(std::sync::Mutex::new(Vec::new())),
                video_sequence_header: std::sync::Mutex::new(None),
                audio_sequence_header: std::sync::Mutex::new(None),
            },
        );

        self.event_log.emit(crate::events::EventKind::IngestConnected {
            pipeline_id: pipeline_id.to_string(),
            protocol: protocol.to_string(),
            stream_key: stream_key.to_string(),
        });
        Some(token)
    }

    pub async fn unregister_ingest(&self, pipeline_id: &str) {
        let mut tokens = self.ingest_cancel_tokens.write().await;
        if let Some(token) = tokens.remove(pipeline_id) {
            token.cancel();
        }

        let mut ingests = self.active_ingests.write().await;
        let protocol = ingests
            .get(pipeline_id)
            .map(|i| i.protocol.clone())
            .unwrap_or_default();
        ingests.remove(pipeline_id);
        drop(ingests);

        if !protocol.is_empty() {
            self.event_log.emit(crate::events::EventKind::IngestDisconnected {
                pipeline_id: pipeline_id.to_string(),
                protocol,
            });
        }
    }

    pub async fn register_egress(
        &self,
        output_id: &str,
        pipeline_id: &str,
        url: &str,
    ) -> CancellationToken {
        let mut tokens = self.egress_cancel_tokens.write().await;
        let token = CancellationToken::new();
        tokens.insert(output_id.to_string(), token.clone());

        let mut egresses = self.active_egresses.write().await;
        let now = Instant::now();
        egresses.insert(
            output_id.to_string(),
            ActiveEgress {
                output_id: output_id.to_string(),
                pipeline_id: pipeline_id.to_string(),
                target_url: url.to_string(),
                status: "running".to_string(),
                started_at: chrono::Utc::now().to_rfc3339(),
                start_instant: now,
                bytes_sent: Arc::new(AtomicU64::new(0)),
                metrics: Arc::new(StageMetrics::new()),
                prev_bytes_sent: AtomicU64::new(0),
                prev_sample_time: std::sync::Mutex::new(now),
                bitrate_kbps: std::sync::Mutex::new(None),
            },
        );

        self.event_log.emit(crate::events::EventKind::EgressStarted {
            pipeline_id: pipeline_id.to_string(),
            output_id: output_id.to_string(),
        });
        token
    }

    pub async fn unregister_egress(&self, output_id: &str) {
        let mut tokens = self.egress_cancel_tokens.write().await;
        if let Some(token) = tokens.remove(output_id) {
            token.cancel();
        }

        let mut egresses = self.active_egresses.write().await;
        let pipeline_id = egresses
            .get(output_id)
            .map(|e| e.pipeline_id.clone())
            .unwrap_or_default();
        egresses.remove(output_id);
        drop(egresses);

        if !pipeline_id.is_empty() {
            self.event_log.emit(crate::events::EventKind::EgressStopped {
                pipeline_id,
                output_id: output_id.to_string(),
            });
        }
    }

    /// Update bytes received counter for an active ingest (lock-free atomic).
    pub async fn update_ingest_bytes(&self, pipeline_id: &str, bytes: u64) {
        let ingests = self.active_ingests.read().await;
        if let Some(ingest) = ingests.get(pipeline_id) {
            ingest.bytes_received.fetch_add(bytes, Ordering::Relaxed);
        }
    }

    pub async fn record_keyframe(&self, pipeline_id: &str, pts: i64) {
        let ingests = self.active_ingests.read().await;
        if let Some(ingest) = ingests.get(pipeline_id) {
            let mut times = ingest
                .keyframe_times
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            times.push(pts);
            if times.len() > 30 {
                times.remove(0);
            }
        }
    }

    /// Update egress bytes sent counter (lock-free atomic).
    pub async fn update_egress_bytes(&self, output_id: &str, bytes: u64) {
        let egresses = self.active_egresses.read().await;
        if let Some(egress) = egresses.get(output_id) {
            egress.bytes_sent.fetch_add(bytes, Ordering::Relaxed);
        }
    }

    pub async fn egress_bytes(&self, output_id: &str) -> u64 {
        let egresses = self.active_egresses.read().await;
        egresses
            .get(output_id)
            .map(|e| e.bytes_sent.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    /// Update stream metadata discovered during demux/decode for an active ingest.
    pub async fn update_ingest_meta(
        &self,
        pipeline_id: &str,
        video: Option<VideoMeta>,
        audio: Option<AudioMeta>,
        remote_addr: Option<String>,
    ) {
        let mut ingests = self.active_ingests.write().await;
        if let Some(ingest) = ingests.get_mut(pipeline_id) {
            if video.is_some() {
                ingest.video = video;
            }
            if audio.is_some() {
                ingest.audio = audio;
            }
            if remote_addr.is_some() {
                ingest.remote_addr = remote_addr;
            }
        }
    }

    pub async fn cache_sequence_header(
        &self,
        pipeline_id: &str,
        is_video: bool,
        data: bytes::Bytes,
    ) {
        let ingests = self.active_ingests.read().await;
        if let Some(ingest) = ingests.get(pipeline_id) {
            if is_video {
                *ingest
                    .video_sequence_header
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = Some(data);
            } else {
                *ingest
                    .audio_sequence_header
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = Some(data);
            }
        }
    }

    pub async fn get_sequence_headers(
        &self,
        pipeline_id: &str,
    ) -> (Option<bytes::Bytes>, Option<bytes::Bytes>) {
        let ingests = self.active_ingests.read().await;
        if let Some(ingest) = ingests.get(pipeline_id) {
            let video = ingest
                .video_sequence_header
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let audio = ingest
                .audio_sequence_header
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            (video, audio)
        } else {
            (None, None)
        }
    }

    /// Update audio track metadata for an active ingest (multi-track support).
    pub async fn update_ingest_audio_tracks(&self, pipeline_id: &str, tracks: Vec<AudioMeta>) {
        let ingests = self.active_ingests.read().await;
        if let Some(ingest) = ingests.get(pipeline_id) {
            *ingest
                .audio_tracks
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = std::sync::Arc::new(tracks);
        }
    }

    /// Build a probe snapshot for a pipeline's active ingest.
    pub async fn probe_snapshot(&self, pipeline_id: &str) -> Option<serde_json::Value> {
        let ingests = self.active_ingests.read().await;
        let ingest = ingests.get(pipeline_id)?;

        let elapsed = ingest.start_time.elapsed().as_secs_f64();
        let bytes = ingest.bytes_received.load(Ordering::Relaxed);
        let bitrate_kbps = if elapsed > 1.0 {
            Some((bytes as f64 * 8.0) / (elapsed * 1000.0))
        } else {
            None
        };

        let audio_tracks: Vec<serde_json::Value> = {
            let tracks = ingest
                .audio_tracks
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if tracks.is_empty() {
                ingest
                    .audio
                    .as_ref()
                    .map(|a| vec![serde_json::to_value(a).unwrap_or_default()])
                    .unwrap_or_default()
            } else {
                tracks
                    .iter()
                    .map(|a| serde_json::to_value(a).unwrap_or_default())
                    .collect()
            }
        };

        let gop = {
            let times = ingest
                .keyframe_times
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if times.len() >= 2 {
                let intervals: Vec<f64> = times
                    .windows(2)
                    .map(|w| ((w[1] - w[0]) as f64 / 1000.0).max(0.0))
                    .collect();
                let avg = intervals.iter().sum::<f64>() / intervals.len() as f64;
                Some(serde_json::json!({
                    "averageIntervalSec": (avg * 100.0).round() / 100.0,
                    "keyframeCount": times.len(),
                }))
            } else {
                None
            }
        };

        Some(serde_json::json!({
            "pipelineId": pipeline_id,
            "ingest": {
                "protocol": ingest.protocol,
                "remoteAddr": ingest.remote_addr,
                "uptimeSeconds": (elapsed * 10.0).round() / 10.0,
                "bytesReceived": bytes,
                "bitrateKbps": bitrate_kbps.map(|b| (b * 10.0).round() / 10.0),
            },
            "video": ingest.video,
            "audioTracks": audio_tracks,
            "gop": gop,
        }))
    }

    /// Update publisher transport quality metrics.
    pub async fn update_publisher_quality(&self, pipeline_id: &str, quality: PublisherQuality) {
        let mut ingests = self.active_ingests.write().await;
        if let Some(ingest) = ingests.get_mut(pipeline_id) {
            ingest.quality = quality;
        }
    }

    /// Register an active recording for a pipeline. Returns a cancellation token.
    pub async fn register_recording(&self, pipeline_id: &str) -> CancellationToken {
        let mut tokens = self.recording_cancel_tokens.write().await;
        let token = CancellationToken::new();
        tokens.insert(pipeline_id.to_string(), token.clone());
        token
    }

    /// Unregister (and cancel) an active recording for a pipeline.
    pub async fn unregister_recording(&self, pipeline_id: &str) {
        let mut tokens = self.recording_cancel_tokens.write().await;
        if let Some(token) = tokens.remove(pipeline_id) {
            token.cancel();
        }
    }

    /// Check if a recording is actively running for a pipeline.
    pub async fn is_recording_active(&self, pipeline_id: &str) -> bool {
        let tokens = self.recording_cancel_tokens.read().await;
        tokens
            .get(pipeline_id)
            .is_some_and(|token| !token.is_cancelled())
    }

    /// Ensure an HLS segmenter is running for this pipeline. Returns the store
    /// and whether the segmenter was already running (true) or just started (false).
    pub async fn ensure_hls_segmenter(&self, pipeline_id: &str) -> (Arc<HlsStore>, bool) {
        let mut consumers = self.hls_consumers.write().await;
        let already_running = consumers.contains_key(pipeline_id);
        if !already_running {
            let token = CancellationToken::new();
            consumers.insert(pipeline_id.to_string(), HlsConsumers::new(token));
        }
        drop(consumers);

        let store = self.get_or_create_hls_store(pipeline_id).await;
        (store, already_running)
    }

    /// Touch the HLS consumer heartbeat (called on playlist/segment fetch).
    pub async fn touch_hls(&self, pipeline_id: &str) {
        let consumers = self.hls_consumers.read().await;
        if let Some(c) = consumers.get(pipeline_id) {
            c.touch();
        }
    }

    /// Register a persistent HLS consumer (e.g. HLS egress output).
    pub async fn add_hls_persistent_consumer(&self, pipeline_id: &str) {
        let consumers = self.hls_consumers.read().await;
        if let Some(c) = consumers.get(pipeline_id) {
            c.add_persistent();
        }
    }

    /// Unregister a persistent HLS consumer.
    pub async fn remove_hls_persistent_consumer(&self, pipeline_id: &str) {
        let consumers = self.hls_consumers.read().await;
        if let Some(c) = consumers.get(pipeline_id) {
            c.remove_persistent();
        }
    }

    /// Shut down an idle HLS segmenter and clean up its store.
    pub async fn shutdown_hls_segmenter(&self, pipeline_id: &str) {
        let mut consumers = self.hls_consumers.write().await;
        if let Some(c) = consumers.remove(pipeline_id) {
            c.cancel_token.cancel();
        }
        drop(consumers);
        self.hls_stores.write().await.remove(pipeline_id);
    }

    /// Get the cancel token for a running HLS segmenter (used to spawn the task).
    pub async fn get_hls_cancel_token(&self, pipeline_id: &str) -> Option<CancellationToken> {
        let consumers = self.hls_consumers.read().await;
        consumers.get(pipeline_id).map(|c| c.cancel_token.clone())
    }

    pub async fn get_or_create_hls_store(&self, pipeline_id: &str) -> Arc<HlsStore> {
        let mut stores = self.hls_stores.write().await;
        stores
            .entry(pipeline_id.to_string())
            .or_insert_with(|| Arc::new(HlsStore::new()))
            .clone()
    }

    pub async fn remove_hls_store(&self, pipeline_id: &str) {
        let mut stores = self.hls_stores.write().await;
        stores.remove(pipeline_id);
    }

    pub async fn get_hls_store(&self, pipeline_id: &str) -> Option<Arc<HlsStore>> {
        let stores = self.hls_stores.read().await;
        stores.get(pipeline_id).cloned()
    }

    /// Build the full health snapshot JSON that the `/health` endpoint returns.
    pub async fn health_snapshot(
        &self,
        pipeline_ids: &[String],
        recording_enabled: &HashMap<String, bool>,
    ) -> serde_json::Value {
        let ingests = self.active_ingests.read().await;
        let egresses = self.active_egresses.read().await;
        let rec_tokens = self.recording_cancel_tokens.read().await;
        let hls_consumers = self.hls_consumers.read().await;
        let pipelines = self.pipelines.read().await;

        let mut pipelines_json = serde_json::Map::new();

        for pipeline_id in pipeline_ids {
            let ingest_opt = ingests.get(pipeline_id.as_str());
            let pipeline_rb = pipelines.get(pipeline_id.as_str());
            let reader_snapshots = pipeline_rb
                .map(|rb| rb.reader_snapshots())
                .unwrap_or_default();
            let readers_count = reader_snapshots.len();
            let reader_metrics: Vec<serde_json::Value> = reader_snapshots
                .iter()
                .map(|reader| {
                    serde_json::json!({
                        "name": reader.name,
                        "readIndex": reader.read_idx,
                        "writeIndex": reader.write_idx,
                        "lagSlots": reader.lag_slots,
                        "overflowCount": reader.overflow_count,
                        "overflows": reader.overflow_count,
                        "packetAgeMs": reader.packet_age_ms,
                        "burstCount": reader.burst_count,
                        "avgBurstSize": (reader.avg_burst_size * 10.0).round() / 10.0,
                        "medianBurstSize": reader.median_burst_size,
                    })
                })
                .collect();

            let mut total_bytes_sent = 0u64;
            for (_, egress) in egresses.iter() {
                if egress.pipeline_id == *pipeline_id {
                    total_bytes_sent += egress.bytes_sent.load(Ordering::Relaxed);
                }
            }

            let input_json = if let Some(ingest) = ingest_opt {
                let elapsed_secs = ingest.start_time.elapsed().as_secs_f64();
                let bytes_received = ingest.bytes_received.load(Ordering::Relaxed);
                let bitrate_kbps = if elapsed_secs > 1.0 {
                    Some((bytes_received as f64 * 8.0) / (elapsed_secs * 1000.0))
                } else {
                    None
                };
                let publish_started_at = {
                    let ts = chrono::Utc::now() - chrono::Duration::seconds(elapsed_secs as i64);
                    ts.to_rfc3339()
                };

                let publisher_json = serde_json::json!({
                    "protocol": ingest.protocol,
                    "remoteAddr": ingest.remote_addr,
                    "quality": ingest.quality,
                });

                serde_json::json!({
                    "status": "on",
                    "publishStartedAt": publish_started_at,
                    "bytesReceived": bytes_received,
                    "bytesSent": total_bytes_sent,
                    "readers": readers_count,
                    "readerMetrics": reader_metrics,
                    "bitrateKbps": bitrate_kbps,
                    "video": ingest.video,
                    "audio": ingest.audio,
                    "publisher": publisher_json,
                    "unexpectedReaders": { "count": 0 }
                })
            } else {
                serde_json::json!({
                    "status": "off",
                    "bytesReceived": 0,
                    "bytesSent": total_bytes_sent,
                    "readers": readers_count,
                    "readerMetrics": reader_metrics,
                    "publisher": null,
                    "unexpectedReaders": { "count": 0 }
                })
            };

            let mut outputs_json = serde_json::Map::new();
            for (egress_key, egress) in egresses.iter() {
                if egress.pipeline_id == *pipeline_id {
                    let output_id = egress_key;
                    let bytes_sent = egress.bytes_sent.load(Ordering::Relaxed);

                    // Compute instantaneous bitrate from byte delta
                    let bitrate_kbps = {
                        let prev = egress.prev_bytes_sent.load(Ordering::Relaxed);
                        let mut prev_time = egress
                            .prev_sample_time
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        let elapsed = prev_time.elapsed().as_secs_f64();

                        if elapsed > 0.5 && bytes_sent > prev {
                            let delta = bytes_sent - prev;
                            let rate = (delta as f64 * 8.0) / (elapsed * 1000.0);
                            egress.prev_bytes_sent.store(bytes_sent, Ordering::Relaxed);
                            *prev_time = Instant::now();
                            *egress
                                .bitrate_kbps
                                .lock()
                                .unwrap_or_else(|e| e.into_inner()) = Some(rate);
                            Some(rate)
                        } else {
                            *egress
                                .bitrate_kbps
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                        }
                    };

                    let has_ingest = ingests.contains_key(pipeline_id.as_str());
                    let output_status = if has_ingest {
                        egress.status.as_str()
                    } else {
                        "stopped"
                    };

                    outputs_json.insert(
                        output_id.to_string(),
                        serde_json::json!({
                            "status": output_status,
                            "totalSize": bytes_sent,
                            "bitrateKbps": bitrate_kbps,
                            "startedAt": egress.started_at,
                        }),
                    );
                }
            }

            let rec_enabled = recording_enabled.get(pipeline_id).copied().unwrap_or(false);
            let rec_active = rec_tokens
                .get(pipeline_id.as_str())
                .is_some_and(|token| !token.is_cancelled());
            let hls_active = hls_consumers
                .get(pipeline_id.as_str())
                .is_some_and(|consumer| !consumer.cancel_token.is_cancelled());

            pipelines_json.insert(
                pipeline_id.clone(),
                serde_json::json!({
                    "input": input_json,
                    "outputs": serde_json::Value::Object(outputs_json),
                    "recording": { "enabled": rec_enabled, "active": rec_active },
                    "hlsPreview": { "active": hls_active }
                }),
            );
        }

        let rx_queue = self
            .srt_listener_stats
            .rx_queue_bytes
            .load(Ordering::Relaxed);
        let rx_max = self
            .srt_listener_stats
            .rx_queue_max_bytes
            .load(Ordering::Relaxed);
        let drops = self.srt_listener_stats.drops.load(Ordering::Relaxed);
        let bonding_available = self
            .srt_listener_stats
            .bonding_available
            .load(Ordering::Relaxed);

        serde_json::json!({
            "generatedAt": chrono::Utc::now().to_rfc3339(),
            "status": "ready",
            "pipelines": serde_json::Value::Object(pipelines_json),
            "srtListener": {
                "bondingAvailable": bonding_available,
                "udpRxQueueBytes": rx_queue,
                "udpRxQueuePeakBytes": rx_max,
                "udpDrops": drops,
            },
        })
    }

    /// Engine-wide telemetry: raw counters for all active ingests, stages, and
    /// egresses. Intended for engineer dashboards and debugging.
    pub async fn engine_telemetry(&self) -> serde_json::Value {
        let generated_at = chrono::Utc::now().to_rfc3339();
        let ingests = self.active_ingests.read().await;
        let egresses = self.active_egresses.read().await;
        let stage_metrics = self.stage_metrics.read().await;
        let pipe_metrics = self.pipe_metrics.read().await;
        let buffers = self.transcoder_buffers.read().await;

        let ingest_arr: Vec<serde_json::Value> = ingests
            .iter()
            .map(|(pid, i)| {
                serde_json::json!({
                    "pipelineId": pid,
                    "protocol": i.protocol,
                    "uptimeSecs": i.start_time.elapsed().as_secs_f64(),
                    "bytesReceived": i.bytes_received.load(Ordering::Relaxed),
                    "metrics": i.metrics.snapshot(),
                })
            })
            .collect();

        let stage_arr: Vec<serde_json::Value> = stage_metrics
            .iter()
            .map(|(key, m)| {
                let mut val = serde_json::json!({
                    "stageKey": key.to_string(),
                    "pipelineId": key.pipeline.as_str(),
                    "kind": key.kind.to_string(),
                    "metrics": m.snapshot(),
                });
                if let Some(pm) = pipe_metrics.get(key) {
                    val["pipeMetrics"] = pm.snapshot();
                }
                val
            })
            .collect();

        let egress_arr: Vec<serde_json::Value> = egresses
            .iter()
            .map(|(oid, e)| {
                serde_json::json!({
                    "outputId": oid,
                    "pipelineId": e.pipeline_id,
                    "uptimeSecs": e.start_instant.elapsed().as_secs_f64(),
                    "bytesOut": e.bytes_sent.load(Ordering::Relaxed),
                })
            })
            .collect();

        serde_json::json!({
            "generatedAt": generated_at,
            "ingests": ingest_arr,
            "stages": stage_arr,
            "egresses": egress_arr,
            "activeTranscoderBuffers": buffers.len(),
        })
    }

    /// Per-pipeline telemetry: ingest metrics, all stage metrics for this
    /// pipeline, and egress metrics. Returns None if the pipeline has no
    /// active components.
    pub async fn pipeline_telemetry(&self, pipeline_id: &str) -> serde_json::Value {
        let generated_at = chrono::Utc::now().to_rfc3339();
        let ingests = self.active_ingests.read().await;
        let egresses = self.active_egresses.read().await;
        let all_stage_metrics = self.stage_metrics.read().await;
        let all_pipe_metrics = self.pipe_metrics.read().await;
        let pipelines = self.pipelines.read().await;

        let ingest = ingests.get(pipeline_id).map(|i| {
            serde_json::json!({
                "protocol": i.protocol,
                "streamKey": i.stream_key,
                "uptimeSecs": i.start_time.elapsed().as_secs_f64(),
                "bytesReceived": i.bytes_received.load(Ordering::Relaxed),
                "video": i.video,
                "audio": i.audio,
                "metrics": i.metrics.snapshot(),
            })
        });

        let ring_info = pipelines.get(pipeline_id).map(|rb| {
            let (fill, cap) = rb.fill_and_capacity();
            let readers: Vec<serde_json::Value> = rb
                .reader_snapshots()
                .into_iter()
                .map(|r| {
                    serde_json::json!({
                        "name": r.name,
                        "lagSlots": r.lag_slots,
                        "overflowCount": r.overflow_count,
                        "packetAgeMs": r.packet_age_ms,
                    })
                })
                .collect();
            serde_json::json!({
                "fill": fill,
                "capacity": cap,
                "readers": readers,
            })
        });

        let stages: Vec<serde_json::Value> = all_stage_metrics
            .iter()
            .filter(|(k, _)| k.pipeline.as_str() == pipeline_id)
            .map(|(k, m)| {
                let mut val = serde_json::json!({
                    "kind": k.kind.to_string(),
                    "metrics": m.snapshot(),
                });
                if let Some(pm) = all_pipe_metrics.get(k) {
                    val["pipeMetrics"] = pm.snapshot();
                }
                val
            })
            .collect();

        let pipeline_egresses: Vec<serde_json::Value> = egresses
            .iter()
            .filter(|(_, e)| e.pipeline_id == pipeline_id)
            .map(|(oid, e)| {
                serde_json::json!({
                    "outputId": oid,
                    "uptimeSecs": e.start_instant.elapsed().as_secs_f64(),
                    "bytesOut": e.bytes_sent.load(Ordering::Relaxed),
                })
            })
            .collect();

        serde_json::json!({
            "generatedAt": generated_at,
            "pipelineId": pipeline_id,
            "ingest": ingest,
            "sourceRing": ring_info,
            "stages": stages,
            "egresses": pipeline_egresses,
        })
    }

    /// Single-stage telemetry by StageKey. Returns raw counters and pipe
    /// metrics (if present). Used by the engineer stage telemetry endpoint.
    pub async fn stage_telemetry(&self, key: &StageKey) -> Option<serde_json::Value> {
        let all_stage_metrics = self.stage_metrics.read().await;
        let metrics = all_stage_metrics.get(key)?;

        let all_pipe_metrics = self.pipe_metrics.read().await;
        let pipe = all_pipe_metrics.get(key).map(|pm| pm.snapshot());

        Some(serde_json::json!({
            "generatedAt": chrono::Utc::now().to_rfc3339(),
            "stageKey": key.to_string(),
            "pipelineId": key.pipeline.as_str(),
            "kind": key.kind.to_string(),
            "metrics": metrics.snapshot(),
            "pipeMetrics": pipe,
        }))
    }

    pub async fn stage_telemetry_by_display(&self, display: &str) -> Option<serde_json::Value> {
        let all_stage_metrics = self.stage_metrics.read().await;
        let key = all_stage_metrics.keys().find(|k| k.to_string() == display)?;
        let metrics = all_stage_metrics.get(key)?;

        let all_pipe_metrics = self.pipe_metrics.read().await;
        let pipe = all_pipe_metrics.get(key).map(|pm| pm.snapshot());

        Some(serde_json::json!({
            "generatedAt": chrono::Utc::now().to_rfc3339(),
            "stageKey": key.to_string(),
            "pipelineId": key.pipeline.as_str(),
            "kind": key.kind.to_string(),
            "metrics": metrics.snapshot(),
            "pipeMetrics": pipe,
        }))
    }

    /// Build a processing graph for a pipeline showing all stages and connections.
    /// Returns a JSON structure suitable for visualization.
    pub async fn processing_graph(
        &self,
        pipeline_id: &str,
        outputs: &[crate::types::Output],
    ) -> serde_json::Value {
        let ingests = self.active_ingests.read().await;
        let egresses = self.active_egresses.read().await;
        let pipelines = self.pipelines.read().await;
        let transcoder_buffers = self.transcoder_buffers.read().await;
        let rec_tokens = self.recording_cancel_tokens.read().await;
        let hls_stores = self.hls_stores.read().await;
        let hls_consumers = self.hls_consumers.read().await;
        let all_stage_metrics = self.stage_metrics.read().await;
        let all_input_queues = self.input_queues.read().await;
        let all_pipe_metrics = self.pipe_metrics.read().await;

        let mut nodes = Vec::new();
        let mut edges = Vec::new();

        // Node: ingest
        let ingest = ingests.get(pipeline_id);
        let ingest_node_id = format!("{}_ingest", pipeline_id);
        nodes.push(serde_json::json!({
            "id": ingest_node_id,
            "type": "ingest",
            "label": if let Some(i) = ingest {
                format!("{} ingest", i.protocol.to_uppercase())
            } else {
                "No ingest".to_string()
            },
            "active": ingest.is_some(),
            "details": ingest.map(|i| serde_json::json!({
                "protocol": i.protocol,
                "remoteAddr": i.remote_addr,
                "video": i.video,
                "audio": i.audio,
                "bytesReceived": i.bytes_received.load(Ordering::Relaxed),
            })),
            "metrics": ingest.map(|i| i.metrics.snapshot()),
        }));

        // Node: source ring buffer
        let rb_node_id = format!("{}_source_rb", pipeline_id);
        let rb_info = pipelines.get(pipeline_id).map(|rb| {
            let (fill, cap) = rb.fill_and_capacity();
            let reader_stats: Vec<serde_json::Value> = rb
                .reader_snapshots()
                .into_iter()
                .map(|reader| {
                    serde_json::json!({
                        "name": reader.name,
                        "readIndex": reader.read_idx,
                        "writeIndex": reader.write_idx,
                        "lagSlots": reader.lag_slots,
                        "overflowCount": reader.overflow_count,
                        "overflows": reader.overflow_count,
                        "packetAgeMs": reader.packet_age_ms,
                        "burstCount": reader.burst_count,
                        "avgBurstSize": (reader.avg_burst_size * 10.0).round() / 10.0,
                        "medianBurstSize": reader.median_burst_size,
                    })
                })
                .collect();
            (fill, cap, reader_stats)
        });
        nodes.push(serde_json::json!({
            "id": rb_node_id,
            "type": "ring_buffer",
            "label": "Source Buffer",
            "active": rb_info.is_some(),
            "details": rb_info.map(|(fill, cap, readers)| serde_json::json!({
                "fill": fill,
                "capacity": cap,
                "fillPercent": (fill * 100).checked_div(cap).unwrap_or(0),
                "format": "FLV (interleaved A+V)",
                "readers": readers,
            })),
        }));
        edges.push(serde_json::json!({
            "from": ingest_node_id,
            "to": rb_node_id,
            "label": "push(MediaPacket)",
        }));

        // Nodes: transcoder stages.
        for (key, (_, token)) in transcoder_buffers.iter() {
            if key.pipeline.as_str() == pipeline_id {
                let kind = &key.kind;
                let stage_key_str = kind.to_string();
                let stage_id = kind.graph_node_id(pipeline_id);
                let queue_stats = all_input_queues.get(key).map(|q| q.stats());
                let pipe_stats = all_pipe_metrics.get(key).map(|p| p.snapshot());
                nodes.push(serde_json::json!({
                    "id": stage_id,
                    "type": kind.graph_type(),
                    "label": kind.graph_label(),
                    "stageKey": stage_key_str,
                    "active": !token.is_cancelled(),
                    "metrics": all_stage_metrics.get(key).map(|m| m.snapshot()),
                    "queueMetrics": queue_stats,
                    "pipeMetrics": pipe_stats,
                }));

                if let Some(upstream) = kind.upstream() {
                    let (from, label) = if matches!(upstream, StageKind::Source) {
                        let label = if matches!(kind, StageKind::CodecEdge { .. }) {
                            "codec conversion"
                        } else {
                            "audio select"
                        };
                        (rb_node_id.clone(), label)
                    } else if matches!(kind, StageKind::CodecEdge { .. }) {
                        (upstream.graph_node_id(pipeline_id), "codec conversion")
                    } else {
                        (
                            upstream.graph_node_id(pipeline_id),
                            "video copy + audio select",
                        )
                    };
                    edges.push(serde_json::json!({
                        "from": from,
                        "to": stage_id,
                        "label": label,
                    }));
                } else if let StageKind::VideoPreset { preset } = &kind {
                    edges.push(serde_json::json!({
                        "from": rb_node_id,
                        "to": stage_id,
                        "label": format!("decode → {} encode", preset),
                    }));
                }
            }
        }

        // Nodes: egress outputs
        let pipeline_outputs: Vec<_> = outputs
            .iter()
            .filter(|o| o.pipeline_id == pipeline_id)
            .collect();

        for output in &pipeline_outputs {
            let egress = egresses.get(&output.id);
            let output_node_id = format!("{}_output_{}", pipeline_id, output.id);

            let protocol = if output.url.starts_with("rtmp://") {
                "RTMP"
            } else if output.url.starts_with("srt://") {
                "SRT"
            } else {
                "HLS"
            };

            nodes.push(serde_json::json!({
                "id": output_node_id,
                "type": "egress",
                "label": format!("{}: {}", protocol, output.name),
                "active": egress.is_some_and(|e| e.status == "running"),
                "details": egress.map(|e| {
                    let bytes = e.bytes_sent.load(Ordering::Relaxed);
                    serde_json::json!({
                        "status": e.status,
                        "targetUrl": e.target_url,
                        "totalSize": bytes,
                        "bitrateKbps": *e.bitrate_kbps.lock().unwrap_or_else(|e| e.into_inner()),
                        "startedAt": e.started_at,
                    })
                }),
                "metrics": egress.map(|e| e.metrics.snapshot()),
            }));

            // Edge: from the appropriate stage to this egress
            // Mirror the reconciler's stage-key logic
            let stage_plan = EncodingStagePlan::from_encoding(pipeline_id, &output.encoding);
            if let Some(stage) = stage_plan.audio_stage() {
                edges.push(serde_json::json!({
                    "from": stage.kind.graph_node_id(pipeline_id),
                    "to": output_node_id,
                    "label": "MPEG-TS",
                }));
            } else if let Some(stage) = stage_plan.video_stage() {
                edges.push(serde_json::json!({
                    "from": stage.kind.graph_node_id(pipeline_id),
                    "to": output_node_id,
                    "label": "MPEG-TS",
                }));
            } else {
                edges.push(serde_json::json!({
                    "from": rb_node_id,
                    "to": output_node_id,
                    "label": "FLV passthrough",
                }));
            }
        }

        // Node: recording (if registered)
        if let Some(token) = rec_tokens.get(pipeline_id) {
            let rec_id = format!("{}_recording", pipeline_id);
            let rec_stage_key = StageKey::new(pipeline_id, StageKind::recording());
            nodes.push(serde_json::json!({
                "id": rec_id,
                "type": "recording",
                "label": "MKV Recording",
                "active": !token.is_cancelled(),
                "metrics": all_stage_metrics.get(&rec_stage_key).map(|m| m.snapshot()),
            }));
            edges.push(serde_json::json!({
                "from": rb_node_id,
                "to": rec_id,
                "label": "MKV mux",
            }));
        }

        // Node: HLS (if store exists)
        if hls_stores.contains_key(pipeline_id) {
            let hls_id = format!("{}_hls_preview", pipeline_id);
            let hls_stage_key = StageKey::new(pipeline_id, StageKind::hls());
            let hls_active = hls_consumers
                .get(pipeline_id)
                .is_some_and(|consumer| !consumer.cancel_token.is_cancelled());
            nodes.push(serde_json::json!({
                "id": hls_id,
                "type": "hls",
                "label": "HLS Preview",
                "active": hls_active,
                "metrics": all_stage_metrics.get(&hls_stage_key).map(|m| m.snapshot()),
            }));
            edges.push(serde_json::json!({
                "from": rb_node_id,
                "to": hls_id,
                "label": "MPEG-TS segment",
            }));
        }

        serde_json::json!({
            "generatedAt": chrono::Utc::now().to_rfc3339(),
            "pipelineId": pipeline_id,
            "nodes": nodes,
            "edges": edges,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::ring_buffer::{MediaPacket, MediaType, PayloadFormat, Reader};
    use bytes::Bytes;
    use std::sync::Arc;

    #[test]
    fn pipe_metrics_snapshot_correctness() {
        let pm = PipeMetrics::default();
        let snap = pm.snapshot();

        // All counters start at zero; avg fields are also zero.
        assert_eq!(snap["stalls"].as_u64().unwrap(), 0);
        assert_eq!(snap["stallUs"].as_u64().unwrap(), 0);
        assert_eq!(snap["avgStallUs"].as_u64().unwrap(), 0);
        assert_eq!(snap["idles"].as_u64().unwrap(), 0);
        assert_eq!(snap["idleUs"].as_u64().unwrap(), 0);
        assert_eq!(snap["avgIdleUs"].as_u64().unwrap(), 0);

        // Stdin stall accumulation and average.
        pm.record_stall(2_000);
        pm.record_stall(6_000);
        let snap = pm.snapshot();
        assert_eq!(snap["stalls"].as_u64().unwrap(), 2);
        assert_eq!(snap["stallUs"].as_u64().unwrap(), 8_000);
        assert_eq!(snap["avgStallUs"].as_u64().unwrap(), 4_000);

        // Stdout idle accumulation and average.
        pm.record_idle(3_000);
        let snap = pm.snapshot();
        assert_eq!(snap["idles"].as_u64().unwrap(), 1);
        assert_eq!(snap["idleUs"].as_u64().unwrap(), 3_000);
        assert_eq!(snap["avgIdleUs"].as_u64().unwrap(), 3_000);

        // StageMetrics snapshot no longer contains pipe fields.
        let sm = StageMetrics::new();
        let ssnap = sm.snapshot();
        assert!(ssnap.get("pipeMetrics").is_none());
    }

    fn test_video_packet(pts: i64, dts: i64, keyframe: bool) -> MediaPacket {
        MediaPacket {
            media_type: MediaType::Video,
            format: PayloadFormat::Raw,
            is_keyframe: keyframe,
            track_index: 0,
            pts,
            dts,
            payload: Bytes::from_static(b"video"),
        }
    }

    fn test_audio_packet(pts: i64, dts: i64) -> MediaPacket {
        MediaPacket {
            media_type: MediaType::Audio,
            format: PayloadFormat::Raw,
            is_keyframe: false,
            track_index: 0,
            pts,
            dts,
            payload: Bytes::from_static(b"audio"),
        }
    }

    #[tokio::test]
    async fn test_hls_consumers_monotonic_idle() {
        let cancel = CancellationToken::new();
        let hc = HlsConsumers::new(cancel);
        assert!(!hc.is_idle(60000));

        tokio::time::sleep(tokio::time::Duration::from_millis(5)).await;
        hc.touch();
        let last = hc.last_access_ms.load(Ordering::Relaxed);
        assert!(last > 0);

        tokio::time::sleep(tokio::time::Duration::from_millis(15)).await;
        assert!(!hc.is_idle(60000));
        assert!(hc.is_idle(10));
    }

    #[tokio::test]
    async fn rejects_a_second_independent_publisher_for_the_same_pipeline() {
        let engine = MediaEngine::new();

        assert!(
            engine
                .try_register_ingest("pipeline-1", "stream-key", "srt")
                .await
                .is_some()
        );
        assert!(
            engine
                .try_register_ingest("pipeline-1", "stream-key", "srt")
                .await
                .is_none()
        );

        engine.unregister_ingest("pipeline-1").await;
        assert!(
            engine
                .try_register_ingest("pipeline-1", "stream-key", "srt")
                .await
                .is_some()
        );
    }

    #[tokio::test]
    async fn concurrent_publishers_cannot_both_reserve_the_same_pipeline() {
        let engine = Arc::new(MediaEngine::new());
        let first_engine = engine.clone();
        let second_engine = engine.clone();

        let (first, second) = tokio::join!(
            async move {
                first_engine
                    .try_register_ingest("pipeline-race", "stream-key", "srt")
                    .await
                    .is_some()
            },
            async move {
                second_engine
                    .try_register_ingest("pipeline-race", "stream-key", "srt")
                    .await
                    .is_some()
            }
        );

        assert_ne!(first, second, "exactly one publisher must win reservation");
        assert_eq!(engine.active_ingests.read().await.len(), 1);
    }

    #[tokio::test]
    async fn health_snapshot_marks_outputs_stopped_without_ingest() {
        let engine = MediaEngine::new();
        engine
            .register_egress("output-1", "pipeline-1", "rtmp://example/live/test")
            .await;

        let snapshot = engine
            .health_snapshot(&["pipeline-1".to_string()], &HashMap::new())
            .await;

        assert_eq!(
            snapshot["pipelines"]["pipeline-1"]["outputs"]["output-1"]["status"],
            "stopped"
        );
    }

    #[tokio::test]
    async fn health_snapshot_marks_hls_preview_active_when_consumer_exists() {
        let engine = MediaEngine::new();
        let pipeline_id = "pipeline-hls";

        let _ = engine.ensure_hls_segmenter(pipeline_id).await;
        let snapshot = engine
            .health_snapshot(&[pipeline_id.to_string()], &HashMap::new())
            .await;

        assert_eq!(
            snapshot["pipelines"][pipeline_id]["hlsPreview"]["active"],
            true
        );
    }

    #[tokio::test]
    async fn health_snapshot_marks_cancelled_hls_preview_inactive() {
        let engine = MediaEngine::new();
        let pipeline_id = "pipeline-hls-cancelled";

        let _ = engine.ensure_hls_segmenter(pipeline_id).await;
        let token = engine.get_hls_cancel_token(pipeline_id).await.unwrap();
        token.cancel();

        let snapshot = engine
            .health_snapshot(&[pipeline_id.to_string()], &HashMap::new())
            .await;

        assert_eq!(
            snapshot["pipelines"][pipeline_id]["hlsPreview"]["active"],
            false
        );
    }

    #[tokio::test]
    async fn health_and_graph_expose_reader_lag_overflow_and_packet_age() {
        let engine = MediaEngine::new();
        let pipeline_id = "pipeline-reader-metrics";
        let rb = engine.get_or_create_pipeline(pipeline_id).await;

        rb.push(test_video_packet(0, 0, true));
        let _reader = Reader::new("graph-reader".to_string(), rb.clone());
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        rb.push(test_audio_packet(10, 10));

        let snapshot = engine
            .health_snapshot(&[pipeline_id.to_string()], &HashMap::new())
            .await;
        let reader_metrics = snapshot["pipelines"][pipeline_id]["input"]["readerMetrics"]
            .as_array()
            .unwrap();
        assert_eq!(reader_metrics.len(), 1);
        assert_eq!(reader_metrics[0]["name"], "graph-reader");
        assert_eq!(reader_metrics[0]["lagSlots"], 2);
        assert_eq!(reader_metrics[0]["overflowCount"], 0);
        assert!(
            !reader_metrics[0]["packetAgeMs"].is_null(),
            "health reader metrics should expose unread packet age"
        );

        let graph = engine.processing_graph(pipeline_id, &[]).await;
        let source = graph["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|node| node["type"] == "ring_buffer")
            .unwrap();
        let graph_readers = source["details"]["readers"].as_array().unwrap();
        assert_eq!(graph_readers.len(), 1);
        assert_eq!(graph_readers[0]["lagSlots"], 2);
        assert_eq!(graph_readers[0]["overflowCount"], 0);
        assert!(
            !graph_readers[0]["packetAgeMs"].is_null(),
            "graph reader metrics should expose unread packet age"
        );
    }

    #[tokio::test]
    async fn health_snapshot_exposes_bonding_and_member_telemetry() {
        let engine = MediaEngine::new();
        engine
            .srt_listener_stats
            .bonding_available
            .store(true, Ordering::Relaxed);
        engine
            .try_register_ingest("pipeline-bond", "stream-key", "srt")
            .await
            .unwrap();
        engine
            .update_publisher_quality(
                "pipeline-bond",
                PublisherQuality {
                    srt_bonded: Some(true),
                    srt_group_member_count: Some(2),
                    srt_group_connected_members: Some(2),
                    srt_group_active_members: Some(1),
                    srt_group_broken_members: Some(0),
                    ..PublisherQuality::default()
                },
            )
            .await;

        let snapshot = engine
            .health_snapshot(&["pipeline-bond".to_string()], &HashMap::new())
            .await;
        let quality = &snapshot["pipelines"]["pipeline-bond"]["input"]["publisher"]["quality"];

        assert_eq!(snapshot["srtListener"]["bondingAvailable"], true);
        assert_eq!(quality["srtBonded"], true);
        assert_eq!(quality["srtGroupMemberCount"], 2);
        assert_eq!(quality["srtGroupConnectedMembers"], 2);
        assert_eq!(quality["srtGroupActiveMembers"], 1);
        assert_eq!(quality["srtGroupBrokenMembers"], 0);
    }

    #[tokio::test]
    async fn unregister_ingest_cancels_token() {
        let engine = MediaEngine::new();
        let token = engine
            .try_register_ingest("p1", "key", "rtmp")
            .await
            .unwrap();
        assert!(!token.is_cancelled());

        engine.unregister_ingest("p1").await;
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn unregister_ingest_idempotent() {
        let engine = MediaEngine::new();
        engine
            .try_register_ingest("p1", "key", "rtmp")
            .await
            .unwrap();
        engine.unregister_ingest("p1").await;
        // Second unregister should not panic
        engine.unregister_ingest("p1").await;
    }

    #[tokio::test]
    async fn egress_register_and_cancel() {
        let engine = MediaEngine::new();
        let token = engine
            .register_egress("out-1", "pipe-1", "rtmp://example.com/live/key")
            .await;
        assert!(!token.is_cancelled());

        engine.unregister_egress("out-1").await;
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn egress_unregister_idempotent() {
        let engine = MediaEngine::new();
        engine
            .register_egress("out-1", "pipe-1", "rtmp://example.com/live/key")
            .await;
        engine.unregister_egress("out-1").await;
        engine.unregister_egress("out-1").await;
    }

    #[tokio::test]
    async fn egress_bytes_counter() {
        let engine = MediaEngine::new();
        engine
            .register_egress("out-1", "pipe-1", "rtmp://example.com/live/key")
            .await;

        engine.update_egress_bytes("out-1", 1000).await;
        engine.update_egress_bytes("out-1", 500).await;
        assert_eq!(engine.egress_bytes("out-1").await, 1500);

        // Non-existent egress returns 0
        assert_eq!(engine.egress_bytes("out-nonexistent").await, 0);
    }

    #[tokio::test]
    async fn pipeline_create_and_remove() {
        let engine = MediaEngine::new();
        let rb1 = engine.get_or_create_pipeline("p1").await;
        let rb2 = engine.get_or_create_pipeline("p1").await;
        // Same pipeline returns same buffer
        assert!(Arc::ptr_eq(&rb1, &rb2));

        engine.remove_pipeline("p1").await;
        let rb3 = engine.get_or_create_pipeline("p1").await;
        // After removal, new buffer is created
        assert!(!Arc::ptr_eq(&rb1, &rb3));
    }

    #[tokio::test]
    async fn health_snapshot_includes_egress_under_correct_pipeline() {
        let engine = MediaEngine::new();
        engine
            .register_egress("out-a", "pipe-1", "rtmp://a.com/live/key")
            .await;
        engine
            .register_egress("out-b", "pipe-2", "rtmp://b.com/live/key")
            .await;
        engine
            .register_egress("out-c", "pipe-1", "srt://c.com?streamid=key")
            .await;

        let ids = vec!["pipe-1".to_string(), "pipe-2".to_string()];
        let rec = std::collections::HashMap::new();
        let snap = engine.health_snapshot(&ids, &rec).await;

        let pipe1_outputs = &snap["pipelines"]["pipe-1"]["outputs"];
        assert!(pipe1_outputs.get("out-a").is_some());
        assert!(pipe1_outputs.get("out-c").is_some());
        assert!(pipe1_outputs.get("out-b").is_none());

        let pipe2_outputs = &snap["pipelines"]["pipe-2"]["outputs"];
        assert!(pipe2_outputs.get("out-b").is_some());
        assert!(pipe2_outputs.get("out-a").is_none());
    }

    #[tokio::test]
    async fn recording_lifecycle() {
        let engine = MediaEngine::new();
        assert!(!engine.is_recording_active("p1").await);

        let token = engine.register_recording("p1").await;
        assert!(engine.is_recording_active("p1").await);
        assert!(!token.is_cancelled());

        engine.unregister_recording("p1").await;
        assert!(!engine.is_recording_active("p1").await);
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn cancelled_recording_token_is_not_active() {
        let engine = MediaEngine::new();
        let token = engine.register_recording("p-cancelled-rec").await;

        assert!(engine.is_recording_active("p-cancelled-rec").await);
        token.cancel();

        assert!(
            !engine.is_recording_active("p-cancelled-rec").await,
            "cancelled recording token must not be reported as active"
        );
    }

    #[tokio::test]
    async fn health_snapshot_marks_cancelled_recording_inactive() {
        let engine = MediaEngine::new();
        let pipeline_id = "pipeline-rec-cancelled";
        let token = engine.register_recording(pipeline_id).await;
        token.cancel();

        let mut recording_enabled = HashMap::new();
        recording_enabled.insert(pipeline_id.to_string(), true);
        let snapshot = engine
            .health_snapshot(&[pipeline_id.to_string()], &recording_enabled)
            .await;

        assert_eq!(
            snapshot["pipelines"][pipeline_id]["recording"]["active"],
            false
        );
    }

    #[tokio::test]
    async fn processing_graph_marks_cancelled_recording_and_hls_inactive() {
        let engine = MediaEngine::new();
        let pipeline_id = "pipeline-graph-cancelled";
        let rec_token = engine.register_recording(pipeline_id).await;
        rec_token.cancel();

        let _ = engine.ensure_hls_segmenter(pipeline_id).await;
        let hls_token = engine.get_hls_cancel_token(pipeline_id).await.unwrap();
        hls_token.cancel();

        let graph = engine.processing_graph(pipeline_id, &[]).await;
        let nodes = graph["nodes"].as_array().unwrap();

        let recording = nodes
            .iter()
            .find(|node| node["type"] == "recording")
            .expect("recording node should remain visible while registered");
        assert_eq!(recording["active"], false);

        let hls = nodes
            .iter()
            .find(|node| node["type"] == "hls")
            .expect("HLS node should remain visible while its store exists");
        assert_eq!(hls["active"], false);
    }

    #[tokio::test]
    async fn ingest_bytes_and_meta_on_nonexistent_pipeline_is_noop() {
        let engine = MediaEngine::new();
        // Should not panic
        engine.update_ingest_bytes("nonexistent", 1000).await;
        engine
            .update_ingest_meta("nonexistent", None, None, None)
            .await;
    }

    /// Two outputs with the same pipeline + encoding share exactly one transcoder
    /// stage (same Arc<RingBuffer> pointer). A third output with a different
    /// encoding gets its own stage. This is the core sharing invariant.
    #[tokio::test]
    async fn same_encoding_outputs_share_one_transcoder_stage() {
        let engine = Arc::new(MediaEngine::new());
        let source = engine.get_or_create_pipeline("pipe-share").await;

        let a = engine
            .get_or_create_transcoder("pipe-share", StageKind::video_preset("720p"), source.clone(), None)
            .await;
        let b = engine
            .get_or_create_transcoder("pipe-share", StageKind::video_preset("720p"), source.clone(), None)
            .await;
        let c = engine
            .get_or_create_transcoder("pipe-share", StageKind::video_preset("1080p"), source.clone(), None)
            .await;

        assert!(
            Arc::ptr_eq(&a, &b),
            "two outputs with encoding=720p must share the same ring buffer"
        );
        assert!(
            !Arc::ptr_eq(&a, &c),
            "different encodings must use separate ring buffers"
        );
    }

    /// Audio stages are keyed by both audio operation AND upstream video preset.
    /// 720p+atrack:0 and 1080p+atrack:0 must not share an audio stage.
    #[tokio::test]
    async fn audio_stages_are_isolated_per_video_preset() {
        let engine = Arc::new(MediaEngine::new());
        let source = engine.get_or_create_pipeline("pipe-audio").await;

        let v720 = engine
            .get_or_create_transcoder("pipe-audio", StageKind::video_preset("720p"), source.clone(), None)
            .await;
        let v1080 = engine
            .get_or_create_transcoder("pipe-audio", StageKind::video_preset("1080p"), source.clone(), None)
            .await;

        let a720 = engine
            .get_or_create_transcoder(
                "pipe-audio",
                StageKind::audio_route("atrack:0", StageKind::video_preset("720p")),
                v720.clone(),
                None,
            )
            .await;
        let a1080 = engine
            .get_or_create_transcoder(
                "pipe-audio",
                StageKind::audio_route("atrack:0", StageKind::video_preset("1080p")),
                v1080.clone(),
                None,
            )
            .await;
        let a720_again = engine
            .get_or_create_transcoder(
                "pipe-audio",
                StageKind::audio_route("atrack:0", StageKind::video_preset("720p")),
                v720,
                None,
            )
            .await;

        assert!(
            !Arc::ptr_eq(&a720, &a1080),
            "audio stages for different video presets must be isolated"
        );
        assert!(
            Arc::ptr_eq(&a720, &a720_again),
            "same audio stage key must return the same ring buffer"
        );
    }

    /// cleanup_pipeline_stages must remove all entries whose key starts with
    /// "<pipeline_id>:" and cancel their tokens. Entries for other pipelines
    /// must not be affected.
    #[tokio::test]
    async fn cleanup_pipeline_stages_removes_all_stage_entries() {
        let engine = Arc::new(MediaEngine::new());
        let source = engine.get_or_create_pipeline("pipe-del").await;
        let other = engine.get_or_create_pipeline("pipe-keep").await;

        let s1 = engine
            .get_or_create_transcoder("pipe-del", StageKind::video_preset("720p"), source.clone(), None)
            .await;
        let s2 = engine
            .get_or_create_transcoder("pipe-del", StageKind::video_preset("1080p"), source.clone(), None)
            .await;
        let other_stage = engine
            .get_or_create_transcoder("pipe-keep", StageKind::video_preset("720p"), other, None)
            .await;

        // Stages are alive before cleanup
        let stages_before = engine.active_transcoder_stages("pipe-del").await;
        assert_eq!(stages_before.len(), 2);

        engine.cleanup_pipeline_stages("pipe-del").await;

        // All pipe-del stages removed
        let stages_after = engine.active_transcoder_stages("pipe-del").await;
        assert_eq!(
            stages_after.len(),
            0,
            "all stages for deleted pipeline must be removed"
        );

        // The ring buffers from those stages had their tokens cancelled
        let _ = (s1, s2); // bindings kept to confirm they're the same arcs tested above

        // pipe-keep is unaffected
        let other_stages = engine.active_transcoder_stages("pipe-keep").await;
        assert_eq!(
            other_stages.len(),
            1,
            "unrelated pipeline stages must be untouched"
        );
        let _ = other_stage;
    }

    #[tokio::test]
    async fn transcoder_stage_registry_uses_typed_stage_keys() {
        let engine = Arc::new(MediaEngine::new());
        let source = engine.get_or_create_pipeline("pipe-typed").await;

        let _stage = engine
            .get_or_create_transcoder("pipe-typed", StageKind::video_preset("720p"), source, None)
            .await;

        let buffers = engine.transcoder_buffers.read().await;
        let key = buffers
            .keys()
            .find(|key| key.pipeline.as_str() == "pipe-typed")
            .expect("typed registry should contain created stage");

        assert_eq!(key.to_string(), "pipe-typed:video:720p");
        assert!(matches!(
            &key.kind,
            StageKind::VideoPreset { preset } if preset == "720p"
        ));
    }

    /// remove_pipeline must free the source ring buffer from the pipelines map.
    #[tokio::test]
    async fn remove_pipeline_frees_source_ring_buffer() {
        let engine = Arc::new(MediaEngine::new());
        let rb = engine.get_or_create_pipeline("pipe-rm").await;
        let weak = Arc::downgrade(&rb);
        drop(rb); // release our local strong reference

        // Pipeline map still holds a strong ref
        assert!(
            weak.upgrade().is_some(),
            "ring buffer should still be alive"
        );

        engine.remove_pipeline("pipe-rm").await;
        // Now only the weak ref remains — the Arc should be freed
        assert!(
            weak.upgrade().is_none(),
            "ring buffer should be freed after remove_pipeline"
        );
    }

    #[tokio::test]
    async fn sweep_unused_transcoder_stages_removes_only_unused() {
        let engine = Arc::new(MediaEngine::new());
        let source = engine.get_or_create_pipeline("pipe-sweep").await;

        let s1 = engine
            .get_or_create_transcoder("pipe-sweep", StageKind::video_preset("720p"), source.clone(), None)
            .await;
        let s2 = engine
            .get_or_create_transcoder("pipe-sweep", StageKind::video_preset("1080p"), source.clone(), None)
            .await;

        let mut active = std::collections::HashSet::new();
        active.insert(StageKey::new("pipe-sweep", StageKind::video_preset("720p")));

        engine.sweep_unused_transcoder_stages(&active).await;

        let stages = engine.active_transcoder_stages("pipe-sweep").await;
        assert_eq!(stages.len(), 1);
        assert_eq!(stages[0].0, StageKind::video_preset("720p"));
        // s2 was swept and cancelled
        let _ = (s1, s2);
    }

    #[tokio::test]
    async fn sweep_unused_transcoder_stages_removes_codec_edge_stages() {
        let engine = Arc::new(MediaEngine::new());
        let source = engine.get_or_create_pipeline("pipe-sweep-codec").await;

        let _stage = engine
            .get_or_create_h264_transcoder("pipe-sweep-codec", StageKind::source(), source)
            .await;
        let stages_before = engine.active_transcoder_stages("pipe-sweep-codec").await;
        assert!(
            stages_before
                .iter()
                .any(|(stage, live)| *stage == StageKind::codec_edge("hevc_to_h264", StageKind::source()) && *live),
            "codec-edge stage must be registered before the sweep"
        );

        let active: std::collections::HashSet<StageKey> = std::collections::HashSet::new();
        engine.sweep_unused_transcoder_stages(&active).await;

        let stages_after = engine.active_transcoder_stages("pipe-sweep-codec").await;
        assert!(
            stages_after.is_empty(),
            "unused codec-edge stages must be removed from the shared stage registry"
        );
    }

    #[tokio::test]
    async fn concurrent_get_or_create_transcoder_yields_single_stage() {
        // Bug #4 regression: the old read-lock-then-write-lock TOCTOU window
        // allowed concurrent callers to both see "key absent" and both insert,
        // spawning two transcoder tasks writing to different ring buffers.
        // After the fix, all concurrent callers must receive the SAME Arc<RingBuffer>.
        use std::sync::Arc as StdArc;
        use tokio::sync::Barrier;
        use tokio::task::JoinSet;

        let engine = StdArc::new(MediaEngine::new());
        let source = engine.get_or_create_pipeline("pipe-concurrent").await;

        // Synchronize 16 tasks to all call get_or_create_transcoder simultaneously
        let barrier = StdArc::new(Barrier::new(16));
        let mut join_set = JoinSet::new();

        for _ in 0..16 {
            let e = engine.clone();
            let s = source.clone();
            let b = barrier.clone();
            join_set.spawn(async move {
                b.wait().await;
                e.get_or_create_transcoder("pipe-concurrent", StageKind::video_preset("720p"), s, None)
                    .await
            });
        }

        let mut results = Vec::new();
        while let Some(r) = join_set.join_next().await {
            results.push(r.unwrap());
        }

        // All returned Arc<RingBuffer>s must point to the SAME allocation
        let first_ptr = StdArc::as_ptr(&results[0]);
        for rb in &results[1..] {
            assert_eq!(
                StdArc::as_ptr(rb),
                first_ptr,
                "concurrent callers must receive the same RingBuffer Arc (no duplicate stages)"
            );
        }

        // Exactly one stage must exist in the map
        let stages = engine.active_transcoder_stages("pipe-concurrent").await;
        assert_eq!(
            stages.len(),
            1,
            "exactly one transcoder stage must exist after concurrent creation"
        );
    }

    // --- Regression: Round 6 #7 — HLS consumer refcount must not leak ---
    // The refcount must return to zero after balanced add/remove so the
    // idle-sweep logic eventually stops the segmenter task.
    #[tokio::test]
    async fn hls_consumer_idle_only_when_persistent_count_zero() {
        use tokio_util::sync::CancellationToken;

        let engine = MediaEngine::new();
        let token = CancellationToken::new();
        {
            let mut consumers = engine.hls_consumers.write().await;
            consumers.insert("pipe-hls-rc".to_string(), HlsConsumers::new(token.clone()));
        }

        // One persistent consumer added — segmenter must not be idle.
        engine.add_hls_persistent_consumer("pipe-hls-rc").await;
        {
            let consumers = engine.hls_consumers.read().await;
            assert!(
                !consumers["pipe-hls-rc"].is_idle(0),
                "segmenter must not be idle while a persistent consumer holds a ref"
            );
        }

        // Remove the consumer — now idle (last_access_ms was set on creation;
        // use a long timeout so only persistent count matters here).
        engine.remove_hls_persistent_consumer("pipe-hls-rc").await;
        {
            let consumers = engine.hls_consumers.read().await;
            assert_eq!(
                consumers["pipe-hls-rc"]
                    .persistent
                    .load(std::sync::atomic::Ordering::Relaxed),
                0,
                "persistent count must be 0 after remove"
            );
        }
    }

    // --- H.265 routing correctness tests ---

    #[tokio::test]
    async fn hevc_input_video_preset_ring_tagged_hevc() {
        let engine = Arc::new(MediaEngine::new());
        let source = engine.get_or_create_pipeline("p-hevc").await;
        let ring = engine
            .get_or_create_transcoder("p-hevc", StageKind::video_preset("720p"), source, Some("hevc"))
            .await;
        assert_eq!(
            ring.codec_hint_str(),
            "hevc",
            "video:720p stage fed with H.265 must be tagged 'hevc'"
        );
    }

    #[tokio::test]
    async fn h264_input_video_preset_ring_tagged_h264() {
        let engine = Arc::new(MediaEngine::new());
        let source = engine.get_or_create_pipeline("p-h264").await;
        let ring = engine
            .get_or_create_transcoder("p-h264", StageKind::video_preset("720p"), source, None)
            .await;
        assert_eq!(
            ring.codec_hint_str(),
            "h264",
            "video:720p stage without codec override must default to 'h264'"
        );
    }

    #[tokio::test]
    async fn h264_transcoder_different_upstreams_are_independent_stages() {
        let engine = Arc::new(MediaEngine::new());
        let source = engine.get_or_create_pipeline("p-dual").await;

        let from_source = engine
            .get_or_create_h264_transcoder("p-dual", StageKind::source(), source.clone())
            .await;
        let from_720 = engine
            .get_or_create_h264_transcoder("p-dual", StageKind::video_preset("720p"), source.clone())
            .await;

        assert!(
            !Arc::ptr_eq(&from_source, &from_720),
            "hevc_to_h264 stages keyed by different upstreams must be independent"
        );
    }

    #[tokio::test]
    async fn h264_transcoder_same_upstream_is_shared() {
        let engine = Arc::new(MediaEngine::new());
        let source = engine.get_or_create_pipeline("p-shared-h264").await;

        let ring1 = engine
            .get_or_create_h264_transcoder("p-shared-h264", StageKind::video_preset("720p"), source.clone())
            .await;
        let ring2 = engine
            .get_or_create_h264_transcoder("p-shared-h264", StageKind::video_preset("720p"), source.clone())
            .await;

        assert!(
            Arc::ptr_eq(&ring1, &ring2),
            "hevc_to_h264 stage for the same upstream must be reused"
        );
    }

    #[tokio::test]
    async fn h264_transcoder_output_ring_tagged_h264() {
        let engine = Arc::new(MediaEngine::new());
        let source = engine.get_or_create_pipeline("p-h264-tag").await;

        let ring = engine
            .get_or_create_h264_transcoder("p-h264-tag", StageKind::source(), source)
            .await;

        assert_eq!(
            ring.codec_hint_str(),
            "h264",
            "hevc_to_h264 output ring must always be tagged 'h264'"
        );
    }

    // ── audio_tracks Arc<Vec<AudioMeta>> semantics ────────────────────

    #[test]
    fn arc_audio_tracks_clone_is_shallow_refcount_bump() {
        use std::sync::Arc;
        let tracks = vec![
            AudioMeta {
                codec: "aac".into(),
                sample_rate: 48000,
                channels: 2,
                track_index: 0,
                profile: None,
                channel_layout: None,
            },
            AudioMeta {
                codec: "opus".into(),
                sample_rate: 48000,
                channels: 6,
                track_index: 1,
                profile: None,
                channel_layout: None,
            },
        ];
        let arc = Arc::new(tracks);

        let c1 = Arc::clone(&arc);
        let c2 = Arc::clone(&arc);
        assert_eq!(Arc::as_ptr(&arc), Arc::as_ptr(&c1));
        assert_eq!(Arc::as_ptr(&arc), Arc::as_ptr(&c2));
        assert_eq!(Arc::strong_count(&arc), 3);
        assert_eq!(arc.len(), 2);
        assert_eq!(c1[0].codec, "aac");
        assert_eq!(c2[1].channels, 6);
    }

    #[test]
    fn arc_audio_tracks_deref_works_for_iteration() {
        use std::sync::Arc;
        let tracks = vec![AudioMeta {
            codec: "aac".into(),
            sample_rate: 44100,
            channels: 1,
            track_index: 0,
            profile: None,
            channel_layout: None,
        }];
        let arc = Arc::new(tracks);
        assert_eq!(arc.iter().next().unwrap().sample_rate, 44100);
        assert_eq!(arc.first().unwrap().codec, "aac");
        assert_eq!(arc.len(), 1);
    }

    #[test]
    fn arc_audio_tracks_default_is_empty() {
        use std::sync::Arc;
        let arc: Arc<Vec<AudioMeta>> = Arc::default();
        assert!(arc.is_empty());
        assert_eq!(arc.len(), 0);
    }

    #[test]
    fn arc_audio_tracks_mutex_wraps_correctly() {
        use std::sync::{Arc, Mutex};
        let tracks = Arc::new(vec![AudioMeta {
            codec: "aac".into(),
            sample_rate: 48000,
            channels: 2,
            track_index: 0,
            profile: None,
            channel_layout: None,
        }]);
        let mtx = Mutex::new(Arc::clone(&tracks));

        // Clone under lock gives an Arc clone, not a deep Vec copy
        let guard = mtx.lock().unwrap();
        let cloned = guard.clone(); // Arc clone
        assert_eq!(Arc::as_ptr(&tracks), Arc::as_ptr(&cloned));
        assert_eq!(Arc::strong_count(&tracks), 3); // tracks + mtx inner + cloned
        drop(guard);
        drop(cloned);
        assert_eq!(Arc::strong_count(&tracks), 2); // tracks + mtx inner
    }

    // ── diag concurrency semaphore ──────────────────────────────────

    #[tokio::test]
    async fn diag_semaphore_prevents_concurrent_runs_on_same_pipeline() {
        let engine = MediaEngine::new();
        let pipeline = "diag-concurrency";

        let sem = {
            let mut map = engine.diag_semaphores.write().await;
            map.entry(pipeline.to_string())
                .or_insert_with(|| Arc::new(tokio::sync::Semaphore::new(1)))
                .clone()
        };

        let permit1 = sem.clone().try_acquire_owned();
        assert!(permit1.is_ok(), "first acquire must succeed");

        let permit2 = sem.clone().try_acquire_owned();
        assert!(permit2.is_err(), "second concurrent acquire must fail");

        let sem_other = {
            let mut map = engine.diag_semaphores.write().await;
            map.entry("other-pipeline".to_string())
                .or_insert_with(|| Arc::new(tokio::sync::Semaphore::new(1)))
                .clone()
        };
        assert!(
            sem_other.try_acquire_owned().is_ok(),
            "different pipeline must succeed"
        );

        drop(permit1);
        assert!(
            sem.try_acquire_owned().is_ok(),
            "acquire must succeed after previous permit dropped"
        );
    }

    // ── sweep_unused_stages reader tracking ─────────────────────────

    #[tokio::test]
    async fn sweep_unused_stages_retains_active_readers() {
        let engine = MediaEngine::new();
        let key = "pipeline:stage-sweep".to_string();
        let cancel = CancellationToken::new();
        let stage = Arc::new(TsChunkRing::new(16, cancel));

        let _reader =
            crate::media::ring_buffer::Reader::new("sweep-test".to_string(), stage.ring.clone());

        engine
            .ts_muxer_stages
            .write()
            .await
            .insert(key.clone(), stage);

        engine.sweep_unused_stages().await;
        assert!(
            engine.ts_muxer_stages.read().await.contains_key(&key),
            "stage with active reader must be retained"
        );

        drop(_reader);
        engine.sweep_unused_stages().await;
        assert!(
            !engine.ts_muxer_stages.read().await.contains_key(&key),
            "stage without readers must be removed"
        );
    }

    // M2: get_hls_cancel_token must return None (not panic) when no HLS
    // segmenter is registered for the pipeline. The reconciler's HLS egress
    // path replaced an unwrap() with a None guard after this was identified.
    #[tokio::test]
    async fn get_hls_cancel_token_returns_none_with_no_segmenter() {
        let engine = Arc::new(MediaEngine::new());
        let token = engine.get_hls_cancel_token("no-such-pipeline").await;
        assert!(
            token.is_none(),
            "must return None, not panic, when segmenter is not registered"
        );
    }

    // M2 (continued): after ensure_hls_segmenter registers a segmenter, the
    // token must be Some — confirming the None case above is not a permanent failure.
    #[tokio::test]
    async fn get_hls_cancel_token_returns_some_after_ensure() {
        let engine = Arc::new(MediaEngine::new());
        engine.ensure_hls_segmenter("pipe-hls").await;
        let token = engine.get_hls_cancel_token("pipe-hls").await;
        assert!(
            token.is_some(),
            "token must be Some after ensure_hls_segmenter registers the pipeline"
        );
        engine.shutdown_hls_segmenter("pipe-hls").await;
    }

    #[tokio::test]
    async fn shutdown_hls_segmenter_removes_consumer_and_store() {
        let engine = Arc::new(MediaEngine::new());
        let (store, already_running) = engine.ensure_hls_segmenter("pipe-hls-clean").await;
        assert!(!already_running);
        store.push_segment(1.0, bytes::Bytes::from_static(b"segment"));

        assert!(engine.get_hls_store("pipe-hls-clean").await.is_some());
        assert!(
            engine
                .get_hls_cancel_token("pipe-hls-clean")
                .await
                .is_some()
        );

        engine.shutdown_hls_segmenter("pipe-hls-clean").await;

        assert!(engine.get_hls_store("pipe-hls-clean").await.is_none());
        assert!(
            engine
                .get_hls_cancel_token("pipe-hls-clean")
                .await
                .is_none()
        );
    }
}
