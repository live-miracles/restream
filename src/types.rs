//! Shared domain types mapped to SQLite tables via `sqlx::FromRow`.
//! All structs use `#[serde(rename_all = "camelCase")]` for JSON API compatibility.

use serde::{Deserialize, Serialize};

pub const DEFAULT_SRT_PBKEYLEN: i32 = 16;

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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum SrtGlobalIngestMode {
    Plaintext,
    Encrypted,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SrtGlobalIngestConfig {
    pub mode: SrtGlobalIngestMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passphrase: Option<String>,
    #[serde(default = "default_srt_pbkeylen")]
    pub pbkeylen: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum SrtPipelineIngestMode {
    Inherit,
    Plaintext,
    Encrypted,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SrtPipelineIngestConfig {
    pub mode: SrtPipelineIngestMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passphrase: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pbkeylen: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedSrtIngestConfig {
    Plaintext,
    Encrypted { passphrase: String, pbkeylen: i32 },
}

fn default_srt_pbkeylen() -> i32 {
    DEFAULT_SRT_PBKEYLEN
}

impl Default for SrtGlobalIngestConfig {
    fn default() -> Self {
        Self {
            mode: SrtGlobalIngestMode::Plaintext,
            passphrase: None,
            pbkeylen: DEFAULT_SRT_PBKEYLEN,
        }
    }
}

impl Default for SrtPipelineIngestConfig {
    fn default() -> Self {
        Self {
            mode: SrtPipelineIngestMode::Inherit,
            passphrase: None,
            pbkeylen: None,
        }
    }
}

impl SrtGlobalIngestConfig {
    pub fn validate(&mut self) -> Result<(), String> {
        self.pbkeylen = normalize_srt_pbkeylen(self.pbkeylen)?;
        match self.mode {
            SrtGlobalIngestMode::Plaintext => {
                self.passphrase = None;
            }
            SrtGlobalIngestMode::Encrypted => {
                let passphrase = self
                    .passphrase
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| "SRT encrypted mode requires a passphrase".to_string())?;
                validate_srt_passphrase(passphrase)?;
                self.passphrase = Some(passphrase.to_string());
            }
        }
        Ok(())
    }

    pub fn resolve(&self) -> Result<ResolvedSrtIngestConfig, String> {
        match self.mode {
            SrtGlobalIngestMode::Plaintext => Ok(ResolvedSrtIngestConfig::Plaintext),
            SrtGlobalIngestMode::Encrypted => {
                let passphrase = self
                    .passphrase
                    .clone()
                    .ok_or_else(|| "missing SRT passphrase".to_string())?;
                Ok(ResolvedSrtIngestConfig::Encrypted {
                    passphrase,
                    pbkeylen: normalize_srt_pbkeylen(self.pbkeylen)?,
                })
            }
        }
    }
}

impl SrtPipelineIngestConfig {
    pub fn validate(&mut self) -> Result<(), String> {
        match self.mode {
            SrtPipelineIngestMode::Inherit | SrtPipelineIngestMode::Plaintext => {
                self.passphrase = None;
                self.pbkeylen = None;
            }
            SrtPipelineIngestMode::Encrypted => {
                let passphrase = self
                    .passphrase
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| {
                        "Per-pipeline encrypted SRT ingest requires a passphrase".to_string()
                    })?;
                validate_srt_passphrase(passphrase)?;
                self.passphrase = Some(passphrase.to_string());
                self.pbkeylen = Some(normalize_srt_pbkeylen(
                    self.pbkeylen.unwrap_or(DEFAULT_SRT_PBKEYLEN),
                )?);
            }
        }
        Ok(())
    }

    pub fn resolve(
        &self,
        global: &SrtGlobalIngestConfig,
    ) -> Result<ResolvedSrtIngestConfig, String> {
        match self.mode {
            SrtPipelineIngestMode::Inherit => global.resolve(),
            SrtPipelineIngestMode::Plaintext => Ok(ResolvedSrtIngestConfig::Plaintext),
            SrtPipelineIngestMode::Encrypted => Ok(ResolvedSrtIngestConfig::Encrypted {
                passphrase: self
                    .passphrase
                    .clone()
                    .ok_or_else(|| "missing per-pipeline SRT passphrase".to_string())?,
                pbkeylen: normalize_srt_pbkeylen(self.pbkeylen.unwrap_or(DEFAULT_SRT_PBKEYLEN))?,
            }),
        }
    }
}

fn validate_srt_passphrase(passphrase: &str) -> Result<(), String> {
    let len = passphrase.len();
    if !(10..=79).contains(&len) {
        return Err("SRT passphrase must be 10-79 bytes".to_string());
    }
    Ok(())
}

fn normalize_srt_pbkeylen(pbkeylen: i32) -> Result<i32, String> {
    match pbkeylen {
        16 | 24 | 32 => Ok(pbkeylen),
        _ => Err("SRT pbkeylen must be 16, 24, or 32".to_string()),
    }
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
    pub srt_ingest_policy_json: Option<String>,
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

/// Full row returned by /api/logs.
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

/// Filters for the /api/logs endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppLogFilters {
    pub level: Option<String>,
    pub since: Option<String>,
    pub until: Option<String>,
    pub target: Option<String>,
    pub pipeline_id: Option<String>,
    pub output_id: Option<String>,
    pub event_class: Option<String>,
    pub prefix: Option<String>,
    pub limit: Option<i64>,
    pub order: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_srt_ingest_plaintext_clears_secret() {
        let mut cfg = SrtGlobalIngestConfig {
            mode: SrtGlobalIngestMode::Plaintext,
            passphrase: Some("0123456789".to_string()),
            pbkeylen: 24,
        };
        cfg.validate().unwrap();
        assert_eq!(cfg.passphrase, None);
        assert_eq!(cfg.resolve().unwrap(), ResolvedSrtIngestConfig::Plaintext);
    }

    #[test]
    fn encrypted_pipeline_policy_overrides_global() {
        let mut global = SrtGlobalIngestConfig {
            mode: SrtGlobalIngestMode::Encrypted,
            passphrase: Some("global-pass-123".to_string()),
            pbkeylen: 16,
        };
        global.validate().unwrap();

        let mut pipeline = SrtPipelineIngestConfig {
            mode: SrtPipelineIngestMode::Encrypted,
            passphrase: Some("pipeline-pass-123".to_string()),
            pbkeylen: Some(32),
        };
        pipeline.validate().unwrap();

        assert_eq!(
            pipeline.resolve(&global).unwrap(),
            ResolvedSrtIngestConfig::Encrypted {
                passphrase: "pipeline-pass-123".to_string(),
                pbkeylen: 32,
            }
        );
    }

    #[test]
    fn inherit_pipeline_policy_uses_global() {
        let mut global = SrtGlobalIngestConfig {
            mode: SrtGlobalIngestMode::Encrypted,
            passphrase: Some("global-pass-123".to_string()),
            pbkeylen: 24,
        };
        global.validate().unwrap();
        let mut pipeline = SrtPipelineIngestConfig::default();
        pipeline.validate().unwrap();

        assert_eq!(
            pipeline.resolve(&global).unwrap(),
            ResolvedSrtIngestConfig::Encrypted {
                passphrase: "global-pass-123".to_string(),
                pbkeylen: 24,
            }
        );
    }
}
