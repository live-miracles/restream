//! Back-pressure counters for external subprocess stages (FFmpeg stdin/stdout pipe).
//!
//! [`PipeMetrics`] is kept separate from [`super::stage_metrics::StageMetrics`]
//! because it only exists for the external transcoder: internal and
//! MemoryQueue-backed stages have no kernel pipe to observe.
//!
//! The engine stores `Arc<PipeMetrics>` in its `pipe_metrics` registry keyed by
//! the same storage key as `transcoder_buffers`. The processing graph reads it
//! to populate `pipeMetrics` on transcoder nodes.

use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Default)]
pub struct PipeMetrics {
    /// Stdin writes that stalled: the kernel pipe buffer was full because
    /// FFmpeg was not consuming input fast enough.
    pub stalls: AtomicU64,
    /// Cumulative microseconds spent blocked on stalled stdin writes.
    pub stall_us: AtomicU64,
    /// Stdout reads that idled: the kernel pipe was empty because FFmpeg
    /// had not produced output yet (encode is CPU-bound or stalled).
    pub idles: AtomicU64,
    /// Cumulative microseconds spent waiting for idle stdout reads.
    pub idle_us: AtomicU64,
}

impl PipeMetrics {
    #[inline]
    pub fn record_stall(&self, us: u64) {
        self.stalls.fetch_add(1, Ordering::Relaxed);
        self.stall_us.fetch_add(us, Ordering::Relaxed);
    }

    #[inline]
    pub fn record_idle(&self, us: u64) {
        self.idles.fetch_add(1, Ordering::Relaxed);
        self.idle_us.fetch_add(us, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> serde_json::Value {
        let stalls = self.stalls.load(Ordering::Relaxed);
        let stall_us = self.stall_us.load(Ordering::Relaxed);
        let idles = self.idles.load(Ordering::Relaxed);
        let idle_us = self.idle_us.load(Ordering::Relaxed);
        serde_json::json!({
            "stalls":     stalls,
            "stallUs":    stall_us,
            "avgStallUs": stall_us.checked_div(stalls).unwrap_or(0),
            "idles":      idles,
            "idleUs":     idle_us,
            "avgIdleUs":  idle_us.checked_div(idles).unwrap_or(0),
        })
    }
}
