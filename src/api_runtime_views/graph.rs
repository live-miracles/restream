//! API/runtime graph adapters for visualizing a pipeline's active processing
//! topology.
//! This file owns the HTTP-facing node/edge projection over runtime stage and
//! output state, including packetizer, recording, and preview branches.

use crate::api_view_models;
use crate::application::output_path::OutputPath;
use crate::domain::stage::{StageKey, StageKind};
use crate::media::engine::MediaEngine;
use crate::types::Output;
use std::collections::HashSet;

pub(crate) async fn processing_graph(
    engine: &MediaEngine,
    pipeline_id: &str,
    outputs: &[Output],
) -> serde_json::Value {
    let hls_snapshot = engine.hls_dependency_snapshot(pipeline_id).await;
    let ingests = engine.ingests.active.read().await;
    let egresses = engine.egresses.active.read().await;
    let pipelines = engine.ingests.pipelines.read().await;
    let transcoder_buffers = engine.stages.buffers.read().await;
    let rec_tokens = engine.recordings.cancel_tokens.read().await;
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

    let pipeline_outputs: Vec<_> = outputs
        .iter()
        .filter(|output| output.pipeline_id == pipeline_id)
        .collect();
    let ingest_video_codec = ingest
        .and_then(|ingest| ingest.video.as_ref())
        .map(|video| video.codec.as_str());
    let visible_stage_keys: HashSet<StageKey> = pipeline_outputs
        .iter()
        .filter(|output| {
            ingest.is_some()
                && (output.desired_state == "running" || egresses.contains_key(&output.id))
        })
        .flat_map(|output| {
            OutputPath::resolve(pipeline_id, &output.encoding, &output.url)
                .needed_stage_keys(ingest_video_codec)
        })
        .collect();

    for (key, (stage_ring, token)) in transcoder_buffers.iter() {
        if key.pipeline.as_str() == pipeline_id && visible_stage_keys.contains(key) {
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

    if hls_snapshot.store_exists {
        let hls_id = format!("{pipeline_id}_hls_preview");
        let hls_stage_key = StageKey::new(pipeline_id, StageKind::hls());
        nodes.push(api_view_models::processing_graph_node(
            hls_id.clone(),
            "hls",
            "HLS Preview",
            hls_snapshot.active,
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
