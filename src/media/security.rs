//! Ingest authentication rate limiter — tracks per-IP failure counts and
//! applies time-based bans after exceeding the threshold. Protects RTMP/SRT
//! stream key brute-force attempts.

use crate::types::IngestSecurityConfig;
use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, Instant};

pub const DEFAULT_INGEST_SECURITY_CONFIG: IngestSecurityConfig = IngestSecurityConfig {
    failure_limit: 10,
    failure_window_ms: 60 * 1000,
    ban_ms: 10 * 60 * 1000,
    tracked_ip_limit: 10000,
};

struct FailureRecord {
    failures: Vec<Instant>,
    banned_until: Option<Instant>,
}

pub struct IngestSecurityService {
    config: RwLock<IngestSecurityConfig>,
    state: RwLock<HashMap<String, FailureRecord>>,
}

impl IngestSecurityService {
    pub fn new(config: IngestSecurityConfig) -> Self {
        Self {
            config: RwLock::new(config),
            state: RwLock::new(HashMap::new()),
        }
    }

    pub fn update_config(&self, new_config: IngestSecurityConfig) {
        if let Ok(mut config) = self.config.write() {
            *config = new_config;
        }
    }

    pub fn get_config(&self) -> IngestSecurityConfig {
        self.config
            .read()
            .map(|c| c.clone())
            .unwrap_or(DEFAULT_INGEST_SECURITY_CONFIG)
    }

    pub fn is_ip_banned(&self, ip: &str) -> Option<Duration> {
        let mut state = self.state.write().ok()?;
        let record = state.get_mut(ip)?;
        let now = Instant::now();

        // Prune old failures
        let window = Duration::from_millis(self.get_config().failure_window_ms as u64);
        record.failures.retain(|&t| now.duration_since(t) < window);

        if let Some(banned_until) = record.banned_until {
            if banned_until > now {
                return Some(banned_until.duration_since(now));
            } else {
                record.banned_until = None;
            }
        }

        None
    }

    pub fn record_failure(&self, ip: &str) -> bool {
        let mut state = match self.state.write() {
            Ok(s) => s,
            Err(_) => return false,
        };

        let now = Instant::now();
        let record = state
            .entry(ip.to_string())
            .or_insert_with(|| FailureRecord {
                failures: Vec::new(),
                banned_until: None,
            });

        record.failures.push(now);

        let config = self.get_config();
        let window = Duration::from_millis(config.failure_window_ms as u64);
        record.failures.retain(|&t| now.duration_since(t) < window);

        if record.failures.len() >= config.failure_limit as usize {
            record.banned_until = Some(now + Duration::from_millis(config.ban_ms as u64));
            true // Banned
        } else {
            false // Not yet banned
        }
    }

    pub fn record_success(&self, ip: &str) {
        if let Ok(mut state) = self.state.write() {
            state.remove(ip);
        }
    }
}
