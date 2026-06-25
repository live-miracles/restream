//! Typed stage identity and output encoding planning.
//!
//! The runtime still serializes stage keys with the legacy strings used by
//! metrics and cleanup maps. This module centralizes that compatibility layer
//! so orchestration code can reason in terms of explicit stage kinds.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct PipelineId(String);

impl PipelineId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PipelineId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for PipelineId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for PipelineId {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct StageId(String);

impl StageId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for StageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct WorkerId(String);

impl WorkerId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for WorkerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum StageKind {
    Source,
    VideoPreset {
        preset: String,
    },
    AudioRoute {
        operation: String,
        upstream: Box<StageKind>,
    },
    CodecEdge {
        operation: String,
        upstream: Box<StageKind>,
    },
    Unknown {
        key: String,
    },
}

impl StageKind {
    pub fn source() -> Self {
        Self::Source
    }

    pub fn video_preset(preset: impl Into<String>) -> Self {
        Self::VideoPreset {
            preset: preset.into(),
        }
    }

    pub fn audio_route(operation: impl Into<String>, upstream: StageKind) -> Self {
        Self::AudioRoute {
            operation: operation.into(),
            upstream: Box::new(upstream),
        }
    }

    pub fn codec_edge(operation: impl Into<String>, upstream: StageKind) -> Self {
        Self::CodecEdge {
            operation: operation.into(),
            upstream: Box::new(upstream),
        }
    }

    pub fn parse_legacy_key(key: &str) -> Self {
        if key == "source" {
            return Self::Source;
        }
        if let Some(preset) = key.strip_prefix("video:") {
            return Self::video_preset(preset);
        }
        if let Some(rest) = key.strip_prefix("audio:")
            && let Some((operation, upstream)) = rest.rsplit_once(":from:")
        {
            return Self::audio_route(operation, Self::parse_upstream_ref(upstream));
        }
        if let Some(upstream) = key.strip_prefix("hevc_to_h264:from:") {
            return Self::codec_edge("hevc_to_h264", Self::parse_upstream_ref(upstream));
        }
        Self::Unknown {
            key: key.to_string(),
        }
    }

    pub fn legacy_key(&self) -> String {
        match self {
            Self::Source => "source".to_string(),
            Self::VideoPreset { preset } => format!("video:{preset}"),
            Self::AudioRoute {
                operation,
                upstream,
            } => {
                format!("audio:{operation}:from:{}", upstream.legacy_ref())
            }
            Self::CodecEdge {
                operation,
                upstream,
            } => {
                format!("{operation}:from:{}", upstream.legacy_ref())
            }
            Self::Unknown { key } => key.clone(),
        }
    }

    pub fn legacy_ref(&self) -> String {
        match self {
            Self::Source => "source".to_string(),
            // Preserve existing reconciler keys: audio and codec-edge stages refer
            // to a video stage by preset name, while the stage map stores it as
            // "video:<preset>".
            Self::VideoPreset { preset } => preset.clone(),
            Self::AudioRoute { .. } | Self::CodecEdge { .. } | Self::Unknown { .. } => {
                self.legacy_key()
            }
        }
    }

    pub fn graph_node_id(&self, pipeline_id: &str) -> String {
        format!(
            "{}_{}_stage",
            pipeline_id,
            self.legacy_key().replace([':', '+', ','], "_")
        )
    }

    pub fn graph_label(&self) -> String {
        match self {
            Self::Source => "Source".to_string(),
            Self::VideoPreset { preset } => format!("Video: {preset}"),
            Self::AudioRoute { operation, .. } => format!("Audio: {operation}"),
            Self::CodecEdge { operation, .. } => match operation.as_str() {
                "hevc_to_h264" => "HEVC -> H.264".to_string(),
                other => format!("Codec edge: {other}"),
            },
            Self::Unknown { key } => format!("Stage: {key}"),
        }
    }

    pub fn graph_type(&self) -> &'static str {
        match self {
            Self::AudioRoute { .. } => "audio_filter",
            Self::CodecEdge { .. } => "codec_edge",
            Self::Source => "source",
            Self::VideoPreset { .. } | Self::Unknown { .. } => "transcoder",
        }
    }

    pub fn upstream(&self) -> Option<&StageKind> {
        match self {
            Self::AudioRoute { upstream, .. } | Self::CodecEdge { upstream, .. } => Some(upstream),
            _ => None,
        }
    }

    pub fn is_video_preset(&self) -> bool {
        matches!(self, Self::VideoPreset { .. })
    }

    pub fn audio_operation(&self) -> Option<&str> {
        match self {
            Self::AudioRoute { operation, .. } => Some(operation.as_str()),
            _ => None,
        }
    }

    fn parse_upstream_ref(value: &str) -> Self {
        if value == "source" || value.starts_with("audio:") || value.starts_with("video:") {
            Self::parse_legacy_key(value)
        } else {
            Self::video_preset(value)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct StageKey {
    pub pipeline: PipelineId,
    pub kind: StageKind,
}

impl StageKey {
    pub fn new(pipeline: impl Into<PipelineId>, kind: StageKind) -> Self {
        Self {
            pipeline: pipeline.into(),
            kind,
        }
    }

    pub fn legacy_stage_key(&self) -> String {
        self.kind.legacy_key()
    }

    pub fn storage_key(&self) -> String {
        format!("{}:{}", self.pipeline, self.legacy_stage_key())
    }
}

impl fmt::Display for StageKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.storage_key())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodingStagePlan {
    pipeline: PipelineId,
    source: StageKind,
    video_stage: Option<StageKind>,
    audio_stage: Option<StageKind>,
}

impl EncodingStagePlan {
    pub fn from_encoding(pipeline_id: impl Into<PipelineId>, encoding: &str) -> Self {
        let pipeline = pipeline_id.into();
        let mut parts = encoding.splitn(2, '+');
        let video_preset = parts.next().unwrap_or("source");
        let audio_operation = parts.next().filter(|value| !value.is_empty());

        let source = StageKind::source();
        let needs_video =
            !video_preset.is_empty() && video_preset != "source" && video_preset != "custom";
        let video_stage = needs_video.then(|| StageKind::video_preset(video_preset));
        let upstream = video_stage.clone().unwrap_or_else(|| source.clone());
        let audio_stage =
            audio_operation.map(|operation| StageKind::audio_route(operation, upstream));

        Self {
            pipeline,
            source,
            video_stage,
            audio_stage,
        }
    }

    pub fn video_stage(&self) -> Option<StageKey> {
        self.video_stage
            .clone()
            .map(|kind| StageKey::new(self.pipeline.clone(), kind))
    }

    pub fn audio_stage(&self) -> Option<StageKey> {
        self.audio_stage
            .clone()
            .map(|kind| StageKey::new(self.pipeline.clone(), kind))
    }

    pub fn terminal_stage_ref(&self) -> String {
        self.audio_stage
            .as_ref()
            .or(self.video_stage.as_ref())
            .unwrap_or(&self.source)
            .legacy_ref()
    }

    pub fn codec_edge_stage(&self, operation: &str) -> StageKey {
        StageKey::new(
            self.pipeline.clone(),
            StageKind::codec_edge(operation, self.terminal_kind().clone()),
        )
    }

    fn terminal_kind(&self) -> &StageKind {
        self.audio_stage
            .as_ref()
            .or(self.video_stage.as_ref())
            .unwrap_or(&self.source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoding_plan_preserves_legacy_video_and_audio_keys() {
        let plan = EncodingStagePlan::from_encoding("pipe", "720p+atrack:0");

        assert_eq!(plan.video_stage().unwrap().storage_key(), "pipe:video:720p");
        assert_eq!(
            plan.audio_stage().unwrap().storage_key(),
            "pipe:audio:atrack:0:from:720p"
        );
        assert_eq!(plan.terminal_stage_ref(), "audio:atrack:0:from:720p");
    }

    #[test]
    fn encoding_plan_handles_passthrough_audio_route() {
        let plan = EncodingStagePlan::from_encoding("pipe", "source+remap:0:1");

        assert!(plan.video_stage().is_none());
        assert_eq!(
            plan.audio_stage().unwrap().legacy_stage_key(),
            "audio:remap:0:1:from:source"
        );
        assert_eq!(plan.terminal_stage_ref(), "audio:remap:0:1:from:source");
    }

    #[test]
    fn codec_edge_uses_terminal_stage_reference() {
        let plan = EncodingStagePlan::from_encoding("pipe", "720p");

        assert_eq!(
            plan.codec_edge_stage("hevc_to_h264").storage_key(),
            "pipe:hevc_to_h264:from:720p"
        );
    }

    #[test]
    fn parse_legacy_audio_key_recovers_upstream_kind() {
        let kind = StageKind::parse_legacy_key("audio:atrack:0:from:720p");

        assert_eq!(kind.graph_label(), "Audio: atrack:0");
        assert_eq!(kind.upstream().unwrap().legacy_key(), "video:720p");
    }
}
