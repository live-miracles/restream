//! Application-layer SRT ingest configuration loading and policy-store refresh
//! that connect persisted settings and pipeline catalogs to runtime enforcement.

use crate::application::ports::{MetaStore, PipelineCatalog, PipelineCatalogError};
use crate::domain::srt_ingest::{DEFAULT_SRT_PBKEYLEN, SrtGlobalIngestConfig, SrtGlobalIngestMode};
use crate::media::srt::SrtIngestPolicyStore;
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

pub async fn load_policy_store(
    meta_store: &dyn MetaStore,
    pipeline_catalog: &dyn PipelineCatalog,
) -> Result<SrtIngestPolicyStore, PipelineCatalogError> {
    let global = load_global_srt_ingest_config(meta_store).await;
    let pipelines = pipeline_catalog.list_pipelines().await?;
    Ok(SrtIngestPolicyStore::new(global, &pipelines))
}

pub async fn refresh_policy_store(
    policy_store: &SrtIngestPolicyStore,
    meta_store: &dyn MetaStore,
    pipeline_catalog: &dyn PipelineCatalog,
) -> Result<(), PipelineCatalogError> {
    let global = load_global_srt_ingest_config(meta_store).await;
    let pipelines = pipeline_catalog.list_pipelines().await?;
    policy_store.replace(global, &pipelines);
    Ok(())
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
    use crate::application::ports::{
        MetaLookupError, MetaLookupFuture, MetaStore, PipelineCatalog, PipelineCatalogFuture,
    };
    use crate::domain::srt_ingest::ResolvedSrtIngestConfig;
    use crate::media::srt::serialize_pipeline_srt_ingest_policy;
    use crate::types::Pipeline;

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

    struct FakePipelineCatalog {
        pipelines: Vec<Pipeline>,
    }

    impl PipelineCatalog for FakePipelineCatalog {
        fn list_pipelines<'a>(&'a self) -> PipelineCatalogFuture<'a> {
            Box::pin(async move { Ok(self.pipelines.clone()) })
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

    #[tokio::test]
    async fn load_policy_store_builds_store_from_meta_and_catalog() {
        let store = FakeMetaStore {
            value: Some(
                serde_json::json!({
                    "mode": "encrypted",
                    "passphrase": "global-pass-123",
                    "pbkeylen": 24
                })
                .to_string(),
            ),
        };
        let catalog = FakePipelineCatalog {
            pipelines: vec![Pipeline {
                id: "pipeline-1".to_string(),
                name: "Pipeline One".to_string(),
                stream_key: "stream-one".to_string(),
                input_source: None,
                encoding: None,
                srt_ingest_policy: Some(
                    serialize_pipeline_srt_ingest_policy(
                        &crate::domain::srt_ingest::SrtPipelineIngestConfig::default(),
                    )
                    .unwrap(),
                ),
            }],
        };

        let policy_store = load_policy_store(&store, &catalog).await.unwrap();

        assert_eq!(
            policy_store.global_config().mode,
            SrtGlobalIngestMode::Encrypted
        );
        assert_eq!(
            policy_store.resolved_policy("stream-one"),
            Some(ResolvedSrtIngestConfig::Encrypted {
                passphrase: "global-pass-123".to_string(),
                pbkeylen: 24,
            })
        );
    }

    #[tokio::test]
    async fn refresh_policy_store_replaces_existing_policies() {
        let initial_store = FakeMetaStore {
            value: Some(
                serde_json::json!({
                    "mode": "plaintext"
                })
                .to_string(),
            ),
        };
        let updated_store = FakeMetaStore {
            value: Some(
                serde_json::json!({
                    "mode": "encrypted",
                    "passphrase": "updated-pass-123",
                    "pbkeylen": 32
                })
                .to_string(),
            ),
        };
        let catalog = FakePipelineCatalog {
            pipelines: vec![Pipeline {
                id: "pipeline-1".to_string(),
                name: "Pipeline One".to_string(),
                stream_key: "stream-one".to_string(),
                input_source: None,
                encoding: None,
                srt_ingest_policy: Some(
                    serialize_pipeline_srt_ingest_policy(
                        &crate::domain::srt_ingest::SrtPipelineIngestConfig::default(),
                    )
                    .unwrap(),
                ),
            }],
        };
        let policy_store = load_policy_store(&initial_store, &catalog).await.unwrap();

        refresh_policy_store(&policy_store, &updated_store, &catalog)
            .await
            .unwrap();

        assert_eq!(
            policy_store.global_config().mode,
            SrtGlobalIngestMode::Encrypted
        );
        assert_eq!(
            policy_store.resolved_policy("stream-one"),
            Some(ResolvedSrtIngestConfig::Encrypted {
                passphrase: "updated-pass-123".to_string(),
                pbkeylen: 32,
            })
        );
    }
}
