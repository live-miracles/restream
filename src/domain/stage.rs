//! Typed stage identity and output encoding planning.
//!
//! Every stage in the media graph has a typed `StageKind` that encodes its
//! function (video preset, audio route, codec edge, infrastructure). The
//! `StageKey` pairs a `PipelineId` with a `StageKind` for use as a typed
//! map key in engine registries. No string-based stage identity is used at
//! runtime.

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
    Hls,
    Recording,
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

    pub fn hls() -> Self {
        Self::Hls
    }

    pub fn recording() -> Self {
        Self::Recording
    }

    pub fn graph_node_id(&self, pipeline_id: &str) -> String {
        let slug = self.to_string().replace([':', '+', ','], "_");
        format!("{pipeline_id}_{slug}_stage")
    }

    pub fn graph_label(&self) -> String {
        match self {
            Self::Source => "Source".to_string(),
            Self::Hls => "HLS Preview".to_string(),
            Self::Recording => "MKV Recording".to_string(),
            Self::VideoPreset { preset } => format!("Video: {preset}"),
            Self::AudioRoute { operation, .. } => format!("Audio: {operation}"),
            Self::CodecEdge { operation, .. } => match operation.as_str() {
                "hevc_to_h264" => "HEVC -> H.264".to_string(),
                other => format!("Codec edge: {other}"),
            },
        }
    }

    pub fn graph_type(&self) -> &'static str {
        match self {
            Self::AudioRoute { .. } => "audio_filter",
            Self::CodecEdge { .. } => "codec_edge",
            Self::Source => "source",
            Self::Hls => "hls",
            Self::Recording => "recording",
            Self::VideoPreset { .. } => "transcoder",
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

    /// The video preset name for video stages, used by downstream audio and
    /// codec-edge stages to refer to their upstream in Display output.
    pub fn preset_name(&self) -> Option<&str> {
        match self {
            Self::VideoPreset { preset } => Some(preset.as_str()),
            _ => None,
        }
    }
}

impl fmt::Display for StageKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Source => f.write_str("source"),
            Self::Hls => f.write_str("hls"),
            Self::Recording => f.write_str("recording"),
            Self::VideoPreset { preset } => write!(f, "video:{preset}"),
            Self::AudioRoute {
                operation,
                upstream,
            } => write!(f, "audio:{operation}:from:{upstream}"),
            Self::CodecEdge {
                operation,
                upstream,
            } => write!(f, "{operation}:from:{upstream}"),
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
}

impl fmt::Display for StageKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.pipeline, self.kind)
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
        let first_part = parts.next().unwrap_or("source");
        let second_part = parts.next().filter(|value| !value.is_empty());
        let (video_preset, audio_operation) = if looks_like_audio_operation(first_part) {
            ("source", Some(first_part))
        } else {
            (first_part, second_part)
        };

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

    pub fn terminal_kind(&self) -> &StageKind {
        self.audio_stage
            .as_ref()
            .or(self.video_stage.as_ref())
            .unwrap_or(&self.source)
    }

    pub fn codec_edge_stage(&self, operation: &str) -> StageKey {
        StageKey::new(
            self.pipeline.clone(),
            StageKind::codec_edge(operation, self.terminal_kind().clone()),
        )
    }
}

fn looks_like_audio_operation(value: &str) -> bool {
    value.starts_with("atrack:") || value.starts_with("remap:") || value.starts_with("downmix:")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoding_plan_produces_typed_video_and_audio_stages() {
        let plan = EncodingStagePlan::from_encoding("pipe", "720p+atrack:0");

        let video = plan.video_stage().unwrap();
        assert_eq!(video.kind, StageKind::video_preset("720p"));
        assert_eq!(video.pipeline.as_str(), "pipe");

        let audio = plan.audio_stage().unwrap();
        assert_eq!(
            audio.kind,
            StageKind::audio_route("atrack:0", StageKind::video_preset("720p"))
        );

        assert_eq!(
            *plan.terminal_kind(),
            StageKind::audio_route("atrack:0", StageKind::video_preset("720p"))
        );
    }

    #[test]
    fn encoding_plan_handles_passthrough_audio_route() {
        let plan = EncodingStagePlan::from_encoding("pipe", "source+remap:0:1");

        assert!(plan.video_stage().is_none());
        let audio = plan.audio_stage().unwrap();
        assert_eq!(
            audio.kind,
            StageKind::audio_route("remap:0:1", StageKind::source())
        );
        assert_eq!(
            *plan.terminal_kind(),
            StageKind::audio_route("remap:0:1", StageKind::source())
        );
    }

    #[test]
    fn encoding_plan_treats_plain_atrack_as_source_audio_route() {
        let plan = EncodingStagePlan::from_encoding("pipe", "atrack:0");

        assert!(plan.video_stage().is_none());
        assert_eq!(
            plan.audio_stage().unwrap().kind,
            StageKind::audio_route("atrack:0", StageKind::source())
        );
        assert_eq!(
            *plan.terminal_kind(),
            StageKind::audio_route("atrack:0", StageKind::source())
        );
    }

    #[test]
    fn codec_edge_uses_terminal_kind() {
        let plan = EncodingStagePlan::from_encoding("pipe", "720p");

        let edge = plan.codec_edge_stage("hevc_to_h264");
        assert_eq!(
            edge.kind,
            StageKind::codec_edge("hevc_to_h264", StageKind::video_preset("720p"))
        );
    }

    #[test]
    fn stage_kind_display_round_trips() {
        let cases: Vec<(StageKind, &str)> = vec![
            (StageKind::source(), "source"),
            (StageKind::hls(), "hls"),
            (StageKind::recording(), "recording"),
            (StageKind::video_preset("720p"), "video:720p"),
            (
                StageKind::audio_route("atrack:0", StageKind::video_preset("720p")),
                "audio:atrack:0:from:video:720p",
            ),
            (
                StageKind::codec_edge("hevc_to_h264", StageKind::source()),
                "hevc_to_h264:from:source",
            ),
            (
                StageKind::codec_edge("hevc_to_h264", StageKind::video_preset("720p")),
                "hevc_to_h264:from:video:720p",
            ),
        ];
        for (kind, expected) in cases {
            assert_eq!(kind.to_string(), expected, "Display for {:?}", kind);
        }
    }

    #[test]
    fn stage_key_display() {
        let key = StageKey::new("pipe", StageKind::video_preset("720p"));
        assert_eq!(key.to_string(), "pipe:video:720p");
    }

    #[test]
    fn audio_route_upstream_is_accessible() {
        let kind = StageKind::audio_route("atrack:0", StageKind::video_preset("720p"));
        assert_eq!(kind.graph_label(), "Audio: atrack:0");
        assert_eq!(*kind.upstream().unwrap(), StageKind::video_preset("720p"));
    }
}
