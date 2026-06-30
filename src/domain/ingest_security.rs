//! Domain policy for ingest security limits and ban-window settings.
//! This layer owns the valid shape and defaults of the config shared across
//! API payloads, persistence, and runtime enforcement; it does not own storage
//! or the in-memory rate-limiting algorithm.

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

impl IngestSecurityConfig {
    pub fn normalize(&mut self) {
        self.failure_limit = self.failure_limit.max(1);
        self.failure_window_ms = self.failure_window_ms.max(1);
        self.ban_ms = self.ban_ms.max(1);
        self.tracked_ip_limit = self.tracked_ip_limit.max(1);
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if self.failure_limit < 1 {
            return Err("ingestSecurity.failureLimit must be >= 1");
        }
        if self.failure_window_ms < 1 {
            return Err("ingestSecurity.failureWindowMs must be >= 1");
        }
        if self.ban_ms < 1 {
            return Err("ingestSecurity.banMs must be >= 1");
        }
        if self.tracked_ip_limit < 1 {
            return Err("ingestSecurity.trackedIpLimit must be >= 1");
        }
        Ok(())
    }
}

impl Default for IngestSecurityConfig {
    fn default() -> Self {
        DEFAULT_INGEST_SECURITY_CONFIG
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_clamps_non_positive_values() {
        let mut config = IngestSecurityConfig {
            failure_limit: 0,
            failure_window_ms: -5,
            ban_ms: 0,
            tracked_ip_limit: -10,
        };

        config.normalize();

        assert_eq!(config.failure_limit, 1);
        assert_eq!(config.failure_window_ms, 1);
        assert_eq!(config.ban_ms, 1);
        assert_eq!(config.tracked_ip_limit, 1);
    }

    #[test]
    fn validate_rejects_non_positive_values() {
        let config = IngestSecurityConfig {
            failure_limit: 0,
            ..IngestSecurityConfig::default()
        };

        assert_eq!(
            config.validate(),
            Err("ingestSecurity.failureLimit must be >= 1")
        );
    }
}
