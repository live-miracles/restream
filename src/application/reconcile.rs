//! Application-layer reconciliation logic that compares desired output and
//! recording state with engine reality and computes convergence actions.

use crate::application::output_path::OutputPath;
use crate::application::ports::{MetaStore, PipelineCatalog, PipelineCatalogError};
use crate::domain::stage::StageKey;
use crate::media::engine::MediaEngine;
use crate::types::Output;
use std::collections::HashSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputRetryPolicy {
    pub max_retries: u32,
    pub base_ms: u64,
    pub max_ms: u64,
}

impl OutputRetryPolicy {
    pub fn backoff_ms(&self, retries: u32) -> u64 {
        let shift = retries.min(16);
        let multiplier = 1u64.checked_shl(shift).unwrap_or(u64::MAX);
        self.base_ms.saturating_mul(multiplier).min(self.max_ms)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputFailureWindow {
    pub retries: u32,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputStartAction {
    NotApplicable,
    SkipNoIngest,
    StartNow,
    MarkFailed,
    WaitRetry {
        retries: u32,
        backoff_ms: u64,
        remaining_ms: u64,
    },
}

pub fn decide_output_start_action(
    desired_state: &str,
    is_active: bool,
    effective_has_ingest: bool,
    failure: Option<OutputFailureWindow>,
    policy: OutputRetryPolicy,
) -> OutputStartAction {
    if desired_state != "running" || is_active {
        return OutputStartAction::NotApplicable;
    }
    if !effective_has_ingest {
        return OutputStartAction::SkipNoIngest;
    }
    if let Some(failure) = failure {
        if failure.retries >= policy.max_retries {
            return OutputStartAction::MarkFailed;
        }
        let backoff_ms = policy.backoff_ms(failure.retries);
        if failure.elapsed_ms < backoff_ms {
            return OutputStartAction::WaitRetry {
                retries: failure.retries,
                backoff_ms,
                remaining_ms: backoff_ms.saturating_sub(failure.elapsed_ms),
            };
        }
    }
    OutputStartAction::StartNow
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputStopAction {
    KeepRunning,
    StopBecauseIngestLost,
    StopRequested,
}

pub fn decide_output_stop_action(
    desired_state: &str,
    is_active: bool,
    effective_has_ingest: bool,
) -> OutputStopAction {
    if desired_state == "running" && is_active && !effective_has_ingest {
        OutputStopAction::StopBecauseIngestLost
    } else if desired_state == "stopped" && is_active {
        OutputStopAction::StopRequested
    } else {
        OutputStopAction::KeepRunning
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordingAction {
    Keep,
    Start,
    Stop,
}

pub fn decide_recording_action(
    recording_enabled: bool,
    effective_has_ingest: bool,
    recording_active: bool,
) -> RecordingAction {
    if recording_enabled && effective_has_ingest && !recording_active {
        RecordingAction::Start
    } else if recording_active && (!recording_enabled || !effective_has_ingest) {
        RecordingAction::Stop
    } else {
        RecordingAction::Keep
    }
}

pub fn next_output_retry_count(previous_retries: Option<u32>, had_progress: bool) -> u32 {
    if had_progress {
        1
    } else {
        previous_retries.unwrap_or(0).saturating_add(1).max(1)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputRuntimeSnapshot {
    pub is_active: bool,
    pub effective_has_ingest: bool,
    pub ingest_video_codec: Option<String>,
}

pub async fn load_output_runtime_snapshot(
    engine: &MediaEngine,
    output: &Output,
    ingest_disconnect_grace_ms: u64,
) -> OutputRuntimeSnapshot {
    let is_active = engine.has_active_egress(&output.id).await;
    let has_ingest = engine.has_active_ingest(&output.pipeline_id).await;
    let within_disconnect_grace = engine
        .has_recent_ingest_disconnect(&output.pipeline_id, ingest_disconnect_grace_ms)
        .await;

    OutputRuntimeSnapshot {
        is_active,
        effective_has_ingest: has_ingest || within_disconnect_grace,
        ingest_video_codec: engine.ingest_video_codec(&output.pipeline_id).await,
    }
}

#[derive(Debug, Clone)]
pub struct OutputStageSweepInput<'a> {
    pub pipeline_id: &'a str,
    pub encoding: &'a str,
    pub url: &'a str,
    pub desired_state: &'a str,
    pub is_active: bool,
    pub effective_has_ingest: bool,
    pub ingest_video_codec: Option<String>,
}

pub fn collect_needed_stage_keys<'a>(
    outputs: impl IntoIterator<Item = OutputStageSweepInput<'a>>,
) -> HashSet<StageKey> {
    let mut needed_stages = HashSet::new();
    for output in outputs {
        if output.effective_has_ingest && (output.is_active || output.desired_state == "running") {
            let output_path = OutputPath::resolve(output.pipeline_id, output.encoding, output.url);
            for stage in output_path.needed_stage_keys(output.ingest_video_codec.as_deref()) {
                needed_stages.insert(stage);
            }
        }
    }
    needed_stages
}

pub fn output_stage_sweep_input<'a>(
    output: &'a Output,
    snapshot: &OutputRuntimeSnapshot,
) -> OutputStageSweepInput<'a> {
    OutputStageSweepInput {
        pipeline_id: output.pipeline_id.as_str(),
        encoding: &output.encoding,
        url: &output.url,
        desired_state: &output.desired_state,
        is_active: snapshot.is_active,
        effective_has_ingest: snapshot.effective_has_ingest,
        ingest_video_codec: snapshot.ingest_video_codec.clone(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordingCommand {
    Start {
        pipeline_name: String,
        pipeline_id: String,
        input_source: Option<String>,
    },
    Stop {
        pipeline_id: String,
    },
}

pub async fn build_recording_reconcile_plan(
    engine: &MediaEngine,
    pipeline_catalog: &dyn PipelineCatalog,
    meta_store: &dyn MetaStore,
    ingest_disconnect_grace_ms: u64,
) -> Result<Vec<RecordingCommand>, PipelineCatalogError> {
    let pipelines = pipeline_catalog.list_pipelines().await?;
    let mut commands = Vec::new();

    for pipeline in pipelines {
        let has_ingest = engine.has_active_ingest(&pipeline.id).await;
        let effective_has_ingest = has_ingest
            || engine
                .has_recent_ingest_disconnect(&pipeline.id, ingest_disconnect_grace_ms)
                .await;
        let rec_enabled =
            crate::application::recording::load_recording_enabled(meta_store, &pipeline.id).await;
        let rec_active = engine.is_recording_active(&pipeline.id).await;

        match decide_recording_action(rec_enabled, effective_has_ingest, rec_active) {
            RecordingAction::Keep => {}
            RecordingAction::Start => commands.push(RecordingCommand::Start {
                pipeline_name: pipeline.name,
                pipeline_id: pipeline.id,
                input_source: pipeline.input_source,
            }),
            RecordingAction::Stop => commands.push(RecordingCommand::Stop {
                pipeline_id: pipeline.id,
            }),
        }
    }

    Ok(commands)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::ports::{MetaLookupFuture, PipelineCatalogFuture};
    use crate::domain::stage::StageKind;
    use crate::media::engine::VideoMeta;
    use crate::types::Pipeline;
    use std::collections::HashMap;
    use std::sync::Mutex;

    fn test_retry_policy() -> OutputRetryPolicy {
        OutputRetryPolicy {
            max_retries: 10,
            base_ms: 5_000,
            max_ms: 300_000,
        }
    }

    #[test]
    fn start_action_waits_during_backoff_window() {
        let action = decide_output_start_action(
            "running",
            false,
            true,
            Some(OutputFailureWindow {
                retries: 2,
                elapsed_ms: 5_000,
            }),
            test_retry_policy(),
        );

        assert_eq!(
            action,
            OutputStartAction::WaitRetry {
                retries: 2,
                backoff_ms: 20_000,
                remaining_ms: 15_000,
            }
        );
    }

    #[test]
    fn start_action_marks_failed_after_max_retries() {
        let action = decide_output_start_action(
            "running",
            false,
            true,
            Some(OutputFailureWindow {
                retries: 10,
                elapsed_ms: 999_999,
            }),
            test_retry_policy(),
        );

        assert_eq!(action, OutputStartAction::MarkFailed);
    }

    #[test]
    fn start_action_skips_when_ingest_is_missing() {
        let action = decide_output_start_action("running", false, false, None, test_retry_policy());

        assert_eq!(action, OutputStartAction::SkipNoIngest);
    }

    #[test]
    fn stop_action_distinguishes_requested_stop_from_ingest_loss() {
        assert_eq!(
            decide_output_stop_action("running", true, false),
            OutputStopAction::StopBecauseIngestLost
        );
        assert_eq!(
            decide_output_stop_action("stopped", true, true),
            OutputStopAction::StopRequested
        );
    }

    #[test]
    fn recording_action_is_purely_state_driven() {
        assert_eq!(
            decide_recording_action(true, true, false),
            RecordingAction::Start
        );
        assert_eq!(
            decide_recording_action(false, true, true),
            RecordingAction::Stop
        );
        assert_eq!(
            decide_recording_action(true, true, true),
            RecordingAction::Keep
        );
    }

    #[test]
    fn retry_count_resets_after_progress() {
        assert_eq!(next_output_retry_count(None, false), 1);
        assert_eq!(next_output_retry_count(Some(1), false), 2);
        assert_eq!(next_output_retry_count(Some(4), true), 1);
    }

    #[test]
    fn stage_sweep_collects_only_needed_stage_keys() {
        let stages = collect_needed_stage_keys([
            OutputStageSweepInput {
                pipeline_id: "pipe",
                encoding: "720p+atrack:0",
                url: "rtmp://example/live",
                desired_state: "running",
                is_active: false,
                effective_has_ingest: true,
                ingest_video_codec: Some("hevc".to_string()),
            },
            OutputStageSweepInput {
                pipeline_id: "pipe",
                encoding: "source",
                url: "srt://example:9000",
                desired_state: "stopped",
                is_active: false,
                effective_has_ingest: true,
                ingest_video_codec: Some("hevc".to_string()),
            },
        ]);

        assert!(stages.contains(&StageKey::new("pipe", StageKind::video_preset("720p"))));
        assert!(stages.contains(&StageKey::new(
            "pipe",
            StageKind::audio_route("atrack:0", StageKind::video_preset("720p"))
        )));
        assert!(stages.contains(&StageKey::new(
            "pipe",
            StageKind::codec_edge(
                "hevc_to_h264",
                StageKind::audio_route("atrack:0", StageKind::video_preset("720p"))
            )
        )));
        assert_eq!(stages.len(), 3);
    }

    #[tokio::test]
    async fn output_runtime_snapshot_reads_active_ingest_and_codec() {
        let engine = MediaEngine::new();
        engine
            .try_register_ingest("pipe", "stream-key", "rtmp")
            .await
            .unwrap();
        engine
            .update_ingest_meta(
                "pipe",
                Some(VideoMeta {
                    codec: "hevc".to_string(),
                    ..Default::default()
                }),
                None,
                None,
            )
            .await;
        let output = crate::types::Output {
            id: "out-1".to_string(),
            pipeline_id: "pipe".to_string(),
            name: "Output".to_string(),
            url: "rtmp://example/live/test".to_string(),
            monitoring_url: None,
            desired_state: "running".to_string(),
            encoding: "source".to_string(),
        };

        let snapshot = load_output_runtime_snapshot(&engine, &output, 0).await;

        assert!(!snapshot.is_active);
        assert!(snapshot.effective_has_ingest);
        assert_eq!(snapshot.ingest_video_codec.as_deref(), Some("hevc"));
    }

    #[tokio::test]
    async fn output_runtime_snapshot_honors_recent_disconnect_grace() {
        let engine = MediaEngine::new();
        engine
            .try_register_ingest("pipe", "stream-key", "rtmp")
            .await
            .unwrap();
        engine.unregister_ingest("pipe").await;
        let output = crate::types::Output {
            id: "out-1".to_string(),
            pipeline_id: "pipe".to_string(),
            name: "Output".to_string(),
            url: "srt://example:9000".to_string(),
            monitoring_url: None,
            desired_state: "running".to_string(),
            encoding: "source".to_string(),
        };

        let snapshot = load_output_runtime_snapshot(&engine, &output, 1_000).await;

        assert!(!snapshot.is_active);
        assert!(snapshot.effective_has_ingest);
        assert_eq!(snapshot.ingest_video_codec, None);
    }

    #[test]
    fn output_stage_sweep_input_uses_snapshot_fields() {
        let output = crate::types::Output {
            id: "out-1".to_string(),
            pipeline_id: "pipe".to_string(),
            name: "Output".to_string(),
            url: "rtmp://example/live".to_string(),
            monitoring_url: None,
            desired_state: "running".to_string(),
            encoding: "720p".to_string(),
        };
        let snapshot = OutputRuntimeSnapshot {
            is_active: true,
            effective_has_ingest: false,
            ingest_video_codec: Some("hevc".to_string()),
        };

        let input = output_stage_sweep_input(&output, &snapshot);

        assert_eq!(input.pipeline_id, "pipe");
        assert_eq!(input.encoding, "720p");
        assert!(input.is_active);
        assert!(!input.effective_has_ingest);
        assert_eq!(input.ingest_video_codec.as_deref(), Some("hevc"));
    }

    struct FakePipelineCatalog {
        pipelines: Vec<Pipeline>,
        error: Option<&'static str>,
    }

    impl PipelineCatalog for FakePipelineCatalog {
        fn list_pipelines<'a>(&'a self) -> PipelineCatalogFuture<'a> {
            Box::pin(async move {
                if let Some(message) = self.error {
                    return Err(PipelineCatalogError::new(message));
                }
                Ok(self.pipelines.clone())
            })
        }
    }

    struct FakeMetaStore {
        values: Mutex<HashMap<String, String>>,
    }

    impl MetaStore for FakeMetaStore {
        fn get_meta<'a>(&'a self, key: &'a str) -> MetaLookupFuture<'a> {
            Box::pin(async move {
                Ok(self
                    .values
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(key)
                    .cloned())
            })
        }
    }

    #[tokio::test]
    async fn recording_reconcile_plan_starts_enabled_pipeline_with_ingest() {
        let engine = MediaEngine::new();
        engine
            .try_register_ingest("pipeline-1", "stream-one", "rtmp")
            .await
            .unwrap();
        let catalog = FakePipelineCatalog {
            pipelines: vec![Pipeline {
                id: "pipeline-1".to_string(),
                name: "Pipeline One".to_string(),
                stream_key: "stream-one".to_string(),
                input_source: Some("cam-1".to_string()),
                encoding: None,
                srt_ingest_policy: None,
            }],
            error: None,
        };
        let store = FakeMetaStore {
            values: Mutex::new(HashMap::from([(
                "recording_enabled:pipeline-1".to_string(),
                "1".to_string(),
            )])),
        };

        let commands = build_recording_reconcile_plan(&engine, &catalog, &store, 0)
            .await
            .unwrap();

        assert_eq!(
            commands,
            vec![RecordingCommand::Start {
                pipeline_name: "Pipeline One".to_string(),
                pipeline_id: "pipeline-1".to_string(),
                input_source: Some("cam-1".to_string()),
            }]
        );
    }

    #[tokio::test]
    async fn recording_reconcile_plan_stops_disabled_active_recording() {
        let engine = MediaEngine::new();
        let _token = engine.register_recording("pipeline-1").await;
        let catalog = FakePipelineCatalog {
            pipelines: vec![Pipeline {
                id: "pipeline-1".to_string(),
                name: "Pipeline One".to_string(),
                stream_key: "stream-one".to_string(),
                input_source: None,
                encoding: None,
                srt_ingest_policy: None,
            }],
            error: None,
        };
        let store = FakeMetaStore {
            values: Mutex::new(HashMap::new()),
        };

        let commands = build_recording_reconcile_plan(&engine, &catalog, &store, 0)
            .await
            .unwrap();

        assert_eq!(
            commands,
            vec![RecordingCommand::Stop {
                pipeline_id: "pipeline-1".to_string(),
            }]
        );
    }
}
