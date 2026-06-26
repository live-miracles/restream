//! Native diagnostic runner — replaces the old Node.js shell-command approach.
//!
//! Each `DiagCheck` is an async function that:
//!   1. Emits a `running` SSE event.
//!   2. Performs its work (inspecting `MediaEngine`, `sysinfo`, network sockets, etc.).
//!   3. Emits a `result` SSE event with full output and any detected issues.
//!
//! The endpoint streams these via Server-Sent Events so the browser can show
//! live progress just like the old diagnostics UI.

use std::sync::Arc;
use std::time::Instant;

use serde::Serialize;
use serde_json::json;
use sysinfo::{Disks, Networks, System};

use crate::media::engine::MediaEngine;

// ─── SSE event helpers ────────────────────────────────────────────────────────

/// Format a named SSE event with JSON data.
pub fn sse_event(event: &str, data: &serde_json::Value) -> String {
    format!("event: {}\ndata: {}\n\n", event, data)
}

// ─── Result types ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagResult {
    pub index: u32,
    pub name: String,
    pub description: String,
    /// A human-readable "command" that describes what was checked (for display).
    pub command: String,
    /// 0 = ok, non-zero = check detected a problem.
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u64,
    pub issues: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
}

impl DiagResult {
    fn ok(index: u32, name: &str, desc: &str, cmd: &str, stdout: String, elapsed: u64) -> Self {
        Self {
            index,
            name: name.into(),
            description: desc.into(),
            command: cmd.into(),
            exit_code: 0,
            stdout,
            stderr: String::new(),
            duration_ms: elapsed,
            issues: vec![],
            help: None,
        }
    }

    fn with_issues(mut self, issues: Vec<String>) -> Self {
        self.issues = issues;
        self
    }
}

// ─── Running event ────────────────────────────────────────────────────────────

fn running_event(index: u32, name: &str, description: &str) -> String {
    sse_event(
        "running",
        &json!({
            "index": index,
            "name": name,
            "description": description,
        }),
    )
}

fn result_event(result: &DiagResult) -> String {
    sse_event("result", &serde_json::to_value(result).unwrap_or_default())
}

// ─── Individual checks ────────────────────────────────────────────────────────

async fn check_engine_status(idx: u32, engine: &Arc<MediaEngine>, pipeline_id: &str) -> DiagResult {
    let start = Instant::now();
    let ingests = engine.active_ingests.read().await;
    let egresses = engine.active_egresses.read().await;
    let pipelines = engine.pipelines.read().await;

    let active_ingest = ingests.get(pipeline_id);
    let pipeline_rb = pipelines.get(pipeline_id);
    let active_output_count = egresses
        .iter()
        .filter(|(_, e)| e.pipeline_id == pipeline_id)
        .count();

    let mut issues = vec![];
    let mut lines = vec![];

    lines.push(format!("Pipeline ID: {}", pipeline_id));
    lines.push(format!("Active ingests (all pipelines): {}", ingests.len()));
    lines.push(format!(
        "Active egresses (all pipelines): {}",
        egresses.len()
    ));

    if let Some(ingest) = active_ingest {
        lines.push(format!("Ingest protocol: {}", ingest.protocol));
        lines.push(format!(
            "Ingest uptime: {:.1}s",
            ingest.start_time.elapsed().as_secs_f64()
        ));
        lines.push(format!(
            "Bytes received: {}",
            ingest
                .bytes_received
                .load(std::sync::atomic::Ordering::Relaxed)
        ));
        if let Some(addr) = &ingest.remote_addr {
            lines.push(format!("Publisher remote: {}", addr));
        }
    } else {
        lines.push("No active ingest for this pipeline.".to_string());
        issues.push("No active publisher is connected to this pipeline.".to_string());
    }

    if let Some(rb) = pipeline_rb {
        let (fill, cap) = rb.fill_and_capacity();
        let fill_pct = (fill * 100).checked_div(cap).unwrap_or(0);
        lines.push(format!(
            "Ring buffer: {}/{} slots filled ({}%)",
            fill, cap, fill_pct
        ));
        if fill_pct > 90 {
            issues.push(format!(
                "Ring buffer is {}% full — possible consumer lag or encoder overrun.",
                fill_pct
            ));
        }

        let reader_snapshots = rb.reader_snapshots();
        let max_lag = reader_snapshots
            .iter()
            .map(|reader| reader.lag_slots)
            .max()
            .unwrap_or(0);
        let total_overflows: usize = reader_snapshots
            .iter()
            .map(|reader| reader.overflow_count)
            .sum();
        let max_packet_age_ms = reader_snapshots
            .iter()
            .filter_map(|reader| reader.packet_age_ms)
            .max();
        lines.push(format!(
            "Ring buffer readers: max lag={}, total overflows={}, max packet age={}ms",
            max_lag,
            total_overflows,
            max_packet_age_ms
                .map(|age| age.to_string())
                .unwrap_or_else(|| "n/a".to_string())
        ));
        if total_overflows > 0 {
            issues.push(format!(
                "Consumers are dropping frames due to overflow ({} total overflows).",
                total_overflows
            ));
        }
    } else {
        lines.push("Ring buffer not yet allocated for this pipeline.".to_string());
    }

    lines.push(format!(
        "Active outputs for this pipeline: {}",
        active_output_count
    ));

    DiagResult::ok(
        idx,
        "Engine Status",
        "MediaEngine active state",
        "engine.health_snapshot()",
        lines.join("\n"),
        start.elapsed().as_millis() as u64,
    )
    .with_issues(issues)
}

async fn check_system_resources(idx: u32) -> DiagResult {
    let start = Instant::now();
    let mut sys = System::new_all();
    sys.refresh_all();

    let cpu_pct: f32 = sys.global_cpu_info().cpu_usage();
    let total_mem = sys.total_memory();
    let used_mem = sys.used_memory();
    let mem_pct = (used_mem * 100).checked_div(total_mem).unwrap_or(0);

    let disks = Disks::new_with_refreshed_list();
    let (total_disk, used_disk) = disks.iter().fold((0u64, 0u64), |(t, u), d| {
        (
            t + d.total_space(),
            u + (d.total_space() - d.available_space()),
        )
    });
    let disk_pct = (used_disk * 100).checked_div(total_disk).unwrap_or(0);

    let mut issues = vec![];
    let mut lines = vec![];

    lines.push(format!("CPU cores: {}", sys.cpus().len()));
    lines.push(format!("CPU usage: {:.1}%", cpu_pct));
    lines.push(format!(
        "RAM total: {} GiB",
        total_mem / (1024 * 1024 * 1024)
    ));
    lines.push(format!(
        "RAM used: {} GiB ({:.1}%)",
        used_mem / (1024 * 1024 * 1024),
        mem_pct
    ));
    lines.push(format!(
        "Disk total: {} GiB",
        total_disk / (1024 * 1024 * 1024)
    ));
    lines.push(format!(
        "Disk used: {} GiB ({}%)",
        used_disk / (1024 * 1024 * 1024),
        disk_pct
    ));

    if cpu_pct > 90.0 {
        issues.push(format!(
            "CPU usage is very high ({:.1}%). This may cause encoding delays or stream drops.",
            cpu_pct
        ));
    }
    if mem_pct > 90 {
        issues.push(format!(
            "RAM usage is {}%. Risk of OOM killing the streaming process.",
            mem_pct
        ));
    }
    if disk_pct > 95 {
        issues.push(format!(
            "Disk is {}% full. Recordings and HLS segments may fail.",
            disk_pct
        ));
    }

    DiagResult::ok(
        idx,
        "System Resources",
        "CPU, RAM, and disk utilization",
        "sysinfo::System::new_all()",
        lines.join("\n"),
        start.elapsed().as_millis() as u64,
    )
    .with_issues(issues)
}

async fn check_active_outputs(
    idx: u32,
    engine: &Arc<MediaEngine>,
    pipeline_id: &str,
) -> DiagResult {
    let start = Instant::now();
    let egresses = engine.active_egresses.read().await;

    let my_egresses: Vec<_> = egresses
        .iter()
        .filter(|(_, e)| e.pipeline_id == pipeline_id)
        .collect();

    let mut issues = vec![];
    let mut lines = vec![];

    if my_egresses.is_empty() {
        lines.push("No active outputs for this pipeline.".to_string());
    } else {
        for (key, egress) in &my_egresses {
            let output_id = key.as_str();
            let bytes_sent = egress.bytes_sent.load(std::sync::atomic::Ordering::Relaxed);
            let phase = egress
                .phase
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let target_addr = egress
                .target_addr
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
                .unwrap_or_else(|| "unresolved".to_string());
            let last_progress_ms = egress
                .last_progress_ms
                .load(std::sync::atomic::Ordering::Relaxed);
            let last_progress = if last_progress_ms > 0 {
                let age = chrono::Utc::now()
                    .timestamp_millis()
                    .max(0)
                    .saturating_sub(last_progress_ms as i64);
                format!("{}ms ago", age)
            } else {
                "never".to_string()
            };
            let last_error = egress
                .last_error
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            lines.push(format!(
                "Output {}: protocol={} status={} phase={} target={} target_addr={} bytes_sent={} last_progress={} started_at={}",
                output_id,
                egress.protocol,
                egress.status,
                phase,
                egress.target_url,
                target_addr,
                bytes_sent,
                last_progress,
                egress.started_at
            ));
            if let Some(error) = last_error {
                let failure_phase = egress
                    .failure_phase
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string());
                lines.push(format!(
                    "  last_error_phase={} last_error={}",
                    failure_phase, error
                ));
            }
            let quality = egress
                .quality
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            match egress.protocol.as_str() {
                "rtmp" => {
                    if let Some(reason) = quality.tcp_stats_unavailable_reason {
                        lines.push(format!("  tcp_quality=unavailable reason={}", reason));
                    } else if quality.tcp_rtt_ms.is_some()
                        || quality.tcp_send_rate_mbps.is_some()
                        || quality.tcp_notsent_bytes.is_some()
                    {
                        lines.push(format!(
                            "  tcp_quality cc={:?} rtt_ms={:?} send_mbps={:?} notsent_bytes={:?} unacked={:?} retrans_total={:?} snd_cwnd={:?}",
                            quality.tcp_congestion_algorithm,
                            quality.tcp_rtt_ms,
                            quality.tcp_send_rate_mbps,
                            quality.tcp_notsent_bytes,
                            quality.tcp_unacked,
                            quality.tcp_total_retrans,
                            quality.tcp_snd_cwnd
                        ));
                    }
                }
                "srt" => {
                    if quality.ms_rtt.is_some()
                        || quality.mbps_send_rate.is_some()
                        || quality.srt_bonded.is_some()
                    {
                        lines.push(format!(
                            "  srt_quality rtt_ms={:?} send_mbps={:?} sent_loss={:?} sent_drop={:?} sent_retrans={:?} bonded={:?} members={:?}/{:?} active={:?} broken={:?}",
                            quality.ms_rtt,
                            quality.mbps_send_rate,
                            quality.packets_sent_loss,
                            quality.packets_sent_drop,
                            quality.packets_sent_retrans,
                            quality.srt_bonded,
                            quality.srt_group_connected_members,
                            quality.srt_group_member_count,
                            quality.srt_group_active_members,
                            quality.srt_group_broken_members
                        ));
                    }
                }
                _ => {}
            }
            if egress.status == "failed" || phase == "failed" {
                issues.push(format!(
                    "Output {} has failed in phase {}. Check target URL: {}",
                    output_id, phase, egress.target_url
                ));
            }
        }
    }

    DiagResult::ok(
        idx,
        "Active Outputs",
        "Egress target status and throughput",
        "engine.active_egresses snapshot",
        lines.join("\n"),
        start.elapsed().as_millis() as u64,
    )
    .with_issues(issues)
}

async fn check_ingest_stream_info(
    idx: u32,
    engine: &Arc<MediaEngine>,
    pipeline_id: &str,
) -> DiagResult {
    let start = Instant::now();
    let ingests = engine.active_ingests.read().await;
    let ingest_opt = ingests.get(pipeline_id);

    let mut issues = vec![];
    let mut lines = vec![];

    if let Some(ingest) = ingest_opt {
        if let Some(video) = &ingest.video {
            lines.push(format!("Video codec: {}", video.codec));
            lines.push(format!("Resolution: {}x{}", video.width, video.height));
            lines.push(format!("Frame rate: {:.2} fps", video.fps));
            if let Some(bw) = video.bw {
                lines.push(format!("Video bitrate: {:.1} Kbps", bw));
            }
            if video.width == 0 || video.height == 0 {
                issues
                    .push("Video resolution is 0x0 — stream metadata not yet parsed.".to_string());
            }
        } else {
            lines.push("No video stream metadata available yet.".to_string());
            issues.push(
                "No video stream detected. The publisher may not have sent media yet.".to_string(),
            );
        }
        if let Some(audio) = &ingest.audio {
            lines.push(format!("Audio codec: {}", audio.codec));
            lines.push(format!("Sample rate: {} Hz", audio.sample_rate));
            lines.push(format!("Channels: {}", audio.channels));
        } else {
            lines.push("No audio stream metadata available yet.".to_string());
        }
    } else {
        lines.push("No active ingest — cannot inspect stream info.".to_string());
        issues.push("Pipeline is not actively receiving data.".to_string());
    }

    DiagResult::ok(
        idx,
        "Stream Info",
        "Video and audio codec parameters",
        "engine.active_ingests.video/audio",
        lines.join("\n"),
        start.elapsed().as_millis() as u64,
    )
    .with_issues(issues)
}

async fn check_publisher_transport(
    idx: u32,
    engine: &Arc<MediaEngine>,
    pipeline_id: &str,
    probe_protocol: &str,
) -> DiagResult {
    let start = Instant::now();
    let ingests = engine.active_ingests.read().await;
    let ingest_opt = ingests.get(pipeline_id);

    let mut issues = vec![];
    let mut lines = vec![];

    if let Some(ingest) = ingest_opt {
        let q = &ingest.quality;
        if probe_protocol == "srt" {
            lines.push("Protocol: SRT".to_string());
            if q.srt_bonded == Some(true) {
                let members = q.srt_group_member_count.unwrap_or(0);
                let connected = q.srt_group_connected_members.unwrap_or(0);
                let active = q.srt_group_active_members.unwrap_or(0);
                let broken = q.srt_group_broken_members.unwrap_or(0);
                lines.push(format!(
                    "Bonded group: {} members, {} connected, {} active, {} broken",
                    members, connected, active, broken
                ));
                if active == 0 {
                    issues.push("SRT bond has no active member links.".to_string());
                }
                if broken > 0 {
                    issues.push(format!("SRT bond has {} broken member link(s).", broken));
                }
            } else if q.srt_bonded == Some(false) {
                lines.push("Bonded group: no (single SRT link)".to_string());
            }
            if let Some(rtt) = q.ms_rtt {
                lines.push(format!("RTT: {:.1} ms", rtt));
                if rtt > 200.0 {
                    issues.push(format!("High SRT RTT: {:.1}ms (threshold 200ms)", rtt));
                }
            }
            if let Some(recv_rate) = q.mbps_receive_rate {
                lines.push(format!("Receive rate: {:.2} Mbps", recv_rate));
            }
            if let Some(cap) = q.mbps_link_capacity {
                lines.push(format!("Link capacity: {:.2} Mbps", cap));
            }
            let loss_total = q.packets_received_loss.unwrap_or(0);
            match q.packets_received_loss_per_sec {
                Some(rate) => {
                    lines.push(format!(
                        "Packets lost: {:.1}/s ({} total)",
                        rate, loss_total
                    ));
                    if rate >= 5.0 {
                        issues.push(format!(
                            "High SRT packet loss rate: {:.1}/s (threshold 5/s)",
                            rate
                        ));
                    }
                }
                None => lines.push(format!("Packets lost: —/s ({} total)", loss_total)),
            }
            let drop_total = q.packets_received_drop.unwrap_or(0);
            match q.packets_received_drop_per_sec {
                Some(rate) => {
                    lines.push(format!(
                        "Packets dropped: {:.1}/s ({} total)",
                        rate, drop_total
                    ));
                    if rate >= 1.0 {
                        issues.push(format!(
                            "SRT packet drop rate: {:.1}/s (threshold 1/s)",
                            rate
                        ));
                    }
                }
                None => lines.push(format!("Packets dropped: —/s ({} total)", drop_total)),
            }
            let retrans_total = q.packets_received_retrans.unwrap_or(0);
            match q.packets_received_retrans_per_sec {
                Some(rate) => {
                    lines.push(format!(
                        "Packets retransmitted: {:.1}/s ({} total)",
                        rate, retrans_total
                    ));
                    if rate >= 10.0 {
                        issues.push(format!(
                            "High SRT retransmission rate: {:.1}/s (threshold 10/s)",
                            rate
                        ));
                    }
                }
                None => lines.push(format!(
                    "Packets retransmitted: —/s ({} total)",
                    retrans_total
                )),
            }
            let undecrypt_total = q.packets_received_undecrypt.unwrap_or(0);
            match q.packets_received_undecrypt_per_sec {
                Some(rate) => {
                    lines.push(format!(
                        "Packets undecrypted: {:.1}/s ({} total)",
                        rate, undecrypt_total
                    ));
                    if rate > 0.0 {
                        issues.push(format!(
                            "SRT undecrypted packet rate: {:.1}/s (expected 0/s)",
                            rate
                        ));
                    }
                }
                None => lines.push(format!(
                    "Packets undecrypted: —/s ({} total)",
                    undecrypt_total
                )),
            }
            if let Some(latency) = q.ms_receive_tsb_pd_delay {
                lines.push(format!("Negotiated latency buffer: {:.0}ms", latency));
            }
            if let Some(buf) = q.ms_receive_buf {
                lines.push(format!("Current latency buffer: {:.0}ms", buf));
            }
            if let (Some(snd), Some(snd_avail)) = (q.srt_send_buf_bytes, q.srt_send_buf_avail_bytes)
            {
                let total = snd + snd_avail;
                let pct = if total > 0 {
                    (snd as f64 / total as f64) * 100.0
                } else {
                    0.0
                };
                lines.push(format!(
                    "Send buffer: {}KB / {}KB ({:.0}%)",
                    snd / 1024,
                    total / 1024,
                    pct
                ));
            }
            if let (Some(rcv), Some(rcv_avail)) = (q.srt_recv_buf_bytes, q.srt_recv_buf_avail_bytes)
            {
                let total = rcv + rcv_avail;
                let pct = if total > 0 {
                    (rcv as f64 / total as f64) * 100.0
                } else {
                    0.0
                };
                lines.push(format!(
                    "Recv buffer: {}KB / {}KB ({:.0}%)",
                    rcv / 1024,
                    total / 1024,
                    pct
                ));
            }
            if let Some(flight) = q.srt_flight_size_pkts {
                lines.push(format!("Packets in flight: {}", flight));
            }
            if lines.len() == 1 {
                lines.push("No SRT transport stats available yet.".to_string());
                issues.push("SRT quality metrics not yet populated. Stats update after first packets arrive.".to_string());
            }
        } else {
            // RTMP/TCP
            lines.push("Protocol: RTMP (TCP)".to_string());
            if let Some(reason) = &q.tcp_stats_unavailable_reason {
                lines.push(format!("TCP stats unavailable: {}", reason));
                issues.push(format!(
                    "TCP transport stats could not be collected: {}",
                    reason
                ));
            } else {
                if let Some(rtt) = q.tcp_rtt_ms {
                    lines.push(format!("TCP RTT: {:.1} ms", rtt));
                    if rtt >= 200.0 {
                        issues.push(format!("High TCP RTT: {:.1}ms (threshold 200ms)", rtt));
                    }
                }
                if let Some(rate) = q.tcp_receive_rate_mbps {
                    lines.push(format!("TCP receive rate: {:.2} Mbps", rate));
                }
                if let Some(rcv_rtt) = q.tcp_rcv_rtt_ms {
                    lines.push(format!("TCP receive RTT: {:.1} ms", rcv_rtt));
                }
                if let Some(last_rcv) = q.tcp_last_rcv_ms {
                    lines.push(format!("Time since last receive: {} ms", last_rcv));
                    if last_rcv >= 5_000 {
                        issues.push(format!(
                            "RTMP publisher receive stall: {}ms since last packet (threshold 5000ms)",
                            last_rcv
                        ));
                    }
                }
                if let Some(out_of_order) = q.tcp_rcv_ooopack {
                    lines.push(format!("Out-of-order packets (HOL): {}", out_of_order));
                    if out_of_order >= 50 {
                        issues.push(format!(
                            "High TCP out-of-order packet count: {} (threshold 50)",
                            out_of_order
                        ));
                    }
                }
                if let Some(window) = q.tcp_rcv_space {
                    lines.push(format!("TCP receive window: {} bytes", window));
                }
                if let Some(used) = q.tcp_skmem_rmem_alloc {
                    match q.tcp_skmem_rmem_max {
                        Some(max) if max > 0 => {
                            let saturation = used as f64 / max as f64;
                            lines.push(format!(
                                "TCP receive buffer: {} / {} bytes ({:.1}%)",
                                used,
                                max,
                                saturation * 100.0
                            ));
                            if saturation > 0.8 {
                                issues.push(format!(
                                    "TCP receive buffer is {:.1}% full (threshold 80%)",
                                    saturation * 100.0
                                ));
                            }
                        }
                        _ => lines.push(format!("TCP receive buffer used: {} bytes", used)),
                    }
                }
                if lines.len() == 1 {
                    lines.push("No TCP socket stats available yet.".to_string());
                    issues.push("TCP quality metrics not yet populated. Stats update periodically while RTMP is connected.".to_string());
                }
            }
        }
    } else {
        lines.push("No active ingest — cannot inspect publisher transport.".to_string());
        issues.push("Pipeline has no active publisher.".to_string());
    }

    DiagResult::ok(
        idx,
        "Publisher Transport",
        "Network connection quality metrics",
        if probe_protocol == "srt" {
            "libsrt srt_bistats()"
        } else {
            "getsockopt(TCP_INFO/SO_MEMINFO)"
        },
        lines.join("\n"),
        start.elapsed().as_millis() as u64,
    )
    .with_issues(issues)
}

async fn check_ring_buffer_health(
    idx: u32,
    engine: &Arc<MediaEngine>,
    pipeline_id: &str,
) -> DiagResult {
    let start = Instant::now();
    let pipelines = engine.pipelines.read().await;
    let rb_opt = pipelines.get(pipeline_id);

    let mut issues = vec![];
    let mut lines = vec![];

    if let Some(rb) = rb_opt {
        let (fill, cap) = rb.fill_and_capacity();
        let fill_pct = (fill * 100).checked_div(cap).unwrap_or(0);
        lines.push(format!("Capacity: {} slots", cap));
        lines.push(format!("Filled: {} slots ({}%)", fill, fill_pct));
        lines.push("Compact packet slots: yes".to_string());
        lines.push("Frame size: variable (media packets)".to_string());

        let readers_info = rb.reader_snapshots();

        if !readers_info.is_empty() {
            lines.push("Active readers:".to_string());
            for reader in &readers_info {
                lines.push(format!(
                    "  - {}: lag={} slots, overflows={}, packet_age={}ms",
                    reader.name,
                    reader.lag_slots,
                    reader.overflow_count,
                    reader
                        .packet_age_ms
                        .map(|age| age.to_string())
                        .unwrap_or_else(|| "n/a".to_string())
                ));
                if reader.lag_slots > (cap * 8 / 10) {
                    issues.push(format!(
                        "Reader {} is severely lagging ({} / {} slots). Possible network congestion or performance bottleneck.",
                        reader.name, reader.lag_slots, cap
                    ));
                }
                if reader.overflow_count > 0 {
                    issues.push(format!(
                        "Reader {} has experienced {} overflow(s). Dropped frames occurred.",
                        reader.name, reader.overflow_count
                    ));
                }
            }
        } else {
            lines.push("Active readers: none".to_string());
        }

        if fill_pct > 85 {
            issues.push(format!(
                "Ring buffer is {}% full. Egress consumers may be lagging behind ingest. \
                 Check output target connectivity and bitrate matching.",
                fill_pct
            ));
        }
        if fill == 0 {
            lines.push("Buffer is empty — no media has been received recently.".to_string());
        }
    } else {
        lines.push("Ring buffer not yet allocated for this pipeline.".to_string());
        issues.push(
            "Ring buffer allocation indicates no ingest has started on this pipeline.".to_string(),
        );
    }

    DiagResult::ok(
        idx,
        "Ring Buffer Health",
        "In-process media ring buffer state",
        "RingBuffer::fill_and_capacity()",
        lines.join("\n"),
        start.elapsed().as_millis() as u64,
    )
    .with_issues(issues)
}

async fn check_gop_analysis(idx: u32, engine: &Arc<MediaEngine>, pipeline_id: &str) -> DiagResult {
    let start = Instant::now();
    let ingests = engine.active_ingests.read().await;
    let ingest_opt = ingests.get(pipeline_id);

    let mut issues = vec![];
    let mut lines = vec![];

    if let Some(ingest) = ingest_opt {
        let times = ingest
            .keyframe_times
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if times.len() < 2 {
            lines.push(format!("Keyframes observed: {}", times.len()));
            lines.push("Not enough keyframes to analyze GOP intervals yet.".to_string());
        } else {
            let intervals: Vec<f64> = times
                .windows(2)
                .map(|w| ((w[1] - w[0]) as f64 / 1000.0).max(0.0))
                .collect();
            let avg = intervals.iter().sum::<f64>() / intervals.len() as f64;
            let min = intervals.iter().cloned().fold(f64::INFINITY, f64::min);
            let max = intervals.iter().cloned().fold(0.0f64, f64::max);
            let variance =
                intervals.iter().map(|v| (v - avg).powi(2)).sum::<f64>() / intervals.len() as f64;
            let stddev = variance.sqrt();

            lines.push(format!("Keyframes observed: {}", times.len()));
            lines.push(format!("GOP intervals sampled: {}", intervals.len()));
            lines.push(format!("Average GOP interval: {:.2}s", avg));
            lines.push(format!("Min: {:.2}s  Max: {:.2}s", min, max));
            lines.push(format!("Std deviation: {:.3}s", stddev));

            if stddev > 0.5 {
                issues.push(format!(
                    "Unstable keyframe interval (jitter {:.2}s). Average GOP is {:.2}s. \
                     This causes player buffering and adaptive bitrate switching failures.",
                    stddev, avg
                ));
            }
            if avg > 8.0 {
                issues.push(format!(
                    "Keyframe interval is very high ({:.2}s). \
                     High intervals make seeking sluggish and increase stream latency.",
                    avg
                ));
            }
        }
    } else {
        lines.push("No active ingest — cannot analyze GOP.".to_string());
        issues.push("Pipeline is not actively receiving data.".to_string());
    }

    DiagResult::ok(
        idx,
        "GOP Analysis",
        "Keyframe interval consistency",
        "engine.keyframe_times analysis",
        lines.join("\n"),
        start.elapsed().as_millis() as u64,
    )
    .with_issues(issues)
}

async fn check_srt_listener_socket(idx: u32, engine: &Arc<MediaEngine>) -> DiagResult {
    let start = Instant::now();
    let stats = &engine.srt_listener_stats;
    let rx_queue = stats
        .rx_queue_bytes
        .load(std::sync::atomic::Ordering::Relaxed);
    let rx_peak = stats
        .rx_queue_max_bytes
        .load(std::sync::atomic::Ordering::Relaxed);
    let drops = stats.drops.load(std::sync::atomic::Ordering::Relaxed);
    let bonding_available = stats
        .bonding_available
        .load(std::sync::atomic::Ordering::Relaxed);

    let configured = crate::media::srt::DESIRED_UDP_BUF as u64;
    let active_count = engine.active_ingests.read().await.len();

    let mut lines = vec![];
    let mut issues = vec![];

    lines.push(format!("Active SRT ingest streams: {}", active_count));
    lines.push(format!(
        "Bonded ingest available: {}",
        if bonding_available { "yes" } else { "no" }
    ));
    lines.push(format!(
        "UDP recv queue: {}KB / {}KB ({:.1}%)",
        rx_queue / 1024,
        configured / 1024,
        if configured > 0 {
            rx_queue as f64 / configured as f64 * 100.0
        } else {
            0.0
        }
    ));
    lines.push(format!("UDP recv queue peak: {}KB", rx_peak / 1024));
    lines.push(format!("Kernel UDP drops (total): {}", drops));

    if drops > 0 {
        issues.push(format!(
            "Kernel has dropped {} UDP packets — data loss occurred. \
             Increase net.core.rmem_max and restart.",
            drops
        ));
    }
    if !bonding_available {
        issues.push(
            "Linked libsrt rejected SRTO_GROUPCONNECT; rebuild it with ENABLE_BONDING=ON for bonded ingest."
                .to_string(),
        );
    }
    if rx_queue > configured * 3 / 4 {
        issues.push(format!(
            "UDP recv queue is {:.0}% full — imminent packet loss risk with {} streams.",
            rx_queue as f64 / configured as f64 * 100.0,
            active_count,
        ));
    } else if rx_queue > configured / 2 {
        issues.push(format!(
            "UDP recv queue is {:.0}% full — buffer pressure building.",
            rx_queue as f64 / configured as f64 * 100.0,
        ));
    }

    DiagResult::ok(
        idx,
        "SRT Listener Socket",
        "Shared UDP socket buffer occupancy for all SRT ingest streams",
        "read /proc/net/udp",
        lines.join("\n"),
        start.elapsed().as_millis() as u64,
    )
    .with_issues(issues)
}

async fn check_network_bandwidth(idx: u32) -> DiagResult {
    let start = Instant::now();
    // Collect two samples 500ms apart for rate calculation
    let net1 = Networks::new_with_refreshed_list();
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    let net2 = Networks::new_with_refreshed_list();

    let mut total_rx_bytes = 0u64;
    let mut total_tx_bytes = 0u64;
    let mut lines = vec![];
    let mut issues = vec![];

    for (iface, data2) in net2.iter() {
        if let Some(data1) = net1.get(iface) {
            let rx = data2
                .total_received()
                .saturating_sub(data1.total_received());
            let tx = data2
                .total_transmitted()
                .saturating_sub(data1.total_transmitted());
            if rx > 0 || tx > 0 {
                // Scale from 500ms sample to per-second
                let rx_kbps = (rx * 8 * 2) / 1000;
                let tx_kbps = (tx * 8 * 2) / 1000;
                lines.push(format!("{}: ↓ {} Kbps ↑ {} Kbps", iface, rx_kbps, tx_kbps));
                total_rx_bytes += rx;
                total_tx_bytes += tx;
            }
        }
    }

    let total_rx_kbps = (total_rx_bytes * 8 * 2) / 1000;
    let total_tx_kbps = (total_tx_bytes * 8 * 2) / 1000;

    if lines.is_empty() {
        lines.push("No active network interfaces detected.".to_string());
        issues.push("No network traffic detected. Check network configuration.".to_string());
    } else {
        lines.push(format!(
            "Total: ↓ {} Kbps ↑ {} Kbps",
            total_rx_kbps, total_tx_kbps
        ));
    }

    DiagResult::ok(
        idx,
        "Network Bandwidth",
        "Per-interface RX/TX throughput (500ms sample)",
        "sysinfo::Networks::new_with_refreshed_list()",
        lines.join("\n"),
        start.elapsed().as_millis() as u64,
    )
    .with_issues(issues)
}

// ─── Top-level runner ─────────────────────────────────────────────────────────

/// Run all diagnostic checks for a pipeline, streaming SSE events as they complete.
pub async fn run_diagnostics(
    engine: Arc<MediaEngine>,
    pipeline_id: String,
    probe_protocol: String,
    tx: tokio::sync::mpsc::Sender<String>,
) {
    let overall_start = Instant::now();

    macro_rules! run_check {
        ($idx:expr, $name:expr, $desc:expr, $check:expr) => {{
            if tx.send(running_event($idx, $name, $desc)).await.is_err() {
                return;
            }
            let result = $check.await;
            if tx.send(result_event(&result)).await.is_err() {
                return;
            }
        }};
    }

    run_check!(
        0,
        "Engine Status",
        "MediaEngine active state for this pipeline",
        check_engine_status(0, &engine, &pipeline_id)
    );

    run_check!(
        1,
        "Stream Info",
        "Video and audio codec parameters from demuxer",
        check_ingest_stream_info(1, &engine, &pipeline_id)
    );

    run_check!(
        2,
        "GOP Analysis",
        "Keyframe interval consistency and stability",
        check_gop_analysis(2, &engine, &pipeline_id)
    );

    run_check!(
        3,
        "Publisher Transport",
        "Network connection quality metrics",
        check_publisher_transport(3, &engine, &pipeline_id, &probe_protocol)
    );

    run_check!(
        4,
        "Ring Buffer Health",
        "In-process media ring buffer fill level and alignment",
        check_ring_buffer_health(4, &engine, &pipeline_id)
    );

    run_check!(
        5,
        "Active Outputs",
        "Egress target status and throughput",
        check_active_outputs(5, &engine, &pipeline_id)
    );

    run_check!(
        6,
        "System Resources",
        "CPU, RAM, and disk utilization",
        check_system_resources(6)
    );

    run_check!(
        7,
        "Network Bandwidth",
        "Per-interface RX/TX throughput measurement",
        check_network_bandwidth(7)
    );

    run_check!(
        8,
        "SRT Listener Socket",
        "Shared UDP socket buffer occupancy",
        check_srt_listener_socket(8, &engine)
    );

    let total_ms = overall_start.elapsed().as_millis() as u64;
    let _ = tx
        .send(sse_event("done", &json!({ "totalDurationMs": total_ms })))
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::engine::MediaEngine;

    #[tokio::test]
    async fn test_run_diagnostics_early_exit_on_disconnect() {
        let engine = Arc::new(MediaEngine::new());
        let (tx, rx) = tokio::sync::mpsc::channel::<String>(32);
        drop(rx);

        let start = Instant::now();
        run_diagnostics(engine, "pipe-test".to_string(), "rtmp".to_string(), tx).await;
        assert!(start.elapsed().as_millis() < 100);
    }
}
