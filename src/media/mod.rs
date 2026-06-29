//! Media stack — in-process RTMP/SRT ingest, ring buffer fan-out, FFmpeg muxing/transcoding.
//!
//! No external MediaMTX or spawned FFmpeg child processes. All media flows through
//! `RingBuffer` (lock-free, cache-line aligned) with `MemoryQueue`-backed AVIO for
//! FFmpeg integration. Supports H.264, H.265/HEVC, and multi-track audio.

pub mod avio;
pub mod codec;
pub mod engine;
pub mod engine_registries;
pub mod external_transcoder;
pub mod feeder;
pub mod file_ingest;
pub mod h264_transcoder;
pub mod hls;
pub mod hls_upload;
pub mod mpegts;
pub mod pipe_metrics;
pub mod profiles;
pub mod recording;
pub mod ring_buffer;
pub mod rtmp;
pub mod security;
pub mod srt;
pub mod stage_metrics;
pub mod tcp_stats;
pub mod timing;
pub mod transcoder;
pub mod ts_chunk_ring;
