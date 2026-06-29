//! Shared audit metadata for agent tool invocations.

/// Identity fields that are useful across HTTP and MCP transports when an
/// agent operation should be attributable to a concrete actor/tool pair.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolAuditIdentity {
    pub actor: Option<String>,
    pub agent_id: Option<String>,
    pub tool_identity: Option<String>,
}

impl ToolAuditIdentity {
    pub fn is_empty(&self) -> bool {
        self.actor.is_none() && self.agent_id.is_none() && self.tool_identity.is_none()
    }
}
