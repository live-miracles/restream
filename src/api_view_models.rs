//! HTTP-facing JSON serializers and view helpers assembled from typed runtime
//! state so API handlers do not need to shape payloads inline.

use crate::domain::stage::StageKey;
use crate::media::engine::{
    ActiveEgress, ActiveIngest, EgressRetryState, MediaEngine, RecentEgressOutcome,
    RecentIngestOutcome,
};
use crate::media::ring_buffer::RingBuffer;
use crate::media::srt::parse_pipeline_srt_ingest_policy;
use crate::types::{Ingest, Job, Output, Pipeline};
use std::sync::atomic::Ordering;

pub(crate) fn pipeline_response_json(
    pipeline: &Pipeline,
    effective_ingest_host: &str,
    rtmp_port: u16,
    srt_port: u16,
) -> serde_json::Value {
    serde_json::json!({
        "id": pipeline.id,
        "name": pipeline.name,
        "streamKey": pipeline.stream_key,
        "inputSource": pipeline.input_source,
        "encoding": pipeline.encoding,
        "srtIngestPolicy": parse_pipeline_srt_ingest_policy(
            pipeline.srt_ingest_policy.as_deref()
        ),
        "ingestUrls": {
            "rtmp": format!("rtmp://{}:{}/live/{}", effective_ingest_host, rtmp_port, pipeline.stream_key),
            "srt": format!("srt://{}:{}?streamid=publish:live/{}", effective_ingest_host, srt_port, pipeline.stream_key)
        }
    })
}

pub(crate) fn pipeline_response_json_with_file_ingest(
    pipeline: &Pipeline,
    effective_ingest_host: &str,
    rtmp_port: u16,
    srt_port: u16,
    ingest: Option<Ingest>,
    running: bool,
) -> serde_json::Value {
    let mut value = pipeline_response_json(pipeline, effective_ingest_host, rtmp_port, srt_port);
    if let Some(object) = value.as_object_mut() {
        object.insert(
            "fileIngest".to_string(),
            file_ingest_response(ingest, running),
        );
    }
    value
}

pub(crate) fn file_ingest_response(ingest: Option<Ingest>, running: bool) -> serde_json::Value {
    match ingest {
        Some(ingest) => serde_json::json!({
            "configured": true,
            "id": ingest.id,
            "filename": ingest.filename,
            "streamKey": ingest.stream_key,
            "loop": ingest.loop_flag,
            "startTime": ingest.start_time,
            "liveOptimized": ingest.live_optimized,
            "targetGopSeconds": ingest.target_gop_seconds,
            "running": running
        }),
        None => serde_json::json!({
            "configured": false,
            "running": false
        }),
    }
}

pub(crate) fn output_response_json(output: &Output) -> serde_json::Value {
    serde_json::json!({
        "id": output.id,
        "pipelineId": output.pipeline_id,
        "name": output.name,
        "url": output.url,
        "monitoringUrl": output.monitoring_url,
        "desiredState": output.desired_state,
        "encoding": output.encoding,
    })
}

pub(crate) fn output_response_json_list(outputs: &[Output]) -> Vec<serde_json::Value> {
    outputs.iter().map(output_response_json).collect()
}

pub(crate) fn job_response_json(job: &Job) -> serde_json::Value {
    serde_json::json!({
        "id": job.id,
        "pipelineId": job.pipeline_id,
        "outputId": job.output_id,
        "pid": job.pid,
        "status": job.status,
        "startedAt": job.started_at,
        "endedAt": job.ended_at,
        "exitCode": job.exit_code,
        "exitSignal": job.exit_signal,
    })
}

pub(crate) fn job_response_json_list(jobs: &[Job]) -> Vec<serde_json::Value> {
    jobs.iter().map(job_response_json).collect()
}

pub(crate) fn latest_job_response_json_list(jobs: &[Job]) -> Vec<serde_json::Value> {
    let mut latest_by_output: std::collections::HashSet<(&str, &str)> =
        std::collections::HashSet::new();
    let mut latest_jobs = Vec::new();

    for job in jobs {
        let key = (job.pipeline_id.as_str(), job.output_id.as_str());
        if latest_by_output.insert(key) {
            latest_jobs.push(job_response_json(job));
        }
    }

    latest_jobs
}

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
        "recentFailureCount": 0,
        "flapping": false,
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
        "recentFailureCount": 0,
        "flapping": false,
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

pub(crate) fn apply_recent_egress_instability_json(
    value: &mut serde_json::Value,
    recent: Option<&RecentEgressOutcome>,
) {
    let (recent_failure_count, flapping) = MediaEngine::recent_egress_flap_state(recent);
    value["recentFailureCount"] = serde_json::json!(recent_failure_count);
    value["flapping"] = serde_json::Value::Bool(flapping);
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

    let video_track_selection = ingest_video_track_selection_json(ingest);

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
        "videoTrackSelection": video_track_selection,
        "audioTracks": audio_tracks,
        "gop": gop,
    })
}

fn ingest_video_track_selection_json(ingest: &ActiveIngest) -> serde_json::Value {
    if ingest.video_track_count == 0 {
        return serde_json::Value::Null;
    }

    serde_json::json!({
        "mode": "firstVideoOnly",
        "selectedTrackIndex": ingest.selected_video_track_index,
        "availableTrackCount": ingest.video_track_count,
        "ignoredTrackCount": ingest.video_track_count.saturating_sub(1),
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
    recent: Option<&RecentIngestOutcome>,
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
    let (recent_disconnect_count, flapping) = MediaEngine::recent_ingest_flap_state(recent);
    let video_track_selection = ingest_video_track_selection_json(ingest);

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
        "videoTrackSelection": video_track_selection,
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
        "recentDisconnectCount": recent_disconnect_count,
        "flapping": flapping,
        "disconnectGraceActive": false,
        "disconnectGraceRemainingMs": null,
        "lastRemoteAddr": null,
        "lastSessionBytesReceived": null
    })
}

pub(crate) fn active_pipeline_input_summary_json(
    ingest: &ActiveIngest,
    total_bytes_sent: u64,
    readers_count: usize,
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
    let audio_tracks = ingest
        .audio_tracks
        .lock()
        .unwrap_or_else(|e| e.into_inner());
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
        "bitrateKbps": bitrate_kbps,
        "publisher": {
            "protocol": ingest.protocol,
            "remoteAddr": ingest.remote_addr,
        },
        "disconnectGraceActive": false,
        "disconnectGraceRemainingMs": null,
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
    let (recent_disconnect_count, flapping) = MediaEngine::recent_ingest_flap_state(recent);
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
        "recentDisconnectCount": recent_disconnect_count,
        "flapping": flapping,
        "disconnectGraceActive": disconnect_grace_remaining_ms.is_some(),
        "disconnectGraceRemainingMs": disconnect_grace_remaining_ms,
        "lastRemoteAddr": recent.and_then(|recent| recent.remote_addr.clone()),
        "lastSessionBytesReceived": recent.map(|recent| recent.bytes_received)
    })
}

pub(crate) fn inactive_pipeline_input_summary_json(
    recent: Option<&RecentIngestOutcome>,
    total_bytes_sent: u64,
    readers_count: usize,
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
        "bitrateKbps": serde_json::Value::Null,
        "publisher": serde_json::Value::Null,
        "disconnectGraceActive": disconnect_grace_remaining_ms.is_some(),
        "disconnectGraceRemainingMs": disconnect_grace_remaining_ms,
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

pub(crate) fn pipeline_health_summary_json(
    input: serde_json::Value,
    outputs: serde_json::Map<String, serde_json::Value>,
    recording_enabled: bool,
    recording_active: bool,
) -> serde_json::Value {
    serde_json::json!({
        "input": input,
        "outputs": serde_json::Value::Object(outputs),
        "recording": { "enabled": recording_enabled, "active": recording_active },
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

pub(crate) fn processing_graph_node(
    id: impl Into<String>,
    node_type: &'static str,
    label: impl Into<String>,
    active: bool,
    details: Option<serde_json::Value>,
    metrics: Option<serde_json::Value>,
) -> serde_json::Value {
    serde_json::json!({
        "id": id.into(),
        "type": node_type,
        "label": label.into(),
        "active": active,
        "details": details,
        "metrics": metrics,
    })
}

pub(crate) fn processing_graph_edge(
    from: impl Into<String>,
    to: impl Into<String>,
    label: impl Into<String>,
) -> serde_json::Value {
    serde_json::json!({
        "from": from.into(),
        "to": to.into(),
        "label": label.into(),
    })
}

pub(crate) fn processing_graph_ingest_details(ingest: &ActiveIngest) -> serde_json::Value {
    let elapsed_secs = ingest.start_time.elapsed().as_secs_f64();
    let bytes_received = ingest.bytes_received.load(Ordering::Relaxed);
    let bitrate_kbps = if elapsed_secs > 1.0 {
        Some(((bytes_received as f64 * 8.0) / (elapsed_secs * 1000.0) * 10.0).round() / 10.0)
    } else {
        None
    };

    serde_json::json!({
        "protocol": ingest.protocol,
        "remoteAddr": ingest.remote_addr,
        "video": ingest.video,
        "videoTrackSelection": ingest_video_track_selection_json(ingest),
        "audio": ingest.audio,
        "bytesReceived": bytes_received,
        "bitrateKbps": bitrate_kbps,
    })
}

pub(crate) fn processing_graph_demux_details(ingest: &ActiveIngest) -> serde_json::Value {
    serde_json::json!({
        "protocol": ingest.protocol,
        "video": ingest.video,
        "videoTrackSelection": ingest_video_track_selection_json(ingest),
        "audio": ingest.audio,
        "audioTracks": ingest
            .audio_tracks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect::<Vec<_>>(),
    })
}

pub(crate) fn processing_graph_source_ring_details(
    fill: usize,
    capacity: usize,
    payload_stats: serde_json::Value,
    format: impl Into<String>,
    readers: Vec<serde_json::Value>,
) -> serde_json::Value {
    serde_json::json!({
        "fill": fill,
        "capacity": capacity,
        "fillPercent": (fill * 100).checked_div(capacity).unwrap_or(0),
        "payloadStats": payload_stats,
        "format": format.into(),
        "readers": readers,
    })
}

pub(crate) fn processing_graph_stage_node(
    id: impl Into<String>,
    node_type: &'static str,
    label: impl Into<String>,
    stage_key: impl Into<String>,
    active: bool,
    metrics: Option<serde_json::Value>,
    queue_metrics: Option<serde_json::Value>,
    pipe_metrics: Option<serde_json::Value>,
    payload_stats: serde_json::Value,
) -> serde_json::Value {
    serde_json::json!({
        "id": id.into(),
        "type": node_type,
        "label": label.into(),
        "stageKey": stage_key.into(),
        "active": active,
        "metrics": metrics,
        "queueMetrics": queue_metrics,
        "pipeMetrics": pipe_metrics,
        "payloadStats": payload_stats,
    })
}

pub(crate) fn processing_graph_egress_details(
    egress: &ActiveEgress,
    has_ingest: bool,
) -> serde_json::Value {
    let bytes = egress.bytes_sent.load(Ordering::Relaxed);
    let mut details = egress_runtime_json(egress, true, has_ingest);
    details["totalSize"] = serde_json::json!(bytes);
    details["bitrateKbps"] = serde_json::json!(
        *egress
            .bitrate_kbps
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    );
    details["startedAt"] = serde_json::Value::String(egress.started_at.clone());
    details
}

pub(crate) fn processing_graph_packetizer_details(
    protocol: &'static str,
    encoding: &str,
    stage_key: String,
    payload_stats: Option<serde_json::Value>,
) -> serde_json::Value {
    serde_json::json!({
        "protocol": protocol,
        "encoding": encoding,
        "stageKey": stage_key,
        "payloadStats": payload_stats,
    })
}

pub(crate) fn processing_graph_json(
    generated_at: String,
    pipeline_id: &str,
    nodes: Vec<serde_json::Value>,
    edges: Vec<serde_json::Value>,
) -> serde_json::Value {
    serde_json::json!({
        "generatedAt": generated_at,
        "pipelineId": pipeline_id,
        "nodes": nodes,
        "edges": edges,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::srt_ingest::{SrtPipelineIngestConfig, SrtPipelineIngestMode};
    use crate::domain::stage::StageKind;
    use crate::media::engine::RecentIngestOutcome;
    use crate::media::srt::serialize_pipeline_srt_ingest_policy;
    use crate::media::stage_metrics::StageMetrics;
    use crate::types::Job;

    #[test]
    fn pipeline_response_helpers_preserve_pipeline_and_file_ingest_shape() {
        let pipeline = Pipeline {
            id: "pipeline-1".to_string(),
            name: "Primary".to_string(),
            stream_key: "stream-key".to_string(),
            input_source: Some("file:clip.mp4".to_string()),
            encoding: Some("copy".to_string()),
            srt_ingest_policy: Some(
                serialize_pipeline_srt_ingest_policy(&SrtPipelineIngestConfig {
                    mode: SrtPipelineIngestMode::Encrypted,
                    passphrase: Some("secret-pass".to_string()),
                    pbkeylen: Some(24),
                })
                .unwrap(),
            ),
        };
        let ingest = Ingest {
            id: "ingest-1".to_string(),
            filename: "clip.mp4".to_string(),
            stream_key: pipeline.stream_key.clone(),
            loop_flag: true,
            start_time: "00:00:03".to_string(),
            live_optimized: true,
            target_gop_seconds: 3,
        };

        let pipeline_json = pipeline_response_json(&pipeline, "ingest.example", 1935, 10080);
        let pipeline_with_ingest = pipeline_response_json_with_file_ingest(
            &pipeline,
            "ingest.example",
            1935,
            10080,
            Some(ingest.clone()),
            true,
        );
        let ingest_json = file_ingest_response(Some(ingest), true);
        let missing_ingest_json = file_ingest_response(None, false);

        assert_eq!(pipeline_json["id"], "pipeline-1");
        assert_eq!(
            pipeline_json["ingestUrls"]["rtmp"],
            "rtmp://ingest.example:1935/live/stream-key"
        );
        assert_eq!(
            pipeline_json["ingestUrls"]["srt"],
            "srt://ingest.example:10080?streamid=publish:live/stream-key"
        );
        assert_eq!(pipeline_json["srtIngestPolicy"]["mode"], "encrypted");
        assert_eq!(pipeline_json["srtIngestPolicy"]["pbkeylen"], 24);
        assert_eq!(pipeline_with_ingest["fileIngest"]["configured"], true);
        assert_eq!(pipeline_with_ingest["fileIngest"]["filename"], "clip.mp4");
        assert_eq!(pipeline_with_ingest["fileIngest"]["running"], true);
        assert_eq!(ingest_json["configured"], true);
        assert_eq!(ingest_json["filename"], "clip.mp4");
        assert_eq!(ingest_json["loop"], true);
        assert_eq!(ingest_json["targetGopSeconds"], 3);
        assert_eq!(ingest_json["running"], true);
        assert_eq!(missing_ingest_json["configured"], false);
        assert_eq!(missing_ingest_json["running"], false);
    }

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
    fn apply_recent_egress_instability_surfaces_flapping_window() {
        let mut value = serde_json::json!({
            "status": "running",
            "recentFailureCount": 0,
            "flapping": false,
        });
        let recent = RecentEgressOutcome {
            output_id: "out-1".to_string(),
            pipeline_id: "pipe-1".to_string(),
            protocol: "rtmp".to_string(),
            target_url: "rtmp://example/live/key".to_string(),
            target_addr: None,
            status: "failed".to_string(),
            raw_status: "running".to_string(),
            phase: "failed".to_string(),
            started_at: chrono::Utc::now().to_rfc3339(),
            uptime_secs: 1.5,
            bytes_sent: 2048,
            last_progress_ms: 0,
            last_error: Some("connection reset by peer".to_string()),
            last_error_ms: MediaEngine::now_epoch_ms(),
            failure_phase: Some("send".to_string()),
            first_failure_at_ms: MediaEngine::now_epoch_ms() - 2_000,
            failure_count: 2,
            quality: Default::default(),
            metrics: serde_json::json!({}),
            ended_at_ms: MediaEngine::now_epoch_ms() - 1_000,
        };

        apply_recent_egress_instability_json(&mut value, Some(&recent));

        assert_eq!(value["recentFailureCount"], 2);
        assert_eq!(value["flapping"], true);
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
            first_disconnect_at_ms: MediaEngine::now_epoch_ms() - 2_000,
            disconnect_count: 1,
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
        assert_eq!(value["recentDisconnectCount"], 1);
        assert_eq!(value["flapping"], false);
        assert_eq!(value["lastSessionProtocol"], "srt");
    }

    #[test]
    fn active_pipeline_input_surfaces_recent_flapping_without_old_disconnect_fields() {
        let ingest = ActiveIngest {
            attempt_id: 1,
            stream_key: "stream".to_string(),
            start_time: std::time::Instant::now(),
            protocol: "rtmp".to_string(),
            bytes_received: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            metrics: std::sync::Arc::new(StageMetrics::new()),
            remote_addr: Some("127.0.0.1:1935".to_string()),
            video: None,
            selected_video_track_index: None,
            video_track_count: 0,
            audio: None,
            audio_tracks: std::sync::Mutex::new(std::sync::Arc::new(Vec::new())),
            quality: crate::media::engine::PublisherQuality::default(),
            keyframe_times: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            video_sequence_header: std::sync::Mutex::new(None),
            audio_sequence_header: std::sync::Mutex::new(None),
        };
        let recent = RecentIngestOutcome {
            protocol: "rtmp".to_string(),
            disconnected_at_ms: MediaEngine::now_epoch_ms(),
            first_disconnect_at_ms: MediaEngine::now_epoch_ms() - 3_000,
            disconnect_count: 2,
            reason: Some("publisher disconnected".to_string()),
            failure_phase: Some("disconnect".to_string()),
            had_error: false,
            remote_addr: Some("127.0.0.1:1935".to_string()),
            bytes_received: 2048,
        };

        let value = active_pipeline_input_json(&ingest, Some(&recent), 0, 0, Vec::new());

        assert_eq!(value["status"], "on");
        assert_eq!(value["recentDisconnectCount"], 2);
        assert_eq!(value["flapping"], true);
        assert!(value["lastSessionProtocol"].is_null());
        assert!(value["lastDisconnectReason"].is_null());
        assert!(value["lastFailurePhase"].is_null());
        assert!(value["lastDisconnectAt"].is_null());
    }

    #[test]
    fn active_pipeline_input_surfaces_single_video_selection_policy() {
        let ingest = ActiveIngest {
            attempt_id: 1,
            stream_key: "stream".to_string(),
            start_time: std::time::Instant::now(),
            protocol: "srt".to_string(),
            bytes_received: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            metrics: std::sync::Arc::new(StageMetrics::new()),
            remote_addr: Some("127.0.0.1:9000".to_string()),
            video: Some(crate::media::engine::VideoMeta {
                codec: "h264".to_string(),
                ..Default::default()
            }),
            selected_video_track_index: Some(0),
            video_track_count: 2,
            audio: None,
            audio_tracks: std::sync::Mutex::new(std::sync::Arc::new(Vec::new())),
            quality: crate::media::engine::PublisherQuality::default(),
            keyframe_times: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            video_sequence_header: std::sync::Mutex::new(None),
            audio_sequence_header: std::sync::Mutex::new(None),
        };

        let value = active_pipeline_input_json(&ingest, None, 0, 0, Vec::new());

        assert_eq!(value["videoTrackSelection"]["mode"], "firstVideoOnly");
        assert_eq!(value["videoTrackSelection"]["selectedTrackIndex"], 0);
        assert_eq!(value["videoTrackSelection"]["availableTrackCount"], 2);
        assert_eq!(value["videoTrackSelection"]["ignoredTrackCount"], 1);
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

    #[test]
    fn processing_graph_helpers_wrap_node_edge_and_root_shape() {
        let node = processing_graph_node(
            "pipe_ingest",
            "ingest",
            "RTMP ingest",
            true,
            Some(serde_json::json!({"protocol": "rtmp"})),
            Some(serde_json::json!({"packetsIn": 1})),
        );
        let edge = processing_graph_edge("pipe_ingest", "pipe_demux", "RTMP");
        let graph = processing_graph_json(
            "2026-06-30T12:00:00Z".to_string(),
            "pipe",
            vec![node.clone()],
            vec![edge.clone()],
        );

        assert_eq!(node["type"], "ingest");
        assert_eq!(node["details"]["protocol"], "rtmp");
        assert_eq!(edge["label"], "RTMP");
        assert_eq!(graph["pipelineId"], "pipe");
        assert_eq!(graph["nodes"][0]["id"], "pipe_ingest");
        assert_eq!(graph["edges"][0]["to"], "pipe_demux");
    }

    #[test]
    fn processing_graph_source_ring_details_reports_fill_percent() {
        let details = processing_graph_source_ring_details(
            2,
            8,
            serde_json::json!({"payloadBytes": 512}),
            "mpegts".to_string(),
            vec![serde_json::json!({"name": "preview"})],
        );

        assert_eq!(details["fill"], 2);
        assert_eq!(details["capacity"], 8);
        assert_eq!(details["fillPercent"], 25);
        assert_eq!(details["payloadStats"]["payloadBytes"], 512);
        assert_eq!(details["readers"][0]["name"], "preview");
    }

    #[test]
    fn latest_job_response_json_list_keeps_only_newest_job_per_output() {
        let jobs = vec![
            Job {
                id: "job-newest".to_string(),
                pipeline_id: "pipe-1".to_string(),
                output_id: "out-1".to_string(),
                pid: Some(200),
                status: "running".to_string(),
                started_at: "2026-06-30T12:00:00Z".to_string(),
                ended_at: None,
                exit_code: None,
                exit_signal: None,
            },
            Job {
                id: "job-older".to_string(),
                pipeline_id: "pipe-1".to_string(),
                output_id: "out-1".to_string(),
                pid: Some(100),
                status: "stopped".to_string(),
                started_at: "2026-06-30T11:00:00Z".to_string(),
                ended_at: Some("2026-06-30T11:30:00Z".to_string()),
                exit_code: Some(0),
                exit_signal: None,
            },
            Job {
                id: "job-other-output".to_string(),
                pipeline_id: "pipe-1".to_string(),
                output_id: "out-2".to_string(),
                pid: Some(300),
                status: "failed".to_string(),
                started_at: "2026-06-30T10:00:00Z".to_string(),
                ended_at: Some("2026-06-30T10:10:00Z".to_string()),
                exit_code: Some(1),
                exit_signal: None,
            },
        ];

        let response = latest_job_response_json_list(&jobs);

        assert_eq!(response.len(), 2);
        assert_eq!(response[0]["id"], "job-newest");
        assert_eq!(response[1]["id"], "job-other-output");
    }
}
