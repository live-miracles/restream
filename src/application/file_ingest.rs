use crate::application::ports::{
    IngestLookup, IngestLookupError, PipelineLookup, PipelineLookupError,
};
use crate::types::{Ingest, Pipeline};

#[derive(Debug)]
pub struct FileIngestContext {
    pub ingest: Ingest,
    pub pipeline: Pipeline,
}

#[derive(Debug)]
pub enum ResolveFileIngestError {
    IngestLookup(IngestLookupError),
    PipelineLookup(PipelineLookupError),
    MissingPipelineForStreamKey(String),
}

pub async fn load_file_ingest_by_id(
    ingest_lookup: &dyn IngestLookup,
    ingest_id: &str,
) -> Result<Option<Ingest>, IngestLookupError> {
    ingest_lookup.get_ingest(ingest_id).await
}

pub async fn load_configured_file_ingest(
    ingest_lookup: &dyn IngestLookup,
    stream_key: &str,
) -> Result<Option<Ingest>, IngestLookupError> {
    ingest_lookup.get_ingest_by_stream_key(stream_key).await
}

pub async fn list_file_ingests_for_stream_key(
    ingest_lookup: &dyn IngestLookup,
    stream_key: &str,
) -> Result<Vec<Ingest>, IngestLookupError> {
    ingest_lookup.list_ingests_for_stream_key(stream_key).await
}

pub async fn resolve_file_ingest_context(
    ingest_lookup: &dyn IngestLookup,
    pipeline_lookup: &dyn PipelineLookup,
    ingest_id: &str,
) -> Result<Option<FileIngestContext>, ResolveFileIngestError> {
    let Some(ingest) = load_file_ingest_by_id(ingest_lookup, ingest_id)
        .await
        .map_err(ResolveFileIngestError::IngestLookup)?
    else {
        return Ok(None);
    };

    let pipeline = pipeline_lookup
        .get_pipeline_by_stream_key(&ingest.stream_key)
        .await
        .map_err(ResolveFileIngestError::PipelineLookup)?
        .ok_or_else(|| {
            ResolveFileIngestError::MissingPipelineForStreamKey(ingest.stream_key.clone())
        })?;

    Ok(Some(FileIngestContext { ingest, pipeline }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::ports::{
        IngestCatalogFuture, IngestLookupFuture, PipelineLookupFuture,
    };
    use std::collections::HashMap;

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

    struct FakePipelineLookup {
        pipelines: HashMap<String, Pipeline>,
        error: Option<&'static str>,
    }

    impl PipelineLookup for FakePipelineLookup {
        fn get_pipeline_by_stream_key<'a>(
            &'a self,
            stream_key: &'a str,
        ) -> PipelineLookupFuture<'a> {
            Box::pin(async move {
                if let Some(message) = self.error {
                    return Err(PipelineLookupError::new(message));
                }
                Ok(self.pipelines.get(stream_key).cloned())
            })
        }
    }

    #[tokio::test]
    async fn resolve_file_ingest_context_returns_none_for_missing_ingest() {
        let ingest_lookup = FakeIngestLookup {
            by_id: HashMap::new(),
            by_stream_key: HashMap::new(),
            error: None,
        };
        let pipeline_lookup = FakePipelineLookup {
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
        let pipeline_lookup = FakePipelineLookup {
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
        let pipeline_lookup = FakePipelineLookup {
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
    async fn list_file_ingests_returns_stream_key_slice() {
        let first = FakeIngestLookup::ingest("ingest-1", "stream-key");
        let second = FakeIngestLookup::ingest("ingest-2", "stream-key");
        let ingest_lookup = FakeIngestLookup {
            by_id: HashMap::from([
                (first.id.clone(), first.clone()),
                (second.id.clone(), second.clone()),
            ]),
            by_stream_key: HashMap::from([(
                "stream-key".to_string(),
                vec![first.clone(), second.clone()],
            )]),
            error: None,
        };

        let ingests = list_file_ingests_for_stream_key(&ingest_lookup, "stream-key")
            .await
            .unwrap();

        assert_eq!(ingests.len(), 2);
        assert_eq!(ingests[0].id, "ingest-1");
        assert_eq!(ingests[1].id, "ingest-2");
    }
}
