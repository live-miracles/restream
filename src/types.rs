//! Shared domain types mapped to SQLite tables via `sqlx::FromRow`.
//! All structs use `#[serde(rename_all = "camelCase")]` for the JSON API.

pub use crate::domain::srt_ingest::{
    DEFAULT_SRT_PBKEYLEN, ResolvedSrtIngestConfig, SrtGlobalIngestConfig, SrtGlobalIngestMode,
    SrtPipelineIngestConfig, SrtPipelineIngestMode,
};
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IngestSecurityConfig {
    pub failure_limit: i64,
    pub failure_window_ms: i64,
    pub ban_ms: i64,
    pub tracked_ip_limit: i64,
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

/// Full row returned by /api/v1/logs.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct AppLogRow {
    pub id: i64,
    pub ts: String,
    pub level: String,
    pub target: String,
    pub message: String,
    pub fields: Option<String>,
    pub pipeline_id: Option<String>,
    pub output_id: Option<String>,
    pub event_type: Option<String>,
}

/// Entry written by the DbLayer drain task.
#[derive(Debug, Clone)]
pub struct AppLogEntry {
    pub ts: String,
    pub level: String,
    pub target: String,
    pub message: String,
    pub fields: Option<String>,
    pub pipeline_id: Option<String>,
    pub output_id: Option<String>,
    pub event_type: Option<String>,
    pub event_class: Option<String>,
}

/// Filters for the /api/v1/logs endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppLogFilters {
    pub level: Option<String>,
    pub since: Option<String>,
    pub until: Option<String>,
    pub target: Option<String>,
    pub scope: Option<String>,
    pub pipeline_id: Option<String>,
    pub output_id: Option<String>,
    pub event_class: Option<String>,
    pub prefix: Option<String>,
    pub limit: Option<i64>,
    pub order: Option<String>,
}
