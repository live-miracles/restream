use std::collections::HashMap;
use std::sync::atomic::Ordering;

use crate::api_view_models;
use crate::application::output_path::OutputPath;
use crate::domain::stage::{StageKey, StageKind};
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
        } else {
            let recent = recent_ingests.get(pipeline_id.as_str());
            let last_disconnect_age_ms = recent.map(|recent| {
                MediaEngine::now_epoch_ms().saturating_sub(recent.disconnected_at_ms)
            });
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
        };

        let mut outputs_json = serde_json::Map::new();
        for (egress_key, egress) in egresses.iter() {
            if egress.pipeline_id == *pipeline_id {
                let output_id = egress_key;
                let bytes_sent = egress.bytes_sent.load(Ordering::Relaxed);
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
                        *prev_time = std::time::Instant::now();
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
            serde_json::json!({
                "input": input_json,
                "outputs": serde_json::Value::Object(outputs_json),
                "recording": { "enabled": rec_enabled, "active": rec_active },
                "hlsPreview": {
                    "active": hls_active,
                    "persistentConsumers": hls_persistent_consumers,
                    "lastAccessAgeMs": hls_last_access_age_ms,
                    "segments": hls_segments,
                    "playlistBytes": hls_playlist_bytes,
                }
            }),
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

pub(crate) async fn engine_telemetry(engine: &MediaEngine) -> serde_json::Value {
    let generated_at = chrono::Utc::now().to_rfc3339();
    let ingests = engine.ingests.active.read().await;
    let egresses = engine.egresses.active.read().await;
    let stage_metrics = engine.stages.metrics.read().await;
    let pipe_metrics = engine.stages.pipe_metrics.read().await;
    let buffers = engine.stages.buffers.read().await;
    let pipelines = engine.ingests.pipelines.read().await;
    let ts_muxers = engine.stages.ts_muxers.read().await;
    let input_queues = engine.stages.input_queues.read().await;
    let egress_queues = engine.egresses.queues.read().await;

    let ingest_arr: Vec<serde_json::Value> = ingests
        .iter()
        .map(|(pid, ingest)| {
            serde_json::json!({
                "pipelineId": pid,
                "protocol": ingest.protocol,
                "uptimeSecs": ingest.start_time.elapsed().as_secs_f64(),
                "bytesReceived": ingest.bytes_received.load(Ordering::Relaxed),
                "metrics": ingest.metrics.snapshot(),
            })
        })
        .collect();

    let stage_arr: Vec<serde_json::Value> = stage_metrics
        .iter()
        .map(|(key, metrics)| {
            let mut val = serde_json::json!({
                "stageKey": key.to_string(),
                "pipelineId": key.pipeline.as_str(),
                "kind": key.kind.to_string(),
                "metrics": metrics.snapshot(),
            });
            if let Some(pm) = pipe_metrics.get(key) {
                val["pipeMetrics"] = pm.snapshot();
            }
            val
        })
        .collect();

    let egress_arr: Vec<serde_json::Value> = egresses
        .values()
        .map(|egress| {
            api_view_models::egress_runtime_json(
                egress,
                true,
                ingests.contains_key(egress.pipeline_id.as_str()),
            )
        })
        .collect();

    let source_rings: Vec<serde_json::Value> = pipelines
        .iter()
        .map(|(pipeline_id, ring)| {
            serde_json::json!({
                "pipelineId": pipeline_id,
                "payloadStats": api_view_models::ring_payload_stats_json(ring),
            })
        })
        .collect();
    let transcoder_rings: Vec<serde_json::Value> = buffers
        .iter()
        .map(|(key, (ring, token))| {
            serde_json::json!({
                "stageKey": key.to_string(),
                "pipelineId": key.pipeline.as_str(),
                "kind": key.kind.to_string(),
                "active": !token.is_cancelled(),
                "payloadStats": api_view_models::ring_payload_stats_json(ring),
            })
        })
        .collect();
    let ts_muxer_rings: Vec<serde_json::Value> = ts_muxers
        .iter()
        .map(|(stage_key, stage)| {
            serde_json::json!({
                "stageKey": stage_key,
                "active": !stage.cancel.is_cancelled(),
                "payloadStats": api_view_models::ring_payload_stats_json(&stage.ring),
            })
        })
        .collect();
    let retained_payload_bytes = source_rings
        .iter()
        .chain(transcoder_rings.iter())
        .chain(ts_muxer_rings.iter())
        .filter_map(|entry| entry["payloadStats"]["payloadBytes"].as_u64())
        .sum::<u64>();

    let avio_input_queues: Vec<serde_json::Value> = input_queues
        .iter()
        .map(|(key, queue)| {
            let stats = queue.stats();
            serde_json::json!({
                "stageKey": key.to_string(),
                "pipelineId": key.pipeline.as_str(),
                "lenBytes": stats.len,
                "capacityBytes": stats.capacity,
                "highWaterBytes": stats.high_water_bytes,
                "blockedWrites": stats.blocked_writes,
                "blockedWriteUs": stats.blocked_write_us,
            })
        })
        .collect();
    let avio_egress_queues: Vec<serde_json::Value> = egress_queues
        .iter()
        .map(|(output_id, queue)| {
            let stats = queue.stats();
            serde_json::json!({
                "outputId": output_id,
                "lenBytes": stats.len,
                "capacityBytes": stats.capacity,
                "highWaterBytes": stats.high_water_bytes,
                "blockedWrites": stats.blocked_writes,
                "blockedWriteUs": stats.blocked_write_us,
            })
        })
        .collect();
    let avio_total_len_bytes: usize = avio_input_queues
        .iter()
        .chain(avio_egress_queues.iter())
        .filter_map(|entry| entry["lenBytes"].as_u64())
        .map(|value| value as usize)
        .sum();
    let avio_total_capacity_bytes: usize = avio_input_queues
        .iter()
        .chain(avio_egress_queues.iter())
        .filter_map(|entry| entry["capacityBytes"].as_u64())
        .map(|value| value as usize)
        .sum();

    serde_json::json!({
        "generatedAt": generated_at,
        "ingests": ingest_arr,
        "stages": stage_arr,
        "egresses": egress_arr,
        "activeTranscoderBuffers": buffers.len(),
        "memoryAccounting": {
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
        },
    })
}

pub(crate) async fn pipeline_telemetry(
    engine: &MediaEngine,
    pipeline_id: &str,
) -> serde_json::Value {
    let generated_at = chrono::Utc::now().to_rfc3339();
    let ingests = engine.ingests.active.read().await;
    let egresses = engine.egresses.active.read().await;
    let all_stage_metrics = engine.stages.metrics.read().await;
    let all_pipe_metrics = engine.stages.pipe_metrics.read().await;
    let pipelines = engine.ingests.pipelines.read().await;
    let buffers = engine.stages.buffers.read().await;

    let ingest = ingests.get(pipeline_id).map(|ingest| {
        serde_json::json!({
            "protocol": ingest.protocol,
            "streamKey": ingest.stream_key,
            "uptimeSecs": ingest.start_time.elapsed().as_secs_f64(),
            "bytesReceived": ingest.bytes_received.load(Ordering::Relaxed),
            "video": ingest.video,
            "audio": ingest.audio,
            "metrics": ingest.metrics.snapshot(),
        })
    });

    let ring_info = pipelines.get(pipeline_id).map(|ring| {
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
            "payloadStats": api_view_models::ring_payload_stats_json(ring),
            "readers": readers,
        })
    });

    let stages: Vec<serde_json::Value> = all_stage_metrics
        .iter()
        .filter(|(key, _)| key.pipeline.as_str() == pipeline_id)
        .map(|(key, metrics)| {
            let mut val = serde_json::json!({
                "kind": key.kind.to_string(),
                "metrics": metrics.snapshot(),
            });
            if let Some(pm) = all_pipe_metrics.get(key) {
                val["pipeMetrics"] = pm.snapshot();
            }
            if let Some((ring, token)) = buffers.get(key) {
                val["active"] = serde_json::json!(!token.is_cancelled());
                val["payloadStats"] = api_view_models::ring_payload_stats_json(ring);
            }
            val
        })
        .collect();

    let pipeline_egresses: Vec<serde_json::Value> = egresses
        .values()
        .filter(|egress| egress.pipeline_id == pipeline_id)
        .map(|egress| {
            api_view_models::egress_runtime_json(egress, true, ingests.contains_key(pipeline_id))
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

pub(crate) async fn stage_telemetry(
    engine: &MediaEngine,
    key: &StageKey,
) -> Option<serde_json::Value> {
    let all_stage_metrics = engine.stages.metrics.read().await;
    let metrics = all_stage_metrics.get(key)?;

    let all_pipe_metrics = engine.stages.pipe_metrics.read().await;
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

pub(crate) async fn stage_telemetry_by_display(
    engine: &MediaEngine,
    display: &str,
) -> Option<serde_json::Value> {
    let all_stage_metrics = engine.stages.metrics.read().await;
    let key = all_stage_metrics
        .keys()
        .find(|key| key.to_string() == display)?;
    let metrics = all_stage_metrics.get(key)?;

    let all_pipe_metrics = engine.stages.pipe_metrics.read().await;
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

pub(crate) async fn processing_graph(
    engine: &MediaEngine,
    pipeline_id: &str,
    outputs: &[crate::types::Output],
) -> serde_json::Value {
    let ingests = engine.ingests.active.read().await;
    let egresses = engine.egresses.active.read().await;
    let pipelines = engine.ingests.pipelines.read().await;
    let transcoder_buffers = engine.stages.buffers.read().await;
    let rec_tokens = engine.recordings.cancel_tokens.read().await;
    let hls_stores = engine.hls.stores.read().await;
    let hls_consumers = engine.hls.consumers.read().await;
    let all_stage_metrics = engine.stages.metrics.read().await;
    let all_input_queues = engine.stages.input_queues.read().await;
    let all_pipe_metrics = engine.stages.pipe_metrics.read().await;
    let ts_muxers = engine.stages.ts_muxers.read().await;

    let mut nodes = Vec::new();
    let mut edges = Vec::new();

    let ingest = ingests.get(pipeline_id);
    let ingest_node_id = format!("{pipeline_id}_ingest");
    let ingest_protocol = ingest.map(|ingest| ingest.protocol.as_str());
    nodes.push(serde_json::json!({
        "id": ingest_node_id,
        "type": "ingest",
        "label": if let Some(ingest) = ingest {
            format!("{} ingest", ingest.protocol.to_uppercase())
        } else {
            "No ingest".to_string()
        },
        "active": ingest.is_some(),
        "details": ingest.map(|ingest| {
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
                "audio": ingest.audio,
                "bytesReceived": bytes_received,
                "bitrateKbps": bitrate_kbps,
            })
        }),
        "metrics": ingest.map(|ingest| ingest.metrics.snapshot()),
    }));

    let demux_node_id = format!("{pipeline_id}_ingest_demux");
    nodes.push(serde_json::json!({
        "id": demux_node_id,
        "type": "demux",
        "label": ingest
            .map(|ingest| format!("{} demux/probe", MediaEngine::graph_protocol_label(&ingest.protocol)))
            .unwrap_or_else(|| "Demux/probe idle".to_string()),
        "active": ingest.is_some(),
        "details": ingest.map(|ingest| serde_json::json!({
            "protocol": ingest.protocol,
            "video": ingest.video,
            "audio": ingest.audio,
            "audioTracks": ingest.audio_tracks.lock().unwrap_or_else(|e| e.into_inner()).iter().cloned().collect::<Vec<_>>(),
        })),
        "metrics": ingest.map(|ingest| ingest.metrics.snapshot()),
    }));

    let rb_node_id = format!("{pipeline_id}_source_rb");
    let rb_info = pipelines.get(pipeline_id).map(|ring| {
        let (fill, cap) = ring.fill_and_capacity();
        let reader_stats: Vec<serde_json::Value> = ring
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
        (
            fill,
            cap,
            api_view_models::ring_payload_stats_json(ring),
            reader_stats,
        )
    });
    nodes.push(serde_json::json!({
        "id": rb_node_id,
        "type": "ring_buffer",
        "label": "Source Buffer",
        "active": rb_info.is_some(),
        "details": rb_info.map(|(fill, cap, payload_stats, readers)| serde_json::json!({
            "fill": fill,
            "capacity": cap,
            "fillPercent": (fill * 100).checked_div(cap).unwrap_or(0),
            "payloadStats": payload_stats,
            "format": MediaEngine::source_buffer_format(ingest_protocol),
            "readers": readers,
        })),
    }));
    edges.push(serde_json::json!({
        "from": ingest_node_id,
        "to": demux_node_id,
        "label": ingest_protocol
            .map(MediaEngine::graph_protocol_label)
            .unwrap_or_else(|| "input".to_string()),
    }));
    edges.push(serde_json::json!({
        "from": demux_node_id,
        "to": rb_node_id,
        "label": "push(MediaPacket)",
    }));

    for (key, (stage_ring, token)) in transcoder_buffers.iter() {
        if key.pipeline.as_str() == pipeline_id {
            let kind = &key.kind;
            let stage_key_str = kind.to_string();
            let stage_id = kind.graph_node_id(pipeline_id);
            let queue_stats = all_input_queues.get(key).map(|queue| queue.stats());
            let pipe_stats = all_pipe_metrics.get(key).map(|pipe| pipe.snapshot());
            nodes.push(serde_json::json!({
                "id": stage_id,
                "type": kind.graph_type(),
                "label": kind.graph_label(),
                "stageKey": stage_key_str,
                "active": !token.is_cancelled(),
                "metrics": all_stage_metrics.get(key).map(|metrics| metrics.snapshot()),
                "queueMetrics": queue_stats,
                "pipeMetrics": pipe_stats,
                "payloadStats": api_view_models::ring_payload_stats_json(stage_ring),
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
                    "label": format!("decode → {preset} encode"),
                }));
            }
        }
    }

    let pipeline_outputs: Vec<_> = outputs
        .iter()
        .filter(|output| output.pipeline_id == pipeline_id)
        .collect();
    let ingest_is_hevc = ingest
        .and_then(|ingest| ingest.video.as_ref())
        .map(|video| video.codec == "hevc" || video.codec == "h265")
        .unwrap_or(false);
    let mut added_packetizers = std::collections::HashSet::new();

    for output in &pipeline_outputs {
        let egress = egresses.get(&output.id);
        let output_node_id = format!("{pipeline_id}_output_{}", output.id);

        let protocol = MediaEngine::egress_protocol_from_url(&output.url);
        let protocol_label = MediaEngine::graph_protocol_label(protocol);

        nodes.push(serde_json::json!({
            "id": output_node_id,
            "type": "egress",
            "label": format!("{protocol_label} sender: {}", output.name.as_str()),
            "active": egress.is_some_and(|egress| MediaEngine::egress_effective_status(egress, ingest.is_some()) == "running"),
            "details": egress.map(|egress| {
                let bytes = egress.bytes_sent.load(Ordering::Relaxed);
                let mut details = api_view_models::egress_runtime_json(egress, true, ingest.is_some());
                details["totalSize"] = serde_json::json!(bytes);
                details["bitrateKbps"] =
                    serde_json::json!(*egress.bitrate_kbps.lock().unwrap_or_else(|e| e.into_inner()));
                details["startedAt"] = serde_json::Value::String(egress.started_at.clone());
                details
            }),
            "metrics": egress.map(|egress| egress.metrics.snapshot()),
        }));

        let output_path = OutputPath::resolve(pipeline_id, &output.encoding, &output.url);
        let terminal_kind = output_path.terminal_stage_kind(ingest_is_hevc.then_some("hevc"));
        let terminal_node_id = if matches!(terminal_kind, StageKind::Source) {
            rb_node_id.clone()
        } else {
            terminal_kind.graph_node_id(pipeline_id)
        };

        if protocol == "srt" {
            let mux_slug = MediaEngine::graph_slug(output.encoding.as_str());
            let mux_node_id = format!(
                "{pipeline_id}_ts_mux_{}",
                if mux_slug.is_empty() {
                    "source"
                } else {
                    mux_slug.as_str()
                }
            );
            let mux_key = format!("{pipeline_id}:{}", output.encoding.as_str());
            let mux_active = ts_muxers
                .get(&mux_key)
                .is_some_and(|stage| !stage.cancel.is_cancelled());
            let mux_payload_stats = ts_muxers
                .get(&mux_key)
                .map(|stage| api_view_models::ring_payload_stats_json(&stage.ring));
            if added_packetizers.insert(mux_node_id.clone()) {
                nodes.push(serde_json::json!({
                    "id": mux_node_id.clone(),
                    "type": "packetizer",
                    "label": format!("MPEG-TS mux: {}", output.encoding.as_str()),
                    "active": mux_active,
                    "details": serde_json::json!({
                        "protocol": "srt",
                        "encoding": output.encoding.as_str(),
                        "stageKey": mux_key,
                        "payloadStats": mux_payload_stats,
                    }),
                }));
                edges.push(serde_json::json!({
                    "from": terminal_node_id,
                    "to": mux_node_id.clone(),
                    "label": "media packets",
                }));
            }
            edges.push(serde_json::json!({
                "from": mux_node_id,
                "to": output_node_id,
                "label": "SRT send",
            }));
        } else {
            edges.push(serde_json::json!({
                "from": terminal_node_id,
                "to": output_node_id,
                "label": MediaEngine::source_to_egress_label(protocol),
            }));
        }
    }

    if let Some(token) = rec_tokens.get(pipeline_id) {
        let rec_id = format!("{pipeline_id}_recording");
        let rec_stage_key = StageKey::new(pipeline_id, StageKind::recording());
        nodes.push(serde_json::json!({
            "id": rec_id,
            "type": "recording",
            "label": "MKV Recording",
            "active": !token.is_cancelled(),
            "metrics": all_stage_metrics.get(&rec_stage_key).map(|metrics| metrics.snapshot()),
        }));
        edges.push(serde_json::json!({
            "from": rb_node_id,
            "to": rec_id,
            "label": "MKV mux",
        }));
    }

    if hls_stores.contains_key(pipeline_id) {
        let hls_id = format!("{pipeline_id}_hls_preview");
        let hls_stage_key = StageKey::new(pipeline_id, StageKind::hls());
        let hls_active = hls_consumers
            .get(pipeline_id)
            .is_some_and(|consumer| !consumer.cancel_token.is_cancelled());
        nodes.push(serde_json::json!({
            "id": hls_id,
            "type": "hls",
            "label": "HLS Preview",
            "active": hls_active,
            "metrics": all_stage_metrics.get(&hls_stage_key).map(|metrics| metrics.snapshot()),
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
