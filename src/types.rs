//! Shared domain types mapped to SQLite tables via `sqlx::FromRow`.
//! All structs use `#[serde(rename_all = "camelCase")]` for the JSON API.

pub use crate::domain::ingest_security::IngestSecurityConfig;
pub use crate::domain::srt_ingest::{
    DEFAULT_SRT_PBKEYLEN, ResolvedSrtIngestConfig, SrtGlobalIngestConfig, SrtGlobalIngestMode,
    SrtPipelineIngestConfig, SrtPipelineIngestMode,
};
pub use crate::logging::types::{AppLogEntry, AppLogFilters, AppLogRow};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Ingest {
    pub id: String,
    pub filename: String,
    pub stream_key: String,
    #[serde(rename = "loop")]
    #[sqlx(rename = "loop")]
    pub loop_flag: bool,
    pub start_time: String,
    #[serde(default)]
    pub live_optimized: bool,
    #[serde(default = "default_file_ingest_target_gop_seconds")]
    pub target_gop_seconds: u32,
}

pub const DEFAULT_FILE_INGEST_TARGET_GOP_SECONDS: u32 = 2;

pub fn default_file_ingest_target_gop_seconds() -> u32 {
    DEFAULT_FILE_INGEST_TARGET_GOP_SECONDS
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Pipeline {
    pub id: String,
    pub name: String,
    pub stream_key: String,
    pub input_source: Option<String>,
    pub encoding: Option<String>,
    #[serde(skip_serializing, skip_deserializing)]
    pub srt_ingest_policy: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Output {
    pub id: String,
    pub pipeline_id: String,
    pub name: String,
    pub url: String,
    pub monitoring_url: Option<String>,
    pub desired_state: String, // "running" | "stopped"
    pub encoding: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Job {
    pub id: String,
    pub pipeline_id: String,
    pub output_id: String,
    pub pid: Option<i64>,
    pub status: String, // "running" | "stopped" | "failed"
    pub started_at: String,
    pub ended_at: Option<String>,
    pub exit_code: Option<i64>,
    pub exit_signal: Option<String>,
}
