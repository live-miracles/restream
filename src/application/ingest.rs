//! Application-layer ingest coordination that resolves pipelines, loads
//! file-ingest context, and validates stream access before media processing begins.

use crate::application::ports::{
    IngestLookup, IngestLookupError, PipelineStore, PipelineStoreError,
};
use crate::media::engine::MediaEngine;
use crate::media::security::IngestSecurityService;
use crate::types::{Ingest, Pipeline};

#[derive(Debug)]
pub enum IngestAuthError {
    InvalidStreamKey,
    LookupFailed(PipelineStoreError),
}

#[derive(Debug)]
pub struct FileIngestContext {
    pub ingest: Ingest,
    pub pipeline: Pipeline,
}

#[derive(Debug)]
pub struct PipelineFileIngestState {
    pub ingest: Option<Ingest>,
    pub running: bool,
}

#[derive(Debug)]
pub enum ResolveFileIngestError {
    IngestLookup(IngestLookupError),
    PipelineStore(PipelineStoreError),
    MissingPipelineForStreamKey(String),
}

pub async fn resolve_file_ingest_context(
    ingest_lookup: &dyn IngestLookup,
    pipeline_lookup: &dyn PipelineStore,
    ingest_id: &str,
) -> Result<Option<FileIngestContext>, ResolveFileIngestError> {
    let Some(ingest) = ingest_lookup
        .get_ingest(ingest_id)
        .await
        .map_err(ResolveFileIngestError::IngestLookup)?
    else {
        return Ok(None);
    };

    let pipeline = pipeline_lookup
        .get_pipeline_by_stream_key(&ingest.stream_key)
        .await
        .map_err(ResolveFileIngestError::PipelineStore)?
        .ok_or_else(|| {
            ResolveFileIngestError::MissingPipelineForStreamKey(ingest.stream_key.clone())
        })?;

    Ok(Some(FileIngestContext { ingest, pipeline }))
}

pub async fn load_pipeline_file_ingest_state(
    ingest_lookup: &dyn IngestLookup,
    engine: &MediaEngine,
    pipeline: &Pipeline,
) -> Result<PipelineFileIngestState, IngestLookupError> {
    let ingest = ingest_lookup
        .get_ingest_by_stream_key(&pipeline.stream_key)
        .await?;
    let running = match ingest.as_ref() {
        Some(ingest) => engine.is_file_ingest_running(&ingest.id).await,
        None => false,
    };

    Ok(PipelineFileIngestState { ingest, running })
}

pub async fn authenticate_publish_stream_key(
    pipeline_lookup: &dyn PipelineStore,
    security: &IngestSecurityService,
    stream_key: &str,
    client_ip: &str,
) -> Result<Pipeline, IngestAuthError> {
    match pipeline_lookup.get_pipeline_by_stream_key(stream_key).await {
        Ok(Some(pipeline)) => Ok(pipeline),
        Ok(None) => {
            security.record_failure(client_ip);
            Err(IngestAuthError::InvalidStreamKey)
        }
        Err(err) => {
            security.record_failure(client_ip);
            Err(IngestAuthError::LookupFailed(err))
        }
    }
}

pub async fn authenticate_srt_stream_key(
    pipeline_lookup: &dyn PipelineStore,
    security: &IngestSecurityService,
    stream_key: &str,
    client_ip: &str,
) -> Result<Pipeline, IngestAuthError> {
    let pipeline =
        authenticate_publish_stream_key(pipeline_lookup, security, stream_key, client_ip).await?;
    security.record_success(client_ip);
    Ok(pipeline)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::ports::{
        IngestCatalogFuture, IngestLookupFuture, PipelineLookupFuture,
    };
    use crate::domain::ingest_security::IngestSecurityConfig;
    use crate::media::security::IngestSecurityService;
    use std::collections::HashMap;
    use std::sync::Arc;

    fn test_security_config() -> IngestSecurityConfig {
        IngestSecurityConfig {
            failure_limit: 2,
            failure_window_ms: 60_000,
            ban_ms: 60_000,
            tracked_ip_limit: 100,
        }
    }

    struct FakePipelineStore {
        pipelines: HashMap<String, Pipeline>,
        error: Option<&'static str>,
    }

    impl FakePipelineStore {
        fn success(stream_key: &str) -> Self {
            let mut pipelines = HashMap::new();
            pipelines.insert(
                stream_key.to_string(),
                Pipeline {
                    id: "pipeline-1".to_string(),
                    name: "Pipeline".to_string(),
                    stream_key: stream_key.to_string(),
                    input_source: None,
                    encoding: None,
                    srt_ingest_policy: None,
                },
            );
            Self {
                pipelines,
                error: None,
            }
        }
    }

    impl PipelineStore for FakePipelineStore {
        fn get_pipeline_by_stream_key<'a>(
            &'a self,
            stream_key: &'a str,
        ) -> PipelineLookupFuture<'a> {
            Box::pin(async move {
                if let Some(message) = self.error {
                    return Err(PipelineStoreError::new(message));
                }
                Ok(self.pipelines.get(stream_key).cloned())
            })
        }

        fn list_pipelines<'a>(&'a self) -> crate::application::ports::PipelineListFuture<'a> {
            Box::pin(async move { Ok(self.pipelines.values().cloned().collect()) })
        }
    }

    struct FakeIngestLookup {
        by_id: HashMap<String, Ingest>,
        by_stream_key: HashMap<String, Vec<Ingest>>,
        error: Option<&'static str>,
    }

    impl FakeIngestLookup {
        fn ingest(id: &str, stream_key: &str) -> Ingest {
            Ingest {
                id: id.to_string(),
                filename: "clip.mp4".to_string(),
                stream_key: stream_key.to_string(),
                loop_flag: true,
                start_time: "00:00:05".to_string(),
                live_optimized: true,
                target_gop_seconds: 4,
            }
        }
    }

    impl IngestLookup for FakeIngestLookup {
        fn get_ingest<'a>(&'a self, id: &'a str) -> IngestLookupFuture<'a> {
            Box::pin(async move {
                if let Some(message) = self.error {
                    return Err(IngestLookupError::new(message));
                }
                Ok(self.by_id.get(id).cloned())
            })
        }

        fn get_ingest_by_stream_key<'a>(&'a self, stream_key: &'a str) -> IngestLookupFuture<'a> {
            Box::pin(async move {
                if let Some(message) = self.error {
                    return Err(IngestLookupError::new(message));
                }
                Ok(self
                    .by_stream_key
                    .get(stream_key)
                    .and_then(|ingests| ingests.last().cloned()))
            })
        }

        fn list_ingests_for_stream_key<'a>(
            &'a self,
            stream_key: &'a str,
        ) -> IngestCatalogFuture<'a> {
            Box::pin(async move {
                if let Some(message) = self.error {
                    return Err(IngestLookupError::new(message));
                }
                Ok(self
                    .by_stream_key
                    .get(stream_key)
                    .cloned()
                    .unwrap_or_default())
            })
        }
    }

    #[tokio::test]
    async fn publish_auth_records_failure_for_missing_stream_key() {
        let lookup = FakePipelineStore {
            pipelines: HashMap::new(),
            error: None,
        };
        let security = IngestSecurityService::new(test_security_config());

        let result =
            authenticate_publish_stream_key(&lookup, &security, "missing", "10.0.0.1").await;

        assert!(matches!(result, Err(IngestAuthError::InvalidStreamKey)));
        assert!(security.is_ip_banned("10.0.0.1").is_none());
        assert!(security.record_failure("10.0.0.1"));
    }

    #[tokio::test]
    async fn publish_auth_returns_pipeline_on_success() {
        let lookup = FakePipelineStore::success("live");
        let security = IngestSecurityService::new(test_security_config());

        let pipeline = authenticate_publish_stream_key(&lookup, &security, "live", "10.0.0.1")
            .await
            .unwrap();

        assert_eq!(pipeline.id, "pipeline-1");
    }

    #[tokio::test]
    async fn publish_auth_surfaces_lookup_error_and_records_failure() {
        let lookup = FakePipelineStore {
            pipelines: HashMap::new(),
            error: Some("db unavailable"),
        };
        let security = IngestSecurityService::new(test_security_config());
        let ip = "10.0.0.3";

        let result = authenticate_publish_stream_key(&lookup, &security, "live", ip).await;

        assert!(matches!(result, Err(IngestAuthError::LookupFailed(_))));
        assert!(security.is_ip_banned(ip).is_none());
        assert!(security.record_failure(ip));
    }

    #[tokio::test]
    async fn srt_auth_clears_failure_state_after_success() {
        let lookup = FakePipelineStore::success("live");
        let security = Arc::new(IngestSecurityService::new(test_security_config()));
        let ip = "10.0.0.2";

        assert!(!security.record_failure(ip));
        assert!(security.record_failure(ip));
        assert!(security.is_ip_banned(ip).is_some());

        let pipeline = authenticate_srt_stream_key(&lookup, &security, "live", ip)
            .await
            .unwrap();

        assert_eq!(pipeline.id, "pipeline-1");
        assert!(security.is_ip_banned(ip).is_none());
    }

    #[tokio::test]
    async fn resolve_file_ingest_context_returns_none_for_missing_ingest() {
        let ingest_lookup = FakeIngestLookup {
            by_id: HashMap::new(),
            by_stream_key: HashMap::new(),
            error: None,
        };
        let pipeline_lookup = FakePipelineStore {
            pipelines: HashMap::new(),
            error: None,
        };

        let result = resolve_file_ingest_context(&ingest_lookup, &pipeline_lookup, "missing")
            .await
            .unwrap();

        assert!(result.is_none());
    }

    #[tokio::test]
    async fn resolve_file_ingest_context_surfaces_missing_pipeline() {
        let ingest = FakeIngestLookup::ingest("ingest-1", "stream-key");
        let ingest_lookup = FakeIngestLookup {
            by_id: HashMap::from([(ingest.id.clone(), ingest.clone())]),
            by_stream_key: HashMap::from([("stream-key".to_string(), vec![ingest])]),
            error: None,
        };
        let pipeline_lookup = FakePipelineStore {
            pipelines: HashMap::new(),
            error: None,
        };

        let result =
            resolve_file_ingest_context(&ingest_lookup, &pipeline_lookup, "ingest-1").await;

        assert!(matches!(
            result,
            Err(ResolveFileIngestError::MissingPipelineForStreamKey(stream_key))
                if stream_key == "stream-key"
        ));
    }

    #[tokio::test]
    async fn resolve_file_ingest_context_returns_ingest_and_pipeline() {
        let ingest = FakeIngestLookup::ingest("ingest-1", "stream-key");
        let pipeline = Pipeline {
            id: "pipeline-1".to_string(),
            name: "Pipeline".to_string(),
            stream_key: "stream-key".to_string(),
            input_source: None,
            encoding: None,
            srt_ingest_policy: None,
        };
        let ingest_lookup = FakeIngestLookup {
            by_id: HashMap::from([(ingest.id.clone(), ingest.clone())]),
            by_stream_key: HashMap::from([("stream-key".to_string(), vec![ingest.clone()])]),
            error: None,
        };
        let pipeline_lookup = FakePipelineStore {
            pipelines: HashMap::from([("stream-key".to_string(), pipeline.clone())]),
            error: None,
        };

        let result = resolve_file_ingest_context(&ingest_lookup, &pipeline_lookup, "ingest-1")
            .await
            .unwrap()
            .unwrap();

        assert_eq!(result.ingest.id, "ingest-1");
        assert_eq!(result.pipeline.id, "pipeline-1");
    }

    #[tokio::test]
    async fn load_pipeline_file_ingest_state_returns_latest_ingest_and_running_flag() {
        let ingest = FakeIngestLookup::ingest("ingest-1", "stream-key");
        let lookup = FakeIngestLookup {
            by_id: HashMap::new(),
            by_stream_key: HashMap::from([("stream-key".to_string(), vec![ingest.clone()])]),
            error: None,
        };
        let engine = MediaEngine::new();
        engine.mark_file_ingest_running(&ingest.id).await;
        let pipeline = Pipeline {
            id: "pipeline-1".to_string(),
            name: "Pipeline".to_string(),
            stream_key: "stream-key".to_string(),
            input_source: None,
            encoding: None,
            srt_ingest_policy: None,
        };

        let state = load_pipeline_file_ingest_state(&lookup, &engine, &pipeline)
            .await
            .unwrap();

        assert_eq!(state.ingest.unwrap().id, "ingest-1");
        assert!(state.running);
    }
}
