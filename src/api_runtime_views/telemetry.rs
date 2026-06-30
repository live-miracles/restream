use crate::api_view_models;
use crate::media::engine::MediaEngine;

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
