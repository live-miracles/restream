//! API/runtime adapter views that project `MediaEngine` state into HTTP-facing
//! JSON payloads.
//!
//! This layer still needs direct access to runtime registries, but it keeps
//! API-facing shaping out of the media modules themselves. The adapter families
//! stay split by responsibility here rather than inside `media`.

mod graph;
mod status;
mod telemetry;

pub(crate) use graph::processing_graph;
pub(crate) use status::{health_snapshot, output_status};
pub(crate) use telemetry::{engine_telemetry, pipeline_telemetry, stage_telemetry_by_display};
