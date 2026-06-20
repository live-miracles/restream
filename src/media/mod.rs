//! Media stack — in-process RTMP/SRT ingest, ring buffer fan-out, FFmpeg muxing/transcoding.
//!
//! No external MediaMTX or spawned FFmpeg child processes. All media flows through
//! `RingBuffer` (lock-free, cache-line aligned) with `MemoryQueue`-backed AVIO for
//! FFmpeg integration. Supports H.264, H.265/HEVC, and multi-track audio.

pub mod avio;
pub mod engine;
pub mod hls;
pub mod recording;
pub mod ring_buffer;
pub mod rtmp;
pub mod security;
pub mod simd;
pub mod srt;
pub mod tcp_stats;
pub mod transcoder;
