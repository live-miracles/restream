//! Shared domain types mapped to SQLite tables via `sqlx::FromRow`.
//! All structs use `#[serde(rename_all = "camelCase")]` for JSON API compatibility.

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
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Output {
    pub id: String,
    pub pipeline_id: String,
    pub name: String,
    pub url: String,
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

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct JobLog {
    pub ts: String,
    pub message: String,
    pub event_type: String,
    pub event_data: Option<String>, // Serialized JSON or null
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryFilters {
    pub since: Option<String>,
    pub until: Option<String>,
    pub limit: Option<i64>,
    pub order: Option<String>, // "asc" | "desc"
    pub prefixes: Option<Vec<String>>,
}
