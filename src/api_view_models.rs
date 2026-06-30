use crate::media::engine::{
    ActiveEgress, ActiveIngest, EgressRetryState, MediaEngine, RecentEgressOutcome,
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
