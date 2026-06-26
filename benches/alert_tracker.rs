//! Benchmarks for AlertTracker and alert derivation cost.
//!
//! Key questions answered:
//!   1. How much does derive_alerts cost for a typical health snapshot?
//!   2. How much does AlertTracker::track cost per call?
//!   3. What is the cost of the full derive + track cycle?
//!
//! Run:
//!   cargo bench --bench alert_tracker --profile bench-dev

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use restream::alerts::{AlertTracker, derive_alerts};

fn empty_snapshot() -> serde_json::Value {
    serde_json::json!({
        "generatedAt": "2026-06-26T00:00:00Z",
        "pipelines": {}
    })
}

fn active_snapshot(n: usize) -> serde_json::Value {
    let mut pipelines = serde_json::Map::new();
    for i in 0..n {
        pipelines.insert(
            format!("pipe{i}"),
            serde_json::json!({
                "input": {
                    "status": "on",
                    "publisherMetrics": {}
                },
                "outputs": {
                    format!("out{i}"): {
                        "active": true,
                        "bytesOut": 1000
                    }
                },
                "recording": { "status": "off" },
                "sourceRing": {
                    "readers": [
                        {
                            "name": "reader0",
                            "lagSlots": 10,
                            "overflowCount": 0,
                            "burstCount": 0
                        }
                    ]
                }
            }),
        );
    }
    serde_json::json!({
        "generatedAt": "2026-06-26T00:00:00Z",
        "pipelines": pipelines
    })
}

fn bench_derive_alerts_empty(c: &mut Criterion) {
    let snapshot = empty_snapshot();
    c.bench_function("alerts/derive_empty", |b| {
        b.iter(|| black_box(derive_alerts(&snapshot)));
    });
}

fn bench_derive_alerts_5_pipelines(c: &mut Criterion) {
    let snapshot = active_snapshot(5);
    c.bench_function("alerts/derive_5_pipelines", |b| {
        b.iter(|| black_box(derive_alerts(&snapshot)));
    });
}

fn bench_tracker_track_5_alerts(c: &mut Criterion) {
    let tracker = AlertTracker::new();
    let snapshot = active_snapshot(5);
    c.bench_function("alerts/tracker_track_5", |b| {
        b.iter(|| {
            let mut alerts = derive_alerts(&snapshot);
            tracker.track(&mut alerts);
            black_box(&alerts);
        });
    });
}

fn bench_tracker_track_churn(c: &mut Criterion) {
    let tracker = AlertTracker::new();
    let snap_a = active_snapshot(3);
    let snap_b = active_snapshot(5);
    c.bench_function("alerts/tracker_track_churn", |b| {
        b.iter(|| {
            let mut alerts_a = derive_alerts(&snap_a);
            tracker.track(&mut alerts_a);
            let mut alerts_b = derive_alerts(&snap_b);
            tracker.track(&mut alerts_b);
            black_box(tracker.active_count());
        });
    });
}

criterion_group!(
    benches,
    bench_derive_alerts_empty,
    bench_derive_alerts_5_pipelines,
    bench_tracker_track_5_alerts,
    bench_tracker_track_churn,
);
criterion_main!(benches);
