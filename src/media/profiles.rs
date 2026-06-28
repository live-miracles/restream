//! Transcode profile configuration — stored in DB `meta` table, editable
//! via the settings API + frontend.
//!
//! Profiles are looked up by name (e.g. "h264", "720p") and control all
//! encoder settings. Stored as JSON in `meta.transcode_profiles`.
//! If the DB key is absent, built-in realtime defaults are used.
//!
//! Schema (stored as `meta.transcode_profiles` value):
//! ```json
//! {
//!   "h264": {
//!     "preset": "ultrafast",
//!     "tune": "zerolatency",
//!     "crf": 23,
//!     "gop": 60,
//!     "bframes": 0,
//!     "bitrate": 0,
//!     "maxBitrate": 0,
//!     "width": 0,
//!     "height": 0
//!   }
//! }
//! ```
//!
//! - `bitrate: 0` → CRF mode (constant quality, adapts to content)
//! - `width/height: 0` → passthrough (match source resolution)

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

/// Meta key in the DB `meta` table.
pub const META_KEY: &str = "transcode_profiles";

/// Encoder settings for a single transcode profile.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct TranscodeProfile {
    /// x264 preset: ultrafast, superfast, veryfast, faster, fast, medium, slow, slower
    #[serde(default = "default_preset")]
    pub preset: String,

    /// x264 tune: zerolatency, fastdecode, animation, film, etc.
    #[serde(default = "default_tune")]
    pub tune: String,

    /// CRF (constant quality) value. Used when bitrate == 0.
    /// Range 0-51, lower = higher quality. 23 is x264 default.
    #[serde(default = "default_crf")]
    pub crf: i32,

    /// GOP size (keyframe interval in frames).
    #[serde(default = "default_gop")]
    pub gop: u32,

    /// Max B-frames. 0 for realtime (no reordering, lowest latency).
    #[serde(default = "default_bframes")]
    pub bframes: usize,

    /// Target bitrate in bps. 0 = use CRF mode.
    #[serde(default)]
    pub bitrate: i64,

    /// Max bitrate in bps (for VBV). 0 = no VBV limit.
    #[serde(default, rename = "maxBitrate")]
    pub max_bitrate: i64,

    /// Output width. 0 = match source.
    #[serde(default)]
    pub width: u32,

    /// Output height. 0 = match source.
    #[serde(default)]
    pub height: u32,
}

fn default_preset() -> String {
    "ultrafast".to_string()
}
fn default_tune() -> String {
    "zerolatency".to_string()
}
fn default_crf() -> i32 {
    23
}
fn default_gop() -> u32 {
    60
}
fn default_bframes() -> usize {
    0
}

impl Default for TranscodeProfile {
    fn default() -> Self {
        Self {
            preset: default_preset(),
            tune: default_tune(),
            crf: default_crf(),
            gop: default_gop(),
            bframes: default_bframes(),
            bitrate: 0,
            max_bitrate: 0,
            width: 0,
            height: 0,
        }
    }
}

impl TranscodeProfile {
    pub fn validate(&self) -> Result<(), &'static str> {
        let valid_presets = [
            "ultrafast",
            "superfast",
            "veryfast",
            "faster",
            "fast",
            "medium",
            "slow",
            "slower",
            "veryslow",
            "placebo",
        ];
        if !valid_presets.contains(&self.preset.as_str()) {
            return Err(
                "preset must be one of: ultrafast, superfast, veryfast, faster, fast, medium, slow, slower, veryslow, placebo",
            );
        }
        let valid_tunes = [
            "",
            "film",
            "animation",
            "grain",
            "stillimage",
            "psnr",
            "ssim",
            "fastdecode",
            "zerolatency",
        ];
        if !valid_tunes.contains(&self.tune.as_str()) {
            return Err(
                "tune must be one of: film, animation, grain, stillimage, psnr, ssim, fastdecode, zerolatency, or empty",
            );
        }
        if !(0..=51).contains(&self.crf) {
            return Err("crf must be between 0 and 51");
        }
        Ok(())
    }
}

/// All profiles, keyed by name.
pub type TranscodeProfiles = HashMap<String, TranscodeProfile>;

/// Runtime cache of profiles. Loaded from DB at startup, updated when
/// the settings API patches the config. The transcoder reads from this
/// cache when initializing an encoder.
static PROFILES: std::sync::OnceLock<Arc<RwLock<TranscodeProfiles>>> = std::sync::OnceLock::new();

/// Get the global profiles cache (initializes on first call).
pub fn cache() -> &'static Arc<RwLock<TranscodeProfiles>> {
    PROFILES.get_or_init(|| Arc::new(RwLock::new(built_in_defaults())))
}

/// Return built-ins plus configured profiles, with configured profiles
/// overriding same-named built-ins.
pub fn effective_profiles(profiles: &TranscodeProfiles) -> TranscodeProfiles {
    let mut effective = built_in_defaults();
    for (name, profile) in profiles {
        effective.insert(name.clone(), profile.clone());
    }
    effective
}

/// Get the profile set currently exposed to API consumers and transcoders.
pub async fn current_effective() -> TranscodeProfiles {
    let cache = cache().read().await;
    effective_profiles(&cache)
}

/// Load profiles from the DB `meta` table and update the runtime cache.
/// Called at startup and when the settings API patches the config.
pub async fn load_from_db(pool: &sqlx::SqlitePool) {
    let profiles = match crate::db::get_meta(pool, META_KEY).await {
        Ok(Some(json_str)) => match serde_json::from_str::<TranscodeProfiles>(&json_str) {
            Ok(p) if !p.is_empty() => {
                info!(count = p.len(), "loaded profiles from DB");
                p
            }
            Ok(_) => {
                warn!("DB has empty profiles, using defaults");
                built_in_defaults()
            }
            Err(e) => {
                warn!(err = %e, "failed to parse DB profiles, using defaults");
                built_in_defaults()
            }
        },
        _ => {
            info!("no DB profiles found, using built-in defaults");
            built_in_defaults()
        }
    };

    let mut cache = cache().write().await;
    *cache = profiles;
}

/// Save profiles to the DB and update the runtime cache.
/// Called by the settings API PATCH handler.
pub async fn save_to_db(
    pool: &sqlx::SqlitePool,
    profiles: &TranscodeProfiles,
) -> Result<(), String> {
    let json = serde_json::to_string(profiles).map_err(|e| e.to_string())?;
    crate::db::set_meta(pool, META_KEY, &json)
        .await
        .map_err(|e| e.to_string())?;

    let mut cache = cache().write().await;
    *cache = effective_profiles(profiles);
    info!(count = profiles.len(), "updated profiles in DB + cache");
    Ok(())
}

/// Get a profile by name. Falls back to "h264", then to default.
/// Called by the transcoder when initializing an encoder.
pub async fn get(name: &str) -> TranscodeProfile {
    let cache = cache().read().await;
    cache
        .get(name)
        .or_else(|| cache.get("h264"))
        .cloned()
        .unwrap_or_default()
}

/// Built-in realtime defaults. Used when no DB config is present.
/// All settings are optimized for live streaming: lowest latency, no reordering.
pub fn built_in_defaults() -> TranscodeProfiles {
    let mut profiles = HashMap::new();

    // H.265→H.264 transcode: same resolution, CRF mode
    profiles.insert(
        "h264".to_string(),
        TranscodeProfile {
            preset: "ultrafast".into(),
            tune: "zerolatency".into(),
            crf: 23,
            gop: 60,
            bframes: 0,
            bitrate: 0,
            max_bitrate: 0,
            width: 0,
            height: 0,
        },
    );

    // 720p preset
    profiles.insert(
        "720p".to_string(),
        TranscodeProfile {
            preset: "ultrafast".into(),
            tune: "zerolatency".into(),
            crf: 23,
            gop: 60,
            bframes: 0,
            bitrate: 0,
            max_bitrate: 0,
            width: 1280,
            height: 720,
        },
    );

    // 1080p preset
    profiles.insert(
        "1080p".to_string(),
        TranscodeProfile {
            preset: "ultrafast".into(),
            tune: "zerolatency".into(),
            crf: 23,
            gop: 60,
            bframes: 0,
            bitrate: 0,
            max_bitrate: 0,
            width: 1920,
            height: 1080,
        },
    );

    profiles
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_realtime() {
        let p = TranscodeProfile::default();
        assert_eq!(p.preset, "ultrafast");
        assert_eq!(p.tune, "zerolatency");
        assert_eq!(p.bframes, 0);
        assert_eq!(p.bitrate, 0); // CRF mode
    }

    #[test]
    fn built_in_has_h264_and_720p() {
        let profiles = built_in_defaults();
        assert!(profiles.contains_key("h264"));
        assert!(profiles.contains_key("720p"));
        assert!(profiles.contains_key("1080p"));
    }

    #[test]
    fn empty_profiles_resolve_to_built_ins() {
        let profiles = TranscodeProfiles::new();
        let effective = effective_profiles(&profiles);
        assert!(effective.contains_key("h264"));
        assert!(effective.contains_key("720p"));
        assert!(effective.contains_key("1080p"));
    }

    #[test]
    fn configured_profiles_extend_and_override_built_ins() {
        let mut profiles = TranscodeProfiles::new();
        profiles.insert(
            "custom_4k".to_string(),
            TranscodeProfile {
                width: 3840,
                height: 2160,
                ..TranscodeProfile::default()
            },
        );
        profiles.insert(
            "720p".to_string(),
            TranscodeProfile {
                crf: 20,
                width: 1280,
                height: 720,
                ..TranscodeProfile::default()
            },
        );

        let effective = effective_profiles(&profiles);
        assert_eq!(effective["720p"].crf, 20);
        assert_eq!(effective["custom_4k"].width, 3840);
        assert!(effective.contains_key("h264"));
        assert!(effective.contains_key("1080p"));
    }

    #[test]
    fn serialize_deserialize_roundtrip() {
        let mut profiles = built_in_defaults();
        profiles.insert(
            "custom".to_string(),
            TranscodeProfile {
                preset: "veryfast".into(),
                tune: "film".into(),
                crf: 18,
                gop: 120,
                bframes: 2,
                bitrate: 15000000,
                max_bitrate: 20000000,
                width: 3840,
                height: 2160,
            },
        );

        let json = serde_json::to_string(&profiles).unwrap();
        let parsed: TranscodeProfiles = serde_json::from_str(&json).unwrap();

        let custom = parsed.get("custom").unwrap();
        assert_eq!(custom.preset, "veryfast");
        assert_eq!(custom.crf, 18);
        assert_eq!(custom.bitrate, 15000000);
        assert_eq!(custom.width, 3840);

        // Defaults still present
        assert!(parsed.contains_key("h264"));
    }

    #[test]
    fn partial_json_uses_defaults() {
        // Only specify preset + crf, rest should default
        let json = r#"{"test": {"preset": "slow", "crf": 18}}"#;
        let parsed: TranscodeProfiles = serde_json::from_str(json).unwrap();
        let p = parsed.get("test").unwrap();
        assert_eq!(p.preset, "slow");
        assert_eq!(p.crf, 18);
        assert_eq!(p.tune, "zerolatency"); // defaulted
        assert_eq!(p.bframes, 0); // defaulted
        assert_eq!(p.gop, 60); // defaulted
    }

    #[test]
    fn validate_all_valid_presets_pass() {
        for preset in [
            "ultrafast",
            "superfast",
            "veryfast",
            "faster",
            "fast",
            "medium",
            "slow",
            "slower",
            "veryslow",
            "placebo",
        ] {
            let p = TranscodeProfile {
                preset: preset.into(),
                ..Default::default()
            };
            assert!(p.validate().is_ok(), "preset '{preset}' should be valid");
        }
    }

    #[test]
    fn validate_invalid_preset_rejected() {
        let p = TranscodeProfile {
            preset: "bogus".into(),
            ..Default::default()
        };
        assert!(p.validate().is_err());
    }

    #[test]
    fn validate_invalid_tune_rejected() {
        let p = TranscodeProfile {
            tune: "bogus".into(),
            ..Default::default()
        };
        assert!(p.validate().is_err());
    }

    #[test]
    fn validate_empty_tune_passes() {
        let p = TranscodeProfile {
            tune: String::new(),
            ..Default::default()
        };
        assert!(p.validate().is_ok());
    }

    #[test]
    fn validate_crf_boundaries() {
        assert!(
            TranscodeProfile {
                crf: 0,
                ..Default::default()
            }
            .validate()
            .is_ok()
        );
        assert!(
            TranscodeProfile {
                crf: 51,
                ..Default::default()
            }
            .validate()
            .is_ok()
        );
        assert!(
            TranscodeProfile {
                crf: -1,
                ..Default::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            TranscodeProfile {
                crf: 52,
                ..Default::default()
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn validate_default_passes() {
        assert!(TranscodeProfile::default().validate().is_ok());
    }

    #[test]
    fn builtin_720p_has_correct_dimensions() {
        let profiles = built_in_defaults();
        let p = &profiles["720p"];
        assert_eq!(p.width, 1280);
        assert_eq!(p.height, 720);
    }

    #[test]
    fn builtin_h264_is_passthrough() {
        let profiles = built_in_defaults();
        let p = &profiles["h264"];
        assert_eq!(p.width, 0);
        assert_eq!(p.height, 0);
        assert_eq!(p.preset, "ultrafast");
        assert_eq!(p.tune, "zerolatency");
    }
}
