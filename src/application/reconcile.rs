use crate::application::output_path::OutputPath;
use crate::domain::stage::StageKey;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::stage::StageKind;

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
}
