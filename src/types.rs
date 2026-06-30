//! Shared DTOs and persistence row shapes that are still used across storage,
//! orchestration, and API layers.
//! This file is intentionally narrower than the domain modules: reusable policy
//! configs are re-exported from `crate::domain`, while these structs remain the
//! cross-layer records that still back SQLite rows and JSON payloads.

pub use crate::logging::types::{AppLogEntry, AppLogFilters, AppLogRow};
use serde::{Deserialize, Serialize};
use std::str::FromStr;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobStatus {
    Running,
    Stopped,
    Failed,
}

impl JobStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
        }
    }

    pub const fn is_running(self) -> bool {
        matches!(self, Self::Running)
    }
}

impl FromStr for JobStatus {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "running" => Ok(Self::Running),
            "stopped" => Ok(Self::Stopped),
            "failed" => Ok(Self::Failed),
            _ => Err("unknown job status"),
        }
    }
}

impl TryFrom<&str> for JobStatus {
    type Error = &'static str;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::from_str(value)
    }
}

impl Job {
    pub fn status_typed(&self) -> Option<JobStatus> {
        JobStatus::try_from(self.status.as_str()).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_status_round_trips_through_strings() {
        assert_eq!(JobStatus::Running.as_str(), "running");
        assert_eq!(JobStatus::Stopped.as_str(), "stopped");
        assert_eq!(JobStatus::Failed.as_str(), "failed");
        assert_eq!(JobStatus::try_from("running"), Ok(JobStatus::Running));
        assert_eq!(JobStatus::try_from("stopped"), Ok(JobStatus::Stopped));
        assert_eq!(JobStatus::try_from("failed"), Ok(JobStatus::Failed));
        assert!(JobStatus::try_from("retrying").is_err());
    }

    #[test]
    fn job_status_accessor_parses_known_status() {
        let job = Job {
            id: "job-1".to_string(),
            pipeline_id: "pipe".to_string(),
            output_id: "out".to_string(),
            pid: Some(42),
            status: "running".to_string(),
            started_at: "2024-01-01T00:00:00Z".to_string(),
            ended_at: None,
            exit_code: None,
            exit_signal: None,
        };

        assert_eq!(job.status_typed(), Some(JobStatus::Running));
    }
}
