use crate::application::ports::MetaStore;
use crate::domain::srt_ingest::{DEFAULT_SRT_PBKEYLEN, SrtGlobalIngestConfig, SrtGlobalIngestMode};
use tracing::warn;

pub const SRT_INGEST_GLOBAL_CONFIG_META_KEY: &str = "srt_ingest_global_config";

pub async fn load_global_srt_ingest_config(meta_store: &dyn MetaStore) -> SrtGlobalIngestConfig {
    let from_store = meta_store
        .get_meta(SRT_INGEST_GLOBAL_CONFIG_META_KEY)
        .await
        .ok()
        .flatten()
        .and_then(|raw| serde_json::from_str::<SrtGlobalIngestConfig>(&raw).ok());
    let mut config = from_store
        .or_else(legacy_srt_global_config_from_env)
        .unwrap_or_default();
    if let Err(error) = config.validate() {
        warn!(err = %error, "invalid global SRT ingest config; falling back to plaintext");
        config = SrtGlobalIngestConfig::default();
    }
    config
}

fn legacy_srt_global_config_from_env() -> Option<SrtGlobalIngestConfig> {
    let passphrase = std::env::var("RESTREAM_SRT_PASSPHRASE").ok()?;
    if passphrase.is_empty() {
        return None;
    }
    let pbkeylen = std::env::var("RESTREAM_SRT_PBKEYLEN")
        .ok()
        .and_then(|value| value.parse::<i32>().ok())
        .unwrap_or(DEFAULT_SRT_PBKEYLEN);
    Some(SrtGlobalIngestConfig {
        mode: SrtGlobalIngestMode::Encrypted,
        passphrase: Some(passphrase),
        pbkeylen,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::ports::{MetaLookupError, MetaLookupFuture, MetaStore};

    struct FakeMetaStore {
        value: Option<String>,
    }

    impl MetaStore for FakeMetaStore {
        fn get_meta<'a>(&'a self, key: &'a str) -> MetaLookupFuture<'a> {
            Box::pin(async move {
                if key != SRT_INGEST_GLOBAL_CONFIG_META_KEY {
                    return Err(MetaLookupError::new("unexpected key"));
                }
                Ok(self.value.clone())
            })
        }
    }

    #[tokio::test]
    async fn global_srt_ingest_config_loads_from_meta_store() {
        let store = FakeMetaStore {
            value: Some(
                serde_json::json!({
                    "mode": "encrypted",
                    "passphrase": "secret-pass-123",
                    "pbkeylen": 24
                })
                .to_string(),
            ),
        };

        let config = load_global_srt_ingest_config(&store).await;

        assert_eq!(config.mode, SrtGlobalIngestMode::Encrypted);
        assert_eq!(config.passphrase.as_deref(), Some("secret-pass-123"));
        assert_eq!(config.pbkeylen, 24);
    }

    #[tokio::test]
    async fn invalid_global_srt_ingest_config_falls_back_to_default() {
        let store = FakeMetaStore {
            value: Some(
                serde_json::json!({
                    "mode": "encrypted",
                    "passphrase": "short",
                    "pbkeylen": 99
                })
                .to_string(),
            ),
        };

        let config = load_global_srt_ingest_config(&store).await;

        assert_eq!(config, SrtGlobalIngestConfig::default());
    }
}
