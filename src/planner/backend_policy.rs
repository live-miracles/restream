//! Backend selection for runtime stages.
//!
//! The engine owns stage lifecycles; this module owns the policy choice for how
//! a typed stage should run.

use crate::domain::audio_routing::{AudioRouting, parse_audio_routing};
use crate::domain::stage::StageKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageBackend {
    AudioRouter,
    InternalFfmpeg,
    ExternalFfmpeg,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackendPolicy {
    pub use_internal_transcoder: bool,
}

impl BackendPolicy {
    pub fn from_env() -> Self {
        Self {
            use_internal_transcoder: std::env::var("RESTREAM_USE_INTERNAL_TRANSCODER")
                .map(|value| {
                    matches!(
                        value.trim().to_ascii_lowercase().as_str(),
                        "1" | "true" | "yes" | "on"
                    )
                })
                .unwrap_or(false),
        }
    }

    pub fn select_backend(&self, stage: &StageKind) -> StageBackend {
        match stage {
            StageKind::AudioRoute { operation, .. } => {
                let routing = parse_audio_routing(&format!("source+{operation}"));
                if is_lightweight_audio_route(&routing) {
                    StageBackend::AudioRouter
                } else {
                    StageBackend::ExternalFfmpeg
                }
            }
            StageKind::VideoPreset { .. } if self.use_internal_transcoder => {
                StageBackend::InternalFfmpeg
            }
            StageKind::VideoPreset { .. } => StageBackend::ExternalFfmpeg,
            _ if self.use_internal_transcoder => StageBackend::InternalFfmpeg,
            _ => StageBackend::ExternalFfmpeg,
        }
    }
}

pub fn is_lightweight_audio_route(routing: &AudioRouting) -> bool {
    matches!(
        routing,
        AudioRouting::SelectTracks(_) | AudioRouting::Passthrough
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_audio_router_for_lightweight_audio_routes() {
        let policy = BackendPolicy {
            use_internal_transcoder: false,
        };
        let stage = StageKind::audio_route("atrack:0", StageKind::source());

        assert_eq!(policy.select_backend(&stage), StageBackend::AudioRouter);
    }

    #[test]
    fn selects_external_ffmpeg_for_downmix_audio_routes() {
        let policy = BackendPolicy {
            use_internal_transcoder: false,
        };
        let stage = StageKind::audio_route("downmix:0", StageKind::source());

        assert_eq!(policy.select_backend(&stage), StageBackend::ExternalFfmpeg);
    }

    #[test]
    fn selects_external_ffmpeg_for_channel_remap_routes() {
        let policy = BackendPolicy {
            use_internal_transcoder: false,
        };
        let stage = StageKind::audio_route("remap:0:1", StageKind::source());

        assert_eq!(policy.select_backend(&stage), StageBackend::ExternalFfmpeg);
    }

    #[test]
    fn selects_external_ffmpeg_for_video_by_default() {
        let policy = BackendPolicy {
            use_internal_transcoder: false,
        };

        assert_eq!(
            policy.select_backend(&StageKind::video_preset("720p")),
            StageBackend::ExternalFfmpeg
        );
    }

    #[test]
    fn selects_internal_ffmpeg_for_video_when_enabled() {
        let policy = BackendPolicy {
            use_internal_transcoder: true,
        };

        assert_eq!(
            policy.select_backend(&StageKind::video_preset("720p")),
            StageBackend::InternalFfmpeg
        );
    }

    #[test]
    fn selects_internal_ffmpeg_for_codec_edges_when_enabled() {
        let policy = BackendPolicy {
            use_internal_transcoder: true,
        };

        assert_eq!(
            policy.select_backend(&StageKind::codec_edge("hevc_to_h264", StageKind::source())),
            StageBackend::InternalFfmpeg
        );
    }
}
