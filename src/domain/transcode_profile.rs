//! Domain model for named transcode profile settings.

use std::collections::HashMap;

/// Encoder settings for a single transcode profile.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct TranscodeProfile {
    /// x264 preset: ultrafast, superfast, veryfast, faster, fast, medium, slow, slower.
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
