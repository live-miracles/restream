//! Thin MCP handlers that parse tool input and delegate to `AgentBackend`.

use crate::agent_core::backend::AgentBackend;
use crate::agent_core::errors::AgentError;
use crate::agent_core::types::{
    InvestigationRequest, OperationApprovalInput, OperationCreateRequest, OperationIdInput,
    PlanRequest, VerifyRequest,
};
use serde_json::Value;
use std::sync::Arc;

pub struct AgentToolHandler<B: AgentBackend> {
    backend: Arc<B>,
}

impl<B: AgentBackend> AgentToolHandler<B> {
    pub fn new(backend: Arc<B>) -> Self {
        Self { backend }
    }

    pub async fn dispatch(&self, tool_name: &str, input: Value) -> Result<Value, AgentError> {
        match tool_name {
            "get_agent_capabilities" => self.backend.capabilities().await,
            "get_agent_context" => self.backend.context().await,
            "investigate_pipeline_issue" => {
                let request: InvestigationRequest = serde_json::from_value(input)?;
                self.backend.investigate(request).await
            }
            "plan_pipeline_change" => {
                let request: PlanRequest = serde_json::from_value(input)?;
                self.backend.plan(request).await
            }
            "validate_change" => {
                let request: PlanRequest = serde_json::from_value(input)?;
                self.backend.validate(request).await
            }
            "preview_graph_diff" => {
                let request: PlanRequest = serde_json::from_value(input)?;
                self.backend.graph_diff(request).await
            }
            "create_agent_operation" => {
                let request: OperationCreateRequest = serde_json::from_value(input)?;
                self.backend.create_operation(request).await
            }
            "get_agent_operation" => {
                let request: OperationIdInput = serde_json::from_value(input)?;
                self.backend.get_operation(&request.operation_id).await
            }
            "approve_agent_operation" => {
                let request: OperationApprovalInput = serde_json::from_value(input)?;
                self.backend
                    .approve_operation(&request.operation_id, request.body())
                    .await
            }
            "apply_agent_operation" => {
                let request: OperationIdInput = serde_json::from_value(input)?;
                self.backend.apply_operation(&request.operation_id).await
            }
            "verify_agent_operation" => {
                let request: OperationIdInput = serde_json::from_value(input)?;
                self.backend
                    .verify_operation(VerifyRequest {
                        operation_id: request.operation_id,
                    })
                    .await
            }
            other => Err(AgentError::InvalidRequest(format!(
                "unknown MCP tool '{other}'"
            ))),
        }
    }
}
