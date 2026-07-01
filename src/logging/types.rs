//! Shared logging DTOs and filter shapes exchanged between persistence, API,
//! and runtime logging code.

use serde::{Deserialize, Serialize};

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
    pub after_id: Option<i64>,
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
