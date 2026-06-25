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

    fn is_loopback_ip(ip: &str) -> bool {
        // Parse as IpAddr to cover the full loopback ranges:
        //   IPv4: 127.0.0.0/8 (not just 127.0.0.1)
        //   IPv6: ::1 and IPv4-mapped ::ffff:127.x.x.x
        //   Literal "localhost" fallback for non-parseable strings.
        if ip == "localhost" {
            return true;
        }
        ip.parse::<std::net::IpAddr>()
            .map(|a| a.is_loopback())
            .unwrap_or(false)
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
        if Self::is_loopback_ip(ip) {
            return None;
        }

        // Read lock only — no mutations. Cleanup of stale entries happens
        // lazily in record_failure, keeping this hot check lock-free under
        // concurrent ban lookups (e.g., flood from many IPs).
        let state = self.state.read().ok()?;
        let record = state.get(ip)?;
        let now = Instant::now();

        if let Some(banned_until) = record.banned_until {
            if banned_until > now {
                return Some(banned_until.duration_since(now));
            }
        }

        None
    }

    pub fn record_failure(&self, ip: &str) -> bool {
        if Self::is_loopback_ip(ip) {
            return false;
        }

        let mut state = match self.state.write() {
            Ok(s) => s,
            Err(_) => return false,
        };

        let now = Instant::now();
        let config = self.get_config();

        // Enforce the tracked-IP limit before inserting a new entry.
        let limit = config.tracked_ip_limit.max(1) as usize;
        Self::evict_oldest_if_needed(&mut state, limit.saturating_sub(1));

        let record = state
            .entry(ip.to_string())
            .or_insert_with(|| FailureRecord {
                failures: Vec::new(),
                banned_until: None,
            });

        record.failures.push(now);

        let window = Duration::from_millis(config.failure_window_ms as u64);
        record.failures.retain(|&t| now.duration_since(t) < window);

        if record.failures.len() >= config.failure_limit as usize {
            record.banned_until = Some(now + Duration::from_millis(config.ban_ms as u64));
            true // Banned
        } else {
            false // Not yet banned
        }
    }

    /// Evict the oldest entries when the map is over the tracked-IP limit.
    /// Keeps memory bounded under a sustained flood of distinct IPs.
    fn evict_oldest_if_needed(state: &mut HashMap<String, FailureRecord>, limit: usize) {
        if state.len() <= limit {
            return;
        }
        // Remove IPs whose ban has expired and have no recent failures first,
        // then fall back to evicting by oldest most-recent-failure to keep the map bounded.
        let now = Instant::now();
        state.retain(|_, r| {
            let expired_ban = r.banned_until.is_none_or(|t| t <= now);
            let has_failures = !r.failures.is_empty();
            !expired_ban || has_failures
        });
        // Hard cap: if still over limit, evict by oldest most-recent-failure so
        // actively-attacking IPs (with recent failures) are retained, not dropped.
        // HashMap's arbitrary iteration order would otherwise evict random entries,
        // potentially letting an attacker clear their own record by flooding from
        // many IPs.
        if state.len() > limit {
            let excess = state.len() - limit;
            // Sort by the oldest failure in the record (earliest = least active)
            let mut entries: Vec<(&String, &FailureRecord)> = state.iter().collect();
            entries.sort_by_key(|(_, r)| r.failures.iter().copied().min().unwrap_or(now));
            let keys_to_remove: Vec<String> = entries
                .iter()
                .take(excess)
                .map(|(k, _)| (*k).clone())
                .collect();
            for k in keys_to_remove {
                state.remove(&k);
            }
        }
    }

    pub fn record_success(&self, ip: &str) {
        if Self::is_loopback_ip(ip) {
            return;
        }

        if let Ok(mut state) = self.state.write() {
            state.remove(ip);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_ips_are_not_rate_limited() {
        let service = IngestSecurityService::new(DEFAULT_INGEST_SECURITY_CONFIG);

        // IPv4 loopback variants
        for ip in &["127.0.0.1", "127.0.0.2", "127.255.255.255", "localhost"] {
            assert!(
                service.is_ip_banned(ip).is_none(),
                "loopback {ip} should be exempt"
            );
            assert!(
                !service.record_failure(ip),
                "loopback {ip} should not record failure"
            );
            assert!(
                service.is_ip_banned(ip).is_none(),
                "loopback {ip} should remain exempt after failure"
            );
        }
        // IPv6 loopback
        assert!(service.is_ip_banned("::1").is_none());
        assert!(!service.record_failure("::1"));
    }

    #[test]
    fn non_loopback_ips_are_rate_limited() {
        let cfg = IngestSecurityConfig {
            failure_limit: 2,
            failure_window_ms: 60_000,
            ban_ms: 10_000,
            tracked_ip_limit: 1000,
        };
        let svc = IngestSecurityService::new(cfg);
        // 10.x.x.x is not loopback
        assert!(!svc.record_failure("10.0.0.1"));
        assert!(svc.record_failure("10.0.0.1")); // 2nd → banned
        assert!(svc.is_ip_banned("10.0.0.1").is_some());
        // 192.168.x.x is not loopback
        assert!(!svc.record_failure("192.168.1.1"));
        assert!(svc.is_ip_banned("192.168.1.1").is_none()); // only 1 failure, not banned
    }

    #[test]
    fn ip_is_banned_after_failure_limit() {
        let cfg = IngestSecurityConfig {
            failure_limit: 3,
            failure_window_ms: 60_000,
            ban_ms: 10_000,
            tracked_ip_limit: 1000,
        };
        let svc = IngestSecurityService::new(cfg);
        let ip = "1.2.3.4";

        assert!(!svc.record_failure(ip)); // 1
        assert!(!svc.record_failure(ip)); // 2
        assert!(svc.record_failure(ip)); // 3 → banned
        assert!(svc.is_ip_banned(ip).is_some(), "IP should be banned");
    }

    #[test]
    fn record_success_clears_failure_state() {
        let cfg = IngestSecurityConfig {
            failure_limit: 3,
            failure_window_ms: 60_000,
            ban_ms: 10_000,
            tracked_ip_limit: 1000,
        };
        let svc = IngestSecurityService::new(cfg);
        let ip = "5.6.7.8";

        svc.record_failure(ip);
        svc.record_failure(ip);
        svc.record_success(ip); // should clear state
        // After success, two more failures should not ban (below limit)
        assert!(!svc.record_failure(ip));
        assert!(!svc.record_failure(ip));
        assert!(svc.is_ip_banned(ip).is_none());
    }

    #[test]
    fn tracked_ip_limit_is_enforced() {
        let cfg = IngestSecurityConfig {
            failure_limit: 100,
            failure_window_ms: 60_000,
            ban_ms: 60_000,
            tracked_ip_limit: 5, // very small limit
        };
        let svc = IngestSecurityService::new(cfg);

        // Insert 10 distinct IPs — the map must not exceed the limit
        for i in 0..10u8 {
            svc.record_failure(&format!("10.0.0.{i}"));
        }

        let state = svc.state.read().unwrap();
        assert!(
            state.len() <= 5,
            "tracked IP map must not exceed limit, got {}",
            state.len()
        );
    }
}
