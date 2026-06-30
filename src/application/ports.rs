use crate::types::Pipeline;
use sqlx::SqlitePool;
use std::fmt;
use std::future::Future;
use std::pin::Pin;

pub type PipelineLookupFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Option<Pipeline>, PipelineLookupError>> + Send + 'a>>;

#[derive(Debug, Clone)]
pub struct PipelineLookupError {
    message: String,
}

impl PipelineLookupError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for PipelineLookupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for PipelineLookupError {}

pub trait PipelineLookup: Send + Sync {
    fn get_pipeline_by_stream_key<'a>(&'a self, stream_key: &'a str) -> PipelineLookupFuture<'a>;
}

#[derive(Clone)]
pub struct SqlitePipelineLookup {
    pool: SqlitePool,
}

impl SqlitePipelineLookup {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

impl PipelineLookup for SqlitePipelineLookup {
    fn get_pipeline_by_stream_key<'a>(&'a self, stream_key: &'a str) -> PipelineLookupFuture<'a> {
        Box::pin(async move {
            crate::db::get_pipeline_by_stream_key(&self.pool, stream_key)
                .await
                .map_err(|err| PipelineLookupError::new(err.to_string()))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_pool() -> SqlitePool {
        let pool = crate::db::create_pool("sqlite::memory:").await.unwrap();
        crate::db::setup_database_schema(&pool).await.unwrap();
        pool
    }

    #[tokio::test]
    async fn sqlite_pipeline_lookup_returns_pipeline_for_stream_key() {
        let pool = test_pool().await;
        crate::db::create_pipeline(&pool, "p1", "Pipeline", "stream-key", None, None, None)
            .await
            .unwrap();
        let lookup = SqlitePipelineLookup::new(pool);

        let pipeline = lookup
            .get_pipeline_by_stream_key("stream-key")
            .await
            .unwrap();

        assert_eq!(pipeline.unwrap().id, "p1");
    }

    #[tokio::test]
    async fn sqlite_pipeline_lookup_returns_none_for_missing_stream_key() {
        let pool = test_pool().await;
        let lookup = SqlitePipelineLookup::new(pool);

        let pipeline = lookup.get_pipeline_by_stream_key("missing").await.unwrap();

        assert!(pipeline.is_none());
    }
}
