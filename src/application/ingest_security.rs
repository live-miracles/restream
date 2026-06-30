//! Application-layer persistence adapter for ingest security configuration.
//! This file owns the metadata key and JSON round-tripping between the
//! `MetaStore` port and the domain config; validation semantics live in
//! `crate::domain::ingest_security`, while runtime enforcement lives in
//! `crate::media::security`.

use crate::application::ports::{MetaLookupError, MetaStore, MetaStoreWriter};
use crate::domain::ingest_security::IngestSecurityConfig;

pub const INGEST_SECURITY_CONFIG_META_KEY: &str = "ingest_security_config";

pub async fn load_ingest_security_config(meta_store: &dyn MetaStore) -> IngestSecurityConfig {
    let mut config = meta_store
        .get_meta(INGEST_SECURITY_CONFIG_META_KEY)
        .await
        .ok()
        .flatten()
        .and_then(|raw| serde_json::from_str::<IngestSecurityConfig>(&raw).ok())
        .unwrap_or_default();
    config.normalize();
    config
}

pub async fn save_ingest_security_config(
    meta_store: &dyn MetaStoreWriter,
    config: &IngestSecurityConfig,
) -> Result<(), MetaLookupError> {
    let mut config = config.clone();
    config.normalize();
    let raw =
        serde_json::to_string(&config).map_err(|error| MetaLookupError::new(error.to_string()))?;
    meta_store
        .set_meta(INGEST_SECURITY_CONFIG_META_KEY, &raw)
        .await
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::ports::{MetaLookupFuture, MetaWriteFuture};
    use std::sync::Mutex;

    struct FakeMetaStore {
        value: Mutex<Option<String>>,
        fail: bool,
    }

    impl MetaStore for FakeMetaStore {
        fn get_meta<'a>(&'a self, key: &'a str) -> MetaLookupFuture<'a> {
            Box::pin(async move {
                if key != INGEST_SECURITY_CONFIG_META_KEY {
                    return Err(MetaLookupError::new("unexpected key"));
                }
                if self.fail {
                    return Err(MetaLookupError::new("db unavailable"));
                }
                Ok(self.value.lock().unwrap_or_else(|e| e.into_inner()).clone())
            })
        }
    }

    impl MetaStoreWriter for FakeMetaStore {
        fn set_meta<'a>(&'a self, key: &'a str, value: &'a str) -> MetaWriteFuture<'a> {
            Box::pin(async move {
                if key != INGEST_SECURITY_CONFIG_META_KEY {
                    return Err(MetaLookupError::new("unexpected key"));
                }
                if self.fail {
                    return Err(MetaLookupError::new("db unavailable"));
                }
                *self.value.lock().unwrap_or_else(|e| e.into_inner()) = Some(value.to_string());
                Ok(value.to_string())
            })
        }
    }

    #[tokio::test]
    async fn load_ingest_security_config_reads_valid_json() {
        let store = FakeMetaStore {
            value: Mutex::new(Some(
                serde_json::json!({
                    "failureLimit": 3,
                    "failureWindowMs": 10_000,
                    "banMs": 30_000,
                    "trackedIpLimit": 42
                })
                .to_string(),
            )),
            fail: false,
        };

        let config = load_ingest_security_config(&store).await;

        assert_eq!(config.failure_limit, 3);
        assert_eq!(config.failure_window_ms, 10_000);
        assert_eq!(config.ban_ms, 30_000);
        assert_eq!(config.tracked_ip_limit, 42);
    }

    #[tokio::test]
    async fn load_ingest_security_config_falls_back_to_default_on_error() {
        let store = FakeMetaStore {
            value: Mutex::new(Some("{\"failureLimit\":\"bad\"}".to_string())),
            fail: true,
        };

        let config = load_ingest_security_config(&store).await;
        let default = IngestSecurityConfig::default();

        assert_eq!(config.failure_limit, default.failure_limit);
        assert_eq!(config.failure_window_ms, default.failure_window_ms);
        assert_eq!(config.ban_ms, default.ban_ms);
        assert_eq!(config.tracked_ip_limit, default.tracked_ip_limit);
    }

    #[tokio::test]
    async fn load_ingest_security_config_normalizes_persisted_values() {
        let store = FakeMetaStore {
            value: Mutex::new(Some(
                serde_json::json!({
                    "failureLimit": 0,
                    "failureWindowMs": -10,
                    "banMs": 0,
                    "trackedIpLimit": -3
                })
                .to_string(),
            )),
            fail: false,
        };

        let config = load_ingest_security_config(&store).await;

        assert_eq!(config.failure_limit, 1);
        assert_eq!(config.failure_window_ms, 1);
        assert_eq!(config.ban_ms, 1);
        assert_eq!(config.tracked_ip_limit, 1);
    }

    #[tokio::test]
    async fn save_ingest_security_config_serializes_to_meta_store() {
        let store = FakeMetaStore {
            value: Mutex::new(None),
            fail: false,
        };
        let config = IngestSecurityConfig {
            failure_limit: 3,
            failure_window_ms: 10_000,
            ban_ms: 30_000,
            tracked_ip_limit: 42,
        };

        save_ingest_security_config(&store, &config).await.unwrap();

        let persisted = store
            .value
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .unwrap();
        let roundtrip: IngestSecurityConfig = serde_json::from_str(&persisted).unwrap();
        assert_eq!(roundtrip.failure_limit, config.failure_limit);
        assert_eq!(roundtrip.failure_window_ms, config.failure_window_ms);
        assert_eq!(roundtrip.ban_ms, config.ban_ms);
        assert_eq!(roundtrip.tracked_ip_limit, config.tracked_ip_limit);
    }

    #[tokio::test]
    async fn save_ingest_security_config_persists_normalized_values() {
        let store = FakeMetaStore {
            value: Mutex::new(None),
            fail: false,
        };
        let config = IngestSecurityConfig {
            failure_limit: 0,
            failure_window_ms: -10,
            ban_ms: 0,
            tracked_ip_limit: -3,
        };

        save_ingest_security_config(&store, &config).await.unwrap();

        let persisted = store
            .value
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .unwrap();
        let roundtrip: IngestSecurityConfig = serde_json::from_str(&persisted).unwrap();
        assert_eq!(roundtrip.failure_limit, 1);
        assert_eq!(roundtrip.failure_window_ms, 1);
        assert_eq!(roundtrip.ban_ms, 1);
        assert_eq!(roundtrip.tracked_ip_limit, 1);
    }
}
