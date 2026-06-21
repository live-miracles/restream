//! Benchmark the production FFmpeg-backed transcoder stage as it exists today.
//!
//! The current `source` path is a full container passthrough/remux through the
//! runtime `MemoryQueue` + custom AVIO implementation. Resolution presets are
//! intentionally not labelled as transcoding here: the production stage does
//! not yet execute its configured decoder/filter/encoder contexts.

use criterion::{Criterion, Throughput, black_box, criterion_group, criterion_main};
use restream::media::avio::MemoryQueue;
use restream::media::transcoder::run_ffmpeg_transcoder_stage;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

fn run_source_passthrough(fixture: &[u8]) -> usize {
    let input = Arc::new(MemoryQueue::new());
    let output = Arc::new(MemoryQueue::new());
    input.write(fixture);
    input.close();

    run_ffmpeg_transcoder_stage(
        input,
        output.clone(),
        "source",
        CancellationToken::new(),
    )
    .expect("production passthrough stage");

    let mut scratch = vec![0u8; 64 * 1024];
    let mut output_bytes = 0usize;
    loop {
        let read = output.read_nonblocking(&mut scratch);
        if read == 0 {
            break;
        }
        output_bytes += read;
    }
    output_bytes
}

fn benchmark_transcoder_runtime_stage(c: &mut Criterion) {
    let fixture_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/test/artifacts/latest/correctness-h264.ts"
    );
    let Ok(fixture) = std::fs::read(fixture_path) else {
        eprintln!("skipping transcoder runtime benchmark: fixture not found at {fixture_path}");
        return;
    };

    let output_bytes = run_source_passthrough(&fixture);
    assert!(output_bytes > 0, "production stage produced no output");

    let mut group = c.benchmark_group("transcoder_runtime_stage");
    group.sample_size(10);
    group.throughput(Throughput::Bytes(fixture.len() as u64));
    group.bench_function("source_passthrough_full_h264_fixture", |b| {
        b.iter(|| black_box(run_source_passthrough(black_box(&fixture))))
    });
    group.finish();
}

criterion_group!(benches, benchmark_transcoder_runtime_stage);
criterion_main!(benches);
