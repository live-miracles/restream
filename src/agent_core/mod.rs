//! Shared agent-facing core types and abstractions.
//!
//! This module is transport-agnostic. The regular HTTP agent plane and any MCP
//! transport should both be able to build on these primitives.

pub mod audit;
pub mod backend;
pub mod errors;
pub mod types;
pub mod workflows;

pub use audit::ToolAuditIdentity;
pub use backend::{AgentBackend, AgentFuture, AgentResult};
pub use errors::AgentError;
