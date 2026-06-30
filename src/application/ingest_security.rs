use crate::application::ports::MetaStore;
use crate::domain::ingest_security::IngestSecurityConfig;

pub const INGEST_SECURITY_CONFIG_META_KEY: &str = "ingest_security_config";

pub async fn load_ingest_security_config(meta_store: &dyn MetaStore) -> IngestSecurityConfig {
    meta_store
        .get_meta(INGEST_SECURITY_CONFIG_META_KEY)
        .await
        .ok()
        .flatten()
        .and_then(|raw| serde_json::from_str::<IngestSecurityConfig>(&raw).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::ports::{MetaLookupError, MetaLookupFuture, MetaStore};

    struct FakeMetaStore {
        value: Option<String>,
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
                Ok(self.value.clone())
            })
        }
    }

    #[tokio::test]
    async fn load_ingest_security_config_reads_valid_json() {
        let store = FakeMetaStore {
            value: Some(
                serde_json::json!({
                    "failureLimit": 3,
                    "failureWindowMs": 10_000,
                    "banMs": 30_000,
                    "trackedIpLimit": 42
                })
                .to_string(),
            ),
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
            value: Some("{\"failureLimit\":\"bad\"}".to_string()),
            fail: true,
        };

        let config = load_ingest_security_config(&store).await;
        let default = IngestSecurityConfig::default();

        assert_eq!(config.failure_limit, default.failure_limit);
        assert_eq!(config.failure_window_ms, default.failure_window_ms);
        assert_eq!(config.ban_ms, default.ban_ms);
        assert_eq!(config.tracked_ip_limit, default.tracked_ip_limit);
    }
}
