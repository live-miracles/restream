//! Optional agent read/planning plane.
//!
//! This module is compiled only with the `agent-plane` Cargo feature. It keeps
//! phase-4 agent support out of the core runtime while still exposing typed,
//! testable planning primitives when the feature is enabled.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashSet;

use crate::domain::stage::EncodingStagePlan;
use crate::types::{Output, Pipeline};

const OUTPUT_URL_SCHEME_ERROR: &str =
    "Supported schemes are rtmp://, rtmps://, srt://, hls://, http://, and https://";

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCapabilities {
    pub generated_at: String,
    pub feature: &'static str,
    pub version: u32,
    pub compiled_in: bool,
    pub execution_enabled: bool,
    pub read_tools: Vec<&'static str>,
    pub planning_tools: Vec<&'static str>,
    pub execution_tools: Vec<&'static str>,
    pub notes: Vec<&'static str>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InvestigationRequest {
    pub workflow: Option<String>,
    pub pipeline_id: Option<String>,
    pub output_id: Option<String>,
    #[serde(default = "default_event_limit")]
    pub event_limit: usize,
}

fn default_event_limit() -> usize {
    100
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanRequest {
    pub intent: String,
    pub pipeline_id: Option<String>,
    #[serde(default)]
    pub proposed_changes: Vec<ProposedChange>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProposedChange {
    pub kind: String,
    pub pipeline_id: Option<String>,
    pub output_id: Option<String>,
    pub name: Option<String>,
    pub url: Option<String>,
    pub encoding: Option<String>,
    pub desired_state: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidationIssue {
    pub severity: &'static str,
    pub code: &'static str,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<&'static str>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidationResult {
    pub valid: bool,
    pub errors: Vec<ValidationIssue>,
    pub warnings: Vec<ValidationIssue>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphPreview {
    pub mode: &'static str,
    pub added_nodes: Vec<Value>,
    pub removed_nodes: Vec<Value>,
    pub changed_edges: Vec<Value>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImpactPreview {
    pub affected_pipelines: Vec<String>,
    pub affected_outputs: Vec<String>,
    pub shared_stage_candidates: Vec<String>,
    pub operator_summary: String,
    pub engineering_notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanResponse {
    pub generated_at: String,
    pub plan_id: String,
    pub status: &'static str,
    pub intent: String,
    pub execution_enabled: bool,
    pub execution_note: &'static str,
    pub steps: Vec<Value>,
    pub validation: ValidationResult,
    pub graph_preview: GraphPreview,
    pub impact: ImpactPreview,
}

pub fn capabilities() -> AgentCapabilities {
    AgentCapabilities {
        generated_at: now(),
        feature: "agent-plane",
        version: 1,
        compiled_in: true,
        execution_enabled: false,
        read_tools: vec![
            "get_engine_telemetry",
            "get_pipeline_summary",
            "get_pipeline_graph",
            "list_events",
            "list_alerts",
            "investigate_pipeline_issue",
            "trace_output_path",
            "find_first_unhealthy_node",
            "explain_degradation",
            "estimate_change_impact",
        ],
        planning_tools: vec![
            "plan_pipeline_change",
            "validate_change",
            "preview_graph_diff",
            "estimate_change_impact",
        ],
        execution_tools: Vec::new(),
        notes: vec![
            "Phase 4 is read/planning only.",
            "Phase 6 execution is intentionally not compiled into this feature.",
        ],
    }
}

pub fn investigation_response(
    request: InvestigationRequest,
    pipeline_exists: bool,
    output_exists: bool,
    health: Value,
    graph: Option<Value>,
    telemetry: Value,
    alerts: Vec<crate::alerts::Alert>,
    events: Vec<crate::events::Event>,
) -> Value {
    let workflow = request
        .workflow
        .unwrap_or_else(|| "investigatePipelineIssue".to_string());

    let mut findings = Vec::new();
    if request.pipeline_id.is_some() && !pipeline_exists {
        findings.push(serde_json::json!({
            "severity": "error",
            "code": "pipelineNotFound",
            "message": "The requested pipeline does not exist."
        }));
    }
    if request.output_id.is_some() && !output_exists {
        findings.push(serde_json::json!({
            "severity": "error",
            "code": "outputNotFound",
            "message": "The requested output does not exist on the selected pipeline."
        }));
    }
    for alert in &alerts {
        findings.push(serde_json::json!({
            "severity": alert.severity,
            "code": "activeAlert",
            "message": alert.title,
            "evidence": alert.evidence,
            "recommendedAction": alert.recommended_action,
            "pipelineId": alert.pipeline_id,
            "outputId": alert.output_id,
            "stageId": alert.stage_id,
        }));
    }
    if findings.is_empty() {
        findings.push(serde_json::json!({
            "severity": "info",
            "code": "noDerivedAlerts",
            "message": "No derived alerts were found for the requested scope."
        }));
    }

    serde_json::json!({
        "generatedAt": now(),
        "workflow": workflow,
        "pipelineId": request.pipeline_id,
        "outputId": request.output_id,
        "readOnly": true,
        "summary": {
            "alertCount": alerts.len(),
            "eventCount": events.len(),
            "hasGraph": graph.is_some(),
        },
        "findings": findings,
        "evidence": {
            "health": health,
            "graph": graph,
            "telemetry": telemetry,
            "alerts": alerts,
            "events": events,
        }
    })
}

pub fn plan_response(
    request: PlanRequest,
    pipelines: &[Pipeline],
    outputs: &[Output],
    current_graph: Option<&Value>,
) -> PlanResponse {
    let validation = validate_plan(&request, pipelines, outputs);
    let graph_preview = graph_preview(&request, current_graph);
    let impact = impact_preview(&request);
    let plan_id = plan_id(&request);

    let mut steps = vec![
        serde_json::json!({
            "id": "read-current-state",
            "title": "Read current runtime and persisted configuration",
            "phase": "read",
            "status": "planned"
        }),
        serde_json::json!({
            "id": "validate-change",
            "title": "Validate requested change against current platform constraints",
            "phase": "validate",
            "status": if validation.valid { "passed" } else { "failed" }
        }),
        serde_json::json!({
            "id": "preview-graph-impact",
            "title": "Preview graph and shared-stage impact",
            "phase": "preview",
            "status": "planned"
        }),
    ];
    if validation.valid {
        steps.push(serde_json::json!({
            "id": "await-phase-6-execution",
            "title": "Execution requires the separate phase-6 feature",
            "phase": "execute",
            "status": "blocked"
        }));
    }

    PlanResponse {
        generated_at: now(),
        plan_id,
        status: if validation.valid { "draft" } else { "invalid" },
        intent: request.intent,
        execution_enabled: false,
        execution_note: "Phase 4 only plans and validates; no runtime mutation is performed.",
        steps,
        validation,
        graph_preview,
        impact,
    }
}

pub fn validate_plan(
    request: &PlanRequest,
    pipelines: &[Pipeline],
    outputs: &[Output],
) -> ValidationResult {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    let pipeline_ids: HashSet<&str> = pipelines.iter().map(|p| p.id.as_str()).collect();
    let output_ids: HashSet<(&str, &str)> = outputs
        .iter()
        .map(|o| (o.pipeline_id.as_str(), o.id.as_str()))
        .collect();

    if request.intent.trim().is_empty() {
        errors.push(issue(
            "error",
            "emptyIntent",
            "Intent must be a non-empty string.",
            Some("intent"),
        ));
    }

    if request.proposed_changes.is_empty() {
        warnings.push(issue(
            "warning",
            "noStructuredChanges",
            "No proposedChanges were supplied, so the plan can only describe investigation steps.",
            Some("proposedChanges"),
        ));
    }

    for change in &request.proposed_changes {
        let pipeline_id = change
            .pipeline_id
            .as_deref()
            .or(request.pipeline_id.as_deref());
        match pipeline_id {
            Some(pid) if !pipeline_ids.contains(pid) => errors.push(issue(
                "error",
                "pipelineNotFound",
                format!("Pipeline '{pid}' does not exist."),
                Some("pipelineId"),
            )),
            None => errors.push(issue(
                "error",
                "missingPipelineId",
                "A pipelineId is required for each proposed change.",
                Some("pipelineId"),
            )),
            _ => {}
        }

        if matches!(
            change.kind.as_str(),
            "updateOutput" | "deleteOutput" | "startOutput" | "stopOutput"
        ) {
            match (pipeline_id, change.output_id.as_deref()) {
                (Some(pid), Some(oid)) if !output_ids.contains(&(pid, oid)) => {
                    errors.push(issue(
                        "error",
                        "outputNotFound",
                        format!("Output '{oid}' does not exist on pipeline '{pid}'."),
                        Some("outputId"),
                    ));
                }
                (_, None) => errors.push(issue(
                    "error",
                    "missingOutputId",
                    "This change kind requires outputId.",
                    Some("outputId"),
                )),
                _ => {}
            }
        }

        if let Some(url) = change.url.as_deref()
            && !is_supported_output_url(url.trim())
        {
            errors.push(issue(
                "error",
                "unsupportedOutputUrl",
                OUTPUT_URL_SCHEME_ERROR,
                Some("url"),
            ));
        }

        if let Some(encoding) = change.encoding.as_deref() {
            if is_custom_output_encoding(encoding) {
                errors.push(issue(
                    "error",
                    "customEncodingUnsupported",
                    "Custom output encoding is not available in the runtime planner yet.",
                    Some("encoding"),
                ));
            }
            if encoding.trim().is_empty() {
                errors.push(issue(
                    "error",
                    "emptyEncoding",
                    "Encoding must be a non-empty string.",
                    Some("encoding"),
                ));
            }
        }
    }

    ValidationResult {
        valid: errors.is_empty(),
        errors,
        warnings,
    }
}

fn graph_preview(request: &PlanRequest, current_graph: Option<&Value>) -> GraphPreview {
    let existing_stage_keys: HashSet<String> = current_graph
        .and_then(|graph| graph.get("nodes"))
        .and_then(|nodes| nodes.as_array())
        .into_iter()
        .flatten()
        .filter_map(|node| node.get("stageKey").and_then(|v| v.as_str()))
        .map(ToOwned::to_owned)
        .collect();

    let mut candidate_stages = HashSet::new();
    for change in &request.proposed_changes {
        if let (Some(pid), Some(encoding)) = (
            change
                .pipeline_id
                .as_deref()
                .or(request.pipeline_id.as_deref()),
            change.encoding.as_deref(),
        ) {
            let plan = EncodingStagePlan::from_encoding(pid, encoding);
            if let Some(stage) = plan.video_stage() {
                candidate_stages.insert(stage.kind.to_string());
            }
            if let Some(stage) = plan.audio_stage() {
                candidate_stages.insert(stage.kind.to_string());
            }
            if change
                .url
                .as_deref()
                .is_some_and(|url| url.starts_with("rtmp://") || url.starts_with("rtmps://"))
            {
                candidate_stages.insert(plan.codec_edge_stage("hevc_to_h264").kind.to_string());
            }
        }
    }

    let mut added_nodes: Vec<Value> = candidate_stages
        .into_iter()
        .filter(|stage| !existing_stage_keys.contains(stage))
        .map(|stage| {
            serde_json::json!({
                "type": "stage",
                "stageKey": stage,
                "active": false,
                "reason": "Would be materialized by the reconciler if required by outputs and source codec."
            })
        })
        .collect();
    added_nodes.sort_by(|a, b| a["stageKey"].as_str().cmp(&b["stageKey"].as_str()));

    let mut notes = vec![
        "Preview is static and read-only; it does not reserve stages or mutate runtime state."
            .to_string(),
    ];
    if added_nodes.is_empty() {
        notes.push(
            "No new shared stage candidates were identified beyond the current graph.".to_string(),
        );
    }

    GraphPreview {
        mode: "staticPreview",
        added_nodes,
        removed_nodes: Vec::new(),
        changed_edges: Vec::new(),
        notes,
    }
}

fn impact_preview(request: &PlanRequest) -> ImpactPreview {
    let mut affected_pipelines = HashSet::new();
    let mut affected_outputs = HashSet::new();
    let mut shared_stage_candidates = HashSet::new();
    let mut engineering_notes = Vec::new();

    for change in &request.proposed_changes {
        if let Some(pid) = change
            .pipeline_id
            .as_deref()
            .or(request.pipeline_id.as_deref())
        {
            affected_pipelines.insert(pid.to_string());
            if let Some(encoding) = change.encoding.as_deref() {
                let plan = EncodingStagePlan::from_encoding(pid, encoding);
                if let Some(stage) = plan.video_stage() {
                    shared_stage_candidates.insert(stage.kind.to_string());
                }
                if let Some(stage) = plan.audio_stage() {
                    shared_stage_candidates.insert(stage.kind.to_string());
                }
            }
        }
        if let Some(output_id) = &change.output_id {
            affected_outputs.insert(output_id.clone());
        }
        if change
            .url
            .as_deref()
            .is_some_and(|url| url.starts_with("rtmp://") || url.starts_with("rtmps://"))
        {
            engineering_notes.push(
                "RTMP/RTMPS outputs may require a shared HEVC-to-H.264 codec edge when the source is HEVC."
                    .to_string(),
            );
        }
    }

    let mut affected_pipelines: Vec<String> = affected_pipelines.into_iter().collect();
    affected_pipelines.sort();
    let mut affected_outputs: Vec<String> = affected_outputs.into_iter().collect();
    affected_outputs.sort();
    let mut shared_stage_candidates: Vec<String> = shared_stage_candidates.into_iter().collect();
    shared_stage_candidates.sort();

    let operator_summary = if affected_pipelines.is_empty() {
        "No concrete pipeline impact was identified.".to_string()
    } else {
        format!(
            "Would affect {} pipeline(s) and {} existing output(s); execution remains disabled in phase 4.",
            affected_pipelines.len(),
            affected_outputs.len()
        )
    };

    ImpactPreview {
        affected_pipelines,
        affected_outputs,
        shared_stage_candidates,
        operator_summary,
        engineering_notes,
    }
}

fn is_supported_output_url(url: &str) -> bool {
    url.starts_with("rtmp://")
        || url.starts_with("rtmps://")
        || url.starts_with("srt://")
        || url.starts_with("hls://")
        || url.starts_with("http://")
        || url.starts_with("https://")
}

fn is_custom_output_encoding(encoding: &str) -> bool {
    encoding
        .split('+')
        .next()
        .map(|video| video.trim().eq_ignore_ascii_case("custom"))
        .unwrap_or(false)
}

fn issue(
    severity: &'static str,
    code: &'static str,
    message: impl Into<String>,
    field: Option<&'static str>,
) -> ValidationIssue {
    ValidationIssue {
        severity,
        code,
        message: message.into(),
        field,
    }
}

fn plan_id(request: &PlanRequest) -> String {
    let raw = serde_json::to_vec(request).unwrap_or_default();
    let digest = Sha256::digest(raw);
    format!("plan_{}", hex_prefix(&digest, 12))
}

fn hex_prefix(bytes: &[u8], len: usize) -> String {
    bytes
        .iter()
        .flat_map(|b| [b >> 4, b & 0x0f])
        .take(len)
        .map(|n| char::from_digit(n as u32, 16).unwrap_or('0'))
        .collect()
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_rejects_unknown_pipeline_and_bad_url() {
        let req = PlanRequest {
            intent: "add output".to_string(),
            pipeline_id: Some("missing".to_string()),
            proposed_changes: vec![ProposedChange {
                kind: "addOutput".to_string(),
                pipeline_id: None,
                output_id: None,
                name: Some("CDN".to_string()),
                url: Some("ftp://example".to_string()),
                encoding: Some("720p".to_string()),
                desired_state: None,
            }],
        };

        let validation = validate_plan(&req, &[], &[]);
        assert!(!validation.valid);
        assert!(
            validation
                .errors
                .iter()
                .any(|issue| issue.code == "pipelineNotFound")
        );
        assert!(
            validation
                .errors
                .iter()
                .any(|issue| issue.code == "unsupportedOutputUrl")
        );
    }

    #[test]
    fn graph_preview_identifies_shared_stage_candidates() {
        let req = PlanRequest {
            intent: "add 720p rtmp output".to_string(),
            pipeline_id: Some("pipe-a".to_string()),
            proposed_changes: vec![ProposedChange {
                kind: "addOutput".to_string(),
                pipeline_id: None,
                output_id: None,
                name: None,
                url: Some("rtmp://example/live/key".to_string()),
                encoding: Some("720p+atrack:0".to_string()),
                desired_state: None,
            }],
        };

        let preview = graph_preview(&req, None);
        let stages: Vec<_> = preview
            .added_nodes
            .iter()
            .filter_map(|node| node["stageKey"].as_str())
            .collect();
        assert!(stages.contains(&"video:720p"));
        assert!(
            stages
                .iter()
                .any(|stage| stage.starts_with("audio:atrack:0"))
        );
        assert!(stages.iter().any(|stage| stage.starts_with("hevc_to_h264")));
    }
}
