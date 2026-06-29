//! Central media engine state — owns all active ingests, egresses, ring buffers,
//! and recordings. Byte counters use `AtomicU64` for lock-free updates from the
//! hot ingest/egress paths; the `health_snapshot()` method reads them atomically
//! to build the JSON returned by `/api/v1/engine/health`.

use ffmpeg_next as ffmpeg;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info};

use crate::domain::stage::{StageKey, StageKind};
use crate::media::avio::MemoryQueue;
use crate::media::engine_registries::{
    EgressRegistry, FileIngestRegistry, HlsRegistry, IngestRegistry, RecordingRegistry,
    RuntimeInfra, StageRegistry,
};
use crate::media::hls::HlsStore;
use crate::media::ring_buffer::{
    RingBuffer, default_ring_capacity, default_transcoder_ring_capacity,
};
use crate::media::ts_chunk_ring::TsChunkRing;
use crate::planner::backend_policy::{BackendPolicy, StageBackend};

pub use crate::media::pipe_metrics::PipeMetrics;
pub use crate::media::stage_metrics::StageMetrics;

pub(crate) const EGRESS_PROGRESS_STALE_MS: u64 = 10_000;

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
    pub tcp_congestion_algorithm: Option<String>,
    pub tcp_rtt_ms: Option<f64>,
    pub tcp_rtt_var_ms: Option<f64>,
    pub tcp_bytes_received: Option<u64>,
    pub tcp_bytes_sent: Option<u64>,
    pub tcp_bytes_acked: Option<u64>,
    pub tcp_bytes_retrans: Option<u64>,
    pub tcp_last_rcv_ms: Option<u64>,
    pub tcp_last_snd_ms: Option<u64>,
    pub tcp_rcv_rtt_ms: Option<f64>,
    pub tcp_rcv_space: Option<u64>,
    pub tcp_rcv_ooopack: Option<u64>,
    pub tcp_snd_mss: Option<u64>,
    pub tcp_pmtu: Option<u64>,
    pub tcp_unacked: Option<u64>,
    pub tcp_sacked: Option<u64>,
    pub tcp_lost: Option<u64>,
    pub tcp_retrans: Option<u64>,
    pub tcp_snd_cwnd: Option<u64>,
    pub tcp_snd_ssthresh: Option<u64>,
    pub tcp_advmss: Option<u64>,
    pub tcp_reordering: Option<u64>,
    pub tcp_notsent_bytes: Option<u64>,
    pub tcp_total_retrans: Option<u64>,
    pub tcp_pacing_rate_bps: Option<u64>,
    pub tcp_max_pacing_rate_bps: Option<u64>,
    pub tcp_delivery_rate_bps: Option<u64>,
    pub tcp_segs_out: Option<u64>,
    pub tcp_data_segs_out: Option<u64>,
    pub tcp_delivered: Option<u64>,
    pub tcp_delivered_ce: Option<u64>,
    pub tcp_busy_time_ms: Option<u64>,
    pub tcp_rwnd_limited_ms: Option<u64>,
    pub tcp_sndbuf_limited_ms: Option<u64>,
    pub tcp_dsack_dups: Option<u64>,
    pub tcp_reord_seen: Option<u64>,
    pub tcp_snd_wnd: Option<u64>,
    pub tcp_total_rto: Option<u64>,
    pub tcp_total_rto_recoveries: Option<u64>,
    pub tcp_total_rto_time_ms: Option<u64>,
    pub tcp_skmem_rmem_alloc: Option<u64>,
    pub tcp_skmem_rmem_max: Option<u64>,
    pub tcp_skmem_wmem_alloc: Option<u64>,
    pub tcp_skmem_wmem_max: Option<u64>,
    pub tcp_receive_rate_mbps: Option<f64>,
    pub tcp_send_rate_mbps: Option<f64>,
    pub tcp_stats_unavailable_reason: Option<String>,
    // SRT metrics
    pub ms_rtt: Option<f64>,
    pub mbps_send_rate: Option<f64>,
    pub mbps_receive_rate: Option<f64>,
    pub mbps_link_capacity: Option<f64>,
    pub ms_send_tsb_pd_delay: Option<f64>,
    pub ms_receive_tsb_pd_delay: Option<f64>,
    pub ms_send_buf: Option<f64>,
    pub ms_receive_buf: Option<f64>,
    pub packets_sent_loss: Option<u64>,
    pub packets_sent_drop: Option<u64>,
    pub packets_sent_retrans: Option<u64>,
    pub packets_sent_nak: Option<u64>,
    pub packets_received_nak: Option<u64>,
    pub packets_received_loss: Option<u64>,
    pub packets_received_drop: Option<u64>,
    pub packets_received_retrans: Option<u64>,
    pub packets_received_undecrypt: Option<u64>,
    pub packets_sent_loss_per_sec: Option<f64>,
    pub packets_sent_drop_per_sec: Option<f64>,
    pub packets_sent_retrans_per_sec: Option<f64>,
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
    pub srt_flow_window_pkts: Option<i32>,
    pub srt_congestion_window_pkts: Option<i32>,
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
    pub pid: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
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
    pub pid: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
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
    pub protocol: String,
    pub target_url: String,
    pub target_addr: Arc<std::sync::Mutex<Option<String>>>,
    pub status: String, // "running" | "stopped" | "failed"
    pub phase: Arc<std::sync::Mutex<String>>,
    pub started_at: String,
    pub start_instant: Instant,
    pub bytes_sent: Arc<AtomicU64>,
    pub metrics: Arc<StageMetrics>,
    pub last_progress_ms: Arc<AtomicU64>,
    pub last_error: Arc<std::sync::Mutex<Option<String>>>,
    pub last_error_ms: Arc<AtomicU64>,
    pub failure_phase: Arc<std::sync::Mutex<Option<String>>>,
    pub quality: Arc<std::sync::Mutex<PublisherQuality>>,
    pub prev_bytes_sent: AtomicU64,
    pub prev_sample_time: std::sync::Mutex<Instant>,
    pub bitrate_kbps: std::sync::Mutex<Option<f64>>,
}

#[derive(Debug, Clone)]
pub struct RecentIngestOutcome {
    pub protocol: String,
    pub disconnected_at_ms: u64,
    pub reason: Option<String>,
    pub failure_phase: Option<String>,
    pub had_error: bool,
    pub remote_addr: Option<String>,
    pub bytes_received: u64,
}

#[derive(Debug, Clone)]
pub struct RecentEgressOutcome {
    pub output_id: String,
    pub pipeline_id: String,
    pub protocol: String,
    pub target_url: String,
    pub target_addr: Option<String>,
    pub status: String,
    pub raw_status: String,
    pub phase: String,
    pub started_at: String,
    pub uptime_secs: f64,
    pub bytes_sent: u64,
    pub last_progress_ms: u64,
    pub last_error: Option<String>,
    pub last_error_ms: u64,
    pub failure_phase: Option<String>,
    pub quality: PublisherQuality,
    pub metrics: serde_json::Value,
    pub ended_at_ms: u64,
}

#[derive(Debug, Clone)]
pub struct IngestDiagSnapshot {
    pub protocol: String,
    pub uptime_secs: f64,
    pub bytes_received: u64,
    pub remote_addr: Option<String>,
    pub video: Option<VideoMeta>,
    pub audio: Option<AudioMeta>,
    pub quality: PublisherQuality,
    pub keyframe_times: Vec<i64>,
}

#[derive(Debug, Clone)]
pub struct EgressDiagSnapshot {
    pub output_id: String,
    pub pipeline_id: String,
    pub protocol: String,
    pub status: String,
    pub phase: String,
    pub target_addr: Option<String>,
    pub bytes_sent: u64,
    pub last_progress_ms: u64,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RingBufferDiagSnapshot {
    pub fill_slots: usize,
    pub capacity_slots: usize,
    pub readers: Vec<crate::media::ring_buffer::ReaderSnapshot>,
}

#[derive(Debug, Clone)]
pub struct SrtListenerDiagSnapshot {
    pub bonding_available: bool,
    pub rx_queue_bytes: u64,
    pub rx_queue_peak_bytes: u64,
    pub drops: u64,
    pub active_ingest_count: usize,
}

#[derive(Debug, Clone)]
pub struct HlsDependencySnapshot {
    pub store_exists: bool,
    pub active: bool,
    pub persistent_consumers: u64,
    pub last_access_age_ms: Option<u64>,
    pub segments: usize,
    pub playlist_bytes: usize,
}

#[derive(Debug, Clone)]
pub struct FileIngestDependencySnapshot {
    pub marked_active: bool,
    pub child_registered: bool,
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
    pub ingests: IngestRegistry,
    pub egresses: EgressRegistry,
    pub recordings: RecordingRegistry,
    pub hls: HlsRegistry,
    pub file_ingests: FileIngestRegistry,
    pub stages: StageRegistry,
    pub runtime: RuntimeInfra,
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
            error!(err = %e, "fatal: FFmpeg initialization failed; check library paths");
            std::process::exit(1);
        }
        ffmpeg::util::log::set_level(ffmpeg::util::log::Level::Warning);

        Self {
            ingests: IngestRegistry::new(),
            egresses: EgressRegistry::new(),
            recordings: RecordingRegistry::new(),
            hls: HlsRegistry::new(),
            file_ingests: FileIngestRegistry::new(),
            stages: StageRegistry::new(),
            runtime: RuntimeInfra::new(),
        }
    }

    pub(crate) fn now_epoch_ms() -> u64 {
        chrono::Utc::now().timestamp_millis().max(0) as u64
    }

    pub(crate) fn epoch_ms_to_rfc3339(ms: u64) -> Option<String> {
        if ms == 0 {
            return None;
        }
        chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms as i64).map(|dt| dt.to_rfc3339())
    }

    pub fn set_event_sink(&self, sink: tokio::sync::mpsc::UnboundedSender<crate::events::Event>) {
        self.runtime.event_log.set_sink(sink);
    }

    pub fn recent_events(
        &self,
        limit: usize,
        pipeline_id: Option<&str>,
    ) -> Vec<crate::events::Event> {
        self.runtime.event_log.recent(limit, pipeline_id)
    }

    pub async fn with_active_ingest<R>(
        &self,
        pipeline_id: &str,
        f: impl FnOnce(&ActiveIngest) -> R,
    ) -> Option<R> {
        let ingests = self.ingests.active.read().await;
        ingests.get(pipeline_id).map(f)
    }

    pub async fn with_active_egress<R>(
        &self,
        output_id: &str,
        f: impl FnOnce(&ActiveEgress) -> R,
    ) -> Option<R> {
        let egresses = self.egresses.active.read().await;
        egresses.get(output_id).map(f)
    }

    pub fn listener_stats_handle(&self) -> Arc<ListenerSocketStats> {
        self.runtime.listener_stats.clone()
    }

    pub fn sender_semaphore_handle(&self) -> Arc<tokio::sync::Semaphore> {
        self.runtime.sender_semaphore.clone()
    }

    pub async fn stop_file_ingest_child(&self, ingest_id: &str) -> bool {
        let mut children = self.file_ingests.children.write().await;
        let Some(mut child) = children.remove(ingest_id) else {
            return false;
        };
        drop(children);
        let _ = child.kill().await;
        let _ = child.wait().await;
        true
    }

    pub async fn take_file_ingest_child(&self, ingest_id: &str) -> Option<tokio::process::Child> {
        self.file_ingests.children.write().await.remove(ingest_id)
    }

    pub async fn hls_dependency_snapshot(&self, pipeline_id: &str) -> HlsDependencySnapshot {
        let consumers = self.hls.consumers.read().await;
        let stores = self.hls.stores.read().await;

        let consumer = consumers.get(pipeline_id);
        let store = stores.get(pipeline_id);
        let snapshot = store.and_then(|store| store.snapshot());

        HlsDependencySnapshot {
            store_exists: store.is_some(),
            active: consumer.is_some_and(|consumer| !consumer.cancel_token.is_cancelled()),
            persistent_consumers: consumer
                .map(|consumer| consumer.persistent.load(Ordering::Relaxed))
                .unwrap_or(0),
            last_access_age_ms: consumer.map(|consumer| {
                let now = consumer.reference_instant.elapsed().as_millis() as u64;
                let last = consumer.last_access_ms.load(Ordering::Relaxed);
                now.saturating_sub(last)
            }),
            segments: snapshot
                .as_ref()
                .map(|snapshot| snapshot.segments.len())
                .unwrap_or(0),
            playlist_bytes: snapshot
                .as_ref()
                .map(|snapshot| snapshot.playlist.len())
                .unwrap_or(0),
        }
    }

    pub async fn file_ingest_dependency_snapshot(
        &self,
        ingest_id: &str,
    ) -> FileIngestDependencySnapshot {
        let active = self.file_ingests.active.read().await;
        let children = self.file_ingests.children.read().await;
        FileIngestDependencySnapshot {
            marked_active: active.contains(ingest_id),
            child_registered: children.contains_key(ingest_id),
        }
    }

    pub fn bonding_available(&self) -> bool {
        self.runtime
            .listener_stats
            .bonding_available
            .load(Ordering::Relaxed)
    }

    pub(crate) fn egress_protocol_from_url(url: &str) -> &'static str {
        if url.starts_with("rtmp://") || url.starts_with("rtmps://") {
            "rtmp"
        } else if url.starts_with("srt://") {
            "srt"
        } else if url.starts_with("hls://")
            || url.starts_with("http://")
            || url.starts_with("https://")
        {
            "hls"
        } else {
            "unknown"
        }
    }

    pub(crate) fn graph_protocol_label(protocol: &str) -> String {
        if protocol.is_empty() || protocol == "unknown" {
            "Unknown".to_string()
        } else {
            protocol.to_uppercase()
        }
    }

    pub(crate) fn graph_slug(value: &str) -> String {
        let slug: String = value
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() {
                    ch.to_ascii_lowercase()
                } else {
                    '_'
                }
            })
            .collect();
        slug.trim_matches('_').to_string()
    }

    pub(crate) fn source_buffer_format(protocol: Option<&str>) -> &'static str {
        match protocol {
            Some("rtmp") => "FLV media packets",
            Some("srt") => "Demuxed MPEG-TS media packets",
            Some("file") => "Demuxed file media packets",
            _ => "Media packets",
        }
    }

    pub(crate) fn source_to_egress_label(protocol: &str) -> &'static str {
        match protocol {
            "rtmp" => "RTMP publish packets",
            "srt" => "MPEG-TS packetization",
            "hls" => "HLS segment input",
            _ => "media packets",
        }
    }

    pub(crate) fn egress_effective_status(egress: &ActiveEgress, has_ingest: bool) -> String {
        if !has_ingest {
            return "stopped".to_string();
        }

        let phase = egress
            .phase
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if phase == "failed" {
            return "failed".to_string();
        }
        if egress.status != "running" {
            return egress.status.clone();
        }
        if egress.target_url.starts_with("hls://") && phase == "segmenting" {
            return "running".to_string();
        }

        let last_progress_ms = egress.last_progress_ms.load(Ordering::Relaxed);
        let now_ms = Self::now_epoch_ms();
        let no_progress_too_long = last_progress_ms == 0
            && egress.start_instant.elapsed().as_millis() as u64 >= EGRESS_PROGRESS_STALE_MS;
        let stale_progress = last_progress_ms > 0
            && now_ms.saturating_sub(last_progress_ms) >= EGRESS_PROGRESS_STALE_MS;
        if no_progress_too_long || stale_progress {
            return "stalled".to_string();
        }

        "running".to_string()
    }

    pub(crate) fn egress_runtime_json(
        egress: &ActiveEgress,
        include_target_url: bool,
        has_ingest: bool,
    ) -> serde_json::Value {
        let last_progress_ms = egress.last_progress_ms.load(Ordering::Relaxed);
        let last_error_ms = egress.last_error_ms.load(Ordering::Relaxed);
        let now_ms = Self::now_epoch_ms();
        let status = Self::egress_effective_status(egress, has_ingest);
        let mut value = serde_json::json!({
            "outputId": egress.output_id.clone(),
            "pipelineId": egress.pipeline_id.clone(),
            "protocol": egress.protocol.clone(),
            "targetAddr": egress.target_addr.lock().unwrap_or_else(|e| e.into_inner()).clone(),
            "status": status,
            "rawStatus": egress.status.clone(),
            "phase": egress.phase.lock().unwrap_or_else(|e| e.into_inner()).clone(),
            "uptimeSecs": egress.start_instant.elapsed().as_secs_f64(),
            "bytesOut": egress.bytes_sent.load(Ordering::Relaxed),
            "lastProgressAt": Self::epoch_ms_to_rfc3339(last_progress_ms),
            "lastProgressAgeMs": (last_progress_ms > 0).then(|| now_ms.saturating_sub(last_progress_ms)),
            "lastError": egress.last_error.lock().unwrap_or_else(|e| e.into_inner()).clone(),
            "lastErrorAt": Self::epoch_ms_to_rfc3339(last_error_ms),
            "failurePhase": egress.failure_phase.lock().unwrap_or_else(|e| e.into_inner()).clone(),
            "quality": egress.quality.lock().unwrap_or_else(|e| e.into_inner()).clone(),
            "metrics": egress.metrics.snapshot(),
        });
        if include_target_url {
            value["targetUrl"] = serde_json::Value::String(egress.target_url.clone());
        }
        value
    }

    fn recent_egress_status(egress: &ActiveEgress, has_ingest: bool) -> String {
        let phase = egress
            .phase
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if phase == "failed"
            || egress
                .last_error
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_some()
        {
            return "failed".to_string();
        }
        if !has_ingest {
            return "stopped".to_string();
        }
        Self::egress_effective_status(egress, has_ingest)
    }

    pub(crate) fn recent_egress_runtime_json(
        outcome: &RecentEgressOutcome,
        include_target_url: bool,
    ) -> serde_json::Value {
        let now_ms = Self::now_epoch_ms();
        let mut value = serde_json::json!({
            "outputId": outcome.output_id,
            "pipelineId": outcome.pipeline_id,
            "protocol": outcome.protocol,
            "targetAddr": outcome.target_addr,
            "status": outcome.status,
            "rawStatus": outcome.raw_status,
            "phase": outcome.phase,
            "uptimeSecs": outcome.uptime_secs,
            "bytesOut": outcome.bytes_sent,
            "lastProgressAt": Self::epoch_ms_to_rfc3339(outcome.last_progress_ms),
            "lastProgressAgeMs": (outcome.last_progress_ms > 0).then(|| now_ms.saturating_sub(outcome.last_progress_ms)),
            "lastError": outcome.last_error,
            "lastErrorAt": Self::epoch_ms_to_rfc3339(outcome.last_error_ms),
            "failurePhase": outcome.failure_phase,
            "quality": outcome.quality,
            "metrics": outcome.metrics,
            "endedAt": Self::epoch_ms_to_rfc3339(outcome.ended_at_ms),
            "endedAgeMs": now_ms.saturating_sub(outcome.ended_at_ms),
        });
        if include_target_url {
            value["targetUrl"] = serde_json::Value::String(outcome.target_url.clone());
        }
        value
    }

    pub async fn output_status(&self, output_id: &str) -> Option<serde_json::Value> {
        crate::media::engine_views::output_status(self, output_id).await
    }

    /// Register an OS thread JoinHandle so it can be joined at shutdown.
    /// Already-finished handles are pruned opportunistically to prevent unbounded accumulation
    /// in long-running servers with many short-lived per-connection threads.
    pub fn register_os_thread(&self, handle: std::thread::JoinHandle<()>) {
        let mut guards = self
            .runtime
            .os_threads
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        guards.retain(|h| !h.is_finished());
        guards.push(handle);
    }

    /// Drain all registered OS thread handles for joining at shutdown.
    pub fn drain_os_thread_handles(&self) -> Vec<std::thread::JoinHandle<()>> {
        self.runtime
            .os_threads
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
            .collect()
    }

    pub async fn has_active_ingest(&self, pipeline_id: &str) -> bool {
        self.ingests.active.read().await.contains_key(pipeline_id)
    }

    pub async fn has_recent_ingest_disconnect(&self, pipeline_id: &str, grace_ms: u64) -> bool {
        if grace_ms == 0 {
            return false;
        }
        let recent = self.ingests.recent.read().await;
        recent.get(pipeline_id).is_some_and(|outcome| {
            Self::now_epoch_ms().saturating_sub(outcome.disconnected_at_ms) < grace_ms
        })
    }

    pub async fn active_ingest_count(&self) -> usize {
        self.ingests.active.read().await.len()
    }

    pub async fn active_ingest_protocol_for_probe(&self, pipeline_id: &str) -> Option<String> {
        self.ingests
            .active
            .read()
            .await
            .get(pipeline_id)
            .map(|ingest| match ingest.protocol.as_str() {
                "file" => "rtmp".to_string(),
                protocol => protocol.to_string(),
            })
    }

    pub async fn ingest_video_codec(&self, pipeline_id: &str) -> Option<String> {
        self.ingests
            .active
            .read()
            .await
            .get(pipeline_id)
            .and_then(|ingest| ingest.video.as_ref())
            .map(|video| video.codec.clone())
    }

    pub async fn active_ingest_diag_snapshot(
        &self,
        pipeline_id: &str,
    ) -> Option<IngestDiagSnapshot> {
        let ingests = self.ingests.active.read().await;
        let ingest = ingests.get(pipeline_id)?;
        let keyframe_times = ingest
            .keyframe_times
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        Some(IngestDiagSnapshot {
            protocol: ingest.protocol.clone(),
            uptime_secs: ingest.start_time.elapsed().as_secs_f64(),
            bytes_received: ingest.bytes_received.load(Ordering::Relaxed),
            remote_addr: ingest.remote_addr.clone(),
            video: ingest.video.clone(),
            audio: ingest.audio.clone(),
            quality: ingest.quality.clone(),
            keyframe_times,
        })
    }

    pub async fn has_active_egress(&self, output_id: &str) -> bool {
        self.egresses
            .cancel_tokens
            .read()
            .await
            .contains_key(output_id)
    }

    pub async fn active_egress_count(&self) -> usize {
        self.egresses.active.read().await.len()
    }

    pub async fn active_egress_diag_snapshots(&self, pipeline_id: &str) -> Vec<EgressDiagSnapshot> {
        let egresses = self.egresses.active.read().await;
        egresses
            .iter()
            .filter(|(_, egress)| egress.pipeline_id == pipeline_id)
            .map(|(output_id, egress)| EgressDiagSnapshot {
                output_id: output_id.clone(),
                pipeline_id: egress.pipeline_id.clone(),
                protocol: egress.protocol.clone(),
                status: egress.status.clone(),
                phase: egress
                    .phase
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone(),
                target_addr: egress
                    .target_addr
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone(),
                bytes_sent: egress.bytes_sent.load(Ordering::Relaxed),
                last_progress_ms: egress.last_progress_ms.load(Ordering::Relaxed),
                last_error: egress
                    .last_error
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone(),
            })
            .collect()
    }

    pub async fn get_or_create_diag_semaphore(
        &self,
        pipeline_id: &str,
    ) -> Arc<tokio::sync::Semaphore> {
        let mut map = self.runtime.diag_semaphores.write().await;
        map.entry(pipeline_id.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Semaphore::new(1)))
            .clone()
    }

    pub async fn get_or_create_stage_metrics(&self, key: StageKey) -> Arc<StageMetrics> {
        let mut metrics = self.stages.metrics.write().await;
        metrics
            .entry(key)
            .or_insert_with(|| Arc::new(StageMetrics::new()))
            .clone()
    }

    pub async fn remove_stage_metrics(&self, key: &StageKey) {
        self.stages.metrics.write().await.remove(key);
    }

    pub async fn register_input_queue(&self, key: StageKey, queue: Arc<MemoryQueue>) {
        self.stages.input_queues.write().await.insert(key, queue);
    }

    pub async fn remove_input_queue(&self, key: &StageKey) {
        self.stages.input_queues.write().await.remove(key);
    }

    pub async fn register_egress_queue(&self, output_id: &str, queue: Arc<MemoryQueue>) {
        self.egresses
            .queues
            .write()
            .await
            .insert(output_id.to_string(), queue);
    }

    pub async fn remove_egress_queue(&self, output_id: &str) {
        self.egresses.queues.write().await.remove(output_id);
    }

    pub async fn register_pipe_metrics(&self, key: StageKey, metrics: Arc<PipeMetrics>) {
        self.stages.pipe_metrics.write().await.insert(key, metrics);
    }

    pub async fn remove_pipe_metrics(&self, key: &StageKey) {
        self.stages.pipe_metrics.write().await.remove(key);
    }

    pub async fn is_file_ingest_running(&self, id: &str) -> bool {
        let mut children = self.file_ingests.children.write().await;
        if let Some(child) = children.get_mut(id) {
            match child.try_wait() {
                Ok(None) => {
                    self.file_ingests
                        .active
                        .write()
                        .await
                        .insert(id.to_string());
                    true
                }
                _ => {
                    children.remove(id);
                    self.file_ingests.active.write().await.remove(id);
                    false
                }
            }
        } else {
            self.file_ingests.active.read().await.contains(id)
        }
    }

    pub async fn reap_file_ingests(&self) {
        let mut children = self.file_ingests.children.write().await;
        let mut stopped = Vec::new();
        children.retain(|id, child| match child.try_wait() {
            Ok(None) => true,
            _ => {
                info!("File ingest child process {} has exited/stopped", id);
                stopped.push(id.clone());
                false
            }
        });
        drop(children);

        if !stopped.is_empty() {
            let mut active = self.file_ingests.active.write().await;
            for id in stopped {
                active.remove(&id);
            }
        }
    }

    pub async fn mark_file_ingest_running(&self, id: &str) {
        self.file_ingests
            .active
            .write()
            .await
            .insert(id.to_string());
    }

    pub async fn clear_file_ingest_running(&self, id: &str) {
        self.file_ingests.active.write().await.remove(id);
    }

    pub async fn get_or_create_pipeline(&self, pipeline_id: &str) -> Arc<RingBuffer> {
        let mut pipelines = self.ingests.pipelines.write().await;
        if let Some(rb) = pipelines.get(pipeline_id) {
            return rb.clone();
        }
        let rb = Arc::new(RingBuffer::new(default_ring_capacity()));
        pipelines.insert(pipeline_id.to_string(), rb.clone());
        rb
    }

    /// Called after stream probe: sizes the source ring for 5 s jitter headroom.
    ///
    /// Formula: `needed = ceil(pkt_rate × HEADROOM_SECS)`, clamped to
    /// `[default_ring_capacity(), MAX_RING_CAPACITY]`.  If the ring is already
    /// large enough no action is taken.  Otherwise the ring is always swapped in,
    /// even if egress readers are already attached — those readers are cancelled so
    /// the reconciler restarts them (within ~1 s) onto the new correctly-sized ring.
    /// Cancelling early readers is safe: the probe fires at ~2–3 s, before any
    /// viewer has meaningfully started watching, and the reconnect is invisible.
    ///
    /// Returns `Some(new_ring)` when resized so the SRT ingest loop can update its
    /// local `ring_buffer` Arc (the old one is stale and receives no further data).
    pub async fn adapt_pipeline_ring(
        &self,
        pipeline_id: &str,
        video_fps: f64,
        audio_track_count: usize,
    ) -> Option<Arc<RingBuffer>> {
        const AUDIO_PKT_RATE: f64 = 50.0; // AAC 48 kHz, 960 samples/frame
        const HEADROOM_SECS: f64 = 6.0; // 20 % margin above the 5 s requirement
        const MAX_RING_CAPACITY: usize = 16_384;

        let pkt_rate = video_fps.max(0.0) + audio_track_count as f64 * AUDIO_PKT_RATE;
        let needed = ((pkt_rate * HEADROOM_SECS).ceil() as usize)
            .max(default_ring_capacity())
            .min(MAX_RING_CAPACITY);

        let mut pipelines = self.ingests.pipelines.write().await;
        let Some(old_rb) = pipelines.get(pipeline_id).cloned() else {
            return None;
        };

        // Always record the packet rate for buffer-depth telemetry.
        old_rb.set_estimated_pkt_rate(pkt_rate);

        if needed <= old_rb.capacity() {
            return None; // already large enough
        }

        // Create a new ring that continues the write-index sequence of the old
        // one so migrating readers pick up exactly where they left off.
        let old_write_idx = old_rb.get_write_idx();
        let new_rb = Arc::new(RingBuffer::new_continuing(needed, old_write_idx));
        new_rb.set_estimated_pkt_rate(pkt_rate);
        if let Some(hint) = old_rb.codec_hint.get() {
            new_rb.set_codec_hint(hint);
        }
        let new_rb_clone = new_rb.clone();

        // Install the new ring in the engine map so that the producer (SRT
        // ingest) switches to it after we return Some(new_rb_clone).
        pipelines.insert(pipeline_id.to_string(), new_rb.clone());
        drop(pipelines);

        // Seal the old ring and forward its readers to the new one.
        // Readers blocked in wait_for_data() are woken here; they drain any
        // remaining unread slots in the old ring, then migrate autonomously.
        // External egress connections (RTMP/SRT to mediamtx) are never
        // cancelled — they see only a sub-millisecond pause in data flow.
        old_rb.seal_and_forward(new_rb);

        info!(
            pipeline_id,
            pkt_rate = format!("{pkt_rate:.0}"),
            video_fps = format!("{video_fps:.0}"),
            audio_track_count,
            new_capacity = needed,
            headroom_secs = format!("{:.1}", needed as f64 / pkt_rate),
            "adaptive ring resize: readers migrate in-place, no egress reconnect"
        );

        Some(new_rb_clone)
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
        let mut buffers = self.stages.buffers.write().await;
        if let Some((rb, token)) = buffers.get(&key)
            && !token.is_cancelled()
        {
            return rb.clone();
        }
        // Cancelled stage — fall through and replace it

        let output_buf = Arc::new(RingBuffer::new(default_transcoder_ring_capacity()));
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
            let ingests = self.ingests.active.read().await;
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
        info!(pipeline_id = %pipeline_id, encoding = %encoding_str, "spawning transcoder stage");
        self.runtime
            .event_log
            .emit(crate::events::EventKind::StageStarted {
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
                info!(pipeline_id = %pipeline_id, encoding = %encoding_str, "spawning audio-router stage");
                tokio::spawn(async move {
                    crate::media::transcoder::start_audio_router(
                        pid,
                        routing,
                        source_buffer,
                        ob,
                        self_clone,
                        cancel,
                        key,
                    )
                    .await;
                });
                return output_buf;
            }
            // Channel-level DSP routes fall through to the selected FFmpeg backend.
        }

        let backend = backend_policy.select_backend(&stage_kind);

        info!(pipeline_id = %pipeline_id, encoding = %encoding_str, "spawning transcoder stage");

        if backend == StageBackend::InternalFfmpeg {
            if stage_kind.is_video_preset() {
                info!(encoding = %encoding_str, "using in-process decode->scale->encode loop");
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
        let key = StageKey::new(pipeline_id, StageKind::codec_edge("hevc_to_h264", upstream));

        // Single write-lock to avoid the TOCTOU race (see get_or_create_transcoder).
        let mut buffers = self.stages.buffers.write().await;
        if let Some((rb, token)) = buffers.get(&key)
            && !token.is_cancelled()
        {
            return rb.clone();
        }

        let output_buf = Arc::new(RingBuffer::new(default_transcoder_ring_capacity()));
        // hevc_to_h264 stage always produces H.264 — tag the ring so consumers
        // can initialize their TsMuxer / PMT with the correct codec.
        output_buf.set_codec_hint("h264");

        // Inherit audio tracks from source_buffer
        let input_tracks = if let Some(tracks) = source_buffer.audio_tracks() {
            std::sync::Arc::new(tracks.to_vec())
        } else {
            let ingests = self.ingests.active.read().await;
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

        info!(pipeline_id = %pipeline_id, "spawning shared H.265→H.264 transcoder");
        self.runtime
            .event_log
            .emit(crate::events::EventKind::StageStarted {
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
    pub async fn active_transcoder_stages(&self, pipeline_id: &str) -> Vec<(StageKind, bool)> {
        let buffers = self.stages.buffers.read().await;
        buffers
            .iter()
            .filter(|(key, _)| key.pipeline.as_str() == pipeline_id)
            .map(|(k, (_, token))| (k.kind.clone(), !token.is_cancelled()))
            .collect()
    }

    pub async fn remove_pipeline(&self, pipeline_id: &str) {
        let mut pipelines = self.ingests.pipelines.write().await;
        pipelines.remove(pipeline_id);
    }

    /// Remove all transcoder stage entries for a pipeline from `transcoder_buffers`.
    ///
    /// Stages whose cancel tokens have already fired are cleaned up lazily by
    /// `get_or_create_transcoder`. This function does the eager sweep on pipeline
    /// deletion so the `Arc<RingBuffer>` for every stage is freed immediately
    /// instead of surviving until the next reconciler creates a replacement stage.
    pub async fn cleanup_pipeline_stages(&self, pipeline_id: &str) {
        let mut buffers = self.stages.buffers.write().await;
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
        let mut buffers = self.stages.buffers.write().await;
        buffers.retain(|key, (_rb, token)| {
            if !active_keys.contains(key) {
                debug!("Sweeping unused transcoder stage: {}", key);
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

        let mut stages = self.stages.ts_muxers.write().await;
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
        let mut stages = self.stages.ts_muxers.write().await;
        stages.retain(|key, stage| {
            let has_readers = if let Ok(mut r) = stage.ring.readers.lock() {
                r.retain(|w| w.upgrade().is_some());
                !r.is_empty()
            } else {
                false
            };

            let in_use = has_readers;

            if !in_use {
                debug!("Sweeping unused TS muxer stage: {}", key);
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
        let mut tokens = self.ingests.cancel_tokens.write().await;
        if let Some(existing) = tokens.get(pipeline_id)
            && !existing.is_cancelled()
        {
            return None;
        }

        let token = CancellationToken::new();
        tokens.insert(pipeline_id.to_string(), token.clone());
        self.ingests.recent.write().await.remove(pipeline_id);

        let mut ingests = self.ingests.active.write().await;
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

        self.runtime
            .event_log
            .emit(crate::events::EventKind::IngestConnected {
                pipeline_id: pipeline_id.to_string(),
                protocol: protocol.to_string(),
                stream_key: stream_key.to_string(),
            });
        Some(token)
    }

    pub async fn record_ingest_disconnect(
        &self,
        pipeline_id: &str,
        phase: Option<&str>,
        reason: Option<String>,
        had_error: bool,
    ) {
        let ingests = self.ingests.active.read().await;
        let Some(ingest) = ingests.get(pipeline_id) else {
            return;
        };

        let snapshot = RecentIngestOutcome {
            protocol: ingest.protocol.clone(),
            disconnected_at_ms: Self::now_epoch_ms(),
            reason,
            failure_phase: phase.map(ToOwned::to_owned),
            had_error,
            remote_addr: ingest.remote_addr.clone(),
            bytes_received: ingest.bytes_received.load(Ordering::Relaxed),
        };
        drop(ingests);

        self.ingests
            .recent
            .write()
            .await
            .insert(pipeline_id.to_string(), snapshot);
    }

    pub async fn unregister_ingest(&self, pipeline_id: &str) {
        let mut tokens = self.ingests.cancel_tokens.write().await;
        if let Some(token) = tokens.remove(pipeline_id) {
            token.cancel();
        }

        let mut ingests = self.ingests.active.write().await;
        let removed = ingests.remove(pipeline_id);
        drop(ingests);

        let protocol = removed
            .as_ref()
            .map(|ingest| ingest.protocol.clone())
            .unwrap_or_default();
        if let Some(ingest) = removed {
            let mut recent = self.ingests.recent.write().await;
            recent
                .entry(pipeline_id.to_string())
                .or_insert_with(|| RecentIngestOutcome {
                    protocol: ingest.protocol,
                    disconnected_at_ms: Self::now_epoch_ms(),
                    reason: None,
                    failure_phase: None,
                    had_error: false,
                    remote_addr: ingest.remote_addr,
                    bytes_received: ingest.bytes_received.load(Ordering::Relaxed),
                });
        }

        if !protocol.is_empty() {
            self.runtime
                .event_log
                .emit(crate::events::EventKind::IngestDisconnected {
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
        self.egresses.recent.write().await.remove(output_id);

        let mut tokens = self.egresses.cancel_tokens.write().await;
        let token = CancellationToken::new();
        tokens.insert(output_id.to_string(), token.clone());

        let mut egresses = self.egresses.active.write().await;
        let now = Instant::now();
        egresses.insert(
            output_id.to_string(),
            ActiveEgress {
                output_id: output_id.to_string(),
                pipeline_id: pipeline_id.to_string(),
                protocol: Self::egress_protocol_from_url(url).to_string(),
                target_url: url.to_string(),
                target_addr: Arc::new(std::sync::Mutex::new(None)),
                status: "running".to_string(),
                phase: Arc::new(std::sync::Mutex::new("starting".to_string())),
                started_at: chrono::Utc::now().to_rfc3339(),
                start_instant: now,
                bytes_sent: Arc::new(AtomicU64::new(0)),
                metrics: Arc::new(StageMetrics::new()),
                last_progress_ms: Arc::new(AtomicU64::new(0)),
                last_error: Arc::new(std::sync::Mutex::new(None)),
                last_error_ms: Arc::new(AtomicU64::new(0)),
                failure_phase: Arc::new(std::sync::Mutex::new(None)),
                quality: Arc::new(std::sync::Mutex::new(PublisherQuality::default())),
                prev_bytes_sent: AtomicU64::new(0),
                prev_sample_time: std::sync::Mutex::new(now),
                bitrate_kbps: std::sync::Mutex::new(None),
            },
        );

        self.runtime
            .event_log
            .emit(crate::events::EventKind::EgressStarted {
                pipeline_id: pipeline_id.to_string(),
                output_id: output_id.to_string(),
            });
        token
    }

    pub async fn unregister_egress(&self, output_id: &str) {
        let mut tokens = self.egresses.cancel_tokens.write().await;
        if let Some(token) = tokens.remove(output_id) {
            token.cancel();
        }

        let mut egresses = self.egresses.active.write().await;
        let pipeline_id = egresses
            .get(output_id)
            .map(|e| e.pipeline_id.clone())
            .unwrap_or_default();
        let recent_outcome = egresses.get(output_id).map(|egress| {
            let has_ingest = self
                .ingests
                .active
                .try_read()
                .map(|ingests| ingests.contains_key(egress.pipeline_id.as_str()))
                .unwrap_or(false);
            RecentEgressOutcome {
                output_id: egress.output_id.clone(),
                pipeline_id: egress.pipeline_id.clone(),
                protocol: egress.protocol.clone(),
                target_url: egress.target_url.clone(),
                target_addr: egress
                    .target_addr
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone(),
                status: Self::recent_egress_status(egress, has_ingest),
                raw_status: egress.status.clone(),
                phase: egress
                    .phase
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone(),
                started_at: egress.started_at.clone(),
                uptime_secs: egress.start_instant.elapsed().as_secs_f64(),
                bytes_sent: egress.bytes_sent.load(Ordering::Relaxed),
                last_progress_ms: egress.last_progress_ms.load(Ordering::Relaxed),
                last_error: egress
                    .last_error
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone(),
                last_error_ms: egress.last_error_ms.load(Ordering::Relaxed),
                failure_phase: egress
                    .failure_phase
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone(),
                quality: egress
                    .quality
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone(),
                metrics: egress.metrics.snapshot(),
                ended_at_ms: Self::now_epoch_ms(),
            }
        });
        egresses.remove(output_id);
        drop(egresses);

        if let Some(outcome) = recent_outcome {
            self.egresses
                .recent
                .write()
                .await
                .insert(output_id.to_string(), outcome);
        }

        if !pipeline_id.is_empty() {
            self.runtime
                .event_log
                .emit(crate::events::EventKind::EgressStopped {
                    pipeline_id,
                    output_id: output_id.to_string(),
                });
        }
    }

    pub async fn update_egress_phase(&self, output_id: &str, phase: &str) {
        let egresses = self.egresses.active.read().await;
        if let Some(egress) = egresses.get(output_id) {
            *egress.phase.lock().unwrap_or_else(|e| e.into_inner()) = phase.to_string();
        }
    }

    pub async fn update_egress_target_addr(&self, output_id: &str, addr: String) {
        let egresses = self.egresses.active.read().await;
        if let Some(egress) = egresses.get(output_id) {
            *egress.target_addr.lock().unwrap_or_else(|e| e.into_inner()) = Some(addr);
        }
    }

    pub async fn update_egress_quality(&self, output_id: &str, quality: PublisherQuality) {
        let egresses = self.egresses.active.read().await;
        if let Some(egress) = egresses.get(output_id) {
            *egress.quality.lock().unwrap_or_else(|e| e.into_inner()) = quality;
        }
    }

    pub async fn record_egress_progress(&self, output_id: &str, bytes: u64) {
        let egresses = self.egresses.active.read().await;
        if let Some(egress) = egresses.get(output_id) {
            egress.bytes_sent.fetch_add(bytes, Ordering::Relaxed);
            egress.metrics.record_out(bytes);
            egress
                .last_progress_ms
                .store(Self::now_epoch_ms(), Ordering::Relaxed);
            let active_phase = if egress.protocol == "hls" {
                "uploading"
            } else {
                "sending"
            };
            *egress.phase.lock().unwrap_or_else(|e| e.into_inner()) = active_phase.to_string();
            *egress
                .failure_phase
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = None;
            *egress.last_error.lock().unwrap_or_else(|e| e.into_inner()) = None;
            egress.last_error_ms.store(0, Ordering::Relaxed);
        }
    }

    pub async fn egress_has_recorded_progress(&self, output_id: &str) -> bool {
        let egresses = self.egresses.active.read().await;
        egresses
            .get(output_id)
            .is_some_and(|egress| egress.last_progress_ms.load(Ordering::Relaxed) > 0)
    }

    pub async fn recent_egress_outcome(&self, output_id: &str) -> Option<RecentEgressOutcome> {
        self.egresses.recent.read().await.get(output_id).cloned()
    }

    pub async fn record_egress_error(
        &self,
        output_id: &str,
        phase: &str,
        message: impl Into<String>,
    ) {
        let egresses = self.egresses.active.read().await;
        if let Some(egress) = egresses.get(output_id) {
            let message = message.into();
            let pipeline_id = egress.pipeline_id.clone();
            *egress.phase.lock().unwrap_or_else(|e| e.into_inner()) = "failed".to_string();
            *egress
                .failure_phase
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(phase.to_string());
            *egress.last_error.lock().unwrap_or_else(|e| e.into_inner()) = Some(message.clone());
            egress
                .last_error_ms
                .store(Self::now_epoch_ms(), Ordering::Relaxed);
            self.runtime
                .event_log
                .emit(crate::events::EventKind::EgressFailed {
                    pipeline_id,
                    output_id: output_id.to_string(),
                    phase: phase.to_string(),
                    error: message,
                });
        }
    }

    /// Update bytes received counter for an active ingest (lock-free atomic).
    pub async fn update_ingest_bytes(&self, pipeline_id: &str, bytes: u64) {
        let ingests = self.ingests.active.read().await;
        if let Some(ingest) = ingests.get(pipeline_id) {
            ingest.bytes_received.fetch_add(bytes, Ordering::Relaxed);
        }
    }

    pub async fn record_keyframe(&self, pipeline_id: &str, pts: i64) {
        let ingests = self.ingests.active.read().await;
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
        let egresses = self.egresses.active.read().await;
        if let Some(egress) = egresses.get(output_id) {
            egress.bytes_sent.fetch_add(bytes, Ordering::Relaxed);
            egress
                .last_progress_ms
                .store(Self::now_epoch_ms(), Ordering::Relaxed);
        }
    }

    pub async fn egress_bytes(&self, output_id: &str) -> u64 {
        let egresses = self.egresses.active.read().await;
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
        let mut ingests = self.ingests.active.write().await;
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
        let ingests = self.ingests.active.read().await;
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
        let ingests = self.ingests.active.read().await;
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
        let ingests = self.ingests.active.read().await;
        if let Some(ingest) = ingests.get(pipeline_id) {
            *ingest
                .audio_tracks
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = std::sync::Arc::new(tracks);
        }
    }

    /// Build a probe snapshot for a pipeline's active ingest.
    pub async fn probe_snapshot(&self, pipeline_id: &str) -> Option<serde_json::Value> {
        let ingests = self.ingests.active.read().await;
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
        let mut ingests = self.ingests.active.write().await;
        if let Some(ingest) = ingests.get_mut(pipeline_id) {
            ingest.quality = quality;
        }
    }

    pub async fn recent_ingest_outcome(&self, pipeline_id: &str) -> Option<RecentIngestOutcome> {
        self.ingests.recent.read().await.get(pipeline_id).cloned()
    }

    /// Register an active recording for a pipeline. Returns a cancellation token.
    pub async fn register_recording(&self, pipeline_id: &str) -> CancellationToken {
        let mut tokens = self.recordings.cancel_tokens.write().await;
        let token = CancellationToken::new();
        tokens.insert(pipeline_id.to_string(), token.clone());
        token
    }

    /// Unregister (and cancel) an active recording for a pipeline.
    pub async fn unregister_recording(&self, pipeline_id: &str) {
        let mut tokens = self.recordings.cancel_tokens.write().await;
        if let Some(token) = tokens.remove(pipeline_id) {
            token.cancel();
        }
    }

    /// Check if a recording is actively running for a pipeline.
    pub async fn is_recording_active(&self, pipeline_id: &str) -> bool {
        let tokens = self.recordings.cancel_tokens.read().await;
        tokens
            .get(pipeline_id)
            .is_some_and(|token| !token.is_cancelled())
    }

    /// Ensure an HLS segmenter is running for this pipeline. Returns the store
    /// and whether the segmenter was already running (true) or just started (false).
    pub async fn ensure_hls_segmenter(&self, pipeline_id: &str) -> (Arc<HlsStore>, bool) {
        let mut consumers = self.hls.consumers.write().await;
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
        let consumers = self.hls.consumers.read().await;
        if let Some(c) = consumers.get(pipeline_id) {
            c.touch();
        }
    }

    /// Register a persistent HLS consumer (e.g. HLS egress output).
    pub async fn add_hls_persistent_consumer(&self, pipeline_id: &str) {
        let consumers = self.hls.consumers.read().await;
        if let Some(c) = consumers.get(pipeline_id) {
            c.add_persistent();
        }
    }

    /// Unregister a persistent HLS consumer.
    pub async fn remove_hls_persistent_consumer(&self, pipeline_id: &str) {
        let consumers = self.hls.consumers.read().await;
        if let Some(c) = consumers.get(pipeline_id) {
            c.remove_persistent();
        }
    }

    /// Shut down an idle HLS segmenter and clean up its store.
    pub async fn shutdown_hls_segmenter(&self, pipeline_id: &str) {
        let mut consumers = self.hls.consumers.write().await;
        if let Some(c) = consumers.remove(pipeline_id) {
            c.cancel_token.cancel();
        }
        drop(consumers);
        self.hls.stores.write().await.remove(pipeline_id);
    }

    /// Get the cancel token for a running HLS segmenter (used to spawn the task).
    pub async fn get_hls_cancel_token(&self, pipeline_id: &str) -> Option<CancellationToken> {
        let consumers = self.hls.consumers.read().await;
        consumers.get(pipeline_id).map(|c| c.cancel_token.clone())
    }

    pub async fn get_or_create_hls_store(&self, pipeline_id: &str) -> Arc<HlsStore> {
        let mut stores = self.hls.stores.write().await;
        stores
            .entry(pipeline_id.to_string())
            .or_insert_with(|| Arc::new(HlsStore::new()))
            .clone()
    }

    pub async fn remove_hls_store(&self, pipeline_id: &str) {
        let mut stores = self.hls.stores.write().await;
        stores.remove(pipeline_id);
    }

    pub async fn get_hls_store(&self, pipeline_id: &str) -> Option<Arc<HlsStore>> {
        let stores = self.hls.stores.read().await;
        stores.get(pipeline_id).cloned()
    }

    pub async fn pipeline_ring_diag_snapshot(
        &self,
        pipeline_id: &str,
    ) -> Option<RingBufferDiagSnapshot> {
        let pipelines = self.ingests.pipelines.read().await;
        let ring = pipelines.get(pipeline_id)?;
        let (fill_slots, capacity_slots) = ring.fill_and_capacity();
        Some(RingBufferDiagSnapshot {
            fill_slots,
            capacity_slots,
            readers: ring.reader_snapshots(),
        })
    }
    pub async fn hls_pipeline_ids(&self) -> Vec<String> {
        self.hls.consumers.read().await.keys().cloned().collect()
    }

    pub async fn should_shutdown_hls_segmenter(&self, pipeline_id: &str, timeout_ms: u64) -> bool {
        let has_ingest = self.has_active_ingest(pipeline_id).await;
        let consumers = self.hls.consumers.read().await;
        match consumers.get(pipeline_id) {
            Some(consumer) => !has_ingest || consumer.is_idle(timeout_ms),
            None => false,
        }
    }

    pub async fn shutdown_all_hls_segmenters(&self) {
        let pipeline_ids = self.hls_pipeline_ids().await;
        for pipeline_id in pipeline_ids {
            self.shutdown_hls_segmenter(&pipeline_id).await;
        }
    }

    pub async fn cancel_all_active_tasks(&self) {
        {
            let egress = self.egresses.cancel_tokens.read().await;
            for token in egress.values() {
                token.cancel();
            }
        }
        {
            let ingests = self.ingests.cancel_tokens.read().await;
            for token in ingests.values() {
                token.cancel();
            }
        }
        {
            let recordings = self.recordings.cancel_tokens.read().await;
            for token in recordings.values() {
                token.cancel();
            }
        }
    }

    pub async fn srt_listener_diag_snapshot(&self) -> SrtListenerDiagSnapshot {
        SrtListenerDiagSnapshot {
            bonding_available: self.bonding_available(),
            rx_queue_bytes: self
                .runtime
                .listener_stats
                .rx_queue_bytes
                .load(Ordering::Relaxed),
            rx_queue_peak_bytes: self
                .runtime
                .listener_stats
                .rx_queue_max_bytes
                .load(Ordering::Relaxed),
            drops: self.runtime.listener_stats.drops.load(Ordering::Relaxed),
            active_ingest_count: self.active_ingest_count().await,
        }
    }

    /// Build the full health snapshot JSON that the `/api/v1/engine/health`
    /// endpoint returns.
    pub async fn health_snapshot(
        &self,
        pipeline_ids: &[String],
        recording_enabled: &HashMap<String, bool>,
    ) -> serde_json::Value {
        crate::media::engine_views::health_snapshot(self, pipeline_ids, recording_enabled).await
    }

    /// Engine-wide telemetry: raw counters for all active ingests, stages, and
    /// egresses. Intended for engineer dashboards and debugging.
    pub async fn engine_telemetry(&self) -> serde_json::Value {
        crate::media::engine_views::engine_telemetry(self).await
    }

    pub(crate) fn ring_payload_stats_json(ring: &RingBuffer) -> serde_json::Value {
        let stats = ring.payload_stats();
        serde_json::json!({
            "slots": stats.slots,
            "payloadBytes": stats.payload_bytes,
            "videoBytes": stats.video_bytes,
            "audioBytes": stats.audio_bytes,
            "minPayloadBytes": stats.min_payload_bytes,
            "maxPayloadBytes": stats.max_payload_bytes,
            "avgPayloadBytes": if stats.slots > 0 {
                stats.payload_bytes as f64 / stats.slots as f64
            } else {
                0.0
            },
        })
    }

    /// Per-pipeline telemetry: ingest metrics, all stage metrics for this
    /// pipeline, and egress metrics. Returns None if the pipeline has no
    /// active components.
    pub async fn pipeline_telemetry(&self, pipeline_id: &str) -> serde_json::Value {
        crate::media::engine_views::pipeline_telemetry(self, pipeline_id).await
    }

    /// Single-stage telemetry by StageKey. Returns raw counters and pipe
    /// metrics (if present). Used by the engineer stage telemetry endpoint.
    pub async fn stage_telemetry(&self, key: &StageKey) -> Option<serde_json::Value> {
        crate::media::engine_views::stage_telemetry(self, key).await
    }

    pub async fn stage_telemetry_by_display(&self, display: &str) -> Option<serde_json::Value> {
        crate::media::engine_views::stage_telemetry_by_display(self, display).await
    }

    /// Build a processing graph for a pipeline showing all stages and connections.
    /// Returns a JSON structure suitable for visualization.
    pub async fn processing_graph(
        &self,
        pipeline_id: &str,
        outputs: &[crate::types::Output],
    ) -> serde_json::Value {
        crate::media::engine_views::processing_graph(self, pipeline_id, outputs).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::ring_buffer::{MediaPacket, MediaType, PayloadFormat, Reader};
    use bytes::Bytes;
    use std::sync::Arc;
    use tokio::process::Command;

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
    async fn runtime_helpers_expose_registered_ingest_and_egress() {
        let engine = MediaEngine::new();
        engine
            .try_register_ingest("pipe-runtime", "stream-key", "rtmp")
            .await
            .expect("register ingest");
        engine.update_ingest_bytes("pipe-runtime", 2048).await;

        let ingest = engine
            .with_active_ingest("pipe-runtime", |ingest| {
                (
                    ingest.protocol.clone(),
                    ingest.bytes_received.load(Ordering::Relaxed),
                )
            })
            .await;
        assert_eq!(ingest, Some(("rtmp".to_string(), 2048)));
        assert_eq!(engine.with_active_ingest("missing", |_| true).await, None);

        engine
            .register_egress("out-runtime", "pipe-runtime", "rtmp://example.com/live/key")
            .await;
        engine.record_egress_progress("out-runtime", 1316).await;

        let egress = engine
            .with_active_egress("out-runtime", |egress| {
                (
                    egress.protocol.clone(),
                    egress.bytes_sent.load(Ordering::Relaxed),
                    egress
                        .phase
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone(),
                )
            })
            .await;
        assert_eq!(
            egress,
            Some(("rtmp".to_string(), 1316, "sending".to_string()))
        );
        assert_eq!(engine.with_active_egress("missing", |_| true).await, None);
    }

    #[tokio::test]
    async fn hls_dependency_snapshot_reflects_store_and_consumer_state() {
        let engine = MediaEngine::new();
        let (store, already_running) = engine.ensure_hls_segmenter("pipe-hls-snapshot").await;
        assert!(!already_running);

        engine
            .add_hls_persistent_consumer("pipe-hls-snapshot")
            .await;
        engine.touch_hls("pipe-hls-snapshot").await;
        store.push_segment(2.0, Bytes::from_static(b"segment"));

        let snapshot = engine.hls_dependency_snapshot("pipe-hls-snapshot").await;
        assert!(snapshot.store_exists);
        assert!(snapshot.active);
        assert_eq!(snapshot.persistent_consumers, 1);
        assert!(snapshot.last_access_age_ms.is_some());
        assert_eq!(snapshot.segments, 1);
        assert!(snapshot.playlist_bytes > 0);
    }

    #[tokio::test]
    async fn file_ingest_dependency_snapshot_reflects_active_and_child_state() {
        let engine = MediaEngine::new();
        engine
            .mark_file_ingest_running("file-ingest-snapshot")
            .await;

        let child = Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep child");
        engine
            .file_ingests
            .children
            .write()
            .await
            .insert("file-ingest-snapshot".to_string(), child);

        let snapshot = engine
            .file_ingest_dependency_snapshot("file-ingest-snapshot")
            .await;
        assert!(snapshot.marked_active);
        assert!(snapshot.child_registered);

        assert!(engine.stop_file_ingest_child("file-ingest-snapshot").await);
        engine
            .clear_file_ingest_running("file-ingest-snapshot")
            .await;

        let snapshot = engine
            .file_ingest_dependency_snapshot("file-ingest-snapshot")
            .await;
        assert!(!snapshot.marked_active);
        assert!(!snapshot.child_registered);
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
        assert_eq!(engine.ingests.active.read().await.len(), 1);
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
    async fn health_snapshot_marks_failed_egress_status_when_input_is_live() {
        let engine = MediaEngine::new();
        engine
            .try_register_ingest("pipeline-1", "stream-key", "rtmp")
            .await
            .unwrap();
        engine
            .register_egress("output-1", "pipeline-1", "rtmp://example/live/test")
            .await;
        engine
            .record_egress_error("output-1", "send", "connection refused")
            .await;

        let snapshot = engine
            .health_snapshot(&["pipeline-1".to_string()], &HashMap::new())
            .await;

        let output = &snapshot["pipelines"]["pipeline-1"]["outputs"]["output-1"];
        assert_eq!(output["status"], "failed");
        assert_eq!(output["rawStatus"], "running");
        assert_eq!(output["phase"], "failed");
        assert_eq!(output["failurePhase"], "send");
    }

    #[tokio::test]
    async fn health_snapshot_marks_live_output_stalled_without_progress() {
        let engine = MediaEngine::new();
        engine
            .try_register_ingest("pipeline-1", "stream-key", "rtmp")
            .await
            .unwrap();
        engine
            .register_egress("output-1", "pipeline-1", "rtmp://example/live/test")
            .await;
        {
            let mut egresses = engine.egresses.active.write().await;
            let egress = egresses.get_mut("output-1").unwrap();
            egress.start_instant = Instant::now()
                .checked_sub(std::time::Duration::from_millis(
                    EGRESS_PROGRESS_STALE_MS + 1,
                ))
                .unwrap();
        }

        let snapshot = engine
            .health_snapshot(&["pipeline-1".to_string()], &HashMap::new())
            .await;

        assert_eq!(
            snapshot["pipelines"]["pipeline-1"]["outputs"]["output-1"]["status"],
            "stalled"
        );
    }

    #[tokio::test]
    async fn health_snapshot_keeps_local_hls_segmenter_running_without_bytes_out() {
        let engine = MediaEngine::new();
        engine
            .try_register_ingest("pipeline-1", "stream-key", "rtmp")
            .await
            .unwrap();
        engine
            .register_egress("output-1", "pipeline-1", "hls://localhost/hls/test")
            .await;
        engine.update_egress_phase("output-1", "segmenting").await;
        {
            let mut egresses = engine.egresses.active.write().await;
            let egress = egresses.get_mut("output-1").unwrap();
            egress.start_instant = Instant::now()
                .checked_sub(std::time::Duration::from_millis(
                    EGRESS_PROGRESS_STALE_MS + 1,
                ))
                .unwrap();
        }

        let snapshot = engine
            .health_snapshot(&["pipeline-1".to_string()], &HashMap::new())
            .await;

        let output = &snapshot["pipelines"]["pipeline-1"]["outputs"]["output-1"];
        assert_eq!(output["status"], "running");
        assert_eq!(output["phase"], "segmenting");
        assert_eq!(output["bytesOut"], 0);
    }

    #[tokio::test]
    async fn health_snapshot_includes_all_ingest_audio_tracks() {
        let engine = MediaEngine::new();
        engine
            .try_register_ingest("pipeline-audio", "stream-key", "srt")
            .await
            .unwrap();
        engine
            .update_ingest_audio_tracks(
                "pipeline-audio",
                vec![
                    AudioMeta {
                        codec: "aac".to_string(),
                        sample_rate: 48_000,
                        channels: 2,
                        channel_layout: None,
                        track_index: 0,
                        pid: Some(0x101),
                        language: Some("eng".to_string()),
                        title: None,
                        profile: None,
                    },
                    AudioMeta {
                        codec: "aac".to_string(),
                        sample_rate: 44_100,
                        channels: 1,
                        channel_layout: None,
                        track_index: 1,
                        pid: Some(0x102),
                        language: None,
                        title: None,
                        profile: None,
                    },
                ],
            )
            .await;

        let snapshot = engine
            .health_snapshot(&["pipeline-audio".to_string()], &HashMap::new())
            .await;
        let tracks = snapshot["pipelines"]["pipeline-audio"]["input"]["audioTracks"]
            .as_array()
            .unwrap();

        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0]["pid"], 0x101);
        assert_eq!(tracks[0]["language"], "eng");
        assert_eq!(tracks[1]["trackIndex"], 1);
    }

    #[tokio::test]
    async fn health_snapshot_reports_probe_readiness() {
        let engine = MediaEngine::new();
        engine
            .try_register_ingest("pipeline-probe", "stream-key", "srt")
            .await
            .unwrap();

        let pending = engine
            .health_snapshot(&["pipeline-probe".to_string()], &HashMap::new())
            .await;
        let pending_input = &pending["pipelines"]["pipeline-probe"]["input"];
        assert_eq!(pending_input["probeReady"], false);
        assert_eq!(pending_input["probeStatus"], "pending");
        assert!(pending_input["probePendingMs"].as_u64().is_some());

        let video = Some(VideoMeta {
            codec: "h264".to_string(),
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
        });
        let audio = AudioMeta {
            track_index: 0,
            codec: "aac".to_string(),
            sample_rate: 48_000,
            channels: 2,
            channel_layout: None,
            pid: None,
            language: None,
            title: None,
            profile: None,
        };
        engine
            .update_ingest_meta("pipeline-probe", video, Some(audio.clone()), None)
            .await;
        engine
            .update_ingest_audio_tracks("pipeline-probe", vec![audio])
            .await;

        let ready = engine
            .health_snapshot(&["pipeline-probe".to_string()], &HashMap::new())
            .await;
        let ready_input = &ready["pipelines"]["pipeline-probe"]["input"];
        assert_eq!(ready_input["probeReady"], true);
        assert_eq!(ready_input["probeStatus"], "ready");
        assert!(ready_input["probePendingMs"].is_null());
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
            .runtime
            .listener_stats
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
    async fn health_snapshot_exposes_egress_progress_and_error_state() {
        let engine = MediaEngine::new();
        engine
            .try_register_ingest("pipe-1", "stream-key", "srt")
            .await
            .unwrap();
        engine
            .register_egress(
                "out-1",
                "pipe-1",
                "srt://example.com:10080?streamid=live/key",
            )
            .await;
        engine
            .update_egress_target_addr("out-1", "203.0.113.10:10080".to_string())
            .await;
        engine.update_egress_phase("out-1", "sending").await;
        engine
            .update_egress_quality(
                "out-1",
                PublisherQuality {
                    tcp_congestion_algorithm: Some("cubic".to_string()),
                    mbps_send_rate: Some(3.2),
                    packets_sent_retrans: Some(2),
                    srt_bonded: Some(true),
                    srt_group_member_count: Some(2),
                    srt_group_active_members: Some(1),
                    ..PublisherQuality::default()
                },
            )
            .await;
        engine.record_egress_progress("out-1", 1316).await;
        engine
            .record_egress_error("out-1", "send", "synthetic send failure")
            .await;

        let snapshot = engine
            .health_snapshot(&["pipe-1".to_string()], &HashMap::new())
            .await;
        let output = &snapshot["pipelines"]["pipe-1"]["outputs"]["out-1"];

        assert_eq!(output["protocol"], "srt");
        assert_eq!(output["status"], "failed");
        assert_eq!(output["targetAddr"], "203.0.113.10:10080");
        assert_eq!(output["phase"], "failed");
        assert_eq!(output["failurePhase"], "send");
        assert_eq!(output["lastError"], "synthetic send failure");
        assert_eq!(output["totalSize"], 1316);
        assert_eq!(output["quality"]["mbpsSendRate"], 3.2);
        assert_eq!(output["quality"]["tcpCongestionAlgorithm"], "cubic");
        assert_eq!(output["quality"]["packetsSentRetrans"], 2);
        assert_eq!(output["quality"]["srtBonded"], true);
        assert_eq!(output["quality"]["srtGroupMemberCount"], 2);
        assert_eq!(output["quality"]["srtGroupActiveMembers"], 1);
        assert!(!output["lastProgressAt"].is_null());
        assert!(!output["lastErrorAt"].is_null());
    }

    #[tokio::test]
    async fn egress_failure_event_survives_unregister() {
        let engine = MediaEngine::new();
        engine
            .register_egress("out-1", "pipe-1", "rtmp://example.com/live/key")
            .await;
        engine
            .record_egress_error("out-1", "connect", "connection refused")
            .await;
        engine.unregister_egress("out-1").await;

        let events = engine.runtime.event_log.recent(10, Some("pipe-1"));
        assert!(events.iter().any(|event| matches!(
            &event.kind,
            crate::events::EventKind::EgressFailed {
                output_id,
                phase,
                error,
                ..
            } if output_id == "out-1" && phase == "connect" && error == "connection refused"
        )));
    }

    #[tokio::test]
    async fn egress_progress_after_error_clears_failed_phase() {
        let engine = MediaEngine::new();
        engine
            .register_egress(
                "out-1",
                "pipe-1",
                "https://upload.example.com/live/out.m3u8?token=abc",
            )
            .await;
        engine
            .record_egress_error("out-1", "upload_segment", "temporary sink outage")
            .await;
        engine.record_egress_progress("out-1", 4096).await;

        let snapshot = engine
            .health_snapshot(&["pipe-1".to_string()], &HashMap::new())
            .await;
        let output = &snapshot["pipelines"]["pipe-1"]["outputs"]["out-1"];

        assert_eq!(output["phase"], "uploading");
        assert!(output["failurePhase"].is_null());
        assert!(output["lastError"].is_null());
        assert!(output["lastErrorAt"].is_null());
        assert_eq!(output["totalSize"], 4096);
    }

    #[tokio::test]
    async fn egress_has_recorded_progress_only_after_progress_update() {
        let engine = MediaEngine::new();
        engine
            .register_egress("out-1", "pipe-1", "rtmp://example.com/live/key")
            .await;

        assert!(!engine.egress_has_recorded_progress("out-1").await);

        engine.record_egress_progress("out-1", 188).await;

        assert!(engine.egress_has_recorded_progress("out-1").await);
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
    async fn processing_graph_routes_srt_egress_through_ts_mux() {
        let engine = MediaEngine::new();
        let pipeline_id = "pipeline-srt-graph";
        let _source = engine.get_or_create_pipeline(pipeline_id).await;
        let output = crate::types::Output {
            id: "out-srt".to_string(),
            pipeline_id: pipeline_id.to_string(),
            name: "SRT Target".to_string(),
            url: "srt://example.com:9000?streamid=publish:live/test".to_string(),
            monitoring_url: None,
            desired_state: "running".to_string(),
            encoding: "source".to_string(),
        };

        let graph = engine.processing_graph(pipeline_id, &[output]).await;
        let nodes = graph["nodes"].as_array().unwrap();
        let edges = graph["edges"].as_array().unwrap();

        assert!(
            nodes
                .iter()
                .any(|node| node["type"] == "demux" && node["label"] == "Demux/probe idle"),
            "graph should expose the ingest demux/probe boundary"
        );
        assert!(
            nodes
                .iter()
                .any(|node| node["type"] == "packetizer" && node["label"] == "MPEG-TS mux: source"),
            "SRT egress should expose MPEG-TS packetization"
        );
        assert!(
            edges.iter().any(|edge| edge["label"] == "SRT send"),
            "SRT egress should include an explicit sender edge"
        );
        assert!(
            !edges.iter().any(|edge| edge["label"] == "FLV passthrough"),
            "SRT egress must not be labeled as FLV passthrough"
        );
    }

    #[tokio::test]
    async fn processing_graph_marks_failed_egress_inactive() {
        let engine = MediaEngine::new();
        let pipeline_id = "pipeline-failed-output-graph";
        engine
            .try_register_ingest(pipeline_id, "stream-key", "rtmp")
            .await
            .unwrap();
        engine
            .register_egress("out-failed", pipeline_id, "rtmp://example/live/test")
            .await;
        engine
            .record_egress_error("out-failed", "send", "connection refused")
            .await;

        let output = crate::types::Output {
            id: "out-failed".to_string(),
            pipeline_id: pipeline_id.to_string(),
            name: "Failed Target".to_string(),
            url: "rtmp://example/live/test".to_string(),
            monitoring_url: None,
            desired_state: "running".to_string(),
            encoding: "source".to_string(),
        };

        let graph = engine.processing_graph(pipeline_id, &[output]).await;
        let nodes = graph["nodes"].as_array().unwrap();
        let egress = nodes
            .iter()
            .find(|node| node["type"] == "egress")
            .expect("egress node");

        assert_eq!(egress["active"], false);
        assert_eq!(egress["details"]["status"], "failed");
        assert_eq!(egress["details"]["phase"], "failed");
        assert_eq!(egress["details"]["failurePhase"], "send");
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
            .get_or_create_transcoder(
                "pipe-share",
                StageKind::video_preset("720p"),
                source.clone(),
                None,
            )
            .await;
        let b = engine
            .get_or_create_transcoder(
                "pipe-share",
                StageKind::video_preset("720p"),
                source.clone(),
                None,
            )
            .await;
        let c = engine
            .get_or_create_transcoder(
                "pipe-share",
                StageKind::video_preset("1080p"),
                source.clone(),
                None,
            )
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
            .get_or_create_transcoder(
                "pipe-audio",
                StageKind::video_preset("720p"),
                source.clone(),
                None,
            )
            .await;
        let v1080 = engine
            .get_or_create_transcoder(
                "pipe-audio",
                StageKind::video_preset("1080p"),
                source.clone(),
                None,
            )
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
            .get_or_create_transcoder(
                "pipe-del",
                StageKind::video_preset("720p"),
                source.clone(),
                None,
            )
            .await;
        let s2 = engine
            .get_or_create_transcoder(
                "pipe-del",
                StageKind::video_preset("1080p"),
                source.clone(),
                None,
            )
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

        let buffers = engine.stages.buffers.read().await;
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
            .get_or_create_transcoder(
                "pipe-sweep",
                StageKind::video_preset("720p"),
                source.clone(),
                None,
            )
            .await;
        let s2 = engine
            .get_or_create_transcoder(
                "pipe-sweep",
                StageKind::video_preset("1080p"),
                source.clone(),
                None,
            )
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
            stages_before.iter().any(|(stage, live)| *stage
                == StageKind::codec_edge("hevc_to_h264", StageKind::source())
                && *live),
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
                e.get_or_create_transcoder(
                    "pipe-concurrent",
                    StageKind::video_preset("720p"),
                    s,
                    None,
                )
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
            let mut consumers = engine.hls.consumers.write().await;
            consumers.insert("pipe-hls-rc".to_string(), HlsConsumers::new(token.clone()));
        }

        // One persistent consumer added — segmenter must not be idle.
        engine.add_hls_persistent_consumer("pipe-hls-rc").await;
        {
            let consumers = engine.hls.consumers.read().await;
            assert!(
                !consumers["pipe-hls-rc"].is_idle(0),
                "segmenter must not be idle while a persistent consumer holds a ref"
            );
        }

        // Remove the consumer — now idle (last_access_ms was set on creation;
        // use a long timeout so only persistent count matters here).
        engine.remove_hls_persistent_consumer("pipe-hls-rc").await;
        {
            let consumers = engine.hls.consumers.read().await;
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
            .get_or_create_transcoder(
                "p-hevc",
                StageKind::video_preset("720p"),
                source,
                Some("hevc"),
            )
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
            .get_or_create_h264_transcoder(
                "p-dual",
                StageKind::video_preset("720p"),
                source.clone(),
            )
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
            .get_or_create_h264_transcoder(
                "p-shared-h264",
                StageKind::video_preset("720p"),
                source.clone(),
            )
            .await;
        let ring2 = engine
            .get_or_create_h264_transcoder(
                "p-shared-h264",
                StageKind::video_preset("720p"),
                source.clone(),
            )
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
                pid: None,
                language: None,
                title: None,
                profile: None,
                channel_layout: None,
            },
            AudioMeta {
                codec: "opus".into(),
                sample_rate: 48000,
                channels: 6,
                track_index: 1,
                pid: None,
                language: None,
                title: None,
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
            pid: None,
            language: None,
            title: None,
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
            pid: None,
            language: None,
            title: None,
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
            let mut map = engine.runtime.diag_semaphores.write().await;
            map.entry(pipeline.to_string())
                .or_insert_with(|| Arc::new(tokio::sync::Semaphore::new(1)))
                .clone()
        };

        let permit1 = sem.clone().try_acquire_owned();
        assert!(permit1.is_ok(), "first acquire must succeed");

        let permit2 = sem.clone().try_acquire_owned();
        assert!(permit2.is_err(), "second concurrent acquire must fail");

        let sem_other = {
            let mut map = engine.runtime.diag_semaphores.write().await;
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
            .stages
            .ts_muxers
            .write()
            .await
            .insert(key.clone(), stage);

        engine.sweep_unused_stages().await;
        assert!(
            engine.stages.ts_muxers.read().await.contains_key(&key),
            "stage with active reader must be retained"
        );

        drop(_reader);
        engine.sweep_unused_stages().await;
        assert!(
            !engine.stages.ts_muxers.read().await.contains_key(&key),
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

    #[tokio::test]
    async fn hls_segmenter_without_ingest_is_immediately_shutdown_candidate() {
        let engine = Arc::new(MediaEngine::new());
        let pipeline_id = "pipe-hls-no-ingest";

        let _ = engine.ensure_hls_segmenter(pipeline_id).await;
        engine.touch_hls(pipeline_id).await;

        assert!(
            engine
                .should_shutdown_hls_segmenter(pipeline_id, 60_000)
                .await,
            "HLS preview should stop promptly when ingest disappears, regardless of idle timeout"
        );
    }

    // ── Matrix routing with synthetic packets (Phase 0 re-tier) ─────

    #[tokio::test]
    async fn matrix_routing_ingest_to_source_reader() {
        let engine = MediaEngine::new();
        let ring = engine.get_or_create_pipeline("matrix-pipe").await;
        engine
            .try_register_ingest("matrix-pipe", "key", "rtmp")
            .await
            .unwrap();

        ring.push(test_video_packet(0, 0, true));
        ring.push(test_audio_packet(10, 10));
        ring.push(test_video_packet(33, 33, false));

        let mut reader = Reader::new("matrix-reader".to_string(), ring);
        let p1 = reader.pull().unwrap().unwrap();
        assert_eq!(p1.media_type, MediaType::Video);
        assert!(p1.is_keyframe);
        let p2 = reader.pull().unwrap().unwrap();
        assert_eq!(p2.media_type, MediaType::Audio);
        let p3 = reader.pull().unwrap().unwrap();
        assert_eq!(p3.pts, 33);
        assert!(reader.pull().unwrap().is_none());
    }

    #[tokio::test]
    async fn matrix_routing_flv_and_raw_format_dispatch() {
        let engine = MediaEngine::new();
        let ring = engine.get_or_create_pipeline("fmt-pipe").await;

        ring.push(MediaPacket {
            media_type: MediaType::Video,
            format: PayloadFormat::Flv,
            is_keyframe: true,
            track_index: 0,
            pts: 0,
            dts: 0,
            payload: Bytes::from_static(&[0x17, 0x01, 0, 0, 0]),
        });
        ring.push(MediaPacket {
            media_type: MediaType::Video,
            format: PayloadFormat::Raw,
            is_keyframe: false,
            track_index: 0,
            pts: 33,
            dts: 33,
            payload: Bytes::from_static(&[0, 0, 0, 1, 0x41]),
        });

        let mut reader = Reader::new("fmt-reader".to_string(), ring);
        let p1 = reader.pull().unwrap().unwrap();
        assert_eq!(p1.format, PayloadFormat::Flv);
        let p2 = reader.pull().unwrap().unwrap();
        assert_eq!(p2.format, PayloadFormat::Raw);
    }

    #[tokio::test]
    async fn matrix_routing_multi_reader_fan_out() {
        let engine = MediaEngine::new();
        let ring = engine.get_or_create_pipeline("fanout-pipe").await;

        ring.push(test_video_packet(0, 0, true));
        ring.push(test_audio_packet(10, 10));

        let mut r1 = Reader::new("reader-1".to_string(), ring.clone());
        let mut r2 = Reader::new("reader-2".to_string(), ring.clone());
        let mut r3 = Reader::new("reader-3".to_string(), ring);

        for reader in [&mut r1, &mut r2, &mut r3] {
            let p = reader.pull().unwrap().unwrap();
            assert_eq!(p.pts, 0);
            assert!(p.is_keyframe);
        }
    }

    #[tokio::test]
    async fn matrix_routing_transcoder_stage_isolation() {
        let engine = Arc::new(MediaEngine::new());
        let source = engine.get_or_create_pipeline("iso-pipe").await;

        source.push(test_video_packet(0, 0, true));

        let tc_ring = engine
            .get_or_create_transcoder(
                "iso-pipe",
                StageKind::video_preset("720p"),
                source.clone(),
                None,
            )
            .await;

        assert!(
            !Arc::ptr_eq(&source, &tc_ring),
            "transcoder output ring must differ from source ring"
        );

        let mut source_reader = Reader::new("src".to_string(), source);
        let p = source_reader.pull().unwrap().unwrap();
        assert_eq!(p.pts, 0);
    }

    // ── fault resilience: ingest lifecycle ──────────────────────────────

    #[tokio::test]
    async fn health_input_on_after_register_off_after_unregister() {
        let engine = MediaEngine::new();
        let pipelines = vec!["p1".to_string()];

        let snap = engine.health_snapshot(&pipelines, &HashMap::new()).await;
        assert_eq!(snap["pipelines"]["p1"]["input"]["status"], "off");

        engine
            .try_register_ingest("p1", "key", "rtmp")
            .await
            .unwrap();
        let snap = engine.health_snapshot(&pipelines, &HashMap::new()).await;
        assert_eq!(snap["pipelines"]["p1"]["input"]["status"], "on");

        engine.unregister_ingest("p1").await;
        let snap = engine.health_snapshot(&pipelines, &HashMap::new()).await;
        assert_eq!(snap["pipelines"]["p1"]["input"]["status"], "off");
    }

    #[tokio::test]
    async fn health_snapshot_preserves_recent_ingest_disconnect_details_after_unregister() {
        let engine = MediaEngine::new();
        let pipelines = vec!["p1".to_string()];

        engine
            .try_register_ingest("p1", "key", "rtmp")
            .await
            .unwrap();
        engine
            .update_ingest_meta("p1", None, None, Some("127.0.0.1:9000".to_string()))
            .await;
        engine.update_ingest_bytes("p1", 4096).await;
        engine
            .record_ingest_disconnect(
                "p1",
                Some("session"),
                Some("publisher disconnected".to_string()),
                false,
            )
            .await;
        engine.unregister_ingest("p1").await;

        let snap = engine.health_snapshot(&pipelines, &HashMap::new()).await;
        let input = &snap["pipelines"]["p1"]["input"];
        assert_eq!(input["status"], "off");
        assert_eq!(input["probeStatus"], "off");
        assert_eq!(input["lastSessionProtocol"], "rtmp");
        assert_eq!(input["lastDisconnectReason"], "publisher disconnected");
        assert_eq!(input["lastFailurePhase"], "session");
        assert_eq!(input["recentDisconnectError"], false);
        assert_eq!(input["lastRemoteAddr"], "127.0.0.1:9000");
        assert_eq!(input["lastSessionBytesReceived"], 4096);
        assert!(input["lastDisconnectAt"].is_string());
        assert!(input["lastDisconnectAgeMs"].as_u64().is_some());
    }

    #[tokio::test]
    async fn re_register_ingest_clears_recent_disconnect_details() {
        let engine = MediaEngine::new();
        let pipelines = vec!["p1".to_string()];

        engine
            .try_register_ingest("p1", "key", "rtmp")
            .await
            .unwrap();
        engine
            .record_ingest_disconnect(
                "p1",
                Some("receive"),
                Some("connection reset by peer".to_string()),
                true,
            )
            .await;
        engine.unregister_ingest("p1").await;

        let snap = engine.health_snapshot(&pipelines, &HashMap::new()).await;
        assert_eq!(snap["pipelines"]["p1"]["input"]["probeStatus"], "failed");
        assert_eq!(
            snap["pipelines"]["p1"]["input"]["lastDisconnectReason"],
            "connection reset by peer"
        );

        engine
            .try_register_ingest("p1", "key", "srt")
            .await
            .unwrap();
        let snap = engine.health_snapshot(&pipelines, &HashMap::new()).await;
        assert_eq!(snap["pipelines"]["p1"]["input"]["status"], "on");
        assert!(snap["pipelines"]["p1"]["input"]["lastSessionProtocol"].is_null());
        assert!(snap["pipelines"]["p1"]["input"]["lastDisconnectReason"].is_null());
    }

    #[tokio::test]
    async fn double_register_ingest_rejected() {
        let engine = MediaEngine::new();
        let first = engine.try_register_ingest("p1", "key", "rtmp").await;
        assert!(first.is_some());

        let second = engine.try_register_ingest("p1", "key2", "srt").await;
        assert!(
            second.is_none(),
            "second register must be rejected while first is active"
        );
    }

    #[tokio::test]
    async fn re_register_ingest_after_unregister() {
        let engine = MediaEngine::new();
        let pipelines = vec!["p1".to_string()];

        let t1 = engine
            .try_register_ingest("p1", "key", "rtmp")
            .await
            .unwrap();
        engine.unregister_ingest("p1").await;
        assert!(t1.is_cancelled());

        let snap = engine.health_snapshot(&pipelines, &HashMap::new()).await;
        assert_eq!(snap["pipelines"]["p1"]["input"]["status"], "off");

        let t2 = engine.try_register_ingest("p1", "key", "srt").await;
        assert!(t2.is_some(), "re-register after unregister must succeed");

        let snap = engine.health_snapshot(&pipelines, &HashMap::new()).await;
        assert_eq!(snap["pipelines"]["p1"]["input"]["status"], "on");
        assert_eq!(
            snap["pipelines"]["p1"]["input"]["publisher"]["protocol"],
            "srt"
        );
    }

    // ── fault resilience: egress error transitions ─────────────────────

    #[tokio::test]
    async fn egress_error_during_sending_transitions_to_failed() {
        let engine = MediaEngine::new();
        engine
            .register_egress("out-1", "pipe-1", "rtmp://127.0.0.1:1935/live/key")
            .await;
        engine.update_egress_phase("out-1", "sending").await;
        engine.record_egress_progress("out-1", 5000).await;

        let status = engine.output_status("out-1").await.unwrap();
        assert_eq!(status["phase"], "sending");

        engine
            .record_egress_error("out-1", "send", "connection reset by peer")
            .await;

        let status = engine.output_status("out-1").await.unwrap();
        assert_eq!(status["phase"], "failed");
        assert_eq!(status["failurePhase"], "send");
        assert_eq!(status["lastError"], "connection reset by peer");
    }

    #[tokio::test]
    async fn egress_cleaned_up_after_unregister() {
        let engine = MediaEngine::new();
        let token = engine
            .register_egress("out-1", "pipe-1", "rtmp://127.0.0.1:1935/live/key")
            .await;

        assert!(engine.output_status("out-1").await.is_some());

        engine.unregister_egress("out-1").await;
        assert!(token.is_cancelled());
        assert!(
            engine.output_status("out-1").await.is_some(),
            "output_status must preserve the last classified egress state after unregister"
        );
    }

    #[tokio::test]
    async fn recent_egress_failure_survives_unregister_and_preserves_error_fields() {
        let engine = MediaEngine::new();
        engine
            .register_egress("out-1", "pipe-1", "rtmp://127.0.0.1:1935/live/key")
            .await;
        engine.update_egress_phase("out-1", "sending").await;
        engine.record_egress_progress("out-1", 2048).await;
        engine
            .record_egress_error("out-1", "send", "connection reset by peer")
            .await;

        engine.unregister_egress("out-1").await;

        let status = engine.output_status("out-1").await.unwrap();
        assert_eq!(status["status"], "failed");
        assert_eq!(status["rawStatus"], "running");
        assert_eq!(status["phase"], "failed");
        assert_eq!(status["failurePhase"], "send");
        assert_eq!(status["lastError"], "connection reset by peer");
        assert_eq!(status["bytesOut"], 2048);
        assert_eq!(status["totalSize"], serde_json::Value::Null);
        assert!(status["lastErrorAt"].is_string());
        assert!(status["endedAt"].is_string());
        assert!(status["endedAgeMs"].as_u64().is_some());
    }

    #[tokio::test]
    async fn health_snapshot_keeps_recent_egress_status_visible_after_unregister() {
        let engine = MediaEngine::new();
        let pipeline_id = "pipe-1".to_string();
        engine.get_or_create_pipeline(&pipeline_id).await;
        engine
            .register_egress(
                "out-1",
                &pipeline_id,
                "srt://example.com:10080?streamid=live/test",
            )
            .await;
        engine.update_egress_phase("out-1", "sending").await;
        engine
            .record_egress_error("out-1", "connect", "connection failed")
            .await;

        engine.unregister_egress("out-1").await;

        let snapshot = engine
            .health_snapshot(&[pipeline_id], &HashMap::new())
            .await;
        let output = &snapshot["pipelines"]["pipe-1"]["outputs"]["out-1"];
        assert_eq!(output["status"], "failed");
        assert_eq!(output["phase"], "failed");
        assert_eq!(output["failurePhase"], "connect");
        assert_eq!(output["lastError"], "connection failed");
        assert!(output["endedAt"].is_string());
    }

    #[tokio::test]
    async fn re_register_egress_clears_recent_snapshot() {
        let engine = MediaEngine::new();
        engine
            .register_egress("out-1", "pipe-1", "rtmp://127.0.0.1:1935/live/key")
            .await;
        engine
            .record_egress_error("out-1", "connect", "connection refused")
            .await;
        engine.unregister_egress("out-1").await;
        assert!(engine.recent_egress_outcome("out-1").await.is_some());

        engine
            .register_egress("out-1", "pipe-1", "rtmp://127.0.0.1:1935/live/key")
            .await;

        assert!(engine.recent_egress_outcome("out-1").await.is_none());
    }

    // ── adaptive ring sizing ──────────────────────────────────────────────────

    #[tokio::test]
    async fn adapt_pipeline_ring_no_op_when_default_is_sufficient() {
        // 1080p30 + 1 audio = 80 pkt/s → needed = ceil(80 × 6) = 480 < default 1024
        let engine = MediaEngine::new();
        engine.get_or_create_pipeline("p").await;

        let result = engine.adapt_pipeline_ring("p", 30.0, 1).await;
        assert!(
            result.is_none(),
            "no resize needed for single-track 1080p30"
        );

        let ring = engine.get_or_create_pipeline("p").await;
        assert_eq!(ring.capacity(), default_ring_capacity());
        let depth = ring.buffer_depth_secs().unwrap();
        assert!(depth >= 12.0 && depth <= 13.0, "depth={depth}");
    }

    #[tokio::test]
    async fn adapt_pipeline_ring_resizes_for_multi_track_stream() {
        // 2v16a: 30 fps + 16 audio × 50 = 830 pkt/s → needed = ceil(830 × 6) = 4980
        let engine = MediaEngine::new();
        engine.get_or_create_pipeline("p").await;

        let new_ring = engine
            .adapt_pipeline_ring("p", 30.0, 16)
            .await
            .expect("ring must be resized for 830 pkt/s");

        assert_eq!(new_ring.capacity(), 4980);
        let depth = new_ring.buffer_depth_secs().unwrap();
        assert!((depth - 6.0).abs() < 0.1, "depth={depth}");
        assert_eq!(engine.get_or_create_pipeline("p").await.capacity(), 4980);
    }

    #[tokio::test]
    async fn adapt_pipeline_ring_4k60_single_audio_no_resize() {
        // 4K 60fps + 1 audio = 110 pkt/s → needed = 660 < default 1024
        let engine = MediaEngine::new();
        engine.get_or_create_pipeline("p").await;

        let result = engine.adapt_pipeline_ring("p", 60.0, 1).await;
        assert!(
            result.is_none(),
            "default 1024 already covers 4K60 single-track"
        );
    }

    #[tokio::test]
    async fn adapt_pipeline_ring_4k60_multi_audio_resizes() {
        // 4K 60fps + 16 audio = 860 pkt/s → needed = ceil(860 × 6) = 5160
        let engine = MediaEngine::new();
        engine.get_or_create_pipeline("p").await;

        let new_ring = engine
            .adapt_pipeline_ring("p", 60.0, 16)
            .await
            .expect("resize needed for 4K60 + 16 audio");

        assert_eq!(new_ring.capacity(), 5160);
        let depth = new_ring.buffer_depth_secs().unwrap();
        assert!((depth - 6.0).abs() < 0.1, "depth={depth}");
    }

    #[tokio::test]
    async fn get_or_create_pipeline_preserves_adapted_ring_across_calls() {
        // The adapted ring must be returned by all subsequent get_or_create_pipeline
        // calls so egress readers and TS mux stages attach to the correctly-sized ring.
        let engine = MediaEngine::new();
        engine.get_or_create_pipeline("p").await;
        let new_ring = engine
            .adapt_pipeline_ring("p", 30.0, 16)
            .await
            .expect("should resize for 830 pkt/s");
        assert_eq!(new_ring.capacity(), 4980);

        let ring2 = engine.get_or_create_pipeline("p").await;
        assert_eq!(
            ring2.capacity(),
            4980,
            "adapted ring must persist across calls"
        );

        let _reader = crate::media::ring_buffer::Reader::new("hold".to_string(), ring2.clone());
        let ring3 = engine.get_or_create_pipeline("p").await;
        assert_eq!(
            ring3.capacity(),
            4980,
            "ring must not change with active reader"
        );
    }

    #[tokio::test]
    async fn adapt_pipeline_ring_lighter_republish_updates_rate_not_capacity() {
        // A lighter re-publish (1v1a after 2v16a) does not shrink the ring —
        // it just updates estimated_pkt_rate so bufferDepthSecs is correct.
        let engine = MediaEngine::new();
        engine.get_or_create_pipeline("p").await;
        engine.adapt_pipeline_ring("p", 30.0, 16).await; // → 4980 for 830 pkt/s

        // Lighter re-publish: 1v1a = 80 pkt/s → needed = 480 < 4980 → no resize.
        let result = engine.adapt_pipeline_ring("p", 30.0, 1).await;
        assert!(
            result.is_none(),
            "no resize when ring is already large enough"
        );

        let ring = engine.get_or_create_pipeline("p").await;
        assert_eq!(
            ring.capacity(),
            4980,
            "capacity preserved from heavier session"
        );
        let depth = ring.buffer_depth_secs().unwrap();
        // telemetry now reflects the lighter stream's real depth: 4980/80 ≈ 62 s
        assert!(depth > 60.0, "4980/80 ≈ 62.3 s; got {depth}");
    }

    #[tokio::test]
    async fn health_input_protocol_matches_registration() {
        let engine = MediaEngine::new();
        let pipelines = vec!["p1".to_string()];

        for proto in ["rtmp", "srt", "file"] {
            engine
                .try_register_ingest("p1", "key", proto)
                .await
                .unwrap();
            let snap = engine.health_snapshot(&pipelines, &HashMap::new()).await;
            assert_eq!(
                snap["pipelines"]["p1"]["input"]["publisher"]["protocol"], proto,
                "protocol mismatch for {proto}"
            );
            engine.unregister_ingest("p1").await;
        }
    }
}
