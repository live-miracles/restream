# Resource Sweep

The preferred integration entry point is the native Rust harness mode:

```sh
./target/release/test_harness resource-sweep
```

The sweep measures `restream` CPU and memory across a fixed set of live
integration scenarios:

- empty baseline
- ingest-only for the 5 production ingest shapes
- ingest growth with identical pipelines
- ingest growth with mixed ingest types
- egress growth with source outputs only
- egress growth with mixed RTMP+SRT source outputs
- egress growth with 720p transcode outputs only
- egress growth with mixed RTMP+SRT 720p transcode outputs
- HEVC ingest to RTMP source-output growth (captures the internal HEVC bridge)

Artifacts are written to `test/artifacts/resource-sweep/`:

- `resource-sweep-results.json`: stage aggregates
- `resource-sweep-results.csv`: spreadsheet-friendly summary
- `resource-sweep-samples.jsonl`: raw 1 Hz samples
- `restream.log`, `mediamtx.log`, and publisher logs

The sweep combines:

- OS-level process memory from `/proc/<pid>/status` and `/proc/<pid>/smaps_rollup`
- CPU deltas from `/proc/<pid>/stat`
- internal payload accounting from `GET /api/v1/engine/telemetry`
- child FFmpeg RSS and CPU from `/proc`

For transcode scenarios the CSV/JSON now split CPU into:

- `restream_cpu_*`: CPU consumed inside the `restream` process
- `ffmpeg_cpu_*`: CPU consumed by child `ffmpeg` processes
- `total_cpu_*`: combined CPU for the scenario

Run it with the current optimized binary:

```sh
scripts/resource-limit cargo build --profile bench --bin test_harness
./target/release/test_harness resource-sweep
```

Useful env vars:

- `RESTREAM_BIN=/path/to/restream`
- `WORK_DIR=test/artifacts/resource-sweep-custom`
- `RESOURCE_SWEEP_SAMPLE_SECS=10`
- `RESOURCE_SWEEP_SETTLE_SECS=6`
- `RESOURCE_SWEEP_EGRESS_COUNTS=1,3,6`
- `RESOURCE_SWEEP_INGEST_COUNTS=1,2,4`
- `RESOURCE_SWEEP_LIFECYCLE=isolated|continuous|cumulative`
- `RESOURCE_SWEEP_NO_CLEANUP=1` to leave the final scenario running

To leave the last scenario up for interactive inspection:

```sh
RESOURCE_SWEEP_NO_CLEANUP=1 ./target/release/test_harness resource-sweep
```

Lifecycle modes:

```sh
RESOURCE_SWEEP_LIFECYCLE=isolated ./target/release/test_harness resource-sweep
RESOURCE_SWEEP_LIFECYCLE=continuous ./target/release/test_harness resource-sweep
RESOURCE_SWEEP_LIFECYCLE=cumulative ./target/release/test_harness resource-sweep
```

- `isolated`: restart `restream` and `mediamtx` between scenarios for cleaner attribution
- `continuous`: keep the same processes alive across the whole sweep and only rotate pipelines/publishers
- `cumulative`: keep the same processes and leave prior publishers/pipelines running so later rows show additive growth

Then watch:

- Restream UI: `http://127.0.0.1:3030`
- mediamtx API: `http://127.0.0.1:9997/v3/paths/list`

Manual shutdown:

```sh
pkill -9 -x ffmpeg
pkill -9 -x restream
pkill -9 -x mediamtx
```
