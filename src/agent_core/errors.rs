//! Errors shared by MCP-facing backends and handlers.

use std::fmt::{Display, Formatter};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentError {
    FeatureUnavailable(&'static str),
    InvalidRequest(String),
    Transport(String),
    Upstream { status: u16, body: String },
    NotYetImplemented(&'static str),
}

impl Display for AgentError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FeatureUnavailable(feature) => {
                write!(f, "required feature '{feature}' is not compiled in")
            }
            Self::InvalidRequest(message) => write!(f, "{message}"),
            Self::Transport(message) => write!(f, "{message}"),
            Self::Upstream { status, body } => {
                write!(
                    f,
                    "upstream agent route failed with status {status}: {body}"
                )
            }
            Self::NotYetImplemented(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for AgentError {}

impl From<reqwest::Error> for AgentError {
    fn from(value: reqwest::Error) -> Self {
        Self::Transport(value.to_string())
    }
}

impl From<serde_json::Error> for AgentError {
    fn from(value: serde_json::Error) -> Self {
        Self::InvalidRequest(value.to_string())
    }
}
