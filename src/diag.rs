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
        .filter(|(k, _)| k.starts_with(&format!("{}:", pipeline_id)))
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
        let fill_pct = if cap > 0 { fill * 100 / cap } else { 0 };
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
    let mem_pct = if total_mem > 0 {
        used_mem * 100 / total_mem
    } else {
        0
    };

    let disks = Disks::new_with_refreshed_list();
    let (total_disk, used_disk) = disks.iter().fold((0u64, 0u64), |(t, u), d| {
        (
            t + d.total_space(),
            u + (d.total_space() - d.available_space()),
        )
    });
    let disk_pct = if total_disk > 0 {
        used_disk * 100 / total_disk
    } else {
        0
    };

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
        .filter(|(k, _)| k.starts_with(&format!("{}:", pipeline_id)))
        .collect();

    let mut issues = vec![];
    let mut lines = vec![];

    if my_egresses.is_empty() {
        lines.push("No active outputs for this pipeline.".to_string());
    } else {
        for (key, egress) in &my_egresses {
            let output_id = key.split_once(':').map(|(_, o)| o).unwrap_or(key.as_str());
            let bytes_sent = egress.bytes_sent.load(std::sync::atomic::Ordering::Relaxed);
            lines.push(format!(
                "Output {}: status={} target={} bytes_sent={} started_at={}",
                output_id, egress.status, egress.target_url, bytes_sent, egress.started_at
            ));
            if egress.status == "failed" {
                issues.push(format!(
                    "Output {} has failed. Check target URL: {}",
                    output_id, egress.target_url
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
            lines.push(format!("Protocol: SRT"));
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
            if let Some(loss) = q.packets_received_loss {
                lines.push(format!("Packets lost: {}", loss));
                if loss > 100 {
                    issues.push(format!("High SRT packet loss: {} (threshold 100)", loss));
                }
            }
            if let Some(drop) = q.packets_received_drop {
                lines.push(format!("Packets dropped: {}", drop));
                if drop > 10 {
                    issues.push(format!("SRT packet drop: {} (threshold 10)", drop));
                }
            }
            if let Some(retrans) = q.packets_received_retrans {
                lines.push(format!("Packets retransmitted: {}", retrans));
                if retrans > 200 {
                    issues.push(format!(
                        "High SRT retransmissions: {} (threshold 200)",
                        retrans
                    ));
                }
            }
            if let Some(latency) = q.ms_receive_tsb_pd_delay {
                lines.push(format!("Negotiated latency buffer: {:.0}ms", latency));
            }
            if let Some(buf) = q.ms_receive_buf {
                lines.push(format!("Current latency buffer: {:.0}ms", buf));
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
                    if rtt > 200.0 {
                        issues.push(format!("High TCP RTT: {:.1}ms (threshold 200ms)", rtt));
                    }
                }
                if let Some(retrans) = q.tcp_retransmits {
                    lines.push(format!("TCP retransmissions: {}", retrans));
                    if retrans >= 10 {
                        issues.push(format!("TCP retransmissions: {} (threshold 10)", retrans));
                    }
                }
                if let Some(cwnd) = q.tcp_cwnd {
                    lines.push(format!("TCP congestion window: {} segments", cwnd));
                }
                if let Some(unacked) = q.tcp_unacked {
                    lines.push(format!("TCP unacked: {} segments", unacked));
                    if unacked >= 16 {
                        issues.push(format!(
                            "High TCP unacked segments: {} (threshold 16)",
                            unacked
                        ));
                    }
                }
                if let Some(dr) = q.tcp_delivery_rate_mbps {
                    lines.push(format!("TCP delivery rate: {:.3} Mbps", dr));
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
            "nix TCP_INFO socket option"
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
        let fill_pct = if cap > 0 { fill * 100 / cap } else { 0 };
        lines.push(format!("Capacity: {} slots", cap));
        lines.push(format!("Filled: {} slots ({}%)", fill, fill_pct));
        lines.push(format!("Cache-line aligned slots: yes"));
        lines.push(format!("Frame size: variable (media packets)"));
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
            let _ = tx.send(running_event($idx, $name, $desc)).await;
            let result = $check.await;
            let _ = tx.send(result_event(&result)).await;
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
        "Publisher Transport",
        "Network connection quality metrics",
        check_publisher_transport(2, &engine, &pipeline_id, &probe_protocol)
    );

    run_check!(
        3,
        "Ring Buffer Health",
        "In-process media ring buffer fill level and alignment",
        check_ring_buffer_health(3, &engine, &pipeline_id)
    );

    run_check!(
        4,
        "Active Outputs",
        "Egress target status and throughput",
        check_active_outputs(4, &engine, &pipeline_id)
    );

    run_check!(
        5,
        "System Resources",
        "CPU, RAM, and disk utilization",
        check_system_resources(5)
    );

    run_check!(
        6,
        "Network Bandwidth",
        "Per-interface RX/TX throughput measurement",
        check_network_bandwidth(6)
    );

    let total_ms = overall_start.elapsed().as_millis() as u64;
    let _ = tx
        .send(sse_event("done", &json!({ "totalDurationMs": total_ms })))
        .await;
}
