//! Lock-free throughput counters for a processing stage.
//!
//! [`StageMetrics`] is updated atomically on the hot path and read by the
//! `/graph` endpoint for operator visibility. It is shared across all stage
//! types (HLS, recording, external transcoder, h264 transcoder).

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

#[derive(Debug)]
pub struct StageMetrics {
    pub packets_in: AtomicU64,
    pub packets_out: AtomicU64,
    pub bytes_in: AtomicU64,
    pub bytes_out: AtomicU64,
    /// Cumulative processing time in microseconds.
    pub processing_us: AtomicU64,
    pub start_instant: Instant,
}

impl Default for StageMetrics {
    fn default() -> Self {
        Self::new()
    }
}

impl StageMetrics {
    pub fn new() -> Self {
        Self {
            packets_in: AtomicU64::new(0),
            packets_out: AtomicU64::new(0),
            bytes_in: AtomicU64::new(0),
            bytes_out: AtomicU64::new(0),
            processing_us: AtomicU64::new(0),
            start_instant: Instant::now(),
        }
    }

    #[inline]
    pub fn record_in(&self, bytes: u64) {
        self.packets_in.fetch_add(1, Ordering::Relaxed);
        self.bytes_in.fetch_add(bytes, Ordering::Relaxed);
    }

    #[inline]
    pub fn record_out(&self, bytes: u64) {
        self.packets_out.fetch_add(1, Ordering::Relaxed);
        self.bytes_out.fetch_add(bytes, Ordering::Relaxed);
    }

    #[inline]
    pub fn record_processing(&self, us: u64) {
        self.processing_us.fetch_add(us, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> serde_json::Value {
        let pkts_in = self.packets_in.load(Ordering::Relaxed);
        let pkts_out = self.packets_out.load(Ordering::Relaxed);
        let bytes_in = self.bytes_in.load(Ordering::Relaxed);
        let bytes_out = self.bytes_out.load(Ordering::Relaxed);
        let proc_us = self.processing_us.load(Ordering::Relaxed);
        let elapsed = self.start_instant.elapsed().as_secs_f64();

        let avg_us_per_packet = if pkts_in > 0 {
            proc_us as f64 / pkts_in as f64
        } else {
            0.0
        };

        serde_json::json!({
            "packetsIn": pkts_in,
            "packetsOut": pkts_out,
            "bytesIn": bytes_in,
            "bytesOut": bytes_out,
            "processingUs": proc_us,
            "avgUsPerPacket": avg_us_per_packet,
            "uptimeSecs": elapsed,
            "packetsPerSec": if elapsed > 0.0 { pkts_in as f64 / elapsed } else { 0.0 },
        })
    }
}
