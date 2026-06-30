//! Application-layer port traits and adapter types defining the storage and
//! catalog capabilities that orchestration code depends on.

use crate::types::{Ingest, Pipeline};
use sqlx::SqlitePool;
use std::fmt;
use std::future::Future;
use std::pin::Pin;

pub type PipelineLookupFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Option<Pipeline>, PipelineLookupError>> + Send + 'a>>;
pub type PipelineCatalogFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Vec<Pipeline>, PipelineCatalogError>> + Send + 'a>>;
pub type IngestLookupFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Option<Ingest>, IngestLookupError>> + Send + 'a>>;
pub type IngestCatalogFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Vec<Ingest>, IngestLookupError>> + Send + 'a>>;
pub type MetaLookupFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Option<String>, MetaLookupError>> + Send + 'a>>;
pub type MetaWriteFuture<'a> =
    Pin<Box<dyn Future<Output = Result<String, MetaLookupError>> + Send + 'a>>;

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
pub struct PipelineCatalogError {
    message: String,
}

impl PipelineCatalogError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for PipelineCatalogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for PipelineCatalogError {}

#[derive(Debug, Clone)]
pub struct IngestLookupError {
    message: String,
}

impl IngestLookupError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for IngestLookupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for IngestLookupError {}

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

pub trait PipelineCatalog: Send + Sync {
    fn list_pipelines<'a>(&'a self) -> PipelineCatalogFuture<'a>;
}

pub trait IngestLookup: Send + Sync {
    fn get_ingest<'a>(&'a self, id: &'a str) -> IngestLookupFuture<'a>;
    fn get_ingest_by_stream_key<'a>(&'a self, stream_key: &'a str) -> IngestLookupFuture<'a>;
    fn list_ingests_for_stream_key<'a>(&'a self, stream_key: &'a str) -> IngestCatalogFuture<'a>;
}

pub trait MetaStore: Send + Sync {
    fn get_meta<'a>(&'a self, key: &'a str) -> MetaLookupFuture<'a>;
}

pub trait MetaStoreWriter: Send + Sync {
    fn set_meta<'a>(&'a self, key: &'a str, value: &'a str) -> MetaWriteFuture<'a>;
}

#[derive(Clone)]
pub struct SqlitePipelineLookup {
    pool: SqlitePool,
}

#[derive(Clone)]
pub struct SqliteIngestLookup {
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

impl SqliteIngestLookup {
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

impl PipelineCatalog for SqlitePipelineLookup {
    fn list_pipelines<'a>(&'a self) -> PipelineCatalogFuture<'a> {
        Box::pin(async move {
            crate::db::list_pipelines(&self.pool)
                .await
                .map_err(|err| PipelineCatalogError::new(err.to_string()))
        })
    }
}

impl IngestLookup for SqliteIngestLookup {
    fn get_ingest<'a>(&'a self, id: &'a str) -> IngestLookupFuture<'a> {
        Box::pin(async move {
            crate::db::get_ingest(&self.pool, id)
                .await
                .map_err(|err| IngestLookupError::new(err.to_string()))
        })
    }

    fn get_ingest_by_stream_key<'a>(&'a self, stream_key: &'a str) -> IngestLookupFuture<'a> {
        Box::pin(async move {
            crate::db::get_ingest_by_stream_key(&self.pool, stream_key)
                .await
                .map_err(|err| IngestLookupError::new(err.to_string()))
        })
    }

    fn list_ingests_for_stream_key<'a>(&'a self, stream_key: &'a str) -> IngestCatalogFuture<'a> {
        Box::pin(async move {
            crate::db::list_ingests_for_stream_key(&self.pool, stream_key)
                .await
                .map_err(|err| IngestLookupError::new(err.to_string()))
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

impl MetaStoreWriter for SqliteMetaStore {
    fn set_meta<'a>(&'a self, key: &'a str, value: &'a str) -> MetaWriteFuture<'a> {
        Box::pin(async move {
            crate::db::set_meta(&self.pool, key, value)
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
    async fn sqlite_pipeline_catalog_lists_pipelines() {
        let pool = test_pool().await;
        crate::db::create_pipeline(&pool, "p1", "Pipeline One", "stream-one", None, None, None)
            .await
            .unwrap();
        crate::db::create_pipeline(&pool, "p2", "Pipeline Two", "stream-two", None, None, None)
            .await
            .unwrap();
        let lookup = SqlitePipelineLookup::new(pool);

        let pipelines = lookup.list_pipelines().await.unwrap();

        assert_eq!(pipelines.len(), 2);
        assert!(pipelines.iter().any(|pipeline| pipeline.id == "p1"));
        assert!(pipelines.iter().any(|pipeline| pipeline.id == "p2"));
    }

    #[tokio::test]
    async fn sqlite_ingest_lookup_reads_ingest_by_id_and_latest_stream_key_entry() {
        let pool = test_pool().await;
        crate::db::create_ingest(
            &pool,
            "i1",
            "clip.mp4",
            "stream-key",
            true,
            "00:00:05",
            true,
            4,
        )
        .await
        .unwrap();
        crate::db::create_ingest(
            &pool,
            "i2",
            "clip-latest.mp4",
            "stream-key",
            false,
            "00:00:10",
            false,
            2,
        )
        .await
        .unwrap();
        let lookup = SqliteIngestLookup::new(pool);

        let by_id = lookup.get_ingest("i1").await.unwrap();
        let by_stream_key = lookup.get_ingest_by_stream_key("stream-key").await.unwrap();

        assert_eq!(by_id.as_ref().map(|ingest| ingest.id.as_str()), Some("i1"));
        assert_eq!(
            by_stream_key.as_ref().map(|ingest| ingest.id.as_str()),
            Some("i2")
        );
    }

    #[tokio::test]
    async fn sqlite_ingest_lookup_lists_ingests_for_stream_key() {
        let pool = test_pool().await;
        crate::db::create_ingest(&pool, "i1", "clip.mp4", "stream-key", true, "", false, 2)
            .await
            .unwrap();
        crate::db::create_ingest(&pool, "i2", "clip-2.mp4", "other-key", false, "", false, 2)
            .await
            .unwrap();
        crate::db::create_ingest(&pool, "i3", "clip-3.mp4", "stream-key", false, "", false, 2)
            .await
            .unwrap();
        let lookup = SqliteIngestLookup::new(pool);

        let ingests = lookup
            .list_ingests_for_stream_key("stream-key")
            .await
            .unwrap();

        assert_eq!(ingests.len(), 2);
        assert_eq!(ingests[0].id, "i1");
        assert_eq!(ingests[1].id, "i3");
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

    #[tokio::test]
    async fn sqlite_meta_store_writes_meta_value() {
        let pool = test_pool().await;
        let store = SqliteMetaStore::new(pool.clone());

        store.set_meta("test-key", "test-value").await.unwrap();

        let value = crate::db::get_meta(&pool, "test-key").await.unwrap();
        assert_eq!(value.as_deref(), Some("test-value"));
    }
}
