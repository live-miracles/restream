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
