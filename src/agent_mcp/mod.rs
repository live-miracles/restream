//! MCP-facing tool catalog, handlers, and transport scaffolding.

pub mod auth;
pub mod handlers;
pub mod tools;
pub mod transport;

pub use auth::McpAuthContext;
pub use handlers::AgentToolHandler;
pub use tools::{McpToolDefinition, available_tool_catalog, tool_catalog};
pub use transport::{TransportConfig, TransportMode, run_server};
