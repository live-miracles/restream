//! Benchmarks for StageMetrics hot-path overhead and PipeMetrics timing cost.
//!
//! Key questions answered:
//!   1. How much does record_in() cost per packet? (atomic fetch_add × 2)
//!   2. How much does timing::now() + timing::delta_us() cost vs Instant::now() + elapsed()?
//!   3. What is the full per-packet overhead in the stdin write path?
//!
//! Run:
//!   cargo bench --bench stage_metrics --profile bench-dev

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use restream::media::engine::{PipeMetrics, StageMetrics};
use std::sync::Arc;
use std::time::Instant;

// ── StageMetrics hot-path ────────────────────────────────────────────────────

fn bench_stage_metrics_record_in(c: &mut Criterion) {
    let m = StageMetrics::new();
    c.bench_function("stage_metrics/record_in", |b| {
        b.iter(|| {
            m.record_in(black_box(1316));
        });
    });
}

fn bench_stage_metrics_record_out(c: &mut Criterion) {
    let m = StageMetrics::new();
    c.bench_function("stage_metrics/record_out", |b| {
        b.iter(|| {
            m.record_out(black_box(1316));
        });
    });
}

fn bench_stage_metrics_record_in_and_out(c: &mut Criterion) {
    let m = StageMetrics::new();
    c.bench_function("stage_metrics/record_in_and_out", |b| {
        b.iter(|| {
            m.record_in(black_box(1316));
            m.record_out(black_box(1316));
        });
    });
}

fn bench_stage_metrics_snapshot(c: &mut Criterion) {
    let m = StageMetrics::new();
    m.record_in(1316);
    m.record_out(1316);
    c.bench_function("stage_metrics/snapshot", |b| {
        b.iter(|| black_box(m.snapshot()));
    });
}

// ── PipeMetrics ──────────────────────────────────────────────────────────────

fn bench_pipe_metrics_record_stall(c: &mut Criterion) {
    let pm = PipeMetrics::default();
    c.bench_function("pipe_metrics/record_stall", |b| {
        b.iter(|| {
            pm.record_stall(black_box(1500));
        });
    });
}

fn bench_pipe_metrics_record_idle(c: &mut Criterion) {
    let pm = PipeMetrics::default();
    c.bench_function("pipe_metrics/record_idle", |b| {
        b.iter(|| {
            pm.record_idle(black_box(2000));
        });
    });
}

fn bench_pipe_metrics_snapshot(c: &mut Criterion) {
    let pm = Arc::new(PipeMetrics::default());
    pm.record_stall(1_500);
    pm.record_idle(2_000);
    c.bench_function("pipe_metrics/snapshot", |b| {
        b.iter(|| black_box(pm.snapshot()));
    });
}

// ── Timing comparison: rdtsc vs Instant ─────────────────────────────────────

fn bench_instant_now_elapsed(c: &mut Criterion) {
    c.bench_function("timing/instant_now_plus_elapsed", |b| {
        b.iter(|| {
            let t0 = Instant::now();
            let _ = black_box(t0.elapsed().as_micros() as u64);
        });
    });
}

fn bench_tsc_now_delta(c: &mut Criterion) {
    use restream::media::timing;
    let using = timing::calibrate();
    let label = if using {
        "timing/tsc_now_plus_delta_us"
    } else {
        "timing/tsc_now_plus_delta_us_instant_fallback"
    };
    c.bench_function(label, |b| {
        b.iter(|| {
            let t0 = timing::now();
            let _ = black_box(timing::delta_us(t0));
        });
    });
}

// ── Simulated stdin write path ────────────────────────────────────────────────
// Models the per-packet overhead in the external transcoder's stdin loop:
//   timing::now() + (write) + timing::delta_us() + conditional record + record_in.
// The write itself is not benchmarked (it's I/O); we measure the surrounding
// instrumentation cost only.

fn bench_stdin_instrumentation_overhead(c: &mut Criterion) {
    use restream::media::timing;
    let sm = StageMetrics::new();
    let pm = PipeMetrics::default();
    let using = timing::calibrate();
    let label = if using {
        "pipe/stdin_instrumentation_per_packet_tsc"
    } else {
        "pipe/stdin_instrumentation_per_packet_instant_fallback"
    };
    const THRESHOLD: u64 = 1_000;

    c.bench_function(label, |b| {
        b.iter(|| {
            let t0 = timing::now();
            // Simulate a fast (non-stalling) write: delta_us will be near zero.
            let write_us = timing::delta_us(t0);
            if write_us > THRESHOLD {
                pm.record_stall(write_us);
            }
            sm.record_in(black_box(1316));
        });
    });
}

criterion_group!(
    benches,
    bench_stage_metrics_record_in,
    bench_stage_metrics_record_out,
    bench_stage_metrics_record_in_and_out,
    bench_stage_metrics_snapshot,
    bench_pipe_metrics_record_stall,
    bench_pipe_metrics_record_idle,
    bench_pipe_metrics_snapshot,
    bench_instant_now_elapsed,
    bench_tsc_now_delta,
    bench_stdin_instrumentation_overhead,
);
criterion_main!(benches);
