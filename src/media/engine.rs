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

use crate::media::hls::HlsStore;
use crate::media::ring_buffer::RingBuffer;

/// Lock-free counters for one processing stage.
/// Updated atomically on the hot path; read by `/graph` for operator visibility.
#[derive(Debug)]
pub struct StageMetrics {
    pub packets_in: AtomicU64,
    pub packets_out: AtomicU64,
    pub bytes_in: AtomicU64,
    pub bytes_out: AtomicU64,
    /// Cumulative processing time in microseconds.
    pub processing_us: AtomicU64,
    pub start_instant: Instant,
}

impl StageMetrics {
    pub fn new() -> Self {
        Self {
            packets_in: AtomicU64::new(0),
            packets_out: AtomicU64::new(0),
            bytes_in: AtomicU64::new(0),
            bytes_out: AtomicU64::new(0),
            processing_us: AtomicU64::new(0),
            start_instant: Instant::now(),
        }
    }

    #[inline]
    pub fn record_in(&self, bytes: u64) {
        self.packets_in.fetch_add(1, Ordering::Relaxed);
        self.bytes_in.fetch_add(bytes, Ordering::Relaxed);
    }

    #[inline]
    pub fn record_out(&self, bytes: u64) {
        self.packets_out.fetch_add(1, Ordering::Relaxed);
        self.bytes_out.fetch_add(bytes, Ordering::Relaxed);
    }

    #[inline]
    pub fn record_processing(&self, us: u64) {
        self.processing_us.fetch_add(us, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> serde_json::Value {
        let pkts_in = self.packets_in.load(Ordering::Relaxed);
        let pkts_out = self.packets_out.load(Ordering::Relaxed);
        let bytes_in = self.bytes_in.load(Ordering::Relaxed);
        let bytes_out = self.bytes_out.load(Ordering::Relaxed);
        let proc_us = self.processing_us.load(Ordering::Relaxed);
        let elapsed = self.start_instant.elapsed().as_secs_f64();

        let avg_us_per_packet = if pkts_in > 0 {
            proc_us as f64 / pkts_in as f64
        } else {
            0.0
        };

        serde_json::json!({
            "packetsIn": pkts_in,
            "packetsOut": pkts_out,
            "bytesIn": bytes_in,
            "bytesOut": bytes_out,
            "processingUs": proc_us,
            "avgUsPerPacket": avg_us_per_packet,
            "uptimeSecs": elapsed,
            "packetsPerSec": if elapsed > 0.0 { pkts_in as f64 / elapsed } else { 0.0 },
        })
    }
}

/// Tracks HLS consumers for a pipeline. Persistent consumers (egress outputs)
/// register/unregister explicitly. Transient consumers (browser preview) keep
/// the segmenter alive via playlist fetch heartbeats.
pub struct HlsConsumers {
    /// Number of persistent consumers (HLS egress outputs).
    pub persistent: AtomicU64,
    /// Epoch millis of the last playlist/segment fetch (transient consumer heartbeat).
    pub last_access_ms: AtomicU64,
    /// Cancel token for the segmenter task.
    pub cancel_token: CancellationToken,
}

impl HlsConsumers {
    pub fn new(cancel_token: CancellationToken) -> Self {
        Self {
            persistent: AtomicU64::new(0),
            last_access_ms: AtomicU64::new(Self::now_ms()),
            cancel_token,
        }
    }

    fn now_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }

    pub fn touch(&self) {
        self.last_access_ms.store(Self::now_ms(), Ordering::Relaxed);
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
        let now = Self::now_ms();
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
    pub audio_tracks: std::sync::Mutex<Vec<AudioMeta>>,
    pub quality: PublisherQuality,
    pub keyframe_times: std::sync::Mutex<Vec<Instant>>,
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
    // Map of "pipeline_id:encoding" -> transcoded RingBuffer + cancel token
    pub transcoder_buffers: TokioRwLock<HashMap<String, (Arc<RingBuffer>, CancellationToken)>>,
    // SRT listener socket kernel buffer stats
    pub srt_listener_stats: Arc<ListenerSocketStats>,
    // Map of pipeline_id -> HLS consumer tracking (refcount + idle timer)
    pub hls_consumers: TokioRwLock<HashMap<String, HlsConsumers>>,
    // Per-stage processing metrics keyed by "<pipeline_id>:<stage_name>"
    pub stage_metrics: TokioRwLock<HashMap<String, Arc<StageMetrics>>>,
}

impl MediaEngine {
    pub fn new() -> Self {
        // Initialize FFmpeg once
        ffmpeg::init().unwrap();

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
        }
    }

    pub async fn get_or_create_stage_metrics(
        &self,
        pipeline_id: &str,
        stage_name: &str,
    ) -> Arc<StageMetrics> {
        let key = format!("{}:{}", pipeline_id, stage_name);
        let mut metrics = self.stage_metrics.write().await;
        metrics
            .entry(key)
            .or_insert_with(|| Arc::new(StageMetrics::new()))
            .clone()
    }

    pub async fn remove_stage_metrics(&self, pipeline_id: &str, stage_name: &str) {
        let key = format!("{}:{}", pipeline_id, stage_name);
        self.stage_metrics.write().await.remove(&key);
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
        encoding: &str,
        source_buffer: Arc<RingBuffer>,
    ) -> Arc<RingBuffer> {
        let key = format!("{}:{}", pipeline_id, encoding);
        {
            let buffers = self.transcoder_buffers.read().await;
            if let Some((rb, token)) = buffers.get(&key) {
                if !token.is_cancelled() {
                    return rb.clone();
                }
            }
        }

        let output_buf = Arc::new(RingBuffer::new(4096));
        let cancel = CancellationToken::new();
        {
            let mut buffers = self.transcoder_buffers.write().await;
            buffers.insert(key.clone(), (output_buf.clone(), cancel.clone()));
        }

        println!(
            "[transcoder] Spawning stage: pipeline={} encoding={}",
            pipeline_id, encoding
        );

        let pid = pipeline_id.to_string();
        let enc = encoding.to_string();
        let ob = output_buf.clone();
        tokio::spawn(async move {
            crate::media::transcoder::start_transcoder(pid, enc, source_buffer, ob, cancel).await;
        });

        output_buf
    }

    /// Return the active processing stages for a pipeline as (key, is_alive) pairs.
    /// Used for diagnostics and visualization.
    pub async fn active_transcoder_stages(&self, pipeline_id: &str) -> Vec<(String, bool)> {
        let buffers = self.transcoder_buffers.read().await;
        let prefix = format!("{}:", pipeline_id);
        buffers
            .iter()
            .filter(|(k, _)| k.starts_with(&prefix))
            .map(|(k, (_, token))| {
                let encoding = k.strip_prefix(&prefix).unwrap_or(k).to_string();
                (encoding, !token.is_cancelled())
            })
            .collect()
    }

    pub async fn remove_pipeline(&self, pipeline_id: &str) {
        let mut pipelines = self.pipelines.write().await;
        pipelines.remove(pipeline_id);
    }

    /// Register a publisher for a pipeline.
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
        if let Some(existing) = tokens.get(pipeline_id) {
            if !existing.is_cancelled() {
                return None;
            }
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
                audio_tracks: std::sync::Mutex::new(Vec::new()),
                quality: PublisherQuality::default(),
                keyframe_times: std::sync::Mutex::new(Vec::new()),
                video_sequence_header: std::sync::Mutex::new(None),
                audio_sequence_header: std::sync::Mutex::new(None),
            },
        );

        Some(token)
    }

    pub async fn unregister_ingest(&self, pipeline_id: &str) {
        let mut tokens = self.ingest_cancel_tokens.write().await;
        if let Some(token) = tokens.remove(pipeline_id) {
            token.cancel();
        }

        let mut ingests = self.active_ingests.write().await;
        ingests.remove(pipeline_id);
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

        token
    }

    pub async fn unregister_egress(&self, output_id: &str) {
        let mut tokens = self.egress_cancel_tokens.write().await;
        if let Some(token) = tokens.remove(output_id) {
            token.cancel();
        }

        let mut egresses = self.active_egresses.write().await;
        egresses.remove(output_id);
    }

    /// Update bytes received counter for an active ingest (lock-free atomic).
    pub async fn update_ingest_bytes(&self, pipeline_id: &str, bytes: u64) {
        let ingests = self.active_ingests.read().await;
        if let Some(ingest) = ingests.get(pipeline_id) {
            ingest.bytes_received.fetch_add(bytes, Ordering::Relaxed);
        }
    }

    pub async fn record_keyframe(&self, pipeline_id: &str) {
        let ingests = self.active_ingests.read().await;
        if let Some(ingest) = ingests.get(pipeline_id) {
            let mut times = ingest.keyframe_times.lock().unwrap();
            times.push(Instant::now());
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
                *ingest.video_sequence_header.lock().unwrap() = Some(data);
            } else {
                *ingest.audio_sequence_header.lock().unwrap() = Some(data);
            }
        }
    }

    pub async fn get_sequence_headers(
        &self,
        pipeline_id: &str,
    ) -> (Option<bytes::Bytes>, Option<bytes::Bytes>) {
        let ingests = self.active_ingests.read().await;
        if let Some(ingest) = ingests.get(pipeline_id) {
            let video = ingest.video_sequence_header.lock().unwrap().clone();
            let audio = ingest.audio_sequence_header.lock().unwrap().clone();
            (video, audio)
        } else {
            (None, None)
        }
    }

    /// Update audio track metadata for an active ingest (multi-track support).
    pub async fn update_ingest_audio_tracks(&self, pipeline_id: &str, tracks: Vec<AudioMeta>) {
        let ingests = self.active_ingests.read().await;
        if let Some(ingest) = ingests.get(pipeline_id) {
            *ingest.audio_tracks.lock().unwrap() = tracks;
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
            let tracks = ingest.audio_tracks.lock().unwrap();
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
            let times = ingest.keyframe_times.lock().unwrap();
            if times.len() >= 2 {
                let intervals: Vec<f64> = times
                    .windows(2)
                    .map(|w| w[1].duration_since(w[0]).as_secs_f64())
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
        tokens.contains_key(pipeline_id)
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

        let mut pipelines_json = serde_json::Map::new();

        for pipeline_id in pipeline_ids {
            let ingest_opt = ingests.get(pipeline_id.as_str());

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
                    "bytesSent": 0,
                    "readers": 0,
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
                    "bytesSent": 0,
                    "readers": 0,
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
                        let mut prev_time = egress.prev_sample_time.lock().unwrap();
                        let elapsed = prev_time.elapsed().as_secs_f64();
                        let kbps = if elapsed > 0.5 && bytes_sent > prev {
                            let delta = bytes_sent - prev;
                            let rate = (delta as f64 * 8.0) / (elapsed * 1000.0);
                            egress.prev_bytes_sent.store(bytes_sent, Ordering::Relaxed);
                            *prev_time = Instant::now();
                            *egress.bitrate_kbps.lock().unwrap() = Some(rate);
                            Some(rate)
                        } else {
                            *egress.bitrate_kbps.lock().unwrap()
                        };
                        kbps
                    };

                    outputs_json.insert(
                        output_id.to_string(),
                        serde_json::json!({
                            "status": egress.status,
                            "totalSize": bytes_sent,
                            "bitrateKbps": bitrate_kbps,
                            "startedAt": egress.started_at,
                        }),
                    );
                }
            }

            let rec_enabled = recording_enabled.get(pipeline_id).copied().unwrap_or(false);
            let rec_active = rec_tokens.contains_key(pipeline_id.as_str());
            let hls_active = hls_consumers.contains_key(pipeline_id.as_str());

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
        let all_stage_metrics = self.stage_metrics.read().await;

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
            (fill, cap)
        });
        nodes.push(serde_json::json!({
            "id": rb_node_id,
            "type": "ring_buffer",
            "label": "Source Buffer",
            "active": rb_info.is_some(),
            "details": rb_info.map(|(fill, cap)| serde_json::json!({
                "fill": fill,
                "capacity": cap,
                "fillPercent": if cap > 0 { fill * 100 / cap } else { 0 },
                "format": "FLV (interleaved A+V)",
            })),
        }));
        edges.push(serde_json::json!({
            "from": ingest_node_id,
            "to": rb_node_id,
            "label": "push(MediaPacket)",
        }));

        // Nodes: transcoder stages (keys are "video:720p" or "audio:atrack:0:from:720p")
        let prefix = format!("{}:", pipeline_id);
        for (key, (_, token)) in transcoder_buffers.iter() {
            if let Some(stage_key) = key.strip_prefix(&prefix) {
                let stage_id = format!(
                    "{}_{}_stage",
                    pipeline_id,
                    stage_key.replace([':', '+', ','], "_")
                );
                let is_video = stage_key.starts_with("video:");
                let is_audio = stage_key.starts_with("audio:");

                let label = if is_video {
                    let preset = stage_key.strip_prefix("video:").unwrap_or(stage_key);
                    format!("Video: {}", preset)
                } else if is_audio {
                    let rest = stage_key.strip_prefix("audio:").unwrap_or(stage_key);
                    if let Some((audio_op, _from)) = rest.rsplit_once(":from:") {
                        format!("Audio: {}", audio_op)
                    } else {
                        format!("Audio: {}", rest)
                    }
                } else {
                    format!("Stage: {}", stage_key)
                };

                let stage_metrics_key = format!("{}:{}", pipeline_id, stage_key);
                nodes.push(serde_json::json!({
                    "id": stage_id,
                    "type": if is_audio { "audio_filter" } else { "transcoder" },
                    "label": label,
                    "stageKey": stage_key,
                    "active": !token.is_cancelled(),
                    "metrics": all_stage_metrics.get(&stage_metrics_key).map(|m| m.snapshot()),
                }));

                if is_audio {
                    // Audio stage reads from its upstream video stage (encoded in key)
                    let upstream_preset = stage_key
                        .rsplit_once(":from:")
                        .map(|(_, from)| from)
                        .unwrap_or("source");
                    if upstream_preset != "source" {
                        let upstream_id = format!(
                            "{}_video_{}_stage",
                            pipeline_id,
                            upstream_preset.replace([':', '+', ','], "_")
                        );
                        edges.push(serde_json::json!({
                            "from": upstream_id,
                            "to": stage_id,
                            "label": "video copy + audio select",
                        }));
                    } else {
                        edges.push(serde_json::json!({
                            "from": rb_node_id,
                            "to": stage_id,
                            "label": "audio select",
                        }));
                    }
                } else {
                    let preset = stage_key.strip_prefix("video:").unwrap_or(stage_key);
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
                "active": egress.map_or(false, |e| e.status == "running"),
                "details": egress.map(|e| {
                    let bytes = e.bytes_sent.load(Ordering::Relaxed);
                    serde_json::json!({
                        "status": e.status,
                        "targetUrl": e.target_url,
                        "totalSize": bytes,
                        "bitrateKbps": *e.bitrate_kbps.lock().unwrap(),
                        "startedAt": e.started_at,
                    })
                }),
                "metrics": egress.map(|e| e.metrics.snapshot()),
            }));

            // Edge: from the appropriate stage to this egress
            // Mirror the reconciler's stage-key logic
            let video_preset = output.encoding.split('+').next().unwrap_or("source");
            let audio_part = output.encoding.split('+').nth(1);
            let needs_video =
                !video_preset.is_empty() && video_preset != "source" && video_preset != "custom";
            let video_stage_key = if needs_video { video_preset } else { "source" };

            if let Some(audio) = audio_part.filter(|a| !a.is_empty()) {
                let audio_stage_key = format!("audio:{}:from:{}", audio, video_stage_key);
                let stage_id = format!(
                    "{}_{}_stage",
                    pipeline_id,
                    audio_stage_key.replace([':', '+', ','], "_")
                );
                edges.push(serde_json::json!({
                    "from": stage_id,
                    "to": output_node_id,
                    "label": "MPEG-TS",
                }));
            } else if needs_video {
                let stage_id = format!(
                    "{}_video_{}_stage",
                    pipeline_id,
                    video_preset.replace([':', '+', ','], "_")
                );
                edges.push(serde_json::json!({
                    "from": stage_id,
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

        // Node: recording (if active)
        if rec_tokens.contains_key(pipeline_id) {
            let rec_id = format!("{}_recording", pipeline_id);
            let rec_metrics_key = format!("{}:recording", pipeline_id);
            nodes.push(serde_json::json!({
                "id": rec_id,
                "type": "recording",
                "label": "MKV Recording",
                "active": true,
                "metrics": all_stage_metrics.get(&rec_metrics_key).map(|m| m.snapshot()),
            }));
            edges.push(serde_json::json!({
                "from": rb_node_id,
                "to": rec_id,
                "label": "MKV mux",
            }));
        }

        // Node: HLS (if active)
        if hls_stores.contains_key(pipeline_id) {
            let hls_id = format!("{}_hls_preview", pipeline_id);
            let hls_metrics_key = format!("{}:hls", pipeline_id);
            nodes.push(serde_json::json!({
                "id": hls_id,
                "type": "hls",
                "label": "HLS Preview",
                "active": true,
                "metrics": all_stage_metrics.get(&hls_metrics_key).map(|m| m.snapshot()),
            }));
            edges.push(serde_json::json!({
                "from": rb_node_id,
                "to": hls_id,
                "label": "MPEG-TS segment",
            }));
        }

        serde_json::json!({
            "pipelineId": pipeline_id,
            "nodes": nodes,
            "edges": edges,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

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
    async fn ingest_bytes_and_meta_on_nonexistent_pipeline_is_noop() {
        let engine = MediaEngine::new();
        // Should not panic
        engine.update_ingest_bytes("nonexistent", 1000).await;
        engine
            .update_ingest_meta("nonexistent", None, None, None)
            .await;
    }
}
