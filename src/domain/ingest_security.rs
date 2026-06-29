use serde::{Deserialize, Serialize};

pub const DEFAULT_INGEST_SECURITY_CONFIG: IngestSecurityConfig = IngestSecurityConfig {
    failure_limit: 10,
    failure_window_ms: 60 * 1000,
    ban_ms: 10 * 60 * 1000,
    tracked_ip_limit: 10000,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IngestSecurityConfig {
    pub failure_limit: i64,
    pub failure_window_ms: i64,
    pub ban_ms: i64,
    pub tracked_ip_limit: i64,
}

impl Default for IngestSecurityConfig {
    fn default() -> Self {
        DEFAULT_INGEST_SECURITY_CONFIG
    }
}
