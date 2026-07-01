//! API/runtime status adapters for live operational snapshots.
//! This file owns HTTP-facing shaping for output status and health views that
//! read current engine state plus recent outcomes, retry state, recording, and
//! HLS activity without pushing those JSON concerns back into `MediaEngine`.

use std::collections::HashMap;
use std::sync::atomic::Ordering;

use crate::api_view_models;
use crate::media::engine::MediaEngine;

pub(crate) async fn output_status(
    engine: &MediaEngine,
    output_id: &str,
) -> Option<serde_json::Value> {
    let retry = engine.egresses.retry.read().await.get(output_id).cloned();
    let recent = engine.egresses.recent.read().await.get(output_id).cloned();
    let egresses = engine.egresses.active.read().await;
    if let Some(egress) = egresses.get(output_id) {
        let mut value = api_view_models::egress_runtime_json(egress, false, true);
        api_view_models::apply_recent_egress_instability_json(&mut value, recent.as_ref());
        api_view_models::apply_egress_retry_state_json(&mut value, retry.as_ref());
        value["totalSize"] = serde_json::json!(egress.bytes_sent.load(Ordering::Relaxed));
        value["bitrateKbps"] = serde_json::json!(MediaEngine::sample_egress_bitrate_kbps(egress));
        value["startedAt"] = serde_json::Value::String(egress.started_at.clone());
        return Some(value);
    }
    drop(egresses);

    recent.as_ref().map(|outcome| {
        let mut value = api_view_models::recent_egress_runtime_json(outcome, false);
        api_view_models::apply_recent_egress_instability_json(&mut value, Some(outcome));
        api_view_models::apply_egress_retry_state_json(&mut value, retry.as_ref());
        value["totalSize"] = serde_json::json!(outcome.bytes_sent);
        value["bitrateKbps"] = serde_json::Value::Null;
        value["startedAt"] = serde_json::Value::String(outcome.started_at.clone());
        value
    })
}

pub(crate) async fn health_snapshot(
    engine: &MediaEngine,
    pipeline_ids: &[String],
    recording_enabled: &HashMap<String, bool>,
    disconnect_grace_ms: u64,
) -> serde_json::Value {
    let mut hls_snapshots = HashMap::new();
    for pipeline_id in pipeline_ids {
        hls_snapshots.insert(
            pipeline_id.clone(),
            engine.hls_dependency_snapshot(pipeline_id).await,
        );
    }

    let ingests = engine.ingests.active.read().await;
    let egresses = engine.egresses.active.read().await;
    let rec_tokens = engine.recordings.cancel_tokens.read().await;
    let recent_ingests = engine.ingests.recent.read().await;
    let recent_egresses = engine.egresses.recent.read().await;
    let retry_egresses = engine.egresses.retry.read().await;
    let pipelines = engine.ingests.pipelines.read().await;

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
            .map(api_view_models::reader_snapshot_json)
            .collect();

        let mut total_bytes_sent = 0u64;
        for (_, egress) in egresses.iter() {
            if egress.pipeline_id == *pipeline_id {
                total_bytes_sent += egress.bytes_sent.load(Ordering::Relaxed);
            }
        }

        let input_json = if let Some(ingest) = ingest_opt {
            api_view_models::active_pipeline_input_json(
                ingest,
                recent_ingests.get(pipeline_id.as_str()),
                total_bytes_sent,
                readers_count,
                reader_metrics,
            )
        } else {
            let recent = recent_ingests.get(pipeline_id.as_str());
            api_view_models::inactive_pipeline_input_json(
                recent,
                total_bytes_sent,
                readers_count,
                reader_metrics,
                disconnect_grace_ms,
            )
        };

        let mut outputs_json = serde_json::Map::new();
        for (egress_key, egress) in egresses.iter() {
            if egress.pipeline_id == *pipeline_id {
                let output_id = egress_key;
                let bytes_sent = egress.bytes_sent.load(Ordering::Relaxed);
                let bitrate_kbps = MediaEngine::sample_egress_bitrate_kbps(egress);

                let has_ingest = ingests.contains_key(pipeline_id.as_str());

                let mut output_json =
                    api_view_models::egress_runtime_json(egress, false, has_ingest);
                api_view_models::apply_recent_egress_instability_json(
                    &mut output_json,
                    recent_egresses.get(output_id),
                );
                api_view_models::apply_egress_retry_state_json(
                    &mut output_json,
                    retry_egresses.get(output_id),
                );
                output_json["totalSize"] = serde_json::json!(bytes_sent);
                output_json["bitrateKbps"] = serde_json::json!(bitrate_kbps);
                output_json["startedAt"] = serde_json::Value::String(egress.started_at.clone());
                outputs_json.insert(output_id.to_string(), output_json);
            }
        }
        for (output_id, outcome) in recent_egresses.iter() {
            if outcome.pipeline_id == *pipeline_id && !outputs_json.contains_key(output_id) {
                let mut output_json = api_view_models::recent_egress_runtime_json(outcome, false);
                api_view_models::apply_recent_egress_instability_json(
                    &mut output_json,
                    Some(outcome),
                );
                api_view_models::apply_egress_retry_state_json(
                    &mut output_json,
                    retry_egresses.get(output_id),
                );
                output_json["totalSize"] = serde_json::json!(outcome.bytes_sent);
                output_json["bitrateKbps"] = serde_json::Value::Null;
                output_json["startedAt"] = serde_json::Value::String(outcome.started_at.clone());
                outputs_json.insert(output_id.to_string(), output_json);
            }
        }

        let rec_enabled = recording_enabled.get(pipeline_id).copied().unwrap_or(false);
        let rec_active = rec_tokens
            .get(pipeline_id.as_str())
            .is_some_and(|token| !token.is_cancelled());
        let hls_snapshot = hls_snapshots
            .get(pipeline_id)
            .expect("precomputed HLS snapshot");

        pipelines_json.insert(
            pipeline_id.clone(),
            api_view_models::pipeline_health_json(
                input_json,
                outputs_json,
                rec_enabled,
                rec_active,
                api_view_models::hls_preview_json(
                    hls_snapshot.active,
                    hls_snapshot.persistent_consumers,
                    hls_snapshot.last_access_age_ms,
                    hls_snapshot.segments,
                    hls_snapshot.playlist_bytes,
                ),
            ),
        );
    }

    let rx_queue = engine
        .runtime
        .listener_stats
        .rx_queue_bytes
        .load(Ordering::Relaxed);
    let rx_max = engine
        .runtime
        .listener_stats
        .rx_queue_max_bytes
        .load(Ordering::Relaxed);
    let drops = engine.runtime.listener_stats.drops.load(Ordering::Relaxed);
    let bonding_available = engine
        .runtime
        .listener_stats
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

pub(crate) async fn health_summary_snapshot(
    engine: &MediaEngine,
    pipeline_ids: &[String],
    recording_enabled: &HashMap<String, bool>,
    disconnect_grace_ms: u64,
) -> serde_json::Value {
    let ingests = engine.ingests.active.read().await;
    let egresses = engine.egresses.active.read().await;
    let rec_tokens = engine.recordings.cancel_tokens.read().await;
    let recent_ingests = engine.ingests.recent.read().await;
    let recent_egresses = engine.egresses.recent.read().await;
    let retry_egresses = engine.egresses.retry.read().await;
    let pipelines = engine.ingests.pipelines.read().await;

    let mut pipelines_json = serde_json::Map::new();

    for pipeline_id in pipeline_ids {
        let ingest_opt = ingests.get(pipeline_id.as_str());
        let reader_count = pipelines
            .get(pipeline_id.as_str())
            .map(|rb| rb.reader_snapshots().len())
            .unwrap_or(0);

        let mut total_bytes_sent = 0u64;
        for (_, egress) in egresses.iter() {
            if egress.pipeline_id == *pipeline_id {
                total_bytes_sent += egress.bytes_sent.load(Ordering::Relaxed);
            }
        }

        let input_json = if let Some(ingest) = ingest_opt {
            api_view_models::active_pipeline_input_summary_json(
                ingest,
                total_bytes_sent,
                reader_count,
            )
        } else {
            let recent = recent_ingests.get(pipeline_id.as_str());
            api_view_models::inactive_pipeline_input_summary_json(
                recent,
                total_bytes_sent,
                reader_count,
                disconnect_grace_ms,
            )
        };

        let mut outputs_json = serde_json::Map::new();
        for (output_id, egress) in egresses.iter() {
            if egress.pipeline_id != *pipeline_id {
                continue;
            }

            let bytes_sent = egress.bytes_sent.load(Ordering::Relaxed);
            let bitrate_kbps = MediaEngine::sample_egress_bitrate_kbps(egress);
            let has_ingest = ingests.contains_key(pipeline_id.as_str());
            let status = MediaEngine::egress_effective_status(egress, has_ingest);
            let retry_state = retry_egresses.get(output_id);

            outputs_json.insert(
                output_id.to_string(),
                serde_json::json!({
                    "status": if retry_state.is_some() {
                        "retrying".to_string()
                    } else {
                        status
                    },
                    "uptimeSecs": egress.start_instant.elapsed().as_secs_f64(),
                    "totalSize": bytes_sent,
                    "bitrateKbps": bitrate_kbps,
                    "retrying": retry_state.is_some(),
                }),
            );
        }

        for (output_id, outcome) in recent_egresses.iter() {
            if outcome.pipeline_id != *pipeline_id || outputs_json.contains_key(output_id) {
                continue;
            }

            let retry_state = retry_egresses.get(output_id);
            outputs_json.insert(
                output_id.to_string(),
                serde_json::json!({
                    "status": if retry_state.is_some() {
                        "retrying".to_string()
                    } else {
                        outcome.status.clone()
                    },
                    "uptimeSecs": outcome.uptime_secs,
                    "totalSize": outcome.bytes_sent,
                    "bitrateKbps": serde_json::Value::Null,
                    "retrying": retry_state.is_some(),
                }),
            );
        }

        let rec_enabled = recording_enabled.get(pipeline_id).copied().unwrap_or(false);
        let rec_active = rec_tokens
            .get(pipeline_id.as_str())
            .is_some_and(|token| !token.is_cancelled());

        pipelines_json.insert(
            pipeline_id.clone(),
            api_view_models::pipeline_health_summary_json(
                input_json,
                outputs_json,
                rec_enabled,
                rec_active,
            ),
        );
    }

    serde_json::json!({
        "status": "ready",
        "pipelines": serde_json::Value::Object(pipelines_json),
    })
}
