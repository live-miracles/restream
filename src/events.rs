//! Bounded in-memory lifecycle event log.
//!
//! Captures stage/ingest/egress transitions so operators and agents can
//! answer "what changed recently?" without running full diagnostics.
//! Events are derived from engine state changes, not from polling.
//!
//! The log is bounded at `MAX_EVENTS` entries (FIFO eviction). All access
//! is through a `std::sync::Mutex` so the log can be written from tokio
//! tasks and blocking OS threads alike without async overhead.

use std::collections::VecDeque;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use serde::Serialize;

/// Maximum number of events retained. Oldest are evicted when exceeded.
pub const MAX_EVENTS: usize = 1000;

// ─── Event kinds ─────────────────────────────────────────────────────────────

// Serde note: `rename_all` on an enum only renames VARIANT names. To get camelCase
// field names inside each variant we use `#[serde(rename)]` per field.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum EventKind {
    IngestConnected {
        #[serde(rename = "pipelineId")]
        pipeline_id: String,
        protocol: String,
        #[serde(rename = "streamKey")]
        #[serde(skip_serializing)]
        stream_key: String,
    },
    IngestDisconnected {
        #[serde(rename = "pipelineId")]
        pipeline_id: String,
        protocol: String,
    },
    StageStarted {
        #[serde(rename = "pipelineId")]
        pipeline_id: String,
        encoding: String,
    },
    StageStopped {
        #[serde(rename = "pipelineId")]
        pipeline_id: String,
        encoding: String,
    },
    EgressStarted {
        #[serde(rename = "pipelineId")]
        pipeline_id: String,
        #[serde(rename = "outputId")]
        output_id: String,
    },
    EgressStopped {
        #[serde(rename = "pipelineId")]
        pipeline_id: String,
        #[serde(rename = "outputId")]
        output_id: String,
    },
    EgressFailed {
        #[serde(rename = "pipelineId")]
        pipeline_id: String,
        #[serde(rename = "outputId")]
        output_id: String,
        phase: String,
        error: String,
    },
}

impl EventKind {
    pub fn event_type(&self) -> &'static str {
        match self {
            Self::IngestConnected { .. } => "ingest.connected",
            Self::IngestDisconnected { .. } => "ingest.disconnected",
            Self::StageStarted { .. } => "stage.started",
            Self::StageStopped { .. } => "stage.stopped",
            Self::EgressStarted { .. } => "egress.started",
            Self::EgressStopped { .. } => "egress.stopped",
            Self::EgressFailed { .. } => "egress.failed",
        }
    }

    pub fn pipeline_id(&self) -> &str {
        match self {
            Self::IngestConnected { pipeline_id, .. }
            | Self::IngestDisconnected { pipeline_id, .. }
            | Self::StageStarted { pipeline_id, .. }
            | Self::StageStopped { pipeline_id, .. }
            | Self::EgressStarted { pipeline_id, .. }
            | Self::EgressStopped { pipeline_id, .. }
            | Self::EgressFailed { pipeline_id, .. } => pipeline_id,
        }
    }

    pub fn output_id(&self) -> Option<&str> {
        match self {
            Self::EgressStarted { output_id, .. }
            | Self::EgressStopped { output_id, .. }
            | Self::EgressFailed { output_id, .. } => Some(output_id),
            _ => None,
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::IngestConnected { protocol, .. } => {
                format!("{} publisher connected", protocol.to_uppercase())
            }
            Self::IngestDisconnected { protocol, .. } => {
                format!("{} publisher disconnected", protocol.to_uppercase())
            }
            Self::StageStarted { encoding, .. } => format!("Stage started: {}", encoding),
            Self::StageStopped { encoding, .. } => format!("Stage stopped: {}", encoding),
            Self::EgressStarted { output_id, .. } => format!("Output started: {}", output_id),
            Self::EgressStopped { output_id, .. } => format!("Output stopped: {}", output_id),
            Self::EgressFailed {
                output_id,
                phase,
                error,
                ..
            } => format!("Output failed: {} during {} ({})", output_id, phase, error),
        }
    }
}

// ─── Event ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    /// Monotonically increasing sequence number within this process lifetime.
    pub seq: u64,
    pub timestamp: DateTime<Utc>,
    #[serde(flatten)]
    pub kind: EventKind,
}

// ─── EventLog ────────────────────────────────────────────────────────────────

pub struct EventLog {
    inner: Mutex<EventLogInner>,
    sink: Mutex<Option<tokio::sync::mpsc::UnboundedSender<Event>>>,
}

struct EventLogInner {
    events: VecDeque<Event>,
    next_seq: u64,
}

impl Default for EventLog {
    fn default() -> Self {
        Self::new()
    }
}

impl EventLog {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(EventLogInner {
                events: VecDeque::with_capacity(MAX_EVENTS),
                next_seq: 1,
            }),
            sink: Mutex::new(None),
        }
    }

    pub fn set_sink(&self, sink: tokio::sync::mpsc::UnboundedSender<Event>) {
        *self.sink.lock().unwrap_or_else(|e| e.into_inner()) = Some(sink);
    }

    /// Emit an event. The oldest event is dropped when the log is full.
    pub fn emit(&self, kind: EventKind) {
        let event = {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let seq = inner.next_seq;
            inner.next_seq += 1;
            if inner.events.len() >= MAX_EVENTS {
                inner.events.pop_front();
            }
            let event = Event {
                seq,
                timestamp: Utc::now(),
                kind,
            };
            inner.events.push_back(event.clone());
            event
        };
        if let Some(sink) = self.sink.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            let _ = sink.send(event);
        }
    }

    /// Return up to `limit` most-recent events, optionally filtered by pipeline.
    pub fn recent(&self, limit: usize, pipeline_id: Option<&str>) -> Vec<Event> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner
            .events
            .iter()
            .rev()
            .filter(|e| {
                pipeline_id
                    .map(|pid| e.kind.pipeline_id() == pid)
                    .unwrap_or(true)
            })
            .take(limit)
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn connected(pid: &str) -> EventKind {
        EventKind::IngestConnected {
            pipeline_id: pid.to_string(),
            protocol: "rtmp".to_string(),
            stream_key: "key".to_string(),
        }
    }

    #[test]
    fn recent_returns_events_in_chronological_order() {
        let log = EventLog::new();
        log.emit(connected("p1"));
        log.emit(connected("p2"));
        log.emit(connected("p3"));
        let events = log.recent(100, None);
        assert_eq!(events.len(), 3);
        assert!(events[0].seq < events[1].seq);
        assert!(events[1].seq < events[2].seq);
    }

    #[test]
    fn recent_filters_by_pipeline_id() {
        let log = EventLog::new();
        log.emit(connected("p1"));
        log.emit(connected("p2"));
        log.emit(connected("p1"));
        let p1 = log.recent(100, Some("p1"));
        assert_eq!(p1.len(), 2);
        assert!(p1.iter().all(|e| e.kind.pipeline_id() == "p1"));
    }

    #[test]
    fn recent_respects_limit() {
        let log = EventLog::new();
        for _ in 0..10 {
            log.emit(connected("p1"));
        }
        assert_eq!(log.recent(3, None).len(), 3);
    }

    #[test]
    fn bounded_evicts_oldest() {
        let log = EventLog::new();
        // Emit MAX_EVENTS + 1 events
        for i in 0..=(MAX_EVENTS as u64) {
            let _ = i; // suppress unused warning
            log.emit(connected("p1"));
        }
        let events = log.recent(MAX_EVENTS + 1, None);
        assert_eq!(events.len(), MAX_EVENTS);
        // The first seq in the log should be > 1 (oldest evicted)
        assert!(events[0].seq > 1);
    }
}
