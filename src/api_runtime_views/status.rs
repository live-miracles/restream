use std::collections::HashMap;
use std::sync::atomic::Ordering;

use crate::api_view_models;
use crate::media::engine::MediaEngine;

pub(crate) async fn output_status(
    engine: &MediaEngine,
    output_id: &str,
) -> Option<serde_json::Value> {
    let retry = engine.egresses.retry.read().await.get(output_id).cloned();
    let egresses = engine.egresses.active.read().await;
    if let Some(egress) = egresses.get(output_id) {
        let mut value = api_view_models::egress_runtime_json(egress, false, true);
        api_view_models::apply_egress_retry_state_json(&mut value, retry.as_ref());
        return Some(value);
    }
    drop(egresses);

    let recent = engine.egresses.recent.read().await;
    recent.get(output_id).map(|outcome| {
        let mut value = api_view_models::recent_egress_runtime_json(outcome, false);
        api_view_models::apply_egress_retry_state_json(&mut value, retry.as_ref());
        value
    })
}

pub(crate) async fn health_snapshot(
    engine: &MediaEngine,
    pipeline_ids: &[String],
    recording_enabled: &HashMap<String, bool>,
    disconnect_grace_ms: u64,
) -> serde_json::Value {
    let ingests = engine.ingests.active.read().await;
    let egresses = engine.egresses.active.read().await;
    let rec_tokens = engine.recordings.cancel_tokens.read().await;
    let hls_consumers = engine.hls.consumers.read().await;
    let hls_stores = engine.hls.stores.read().await;
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
        let hls_consumer = hls_consumers.get(pipeline_id.as_str());
        let hls_store = hls_stores.get(pipeline_id.as_str());
        let hls_active = hls_consumer.is_some_and(|consumer| !consumer.cancel_token.is_cancelled());
        let hls_persistent_consumers = hls_consumer
            .map(|consumer| consumer.persistent.load(Ordering::Relaxed))
            .unwrap_or(0);
        let hls_last_access_age_ms = hls_consumer.map(|consumer| {
            let now = consumer.reference_instant.elapsed().as_millis() as u64;
            let last = consumer.last_access_ms.load(Ordering::Relaxed);
            now.saturating_sub(last)
        });
        let hls_snapshot = hls_store.and_then(|store| store.snapshot());
        let hls_segments = hls_snapshot
            .as_ref()
            .map(|snapshot| snapshot.segments.len())
            .unwrap_or(0);
        let hls_playlist_bytes = hls_snapshot
            .as_ref()
            .map(|snapshot| snapshot.playlist.len())
            .unwrap_or(0);

        pipelines_json.insert(
            pipeline_id.clone(),
            api_view_models::pipeline_health_json(
                input_json,
                outputs_json,
                rec_enabled,
                rec_active,
                api_view_models::hls_preview_json(
                    hls_active,
                    hls_persistent_consumers,
                    hls_last_access_age_ms,
                    hls_segments,
                    hls_playlist_bytes,
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
