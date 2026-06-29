//! MCP transport auth/session context.

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct McpAuthContext {
    pub session_cookie: Option<String>,
    pub bearer_token: Option<String>,
}

impl McpAuthContext {
    pub fn has_credentials(&self) -> bool {
        self.session_cookie.is_some() || self.bearer_token.is_some()
    }
}
