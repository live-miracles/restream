use crate::types::Pipeline;
use sqlx::SqlitePool;
use std::fmt;
use std::future::Future;
use std::pin::Pin;

pub type PipelineLookupFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Option<Pipeline>, PipelineLookupError>> + Send + 'a>>;
pub type MetaLookupFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Option<String>, MetaLookupError>> + Send + 'a>>;

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

#[derive(Debug, Clone)]
pub struct MetaLookupError {
    message: String,
}

impl MetaLookupError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for MetaLookupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for MetaLookupError {}

pub trait PipelineLookup: Send + Sync {
    fn get_pipeline_by_stream_key<'a>(&'a self, stream_key: &'a str) -> PipelineLookupFuture<'a>;
}

pub trait MetaStore: Send + Sync {
    fn get_meta<'a>(&'a self, key: &'a str) -> MetaLookupFuture<'a>;
}

#[derive(Clone)]
pub struct SqlitePipelineLookup {
    pool: SqlitePool,
}

#[derive(Clone)]
pub struct SqliteMetaStore {
    pool: SqlitePool,
}

impl SqlitePipelineLookup {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

impl SqliteMetaStore {
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

impl MetaStore for SqliteMetaStore {
    fn get_meta<'a>(&'a self, key: &'a str) -> MetaLookupFuture<'a> {
        Box::pin(async move {
            crate::db::get_meta(&self.pool, key)
                .await
                .map_err(|err| MetaLookupError::new(err.to_string()))
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

    #[tokio::test]
    async fn sqlite_meta_store_returns_meta_value() {
        let pool = test_pool().await;
        crate::db::set_meta(&pool, "test-key", "test-value")
            .await
            .unwrap();
        let store = SqliteMetaStore::new(pool);

        let value = store.get_meta("test-key").await.unwrap();

        assert_eq!(value.as_deref(), Some("test-value"));
    }
}
