//! Compatibility view builders that project `MediaEngine` runtime state into
//! API-facing JSON while the engine/view split is still being tightened.

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
        .map(|(pid, ingest)| api_view_models::ingest_telemetry_json(pid, ingest))
        .collect();

    let stage_arr: Vec<serde_json::Value> = stage_metrics
        .iter()
        .map(|(key, metrics)| {
            api_view_models::stage_telemetry_row_json(
                key,
                metrics.snapshot(),
                pipe_metrics.get(key).map(|pm| pm.snapshot()),
                None,
                None,
            )
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
        .map(|(pipeline_id, ring)| api_view_models::source_ring_telemetry_json(pipeline_id, ring))
        .collect();
    let transcoder_rings: Vec<serde_json::Value> = buffers
        .iter()
        .map(|(key, (ring, token))| {
            api_view_models::transcoder_ring_telemetry_json(key, ring, !token.is_cancelled())
        })
        .collect();
    let ts_muxer_rings: Vec<serde_json::Value> = ts_muxers
        .iter()
        .map(|(stage_key, stage)| {
            api_view_models::ts_muxer_ring_telemetry_json(
                stage_key,
                &stage.ring,
                !stage.cancel.is_cancelled(),
            )
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
            api_view_models::avio_input_queue_json(
                key,
                stats.len,
                stats.capacity,
                stats.high_water_bytes,
                stats.blocked_writes,
                stats.blocked_write_us,
            )
        })
        .collect();
    let avio_egress_queues: Vec<serde_json::Value> = egress_queues
        .iter()
        .map(|(output_id, queue)| {
            let stats = queue.stats();
            api_view_models::avio_egress_queue_json(
                output_id,
                stats.len,
                stats.capacity,
                stats.high_water_bytes,
                stats.blocked_writes,
                stats.blocked_write_us,
            )
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

    api_view_models::engine_telemetry_json(
        generated_at,
        ingest_arr,
        stage_arr,
        egress_arr,
        buffers.len(),
        api_view_models::memory_accounting_json(
            retained_payload_bytes,
            source_rings,
            transcoder_rings,
            ts_muxer_rings,
            avio_total_len_bytes,
            avio_total_capacity_bytes,
            avio_input_queues,
            avio_egress_queues,
        ),
    )
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

    let ingest = ingests
        .get(pipeline_id)
        .map(api_view_models::pipeline_ingest_telemetry_json);

    let ring_info = pipelines
        .get(pipeline_id)
        .map(|ring| api_view_models::pipeline_source_ring_json(ring));

    let stages: Vec<serde_json::Value> = all_stage_metrics
        .iter()
        .filter(|(key, _)| key.pipeline.as_str() == pipeline_id)
        .map(|(key, metrics)| {
            let mut val = api_view_models::stage_telemetry_row_json(
                key,
                metrics.snapshot(),
                all_pipe_metrics.get(key).map(|pm| pm.snapshot()),
                None,
                None,
            );
            if let Some((ring, token)) = buffers.get(key) {
                val["active"] = serde_json::json!(!token.is_cancelled());
                val["payloadStats"] = api_view_models::ring_payload_stats_json(ring);
            }
            val.as_object_mut()
                .expect("stage telemetry rows are objects")
                .remove("stageKey");
            val.as_object_mut()
                .expect("stage telemetry rows are objects")
                .remove("pipelineId");
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

    api_view_models::pipeline_telemetry_json(
        generated_at,
        pipeline_id,
        ingest,
        ring_info,
        stages,
        pipeline_egresses,
    )
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

    Some(api_view_models::single_stage_telemetry_json(
        chrono::Utc::now().to_rfc3339(),
        key,
        metrics.snapshot(),
        pipe,
    ))
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
    nodes.push(api_view_models::processing_graph_node(
        ingest_node_id.clone(),
        "ingest",
        ingest
            .map(|ingest| format!("{} ingest", ingest.protocol.to_uppercase()))
            .unwrap_or_else(|| "No ingest".to_string()),
        ingest.is_some(),
        ingest.map(api_view_models::processing_graph_ingest_details),
        ingest.map(|ingest| ingest.metrics.snapshot()),
    ));

    let demux_node_id = format!("{pipeline_id}_ingest_demux");
    nodes.push(api_view_models::processing_graph_node(
        demux_node_id.clone(),
        "demux",
        ingest
            .map(|ingest| {
                format!(
                    "{} demux/probe",
                    MediaEngine::graph_protocol_label(&ingest.protocol)
                )
            })
            .unwrap_or_else(|| "Demux/probe idle".to_string()),
        ingest.is_some(),
        ingest.map(api_view_models::processing_graph_demux_details),
        ingest.map(|ingest| ingest.metrics.snapshot()),
    ));

    let rb_node_id = format!("{pipeline_id}_source_rb");
    let rb_info = pipelines.get(pipeline_id).map(|ring| {
        let (fill, cap) = ring.fill_and_capacity();
        let reader_stats: Vec<serde_json::Value> = ring
            .reader_snapshots()
            .into_iter()
            .map(|reader| api_view_models::reader_snapshot_json(&reader))
            .collect();
        (
            fill,
            cap,
            api_view_models::ring_payload_stats_json(ring),
            reader_stats,
        )
    });
    nodes.push(api_view_models::processing_graph_node(
        rb_node_id.clone(),
        "ring_buffer",
        "Source Buffer",
        rb_info.is_some(),
        rb_info.map(|(fill, cap, payload_stats, readers)| {
            api_view_models::processing_graph_source_ring_details(
                fill,
                cap,
                payload_stats,
                MediaEngine::source_buffer_format(ingest_protocol),
                readers,
            )
        }),
        None,
    ));
    edges.push(api_view_models::processing_graph_edge(
        ingest_node_id,
        demux_node_id.clone(),
        ingest_protocol
            .map(MediaEngine::graph_protocol_label)
            .unwrap_or_else(|| "input".to_string()),
    ));
    edges.push(api_view_models::processing_graph_edge(
        demux_node_id,
        rb_node_id.clone(),
        "push(MediaPacket)",
    ));

    for (key, (stage_ring, token)) in transcoder_buffers.iter() {
        if key.pipeline.as_str() == pipeline_id {
            let kind = &key.kind;
            let stage_key_str = kind.to_string();
            let stage_id = kind.graph_node_id(pipeline_id);
            let queue_stats = all_input_queues.get(key).map(|queue| queue.stats());
            let pipe_stats = all_pipe_metrics.get(key).map(|pipe| pipe.snapshot());
            nodes.push(api_view_models::processing_graph_stage_node(
                stage_id.clone(),
                kind.graph_type(),
                kind.graph_label(),
                stage_key_str,
                !token.is_cancelled(),
                all_stage_metrics.get(key).map(|metrics| metrics.snapshot()),
                queue_stats.map(|stats| serde_json::json!(stats)),
                pipe_stats,
                api_view_models::ring_payload_stats_json(stage_ring),
            ));

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
                edges.push(api_view_models::processing_graph_edge(
                    from, stage_id, label,
                ));
            } else if let StageKind::VideoPreset { preset } = &kind {
                edges.push(api_view_models::processing_graph_edge(
                    rb_node_id.clone(),
                    stage_id,
                    format!("decode → {preset} encode"),
                ));
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

        nodes.push(api_view_models::processing_graph_node(
            output_node_id.clone(),
            "egress",
            format!("{protocol_label} sender: {}", output.name.as_str()),
            egress.is_some_and(|egress| {
                MediaEngine::egress_effective_status(egress, ingest.is_some()) == "running"
            }),
            egress.map(|egress| {
                api_view_models::processing_graph_egress_details(egress, ingest.is_some())
            }),
            egress.map(|egress| egress.metrics.snapshot()),
        ));

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
                nodes.push(api_view_models::processing_graph_node(
                    mux_node_id.clone(),
                    "packetizer",
                    format!("MPEG-TS mux: {}", output.encoding.as_str()),
                    mux_active,
                    Some(api_view_models::processing_graph_packetizer_details(
                        "srt",
                        output.encoding.as_str(),
                        mux_key,
                        mux_payload_stats,
                    )),
                    None,
                ));
                edges.push(api_view_models::processing_graph_edge(
                    terminal_node_id,
                    mux_node_id.clone(),
                    "media packets",
                ));
            }
            edges.push(api_view_models::processing_graph_edge(
                mux_node_id,
                output_node_id,
                "SRT send",
            ));
        } else {
            edges.push(api_view_models::processing_graph_edge(
                terminal_node_id,
                output_node_id,
                MediaEngine::source_to_egress_label(protocol),
            ));
        }
    }

    if let Some(token) = rec_tokens.get(pipeline_id) {
        let rec_id = format!("{pipeline_id}_recording");
        let rec_stage_key = StageKey::new(pipeline_id, StageKind::recording());
        nodes.push(api_view_models::processing_graph_node(
            rec_id.clone(),
            "recording",
            "MKV Recording",
            !token.is_cancelled(),
            None,
            all_stage_metrics
                .get(&rec_stage_key)
                .map(|metrics| metrics.snapshot()),
        ));
        edges.push(api_view_models::processing_graph_edge(
            rb_node_id.clone(),
            rec_id,
            "MKV mux",
        ));
    }

    if hls_stores.contains_key(pipeline_id) {
        let hls_id = format!("{pipeline_id}_hls_preview");
        let hls_stage_key = StageKey::new(pipeline_id, StageKind::hls());
        let hls_active = hls_consumers
            .get(pipeline_id)
            .is_some_and(|consumer| !consumer.cancel_token.is_cancelled());
        nodes.push(api_view_models::processing_graph_node(
            hls_id.clone(),
            "hls",
            "HLS Preview",
            hls_active,
            None,
            all_stage_metrics
                .get(&hls_stage_key)
                .map(|metrics| metrics.snapshot()),
        ));
        edges.push(api_view_models::processing_graph_edge(
            rb_node_id,
            hls_id,
            "MPEG-TS segment",
        ));
    }

    api_view_models::processing_graph_json(
        chrono::Utc::now().to_rfc3339(),
        pipeline_id,
        nodes,
        edges,
    )
}
