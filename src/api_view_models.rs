use crate::domain::stage::StageKey;
use crate::media::engine::{
    ActiveEgress, ActiveIngest, EgressRetryState, MediaEngine, RecentEgressOutcome,
    RecentIngestOutcome,
};
use crate::media::ring_buffer::RingBuffer;
use std::sync::atomic::Ordering;

pub(crate) fn egress_runtime_json(
    egress: &ActiveEgress,
    include_target_url: bool,
    has_ingest: bool,
) -> serde_json::Value {
    let last_progress_ms = egress.last_progress_ms.load(Ordering::Relaxed);
    let last_error_ms = egress.last_error_ms.load(Ordering::Relaxed);
    let now_ms = MediaEngine::now_epoch_ms();
    let status = MediaEngine::egress_effective_status(egress, has_ingest);
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
        "lastProgressAt": MediaEngine::epoch_ms_to_rfc3339(last_progress_ms),
        "lastProgressAgeMs": (last_progress_ms > 0).then(|| now_ms.saturating_sub(last_progress_ms)),
        "lastError": egress.last_error.lock().unwrap_or_else(|e| e.into_inner()).clone(),
        "lastErrorAt": MediaEngine::epoch_ms_to_rfc3339(last_error_ms),
        "failurePhase": egress.failure_phase.lock().unwrap_or_else(|e| e.into_inner()).clone(),
        "retrying": false,
        "retryAttempts": serde_json::Value::Null,
        "retryBackoffMs": serde_json::Value::Null,
        "nextRetryAt": serde_json::Value::Null,
        "retryRemainingMs": serde_json::Value::Null,
        "quality": egress.quality.lock().unwrap_or_else(|e| e.into_inner()).clone(),
        "metrics": egress.metrics.snapshot(),
    });
    if include_target_url {
        value["targetUrl"] = serde_json::Value::String(egress.target_url.clone());
    }
    value
}

pub(crate) fn recent_egress_runtime_json(
    outcome: &RecentEgressOutcome,
    include_target_url: bool,
) -> serde_json::Value {
    let now_ms = MediaEngine::now_epoch_ms();
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
        "lastProgressAt": MediaEngine::epoch_ms_to_rfc3339(outcome.last_progress_ms),
        "lastProgressAgeMs": (outcome.last_progress_ms > 0).then(|| now_ms.saturating_sub(outcome.last_progress_ms)),
        "lastError": outcome.last_error,
        "lastErrorAt": MediaEngine::epoch_ms_to_rfc3339(outcome.last_error_ms),
        "failurePhase": outcome.failure_phase,
        "retrying": false,
        "retryAttempts": serde_json::Value::Null,
        "retryBackoffMs": serde_json::Value::Null,
        "nextRetryAt": serde_json::Value::Null,
        "retryRemainingMs": serde_json::Value::Null,
        "quality": outcome.quality,
        "metrics": outcome.metrics,
        "endedAt": MediaEngine::epoch_ms_to_rfc3339(outcome.ended_at_ms),
        "endedAgeMs": now_ms.saturating_sub(outcome.ended_at_ms),
    });
    if include_target_url {
        value["targetUrl"] = serde_json::Value::String(outcome.target_url.clone());
    }
    value
}

pub(crate) fn apply_egress_retry_state_json(
    value: &mut serde_json::Value,
    retry: Option<&EgressRetryState>,
) {
    let Some(retry) = retry else {
        return;
    };

    let remaining_ms = retry
        .next_retry_at_ms
        .saturating_sub(MediaEngine::now_epoch_ms());
    value["status"] = serde_json::Value::String("retrying".to_string());
    value["retrying"] = serde_json::Value::Bool(true);
    value["retryAttempts"] = serde_json::json!(retry.attempts);
    value["retryBackoffMs"] = serde_json::json!(retry.backoff_ms);
    value["nextRetryAt"] = MediaEngine::epoch_ms_to_rfc3339(retry.next_retry_at_ms)
        .map(serde_json::Value::String)
        .unwrap_or(serde_json::Value::Null);
    value["retryRemainingMs"] = serde_json::json!(remaining_ms);
}

pub(crate) fn probe_snapshot(pipeline_id: &str, ingest: &ActiveIngest) -> serde_json::Value {
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

    serde_json::json!({
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
    })
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

pub(crate) fn reader_snapshot_json(
    reader: &crate::media::ring_buffer::ReaderSnapshot,
) -> serde_json::Value {
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
}

pub(crate) fn active_pipeline_input_json(
    ingest: &ActiveIngest,
    total_bytes_sent: u64,
    readers_count: usize,
    reader_metrics: Vec<serde_json::Value>,
) -> serde_json::Value {
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
    let audio_tracks = ingest
        .audio_tracks
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .cloned()
        .collect::<Vec<_>>();
    let probe_ready = ingest.video.is_some() || !audio_tracks.is_empty();
    let probe_status = if probe_ready { "ready" } else { "pending" };
    let probe_pending_ms = (!probe_ready).then_some((elapsed_secs * 1000.0).round() as u64);

    serde_json::json!({
        "status": "on",
        "publishStartedAt": publish_started_at,
        "probeReady": probe_ready,
        "probeStatus": probe_status,
        "probePendingMs": probe_pending_ms,
        "bytesReceived": bytes_received,
        "bytesSent": total_bytes_sent,
        "readers": readers_count,
        "readerMetrics": reader_metrics,
        "bitrateKbps": bitrate_kbps,
        "video": ingest.video,
        "audio": ingest.audio,
        "audioTracks": audio_tracks,
        "publisher": publisher_json,
        "unexpectedReaders": { "count": 0 },
        "lastSessionProtocol": null,
        "lastDisconnectAt": null,
        "lastDisconnectAgeMs": null,
        "lastDisconnectReason": null,
        "lastFailurePhase": null,
        "recentDisconnectError": false,
        "disconnectGraceActive": false,
        "disconnectGraceRemainingMs": null,
        "lastRemoteAddr": null,
        "lastSessionBytesReceived": null
    })
}

pub(crate) fn inactive_pipeline_input_json(
    recent: Option<&RecentIngestOutcome>,
    total_bytes_sent: u64,
    readers_count: usize,
    reader_metrics: Vec<serde_json::Value>,
    disconnect_grace_ms: u64,
) -> serde_json::Value {
    let last_disconnect_age_ms =
        recent.map(|recent| MediaEngine::now_epoch_ms().saturating_sub(recent.disconnected_at_ms));
    let disconnect_grace_remaining_ms = if disconnect_grace_ms == 0 {
        None
    } else {
        last_disconnect_age_ms.and_then(|age_ms| disconnect_grace_ms.checked_sub(age_ms))
    };
    serde_json::json!({
        "status": "off",
        "probeReady": false,
        "probeStatus": if recent.is_some_and(|recent| recent.had_error) { "failed" } else { "off" },
        "probePendingMs": null,
        "bytesReceived": 0,
        "bytesSent": total_bytes_sent,
        "readers": readers_count,
        "readerMetrics": reader_metrics,
        "publisher": null,
        "unexpectedReaders": { "count": 0 },
        "lastSessionProtocol": recent.map(|recent| recent.protocol.clone()),
        "lastDisconnectAt": recent.and_then(|recent| MediaEngine::epoch_ms_to_rfc3339(recent.disconnected_at_ms)),
        "lastDisconnectAgeMs": last_disconnect_age_ms,
        "lastDisconnectReason": recent.and_then(|recent| recent.reason.clone()),
        "lastFailurePhase": recent.and_then(|recent| recent.failure_phase.clone()),
        "recentDisconnectError": recent.is_some_and(|recent| recent.had_error),
        "disconnectGraceActive": disconnect_grace_remaining_ms.is_some(),
        "disconnectGraceRemainingMs": disconnect_grace_remaining_ms,
        "lastRemoteAddr": recent.and_then(|recent| recent.remote_addr.clone()),
        "lastSessionBytesReceived": recent.map(|recent| recent.bytes_received)
    })
}

pub(crate) fn hls_preview_json(
    active: bool,
    persistent_consumers: u64,
    last_access_age_ms: Option<u64>,
    segments: usize,
    playlist_bytes: usize,
) -> serde_json::Value {
    serde_json::json!({
        "active": active,
        "persistentConsumers": persistent_consumers,
        "lastAccessAgeMs": last_access_age_ms,
        "segments": segments,
        "playlistBytes": playlist_bytes,
    })
}

pub(crate) fn pipeline_health_json(
    input: serde_json::Value,
    outputs: serde_json::Map<String, serde_json::Value>,
    recording_enabled: bool,
    recording_active: bool,
    hls_preview: serde_json::Value,
) -> serde_json::Value {
    serde_json::json!({
        "input": input,
        "outputs": serde_json::Value::Object(outputs),
        "recording": { "enabled": recording_enabled, "active": recording_active },
        "hlsPreview": hls_preview,
    })
}

pub(crate) fn ingest_telemetry_json(pipeline_id: &str, ingest: &ActiveIngest) -> serde_json::Value {
    serde_json::json!({
        "pipelineId": pipeline_id,
        "protocol": ingest.protocol,
        "uptimeSecs": ingest.start_time.elapsed().as_secs_f64(),
        "bytesReceived": ingest.bytes_received.load(Ordering::Relaxed),
        "metrics": ingest.metrics.snapshot(),
    })
}

pub(crate) fn stage_telemetry_row_json(
    key: &StageKey,
    metrics: serde_json::Value,
    pipe_metrics: Option<serde_json::Value>,
    active: Option<bool>,
    payload_stats: Option<serde_json::Value>,
) -> serde_json::Value {
    let mut value = serde_json::json!({
        "stageKey": key.to_string(),
        "pipelineId": key.pipeline.as_str(),
        "kind": key.kind.to_string(),
        "metrics": metrics,
    });
    if let Some(pipe_metrics) = pipe_metrics {
        value["pipeMetrics"] = pipe_metrics;
    }
    if let Some(active) = active {
        value["active"] = serde_json::Value::Bool(active);
    }
    if let Some(payload_stats) = payload_stats {
        value["payloadStats"] = payload_stats;
    }
    value
}

pub(crate) fn source_ring_telemetry_json(
    pipeline_id: &str,
    ring: &RingBuffer,
) -> serde_json::Value {
    serde_json::json!({
        "pipelineId": pipeline_id,
        "payloadStats": ring_payload_stats_json(ring),
    })
}

pub(crate) fn transcoder_ring_telemetry_json(
    key: &StageKey,
    ring: &RingBuffer,
    active: bool,
) -> serde_json::Value {
    serde_json::json!({
        "stageKey": key.to_string(),
        "pipelineId": key.pipeline.as_str(),
        "kind": key.kind.to_string(),
        "active": active,
        "payloadStats": ring_payload_stats_json(ring),
    })
}

pub(crate) fn ts_muxer_ring_telemetry_json(
    stage_key: &str,
    ring: &RingBuffer,
    active: bool,
) -> serde_json::Value {
    serde_json::json!({
        "stageKey": stage_key,
        "active": active,
        "payloadStats": ring_payload_stats_json(ring),
    })
}

pub(crate) fn avio_input_queue_json(
    key: &StageKey,
    len_bytes: usize,
    capacity_bytes: usize,
    high_water_bytes: usize,
    blocked_writes: u64,
    blocked_write_us: u64,
) -> serde_json::Value {
    serde_json::json!({
        "stageKey": key.to_string(),
        "pipelineId": key.pipeline.as_str(),
        "lenBytes": len_bytes,
        "capacityBytes": capacity_bytes,
        "highWaterBytes": high_water_bytes,
        "blockedWrites": blocked_writes,
        "blockedWriteUs": blocked_write_us,
    })
}

pub(crate) fn avio_egress_queue_json(
    output_id: &str,
    len_bytes: usize,
    capacity_bytes: usize,
    high_water_bytes: usize,
    blocked_writes: u64,
    blocked_write_us: u64,
) -> serde_json::Value {
    serde_json::json!({
        "outputId": output_id,
        "lenBytes": len_bytes,
        "capacityBytes": capacity_bytes,
        "highWaterBytes": high_water_bytes,
        "blockedWrites": blocked_writes,
        "blockedWriteUs": blocked_write_us,
    })
}

pub(crate) fn memory_accounting_json(
    retained_payload_bytes: u64,
    source_rings: Vec<serde_json::Value>,
    transcoder_rings: Vec<serde_json::Value>,
    ts_muxer_rings: Vec<serde_json::Value>,
    avio_total_len_bytes: usize,
    avio_total_capacity_bytes: usize,
    avio_input_queues: Vec<serde_json::Value>,
    avio_egress_queues: Vec<serde_json::Value>,
) -> serde_json::Value {
    serde_json::json!({
        "retainedPayloadBytes": retained_payload_bytes,
        "sourceRings": source_rings,
        "transcoderRings": transcoder_rings,
        "tsMuxerRings": ts_muxer_rings,
        "avioQueues": {
            "totalLenBytes": avio_total_len_bytes,
            "totalCapacityBytes": avio_total_capacity_bytes,
            "inputQueues": avio_input_queues,
            "egressQueues": avio_egress_queues,
        },
    })
}

pub(crate) fn engine_telemetry_json(
    generated_at: String,
    ingests: Vec<serde_json::Value>,
    stages: Vec<serde_json::Value>,
    egresses: Vec<serde_json::Value>,
    active_transcoder_buffers: usize,
    memory_accounting: serde_json::Value,
) -> serde_json::Value {
    serde_json::json!({
        "generatedAt": generated_at,
        "ingests": ingests,
        "stages": stages,
        "egresses": egresses,
        "activeTranscoderBuffers": active_transcoder_buffers,
        "memoryAccounting": memory_accounting,
    })
}

pub(crate) fn pipeline_ingest_telemetry_json(ingest: &ActiveIngest) -> serde_json::Value {
    serde_json::json!({
        "protocol": ingest.protocol,
        "streamKey": ingest.stream_key,
        "uptimeSecs": ingest.start_time.elapsed().as_secs_f64(),
        "bytesReceived": ingest.bytes_received.load(Ordering::Relaxed),
        "video": ingest.video,
        "audio": ingest.audio,
        "metrics": ingest.metrics.snapshot(),
    })
}

pub(crate) fn pipeline_source_ring_json(ring: &RingBuffer) -> serde_json::Value {
    let (fill, cap) = ring.fill_and_capacity();
    let readers: Vec<serde_json::Value> = ring
        .reader_snapshots()
        .into_iter()
        .map(|reader| {
            serde_json::json!({
                "name": reader.name,
                "lagSlots": reader.lag_slots,
                "overflowCount": reader.overflow_count,
                "packetAgeMs": reader.packet_age_ms,
            })
        })
        .collect();

    serde_json::json!({
        "fill": fill,
        "capacity": cap,
        "fillPercent": (fill * 100).checked_div(cap).unwrap_or(0),
        "estimatedPktRatePerSec": ring.estimated_pkt_rate.load(Ordering::Relaxed),
        "bufferDepthSecs": ring.buffer_depth_secs(),
        "payloadStats": ring_payload_stats_json(ring),
        "readers": readers,
    })
}

pub(crate) fn pipeline_telemetry_json(
    generated_at: String,
    pipeline_id: &str,
    ingest: Option<serde_json::Value>,
    source_ring: Option<serde_json::Value>,
    stages: Vec<serde_json::Value>,
    egresses: Vec<serde_json::Value>,
) -> serde_json::Value {
    serde_json::json!({
        "generatedAt": generated_at,
        "pipelineId": pipeline_id,
        "ingest": ingest,
        "sourceRing": source_ring,
        "stages": stages,
        "egresses": egresses,
    })
}

pub(crate) fn single_stage_telemetry_json(
    generated_at: String,
    key: &StageKey,
    metrics: serde_json::Value,
    pipe_metrics: Option<serde_json::Value>,
) -> serde_json::Value {
    let mut value = stage_telemetry_row_json(key, metrics, pipe_metrics, None, None);
    value["generatedAt"] = serde_json::Value::String(generated_at);
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::stage::StageKind;
    use crate::media::engine::RecentIngestOutcome;

    #[test]
    fn apply_egress_retry_state_marks_value_as_retrying() {
        let mut value = serde_json::json!({
            "status": "running",
            "retrying": false,
            "retryAttempts": serde_json::Value::Null,
            "retryBackoffMs": serde_json::Value::Null,
            "nextRetryAt": serde_json::Value::Null,
            "retryRemainingMs": serde_json::Value::Null,
        });
        let retry = EgressRetryState {
            attempts: 3,
            backoff_ms: 5_000,
            next_retry_at_ms: MediaEngine::now_epoch_ms() + 5_000,
        };

        apply_egress_retry_state_json(&mut value, Some(&retry));

        assert_eq!(value["status"], "retrying");
        assert_eq!(value["retrying"], true);
        assert_eq!(value["retryAttempts"], 3);
        assert_eq!(value["retryBackoffMs"], 5_000);
        assert!(value["retryRemainingMs"].as_u64().unwrap_or(0) > 0);
    }

    #[test]
    fn ring_payload_stats_reports_zero_average_for_empty_ring() {
        let ring = RingBuffer::new(8);

        let stats = ring_payload_stats_json(&ring);

        assert_eq!(stats["slots"], 0);
        assert_eq!(stats["avgPayloadBytes"], 0.0);
    }

    #[test]
    fn inactive_pipeline_input_reports_disconnect_grace() {
        let recent = RecentIngestOutcome {
            protocol: "srt".to_string(),
            disconnected_at_ms: MediaEngine::now_epoch_ms() - 2_000,
            reason: Some("socket closed".to_string()),
            failure_phase: Some("ingest".to_string()),
            had_error: true,
            remote_addr: Some("10.0.0.1:9000".to_string()),
            bytes_received: 1234,
        };

        let value = inactive_pipeline_input_json(
            Some(&recent),
            5678,
            2,
            vec![serde_json::json!({"name": "preview"})],
            5_000,
        );

        assert_eq!(value["status"], "off");
        assert_eq!(value["probeStatus"], "failed");
        assert_eq!(value["bytesSent"], 5678);
        assert_eq!(value["disconnectGraceActive"], true);
        assert!(value["disconnectGraceRemainingMs"].as_u64().unwrap_or(0) > 0);
        assert_eq!(value["lastSessionProtocol"], "srt");
    }

    #[test]
    fn pipeline_health_json_wraps_input_outputs_recording_and_hls() {
        let mut outputs = serde_json::Map::new();
        outputs.insert(
            "out-1".to_string(),
            serde_json::json!({"status": "running"}),
        );

        let value = pipeline_health_json(
            serde_json::json!({"status": "on"}),
            outputs,
            true,
            false,
            hls_preview_json(true, 1, Some(25), 3, 1024),
        );

        assert_eq!(value["input"]["status"], "on");
        assert_eq!(value["outputs"]["out-1"]["status"], "running");
        assert_eq!(value["recording"]["enabled"], true);
        assert_eq!(value["hlsPreview"]["segments"], 3);
    }

    #[test]
    fn stage_telemetry_row_json_includes_optional_fields() {
        let key = StageKey::new("telemetry-pipe", StageKind::video_preset("720p"));
        let value = stage_telemetry_row_json(
            &key,
            serde_json::json!({"packetsIn": 1}),
            Some(serde_json::json!({"drops": 2})),
            Some(true),
            Some(serde_json::json!({"payloadBytes": 256})),
        );

        assert_eq!(value["stageKey"], "telemetry-pipe:video:720p");
        assert_eq!(value["pipelineId"], "telemetry-pipe");
        assert_eq!(value["kind"], "video:720p");
        assert_eq!(value["metrics"]["packetsIn"], 1);
        assert_eq!(value["pipeMetrics"]["drops"], 2);
        assert_eq!(value["active"], true);
        assert_eq!(value["payloadStats"]["payloadBytes"], 256);
    }

    #[test]
    fn engine_telemetry_json_wraps_memory_accounting() {
        let value = engine_telemetry_json(
            "2026-06-30T12:00:00Z".to_string(),
            vec![serde_json::json!({"pipelineId": "pipeline-a"})],
            vec![serde_json::json!({"stageKey": "pipeline-a:source"})],
            vec![serde_json::json!({"outputId": "egress-a"})],
            2,
            memory_accounting_json(
                4096,
                vec![serde_json::json!({"pipelineId": "pipeline-a"})],
                vec![serde_json::json!({"stageKey": "pipeline-a:video:720p"})],
                vec![serde_json::json!({"stageKey": "pipeline-a_ts"})],
                128,
                1024,
                vec![serde_json::json!({"stageKey": "pipeline-a:source"})],
                vec![serde_json::json!({"outputId": "egress-a"})],
            ),
        );

        assert_eq!(value["generatedAt"], "2026-06-30T12:00:00Z");
        assert_eq!(value["activeTranscoderBuffers"], 2);
        assert_eq!(value["memoryAccounting"]["retainedPayloadBytes"], 4096);
        assert_eq!(
            value["memoryAccounting"]["avioQueues"]["totalCapacityBytes"],
            1024
        );
        assert_eq!(
            value["memoryAccounting"]["tsMuxerRings"][0]["stageKey"],
            "pipeline-a_ts"
        );
    }
}
