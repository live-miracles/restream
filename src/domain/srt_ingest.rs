//! Domain model for SRT ingest security policy and resolution.

use serde::{Deserialize, Serialize};

pub const DEFAULT_SRT_PBKEYLEN: i32 = 16;

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
