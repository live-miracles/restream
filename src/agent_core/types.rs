//! Shared request types used by HTTP handlers, MCP handlers, and backends.

use serde::{Deserialize, Serialize};

/// Investigation request mirrored locally because the current HTTP type is
/// deserialize-only.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InvestigationRequest {
    pub workflow: Option<String>,
    pub pipeline_id: Option<String>,
    pub output_id: Option<String>,
    #[serde(default = "default_event_limit")]
    pub event_limit: usize,
}

const fn default_event_limit() -> usize {
    100
}

pub use crate::agent_plane::PlanRequest;

/// Execution request mirrored locally so MCP core can compile independently of
/// whether the runtime mutation feature is enabled.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationCreateRequest {
    pub intent: String,
    pub pipeline_id: Option<String>,
    #[serde(default)]
    pub proposed_changes: Vec<crate::agent_plane::ProposedChange>,
    pub idempotency_key: Option<String>,
    pub actor: Option<String>,
    pub agent_id: Option<String>,
    pub tool_identity: Option<String>,
    pub incident_id: Option<String>,
    #[serde(default)]
    pub incident_links: Vec<String>,
}

/// Approval request mirrored locally so MCP core can compile without
/// `agent-execution`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalRequest {
    pub approved_by: String,
    pub reason: Option<String>,
}

/// Verification request mirrored locally so MCP core can compile without
/// `agent-execution`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifyRequest {
    pub operation_id: String,
}

/// MCP-specific input wrapper used for tools that take an operation identifier
/// outside the HTTP path structure.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationIdInput {
    pub operation_id: String,
}

/// MCP-specific approval input that combines a path parameter and body payload
/// into one tool input.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationApprovalInput {
    pub operation_id: String,
    pub approved_by: String,
    pub reason: Option<String>,
}

impl OperationApprovalInput {
    pub fn body(&self) -> ApprovalRequest {
        ApprovalRequest {
            approved_by: self.approved_by.clone(),
            reason: self.reason.clone(),
        }
    }
}
