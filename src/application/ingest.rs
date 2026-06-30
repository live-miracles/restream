use crate::application::ports::{PipelineLookup, PipelineLookupError};
use crate::media::security::IngestSecurityService;
use crate::types::Pipeline;

#[derive(Debug)]
pub enum IngestAuthError {
    InvalidStreamKey,
    LookupFailed(PipelineLookupError),
}

pub async fn lookup_pipeline_by_stream_key(
    pipeline_lookup: &dyn PipelineLookup,
    stream_key: &str,
) -> Result<Option<Pipeline>, PipelineLookupError> {
    pipeline_lookup.get_pipeline_by_stream_key(stream_key).await
}

pub async fn authenticate_publish_stream_key(
    pipeline_lookup: &dyn PipelineLookup,
    security: &IngestSecurityService,
    stream_key: &str,
    client_ip: &str,
) -> Result<Pipeline, IngestAuthError> {
    match lookup_pipeline_by_stream_key(pipeline_lookup, stream_key).await {
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
    pipeline_lookup: &dyn PipelineLookup,
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
    use crate::application::ports::PipelineLookupFuture;
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

    struct FakePipelineLookup {
        pipelines: HashMap<String, Pipeline>,
        error: Option<&'static str>,
    }

    impl FakePipelineLookup {
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
    async fn publish_auth_records_failure_for_missing_stream_key() {
        let lookup = FakePipelineLookup {
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
        let lookup = FakePipelineLookup::success("live");
        let security = IngestSecurityService::new(test_security_config());

        let pipeline = authenticate_publish_stream_key(&lookup, &security, "live", "10.0.0.1")
            .await
            .unwrap();

        assert_eq!(pipeline.id, "pipeline-1");
    }

    #[tokio::test]
    async fn srt_auth_clears_failure_state_after_success() {
        let lookup = FakePipelineLookup::success("live");
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
}
