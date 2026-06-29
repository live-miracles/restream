//! In-process backend for an embedded MCP transport.
//!
//! This scaffold keeps the type and ownership shape in place without forcing a
//! larger refactor of the current agent-plane handlers yet.

use crate::agent_core::backend::{AgentBackend, AgentFuture};
use crate::agent_core::errors::AgentError;
use crate::agent_core::types::{
    ApprovalRequest, InvestigationRequest, OperationCreateRequest, PlanRequest, VerifyRequest,
};
use crate::api::AppState;
use serde_json::Value;
use std::sync::Arc;

pub struct InProcessBackend {
    #[allow(dead_code)]
    state: Arc<AppState>,
}

impl InProcessBackend {
    pub fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }

    fn not_wired<T>() -> AgentFuture<'static, T> {
        Box::pin(async {
            Err(AgentError::NotYetImplemented(
                "in-process MCP backend scaffolding exists, but direct AppState wiring is not implemented yet",
            ))
        })
    }
}

impl AgentBackend for InProcessBackend {
    fn capabilities(&self) -> AgentFuture<'_, Value> {
        Box::pin(async {
            Ok(serde_json::to_value(crate::agent_plane::capabilities())
                .unwrap_or(serde_json::Value::Null))
        })
    }

    fn context(&self) -> AgentFuture<'_, Value> {
        Self::not_wired()
    }

    fn investigate(&self, _request: InvestigationRequest) -> AgentFuture<'_, Value> {
        Self::not_wired()
    }

    fn plan(&self, _request: PlanRequest) -> AgentFuture<'_, Value> {
        Self::not_wired()
    }

    fn validate(&self, _request: PlanRequest) -> AgentFuture<'_, Value> {
        Self::not_wired()
    }

    fn graph_diff(&self, _request: PlanRequest) -> AgentFuture<'_, Value> {
        Self::not_wired()
    }

    fn create_operation(&self, _request: OperationCreateRequest) -> AgentFuture<'_, Value> {
        Self::not_wired()
    }

    fn get_operation(&self, _operation_id: &str) -> AgentFuture<'_, Value> {
        Self::not_wired()
    }

    fn approve_operation(
        &self,
        _operation_id: &str,
        _request: ApprovalRequest,
    ) -> AgentFuture<'_, Value> {
        Self::not_wired()
    }

    fn apply_operation(&self, _operation_id: &str) -> AgentFuture<'_, Value> {
        Self::not_wired()
    }

    fn verify_operation(&self, _request: VerifyRequest) -> AgentFuture<'_, Value> {
        Self::not_wired()
    }
}
