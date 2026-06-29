//! Benchmark the production FFmpeg-backed transcoder stage.
//!
//! The stage demuxes input MPEG-TS and pushes `MediaPacket`s directly to the
//! output `RingBuffer` — no output mux/demux round-trip.

use criterion::{Criterion, Throughput, black_box, criterion_group, criterion_main};
use restream::media::avio::MemoryQueue;
use restream::media::ring_buffer::{Reader, RingBuffer};
use restream::media::transcoder::run_ffmpeg_transcoder_stage;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

async fn run_source_passthrough(fixture: &[u8]) -> usize {
    let input = Arc::new(MemoryQueue::new());
    let output = Arc::new(RingBuffer::new(4096));
    input.write(fixture).await;
    input.close();

    run_ffmpeg_transcoder_stage(input, output.clone(), "source", CancellationToken::new())
        .expect("production passthrough stage");

    let mut reader = Reader::new("bench_transcoder".to_string(), output);
    let mut output_bytes = 0usize;
    while let Ok(Some(pkt)) = reader.pull() {
        output_bytes += pkt.payload.len();
    }
    output_bytes
}

fn benchmark_transcoder_runtime_stage(c: &mut Criterion) {
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let fixture_path =
        restream::test_fixtures::canonical_h264_ts_fixture().unwrap_or_else(|e| panic!("{e}"));
    let fixture = std::fs::read(&fixture_path)
        .unwrap_or_else(|e| panic!("failed to read fixture {}: {e}", fixture_path.display()));

    let output_bytes = runtime.block_on(run_source_passthrough(&fixture));
    assert!(output_bytes > 0, "production stage produced no output");

    let mut group = c.benchmark_group("transcoder_runtime_stage");
    group.sample_size(10);
    group.throughput(Throughput::Bytes(fixture.len() as u64));
    group.bench_function("source_passthrough_full_h264_fixture", |b| {
        b.iter(|| runtime.block_on(black_box(run_source_passthrough(black_box(&fixture)))))
    });
    group.finish();
}

criterion_group!(benches, benchmark_transcoder_runtime_stage);
criterion_main!(benches);
