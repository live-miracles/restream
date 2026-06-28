//! Optional agent read/planning plane.
//!
//! This module is compiled only with the `agent-plane` Cargo feature. It keeps
//! phase-4 agent support out of the core runtime while still exposing typed,
//! testable planning primitives when the feature is enabled.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::HashSet;

use crate::domain::stage::EncodingStagePlan;
use crate::types::{Ingest, Job, Output, Pipeline};

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
    pub routes: Value,
    pub schemas: Value,
    pub redaction: Value,
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
    pub monitoring_url: Option<String>,
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

pub fn redacted_context(
    pipelines: &[Pipeline],
    outputs: &[Output],
    jobs: &[Job],
    ingests: &[Ingest],
    status: Value,
    health: Value,
    engine_telemetry: Value,
    pipeline_telemetry: Vec<Value>,
    graphs: Vec<Value>,
    alerts: Vec<crate::alerts::Alert>,
    events: Vec<crate::events::Event>,
    configuration: Value,
    media: Value,
    desired_vs_actual: Value,
    diagnostics: Value,
    dependencies: Value,
    storage: Value,
) -> Value {
    serde_json::json!({
        "generatedAt": now(),
        "readOnly": true,
        "redaction": redaction_policy(),
        "api": {
            "routes": route_catalog(),
            "schemas": schema_catalog()
        },
        "engine": redact_secrets(status),
        "features": {
            "agentPlane": true,
            "agentExecution": cfg!(feature = "agent-execution"),
            "executionCompiledIn": cfg!(feature = "agent-execution")
        },
        "configuration": redact_secrets(configuration),
        "state": {
            "pipelines": pipelines.iter().map(redacted_pipeline).collect::<Vec<_>>(),
            "outputs": outputs.iter().map(redacted_output).collect::<Vec<_>>(),
            "ingests": ingests.iter().map(redacted_ingest).collect::<Vec<_>>(),
            "jobs": jobs.iter().map(redact_serializable).collect::<Vec<_>>()
        },
        "runtime": {
            "health": redact_secrets(health),
            "telemetry": {
                "engine": redact_secrets(engine_telemetry),
                "pipelines": pipeline_telemetry.into_iter().map(redact_secrets).collect::<Vec<_>>()
            },
            "graphs": graphs.into_iter().map(redact_secrets).collect::<Vec<_>>(),
            "alerts": alerts.iter().map(redact_serializable).collect::<Vec<_>>(),
            "events": events.iter().map(redact_serializable).collect::<Vec<_>>()
        },
        "desiredVsActual": redact_secrets(desired_vs_actual),
        "diagnostics": redact_secrets(diagnostics),
        "dependencies": redact_secrets(dependencies),
        "media": redact_secrets(media),
        "storage": redact_secrets(storage)
    })
}

pub fn capabilities() -> AgentCapabilities {
    let execution_enabled = cfg!(feature = "agent-execution");
    AgentCapabilities {
        generated_at: now(),
        feature: "agent-plane",
        version: 1,
        compiled_in: true,
        execution_enabled,
        read_tools: vec![
            "get_agent_context",
            "investigate_pipeline_issue",
            "trace_output_path",
            "find_first_unhealthy_node",
            "explain_degradation",
            "estimate_change_impact",
            "inspect_desired_vs_actual",
            "inspect_diagnostics_summary",
        ],
        planning_tools: vec![
            "plan_pipeline_change",
            "validate_change",
            "preview_graph_diff",
            "estimate_change_impact",
        ],
        execution_tools: if execution_enabled {
            vec![
                "create_agent_operation",
                "get_agent_operation",
                "approve_agent_operation",
                "apply_agent_operation",
                "verify_agent_operation",
            ]
        } else {
            Vec::new()
        },
        routes: route_catalog(),
        schemas: schema_catalog(),
        redaction: redaction_policy(),
        notes: if execution_enabled {
            vec![
                "Phase 6 execution is compiled in.",
                "Operations are approval-gated and emit audit/verification records.",
                "Core operator routes are intentionally omitted from the agent catalog because their responses are not guaranteed to be redacted.",
            ]
        } else {
            vec![
                "Phase 4 is read/planning only.",
                "Phase 6 execution is intentionally not compiled into this feature.",
                "Core operator routes are intentionally omitted from the agent catalog because their responses are not guaranteed to be redacted.",
            ]
        },
    }
}

pub fn redaction_policy() -> Value {
    serde_json::json!({
        "policy": "agentContextV1",
        "streamKeys": "raw stream keys are replaced with stable SHA-256 fingerprints",
        "urls": "raw URLs are replaced with scheme, host, and stable SHA-256 fingerprints",
        "credentials": "credential-bearing fields are recursively redacted when represented as streamKey, stream_key, targetUrl, or url",
        "recursiveFields": ["streamKey", "stream_key", "targetUrl", "url"],
        "fingerprint": {
            "algorithm": "sha256",
            "encoding": "hex",
            "prefixChars": 16
        }
    })
}

pub fn route_catalog() -> Value {
    serde_json::json!([
        {"tool": "get_agent_capabilities", "method": "GET", "path": "/api/v1/agent/capabilities", "auth": "session", "feature": "agent-plane", "mutates": false, "responseSchema": "AgentCapabilities"},
        {"tool": "get_agent_context", "method": "GET", "path": "/api/v1/agent/context", "auth": "session", "feature": "agent-plane", "mutates": false, "responseSchema": "AgentContextV1"},
        {"tool": "investigate_pipeline_issue", "method": "POST", "path": "/api/v1/agent/investigations", "auth": "session", "feature": "agent-plane", "mutates": false, "requestSchema": "InvestigationRequest", "responseSchema": "InvestigationResponse"},
        {"tool": "plan_pipeline_change", "method": "POST", "path": "/api/v1/agent/plans", "auth": "session", "feature": "agent-plane", "mutates": false, "requestSchema": "PlanRequest", "responseSchema": "PlanResponse"},
        {"tool": "validate_change", "method": "POST", "path": "/api/v1/agent/plans/validate", "auth": "session", "feature": "agent-plane", "mutates": false, "requestSchema": "PlanRequest", "responseSchema": "ValidationResult"},
        {"tool": "preview_graph_diff", "method": "POST", "path": "/api/v1/agent/graph-diff-preview", "auth": "session", "feature": "agent-plane", "mutates": false, "requestSchema": "PlanRequest", "responseSchema": "GraphDiffPreview"},
        {"tool": "create_agent_operation", "method": "POST", "path": "/api/v1/agent/operations", "auth": "session", "feature": "agent-execution", "compiledIn": cfg!(feature = "agent-execution"), "mutates": true, "requestSchema": "OperationCreateRequest", "responseSchema": "OperationRecord"},
        {"tool": "get_agent_operation", "method": "GET", "path": "/api/v1/agent/operations/:operation_id", "auth": "session", "feature": "agent-execution", "compiledIn": cfg!(feature = "agent-execution"), "mutates": false, "responseSchema": "OperationRecord"},
        {"tool": "approve_agent_operation", "method": "POST", "path": "/api/v1/agent/operations/:operation_id/approve", "auth": "session", "feature": "agent-execution", "compiledIn": cfg!(feature = "agent-execution"), "mutates": true, "requestSchema": "ApprovalRequest", "responseSchema": "OperationRecord"},
        {"tool": "apply_agent_operation", "method": "POST", "path": "/api/v1/agent/operations/:operation_id/apply", "auth": "session", "feature": "agent-execution", "compiledIn": cfg!(feature = "agent-execution"), "mutates": true, "responseSchema": "OperationRecord"},
        {"tool": "verify_agent_operation", "method": "POST", "path": "/api/v1/agent/operations/:operation_id/verify", "auth": "session", "feature": "agent-execution", "compiledIn": cfg!(feature = "agent-execution"), "mutates": true, "responseSchema": "OperationRecord"},
        {"tool": "verify_agent_operation_by_body", "method": "POST", "path": "/api/v1/agent/verify", "auth": "session", "feature": "agent-execution", "compiledIn": cfg!(feature = "agent-execution"), "mutates": true, "requestSchema": "VerifyRequest", "responseSchema": "OperationRecord"}
    ])
}

pub fn schema_catalog() -> Value {
    serde_json::json!({
        "InvestigationRequest": {
            "type": "object",
            "fields": {
                "workflow": {"type": "string", "optional": true},
                "pipelineId": {"type": "string", "optional": true},
                "outputId": {"type": "string", "optional": true},
                "eventLimit": {"type": "integer", "default": 100, "max": 1000}
            }
        },
        "PlanRequest": {
            "type": "object",
            "required": ["intent"],
            "fields": {
                "intent": {"type": "string"},
                "pipelineId": {"type": "string", "optional": true},
                "proposedChanges": {"type": "array", "items": "ProposedChange", "default": []}
            }
        },
        "ProposedChange": {
            "type": "object",
            "required": ["kind"],
            "fields": {
                "kind": {"type": "string", "enum": ["addOutput", "updateOutput", "removeOutput", "startOutput", "stopOutput"]},
                "pipelineId": {"type": "string", "optional": true},
                "outputId": {"type": "string", "optional": true},
                "name": {"type": "string", "optional": true},
                "url": {"type": "string", "optional": true, "redacted": true},
                "monitoringUrl": {"type": "string", "optional": true, "redacted": true},
                "encoding": {"type": "string", "optional": true},
                "desiredState": {"type": "string", "optional": true, "enum": ["running", "stopped"]}
            }
        },
        "OperationCreateRequest": {
            "type": "object",
            "required": ["intent"],
            "fields": {
                "intent": {"type": "string"},
                "pipelineId": {"type": "string", "optional": true},
                "proposedChanges": {"type": "array", "items": "ProposedChange", "default": []},
                "idempotencyKey": {"type": "string", "optional": true},
                "actor": {"type": "string", "optional": true},
                "agentId": {"type": "string", "optional": true},
                "toolIdentity": {"type": "string", "optional": true},
                "incidentId": {"type": "string", "optional": true},
                "incidentLinks": {"type": "array", "items": "string", "default": []}
            }
        },
        "ApprovalRequest": {
            "type": "object",
            "required": ["approvedBy"],
            "fields": {
                "approvedBy": {"type": "string"},
                "reason": {"type": "string", "optional": true}
            }
        },
        "VerifyRequest": {
            "type": "object",
            "required": ["operationId"],
            "fields": {
                "operationId": {"type": "string"}
            }
        },
        "OperationRecord": {
            "type": "object",
            "sections": ["operationId", "status", "approval", "request", "plan", "affectedObjects", "stateTransitions", "auditLog", "executionResult", "verificationResult"]
        },
        "AgentContextV1": {
            "type": "object",
            "sections": ["api", "engine", "features", "configuration", "state", "runtime", "desiredVsActual", "diagnostics", "dependencies", "media", "storage", "redaction"]
        }
    })
}

pub fn redact_secrets(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(redact_map(map)),
        Value::Array(values) => Value::Array(values.into_iter().map(redact_secrets).collect()),
        other => other,
    }
}

fn redact_map(map: Map<String, Value>) -> Map<String, Value> {
    let mut out = Map::new();
    for (key, value) in map {
        match key.as_str() {
            "streamKey" | "stream_key" => {
                if let Some(raw) = value.as_str() {
                    out.insert(format!("{key}Fingerprint"), fingerprint(raw));
                    out.insert(key, Value::String("redacted".to_string()));
                } else {
                    out.insert(key, redact_secrets(value));
                }
            }
            "targetUrl" | "url" | "monitoringUrl" | "monitoring_url" => {
                if let Some(raw) = value.as_str() {
                    out.insert(format!("{key}Redacted"), redacted_url(raw));
                    out.insert(format!("{key}Fingerprint"), fingerprint(raw));
                    out.insert(key, Value::String("redacted".to_string()));
                } else {
                    out.insert(key, redact_secrets(value));
                }
            }
            _ => {
                out.insert(key, redact_secrets(value));
            }
        }
    }
    out
}

fn redacted_pipeline(pipeline: &Pipeline) -> Value {
    serde_json::json!({
        "id": pipeline.id,
        "name": pipeline.name,
        "streamKey": "redacted",
        "streamKeyFingerprint": fingerprint(&pipeline.stream_key),
        "inputSource": pipeline.input_source,
        "encoding": pipeline.encoding,
    })
}

fn redacted_output(output: &Output) -> Value {
    serde_json::json!({
        "id": output.id,
        "pipelineId": output.pipeline_id,
        "name": output.name,
        "url": "redacted",
        "urlRedacted": redacted_url(&output.url),
        "urlFingerprint": fingerprint(&output.url),
        "desiredState": output.desired_state,
        "encoding": output.encoding,
        "monitoringUrl": output.monitoring_url.as_ref().map(|url| redacted_url(url)),
    })
}

fn redacted_ingest(ingest: &Ingest) -> Value {
    serde_json::json!({
        "id": ingest.id,
        "filename": ingest.filename,
        "streamKey": "redacted",
        "streamKeyFingerprint": fingerprint(&ingest.stream_key),
        "loop": ingest.loop_flag,
        "startTime": ingest.start_time,
    })
}

fn redact_serializable<T: Serialize>(value: &T) -> Value {
    redact_secrets(serde_json::to_value(value).unwrap_or(Value::Null))
}

pub fn redact_secrets_from_serializable<T: Serialize>(value: &T) -> Value {
    redact_serializable(value)
}

fn redacted_url(raw: &str) -> Value {
    let (scheme, rest) = raw
        .split_once("://")
        .map(|(scheme, rest)| (Some(scheme), rest))
        .unwrap_or((None, raw));
    let host = rest
        .split(['/', '?', '#'])
        .next()
        .filter(|part| !part.is_empty());
    serde_json::json!({
        "scheme": scheme,
        "host": host,
        "fingerprint": fingerprint(raw)
    })
}

fn fingerprint(raw: &str) -> Value {
    let digest = Sha256::digest(raw.as_bytes());
    serde_json::json!({
        "sha256Prefix": hex_prefix(&digest, 16)
    })
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
            "health": redact_secrets(health),
            "graph": graph.map(redact_secrets),
            "telemetry": redact_secrets(telemetry),
            "alerts": alerts.iter().map(redact_serializable).collect::<Vec<_>>(),
            "events": events.iter().map(redact_serializable).collect::<Vec<_>>(),
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
        if cfg!(feature = "agent-execution") {
            steps.push(serde_json::json!({
                "id": "create-agent-operation",
                "title": "Create approval-gated operation for execution",
                "phase": "execute",
                "status": "available"
            }));
            steps.push(serde_json::json!({
                "id": "verify-agent-operation",
                "title": "Verify health, graph convergence, and alert delta after application",
                "phase": "verify",
                "status": "available"
            }));
        } else {
            steps.push(serde_json::json!({
                "id": "await-phase-6-execution",
                "title": "Execution requires the separate phase-6 feature",
                "phase": "execute",
                "status": "blocked"
            }));
        }
    }

    PlanResponse {
        generated_at: now(),
        plan_id,
        status: if validation.valid { "draft" } else { "invalid" },
        intent: request.intent,
        execution_enabled: cfg!(feature = "agent-execution"),
        execution_note: if cfg!(feature = "agent-execution") {
            "Phase 6 can create approval-gated operations; mutation happens only after explicit approval and apply."
        } else {
            "Phase 4 only plans and validates; no runtime mutation is performed."
        },
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

        if !matches!(
            change.kind.as_str(),
            "addOutput" | "updateOutput" | "removeOutput" | "startOutput" | "stopOutput"
        ) {
            errors.push(issue(
                "error",
                "unsupportedChangeKind",
                format!("Unsupported change kind '{}'.", change.kind),
                Some("kind"),
            ));
        }

        if matches!(
            change.kind.as_str(),
            "updateOutput" | "removeOutput" | "startOutput" | "stopOutput"
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

        if change.kind == "addOutput" {
            if change
                .name
                .as_deref()
                .is_none_or(|name| name.trim().is_empty())
            {
                errors.push(issue(
                    "error",
                    "missingOutputName",
                    "addOutput requires a non-empty name.",
                    Some("name"),
                ));
            }
            if change
                .url
                .as_deref()
                .is_none_or(|url| url.trim().is_empty())
            {
                errors.push(issue(
                    "error",
                    "missingOutputUrl",
                    "addOutput requires a non-empty url.",
                    Some("url"),
                ));
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

        if let Some(desired_state) = change.desired_state.as_deref()
            && !matches!(desired_state, "running" | "stopped")
        {
            errors.push(issue(
                "error",
                "invalidDesiredState",
                "desiredState must be either 'running' or 'stopped'.",
                Some("desiredState"),
            ));
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
                monitoring_url: None,
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
                monitoring_url: None,
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
