//! Transport-agnostic backend trait for agent operations.

use crate::agent_core::errors::AgentError;
use crate::agent_core::types::{
    ApprovalRequest, InvestigationRequest, OperationCreateRequest, PlanRequest, VerifyRequest,
};
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;

pub type AgentResult<T> = Result<T, AgentError>;
pub type AgentFuture<'a, T> = Pin<Box<dyn Future<Output = AgentResult<T>> + Send + 'a>>;

/// Shared boundary for MCP handlers. Implementations may call into the local
/// process or make HTTP calls to `/api/v1/agent/*`.
pub trait AgentBackend: Send + Sync {
    fn capabilities(&self) -> AgentFuture<'_, Value>;
    fn context(&self) -> AgentFuture<'_, Value>;
    fn investigate(&self, request: InvestigationRequest) -> AgentFuture<'_, Value>;
    fn plan(&self, request: PlanRequest) -> AgentFuture<'_, Value>;
    fn validate(&self, request: PlanRequest) -> AgentFuture<'_, Value>;
    fn graph_diff(&self, request: PlanRequest) -> AgentFuture<'_, Value>;
    fn create_operation(&self, request: OperationCreateRequest) -> AgentFuture<'_, Value>;
    fn get_operation(&self, operation_id: &str) -> AgentFuture<'_, Value>;
    fn approve_operation(
        &self,
        operation_id: &str,
        request: ApprovalRequest,
    ) -> AgentFuture<'_, Value>;
    fn apply_operation(&self, operation_id: &str) -> AgentFuture<'_, Value>;
    fn verify_operation(&self, request: VerifyRequest) -> AgentFuture<'_, Value>;
}
