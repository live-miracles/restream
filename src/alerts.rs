//! Typed alert model and pure derivation from health snapshots.
//!
//! `derive_alerts` is pure — it takes a `health_snapshot()` JSON value and
//! returns a sorted `Vec<Alert>` (Critical before Warning). No I/O, no locks.

use std::collections::HashMap;
use std::sync::Mutex;

use serde::Serialize;

// ─── Severity ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum Severity {
    Critical,
    Warning,
}

impl Severity {
    fn rank(&self) -> u8 {
        match self {
            Severity::Critical => 0,
            Severity::Warning => 1,
        }
    }
}

// ─── Scope ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum Scope {
    Engine,
    Pipeline,
    Stage,
    Output,
}

// ─── Alert ───────────────────────────────────────────────────────────────────

/// A single derived health alert. The `id` field is a stable key for dedup
/// (same condition on the same entity always produces the same id).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Alert {
    pub id: String,
    pub severity: Severity,
    pub scope: Scope,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pipeline_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stage_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_id: Option<String>,
    pub title: String,
    pub cause: String,
    pub evidence: Vec<String>,
    pub recommended_action: String,
    /// Copied from `snapshot.generatedAt`.
    pub generated_at: String,
    /// When this alert condition was first observed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_seen: Option<String>,
    /// When this alert condition was most recently observed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<String>,
}

// ─── Thresholds ──────────────────────────────────────────────────────────────

/// Ring-buffer lag slots above this threshold trigger a Warning.
/// 256 slots ≈ one full ring at standard frame rates (ring capacity is 512).
const LAG_SLOTS_WARN: u64 = 256;

// ─── Derivation ──────────────────────────────────────────────────────────────

/// Derive alerts from a `health_snapshot()` JSON value.
/// Returns alerts sorted Critical-first, then Warning, then by pipeline id.
pub fn derive_alerts(snapshot: &serde_json::Value) -> Vec<Alert> {
    let generated_at = snapshot
        .get("generatedAt")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let mut alerts: Vec<Alert> = Vec::new();

    // ── Engine-level checks ───────────────────────────────────────────────────

    let srt = &snapshot["srtListener"];
    let udp_drops = srt.get("udpDrops").and_then(|v| v.as_u64()).unwrap_or(0);
    if udp_drops > 0 {
        alerts.push(Alert {
            id: "engine:srt_listener:udp_drops".into(),
            severity: Severity::Warning,
            scope: Scope::Engine,
            pipeline_id: None,
            stage_id: None,
            output_id: None,
            title: "SRT listener UDP drops detected".into(),
            cause: "The SRT listener's kernel receive queue is overflowing.".into(),
            evidence: vec![format!("udpDrops = {}", udp_drops)],
            recommended_action: "Increase SO_RCVBUF or reduce SRT publisher bandwidth.".into(),
            generated_at: generated_at.clone(),
            first_seen: None,
            last_seen: None,
        });
    }

    // ── Per-pipeline checks ───────────────────────────────────────────────────

    let pipelines = match snapshot.get("pipelines").and_then(|v| v.as_object()) {
        Some(p) => p,
        None => return sorted(alerts),
    };

    for (pipeline_id, pipeline) in pipelines {
        let input = &pipeline["input"];

        // No publisher
        let input_status = input.get("status").and_then(|v| v.as_str()).unwrap_or("");
        if input_status == "off" {
            alerts.push(Alert {
                id: format!("pipeline:{}:no_publisher", pipeline_id),
                severity: Severity::Critical,
                scope: Scope::Pipeline,
                pipeline_id: Some(pipeline_id.clone()),
                stage_id: None,
                output_id: None,
                title: "No active publisher".into(),
                cause: "The pipeline is configured but not receiving a stream.".into(),
                evidence: vec!["input.status = off".into()],
                recommended_action: "Start the publisher or check the stream key and connection."
                    .into(),
                generated_at: generated_at.clone(),
                first_seen: None,
                last_seen: None,
            });
        }

        // Per-reader: lag and overflow
        if let Some(readers) = input.get("readerMetrics").and_then(|v| v.as_array()) {
            for reader in readers {
                let name = reader
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");

                let lag = reader.get("lagSlots").and_then(|v| v.as_u64()).unwrap_or(0);
                if lag > LAG_SLOTS_WARN {
                    alerts.push(Alert {
                        id: format!("pipeline:{}:stage:{}:lag", pipeline_id, name),
                        severity: Severity::Warning,
                        scope: Scope::Stage,
                        pipeline_id: Some(pipeline_id.clone()),
                        stage_id: Some(name.to_string()),
                        output_id: None,
                        title: format!("Stage '{}' is lagging behind the ring buffer", name),
                        cause: "The consumer is reading slower than the producer is writing."
                            .into(),
                        evidence: vec![format!(
                            "lagSlots = {} (threshold {})",
                            lag, LAG_SLOTS_WARN
                        )],
                        recommended_action:
                            "Check downstream network/encoder throughput or reduce output bitrate."
                                .into(),
                        generated_at: generated_at.clone(),
                        first_seen: None,
                        last_seen: None,
                    });
                }

                let overflows = reader
                    .get("overflowCount")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                if overflows > 0 {
                    alerts.push(Alert {
                        id: format!("pipeline:{}:stage:{}:overflow", pipeline_id, name),
                        severity: Severity::Warning,
                        scope: Scope::Stage,
                        pipeline_id: Some(pipeline_id.clone()),
                        stage_id: Some(name.to_string()),
                        output_id: None,
                        title: format!(
                            "Stage '{}' has overflowed the ring buffer {} time(s)",
                            name, overflows
                        ),
                        cause:
                            "The ring buffer was full when this reader tried to consume packets; \
                                some packets were skipped."
                                .into(),
                        evidence: vec![format!("overflowCount = {}", overflows)],
                        recommended_action:
                            "Reduce output count or increase processing throughput.".into(),
                        generated_at: generated_at.clone(),
                        first_seen: None,
                        last_seen: None,
                    });
                }
            }
        }

        // Per-output: non-running when there is an active publisher
        if input_status == "on"
            && let Some(outputs) = pipeline.get("outputs").and_then(|v| v.as_object())
        {
            for (output_id, output) in outputs {
                let status = output.get("status").and_then(|v| v.as_str()).unwrap_or("");
                if status != "running" {
                    alerts.push(Alert {
                        id: format!("pipeline:{}:output:{}:not_running", pipeline_id, output_id),
                        severity: Severity::Warning,
                        scope: Scope::Output,
                        pipeline_id: Some(pipeline_id.clone()),
                        stage_id: None,
                        output_id: Some(output_id.clone()),
                        title: format!("Output '{}' is not running", output_id),
                        cause: format!(
                            "Output status is '{}' while the pipeline has an active publisher.",
                            status
                        ),
                        evidence: vec![format!("output.status = {}", status)],
                        recommended_action:
                            "Check the destination URL, credentials, and network reachability."
                                .into(),
                        generated_at: generated_at.clone(),
                        first_seen: None,
                        last_seen: None,
                    });
                    continue;
                }

                let phase = output.get("phase").and_then(|v| v.as_str()).unwrap_or("");
                if phase == "failed" {
                    let failure_phase = output
                        .get("failurePhase")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");
                    let last_error = output
                        .get("lastError")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown error");
                    alerts.push(Alert {
                        id: format!("pipeline:{}:output:{}:failed_phase", pipeline_id, output_id),
                        severity: Severity::Warning,
                        scope: Scope::Output,
                        pipeline_id: Some(pipeline_id.clone()),
                        stage_id: None,
                        output_id: Some(output_id.clone()),
                        title: format!("Output '{}' reported an egress failure", output_id),
                        cause: format!("Output failed during the '{}' phase.", failure_phase),
                        evidence: vec![
                            format!("output.phase = {}", phase),
                            format!("lastError = {}", last_error),
                        ],
                        recommended_action:
                            "Check destination reachability, credentials, and protocol settings."
                                .into(),
                        generated_at: generated_at.clone(),
                        first_seen: None,
                        last_seen: None,
                    });
                    continue;
                }

                let last_progress_age_ms = output
                    .get("lastProgressAgeMs")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let total_size = output
                    .get("totalSize")
                    .and_then(|v| v.as_u64())
                    .unwrap_or_else(|| {
                        output.get("bytesOut").and_then(|v| v.as_u64()).unwrap_or(0)
                    });
                if total_size > 0 && last_progress_age_ms >= 10_000 {
                    alerts.push(Alert {
                        id: format!("pipeline:{}:output:{}:stale_progress", pipeline_id, output_id),
                        severity: Severity::Warning,
                        scope: Scope::Output,
                        pipeline_id: Some(pipeline_id.clone()),
                        stage_id: None,
                        output_id: Some(output_id.clone()),
                        title: format!("Output '{}' has stopped making progress", output_id),
                        cause:
                            "The output is still registered but has not completed a send recently."
                                .into(),
                        evidence: vec![format!(
                            "lastProgressAgeMs = {} (threshold 10000)",
                            last_progress_age_ms
                        )],
                        recommended_action:
                            "Check downstream network health or restart the output if it remains stale."
                                .into(),
                        generated_at: generated_at.clone(),
                        first_seen: None,
                        last_seen: None,
                    });
                }
            }
        }
    }

    sorted(alerts)
}

fn sorted(mut alerts: Vec<Alert>) -> Vec<Alert> {
    alerts.sort_by(|a, b| {
        a.severity
            .rank()
            .cmp(&b.severity.rank())
            .then(a.pipeline_id.cmp(&b.pipeline_id))
            .then(a.id.cmp(&b.id))
    });
    alerts
}

// ─── Alert Tracker ──────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct AlertHistory {
    first_seen: String,
    last_seen: String,
    pipeline_id: Option<String>,
}

/// Tracks `first_seen`/`last_seen` timestamps for recurring alert conditions.
///
/// Call one of the `track_*` methods after each `derive_alerts` invocation. It
/// stamps each alert with its history and prunes entries only for the snapshot
/// scope that was actually observed.
pub struct AlertTracker {
    history: Mutex<HashMap<String, AlertHistory>>,
}

impl Default for AlertTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl AlertTracker {
    pub fn new() -> Self {
        Self {
            history: Mutex::new(HashMap::new()),
        }
    }

    /// Stamp alerts from a complete snapshot and prune every resolved entry.
    pub fn track(&self, alerts: &mut [Alert]) {
        self.track_with_prune(alerts, |_| true);
    }

    /// Stamp alerts from a single-pipeline snapshot and prune only resolved
    /// entries for that same pipeline. Alerts for other pipelines remain intact
    /// because this snapshot did not observe them.
    pub fn track_pipeline(&self, pipeline_id: &str, alerts: &mut [Alert]) {
        self.track_with_prune(alerts, |history| {
            history.pipeline_id.as_deref() == Some(pipeline_id)
        });
    }

    fn track_with_prune(
        &self,
        alerts: &mut [Alert],
        mut should_prune_if_absent: impl FnMut(&AlertHistory) -> bool,
    ) {
        let now = chrono::Utc::now().to_rfc3339();
        let mut history = self.history.lock().unwrap_or_else(|e| e.into_inner());
        let mut active_ids: HashMap<&str, ()> = HashMap::with_capacity(alerts.len());

        for alert in alerts.iter_mut() {
            active_ids.insert(&alert.id, ());
            let entry = history
                .entry(alert.id.clone())
                .or_insert_with(|| AlertHistory {
                    first_seen: now.clone(),
                    last_seen: now.clone(),
                    pipeline_id: alert.pipeline_id.clone(),
                });
            entry.last_seen = now.clone();
            alert.first_seen = Some(entry.first_seen.clone());
            alert.last_seen = Some(entry.last_seen.clone());
        }

        history.retain(|id, entry| {
            active_ids.contains_key(id.as_str()) || !should_prune_if_absent(entry)
        });
    }

    pub fn active_count(&self) -> usize {
        self.history.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn snapshot_with_pipeline(pipeline_id: &str, input_status: &str) -> serde_json::Value {
        json!({
            "generatedAt": "2026-06-25T00:00:00Z",
            "srtListener": { "udpDrops": 0 },
            "pipelines": {
                pipeline_id: {
                    "input": {
                        "status": input_status,
                        "readerMetrics": []
                    },
                    "outputs": {}
                }
            }
        })
    }

    #[test]
    fn clean_snapshot_yields_no_alerts() {
        let snap = snapshot_with_pipeline("pipe1", "on");
        assert!(derive_alerts(&snap).is_empty());
    }

    #[test]
    fn publisher_absent_yields_critical_alert() {
        let snap = snapshot_with_pipeline("pipe1", "off");
        let alerts = derive_alerts(&snap);
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].severity, Severity::Critical);
        assert_eq!(alerts[0].scope, Scope::Pipeline);
        assert_eq!(alerts[0].pipeline_id.as_deref(), Some("pipe1"));
        assert!(alerts[0].id.contains("no_publisher"));
    }

    #[test]
    fn reader_lag_above_threshold_yields_warning() {
        let snap = json!({
            "generatedAt": "2026-06-25T00:00:00Z",
            "srtListener": { "udpDrops": 0 },
            "pipelines": {
                "pipe1": {
                    "input": {
                        "status": "on",
                        "readerMetrics": [
                            { "name": "rtmp_egress", "lagSlots": 300, "overflowCount": 0 }
                        ]
                    },
                    "outputs": {}
                }
            }
        });
        let alerts = derive_alerts(&snap);
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].severity, Severity::Warning);
        assert_eq!(alerts[0].scope, Scope::Stage);
        assert!(alerts[0].id.contains("lag"));
    }

    #[test]
    fn reader_lag_below_threshold_yields_no_alert() {
        let snap = json!({
            "generatedAt": "2026-06-25T00:00:00Z",
            "srtListener": { "udpDrops": 0 },
            "pipelines": {
                "pipe1": {
                    "input": {
                        "status": "on",
                        "readerMetrics": [
                            { "name": "rtmp_egress", "lagSlots": 10, "overflowCount": 0 }
                        ]
                    },
                    "outputs": {}
                }
            }
        });
        assert!(derive_alerts(&snap).is_empty());
    }

    #[test]
    fn reader_overflow_yields_warning() {
        let snap = json!({
            "generatedAt": "2026-06-25T00:00:00Z",
            "srtListener": { "udpDrops": 0 },
            "pipelines": {
                "pipe1": {
                    "input": {
                        "status": "on",
                        "readerMetrics": [
                            { "name": "hls", "lagSlots": 0, "overflowCount": 5 }
                        ]
                    },
                    "outputs": {}
                }
            }
        });
        let alerts = derive_alerts(&snap);
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].severity, Severity::Warning);
        assert_eq!(alerts[0].scope, Scope::Stage);
        assert!(alerts[0].id.contains("overflow"));
    }

    #[test]
    fn stopped_output_with_active_publisher_yields_warning() {
        let snap = json!({
            "generatedAt": "2026-06-25T00:00:00Z",
            "srtListener": { "udpDrops": 0 },
            "pipelines": {
                "pipe1": {
                    "input": {
                        "status": "on",
                        "readerMetrics": []
                    },
                    "outputs": {
                        "out1": { "status": "stopped", "totalSize": 0 }
                    }
                }
            }
        });
        let alerts = derive_alerts(&snap);
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].severity, Severity::Warning);
        assert_eq!(alerts[0].scope, Scope::Output);
        assert_eq!(alerts[0].output_id.as_deref(), Some("out1"));
    }

    #[test]
    fn stopped_output_without_publisher_yields_no_alert() {
        // Output warnings are suppressed when there's no publisher — nothing to forward.
        let snap = json!({
            "generatedAt": "2026-06-25T00:00:00Z",
            "srtListener": { "udpDrops": 0 },
            "pipelines": {
                "pipe1": {
                    "input": {
                        "status": "off",
                        "readerMetrics": []
                    },
                    "outputs": {
                        "out1": { "status": "stopped", "totalSize": 0 }
                    }
                }
            }
        });
        let alerts = derive_alerts(&snap);
        // Only the Critical no_publisher alert, not a Warning for output.
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].severity, Severity::Critical);
    }

    #[test]
    fn failed_output_phase_yields_warning() {
        let snap = json!({
            "generatedAt": "2026-06-25T00:00:00Z",
            "srtListener": { "udpDrops": 0 },
            "pipelines": {
                "pipe1": {
                    "input": { "status": "on", "readerMetrics": [] },
                    "outputs": {
                        "out1": {
                            "status": "running",
                            "phase": "failed",
                            "failurePhase": "connect",
                            "lastError": "connection refused"
                        }
                    }
                }
            }
        });

        let alerts = derive_alerts(&snap);
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].scope, Scope::Output);
        assert!(alerts[0].id.contains("failed_phase"));
        assert!(
            alerts[0]
                .evidence
                .iter()
                .any(|e| e.contains("connection refused"))
        );
    }

    #[test]
    fn stale_output_progress_yields_warning_after_successful_send() {
        let snap = json!({
            "generatedAt": "2026-06-25T00:00:00Z",
            "srtListener": { "udpDrops": 0 },
            "pipelines": {
                "pipe1": {
                    "input": { "status": "on", "readerMetrics": [] },
                    "outputs": {
                        "out1": {
                            "status": "running",
                            "phase": "sending",
                            "totalSize": 1316,
                            "lastProgressAgeMs": 12_000
                        }
                    }
                }
            }
        });

        let alerts = derive_alerts(&snap);
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].scope, Scope::Output);
        assert!(alerts[0].id.contains("stale_progress"));
    }

    #[test]
    fn srt_udp_drops_yield_engine_warning() {
        let snap = json!({
            "generatedAt": "2026-06-25T00:00:00Z",
            "srtListener": { "udpDrops": 42 },
            "pipelines": {}
        });
        let alerts = derive_alerts(&snap);
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].severity, Severity::Warning);
        assert_eq!(alerts[0].scope, Scope::Engine);
    }

    #[test]
    fn alerts_sorted_critical_before_warning() {
        let snap = json!({
            "generatedAt": "2026-06-25T00:00:00Z",
            "srtListener": { "udpDrops": 1 },
            "pipelines": {
                "pipe1": {
                    "input": { "status": "off", "readerMetrics": [] },
                    "outputs": {}
                }
            }
        });
        let alerts = derive_alerts(&snap);
        assert_eq!(alerts.len(), 2);
        assert_eq!(alerts[0].severity, Severity::Critical);
        assert_eq!(alerts[1].severity, Severity::Warning);
    }

    #[test]
    fn tracker_stamps_first_and_last_seen() {
        let tracker = AlertTracker::new();
        let snap = snapshot_with_pipeline("pipe1", "off");
        let mut alerts = derive_alerts(&snap);
        assert_eq!(alerts.len(), 1);
        assert!(alerts[0].first_seen.is_none());

        tracker.track(&mut alerts);
        let first = alerts[0].first_seen.clone().unwrap();
        let last = alerts[0].last_seen.clone().unwrap();
        assert_eq!(first, last);
        assert_eq!(tracker.active_count(), 1);
    }

    #[test]
    fn tracker_updates_last_seen_preserves_first_seen() {
        let tracker = AlertTracker::new();
        let snap = snapshot_with_pipeline("pipe1", "off");

        let mut alerts1 = derive_alerts(&snap);
        tracker.track(&mut alerts1);
        let first = alerts1[0].first_seen.clone().unwrap();

        std::thread::sleep(std::time::Duration::from_millis(10));

        let mut alerts2 = derive_alerts(&snap);
        tracker.track(&mut alerts2);
        assert_eq!(alerts2[0].first_seen.as_ref().unwrap(), &first);
        assert_ne!(alerts2[0].last_seen.as_ref().unwrap(), &first);
    }

    #[test]
    fn tracker_prunes_resolved_alerts() {
        let tracker = AlertTracker::new();

        let snap_off = snapshot_with_pipeline("pipe1", "off");
        let mut alerts = derive_alerts(&snap_off);
        tracker.track(&mut alerts);
        assert_eq!(tracker.active_count(), 1);

        let snap_on = snapshot_with_pipeline("pipe1", "on");
        let mut alerts = derive_alerts(&snap_on);
        assert!(alerts.is_empty());
        tracker.track(&mut alerts);
        assert_eq!(tracker.active_count(), 0);
    }

    #[test]
    fn tracker_pipeline_scope_does_not_prune_other_pipelines() {
        let tracker = AlertTracker::new();
        let snap = json!({
            "generatedAt": "2026-06-25T00:00:00Z",
            "srtListener": { "udpDrops": 0 },
            "pipelines": {
                "pipe-a": {
                    "input": { "status": "off", "readerMetrics": [] },
                    "outputs": {}
                },
                "pipe-b": {
                    "input": { "status": "off", "readerMetrics": [] },
                    "outputs": {}
                }
            }
        });

        let mut all_alerts = derive_alerts(&snap);
        tracker.track(&mut all_alerts);
        assert_eq!(tracker.active_count(), 2);

        let pipe_a = snapshot_with_pipeline("pipe-a", "off");
        let mut pipe_a_alerts = derive_alerts(&pipe_a);
        tracker.track_pipeline("pipe-a", &mut pipe_a_alerts);

        assert_eq!(tracker.active_count(), 2);
        assert_eq!(
            pipe_a_alerts[0].first_seen,
            all_alerts
                .iter()
                .find(|alert| alert.pipeline_id.as_deref() == Some("pipe-a"))
                .and_then(|alert| alert.first_seen.clone())
        );
    }
}
