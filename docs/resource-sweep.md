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
- egress growth with mixed RTMP+SRT source plus 720p outputs
- egress growth with mixed RTMP+SRT 720p plus 1080p outputs
- egress growth with mixed RTMP+SRT source plus 720p plus 1080p outputs
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
- `RESOURCE_SWEEP_SCENARIOS=baseline-empty,ingest-only,ingest-growth-same,ingest-growth-mixed,egress-growth-source-same,egress-growth-source-mixed,egress-growth-transcode-same,egress-growth-transcode-mixed,egress-growth-source-plus-transcode-mixed,egress-growth-transcode-dual-mixed,egress-growth-source-plus-transcode-dual-mixed,egress-growth-hevc-bridge`
- `RESOURCE_SWEEP_LIFECYCLE=isolated|continuous|cumulative`
- `RESOURCE_SWEEP_NO_CLEANUP=1` to leave the final scenario running

The scenario filter is the main "cheap loop" knob. It lets you rerun only the
slice you care about instead of paying for the whole sweep.

If you specifically want the current-code passthrough-vs-transcode-family
baseline, use the dedicated harness mode instead:

```sh
./target/release/test_harness branch-matrix
```

Examples:

```sh
RESOURCE_SWEEP_SCENARIOS=egress-growth-transcode-mixed \
RESOURCE_SWEEP_EGRESS_COUNTS=10 \
./target/release/test_harness resource-sweep

RESOURCE_SWEEP_SCENARIOS=egress-growth-hevc-bridge \
RESOURCE_SWEEP_EGRESS_COUNTS=10 \
./target/release/test_harness resource-sweep

RESOURCE_SWEEP_SCENARIOS=egress-growth-transcode-dual-mixed,egress-growth-source-plus-transcode-dual-mixed \
RESOURCE_SWEEP_EGRESS_COUNTS=10 \
./target/release/test_harness resource-sweep

HARNESS_SRT_PASSPHRASE=0123456789abcd \
HARNESS_SRT_PBKEYLEN=16 \
RESOURCE_SWEEP_SCENARIOS=ingest-only,egress-growth-source-mixed \
./target/release/test_harness resource-sweep
```

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

## Current Authoritative Snapshot (June 28, 2026)

Source artifacts:

- `test/artifacts/resource-sweep-authoritative/resource-sweep-results.csv`
- `test/artifacts/bitrate-sweep-authoritative/bitrate-sweep-results.csv`

Peak memory and average CPU for the default isolated sweep:

| Scenario | Restream MB | Child FFmpeg MB | Combined MB | Total CPU % |
|---|---:|---:|---:|---:|
| Empty baseline | 72.8 | 0.0 | 72.8 | 1.15 |
| Ingest-only, H.264 RTMP | 59.0 | 0.0 | 59.0 | 1.65 |
| Ingest-only, H.264 SRT | 59.2 | 0.0 | 59.2 | 2.31 |
| Ingest-only, H.265 SRT | 60.9 | 0.0 | 60.9 | 3.31 |
| Same ingest growth, 5 pipelines | 82.6 | 0.0 | 82.6 | 7.27 |
| Mixed ingest growth, 5 pipelines | 75.9 | 0.0 | 75.9 | 8.92 |
| Mixed source egress, 20 outputs | 83.8 | 0.0 | 83.8 | 11.86 |
| Mixed 720p transcode egress, 20 outputs | 120.3 | 166.5 | 286.8 | 51.65 |
| HEVC bridge, 10 RTMP outputs | 158.7 | 0.0 | 158.7 | 71.82 |

Peak internal buffers for the most relevant scaling rows:

| Scenario | Source Ring MB | Transcoder Ring MB | TsMux Ring MB | AVIO HWM MB |
|---|---:|---:|---:|---:|
| Empty baseline | 0.1 | 0.0 | 0.0 | 0.0 |
| Same ingest growth, 5 pipelines | 19.0 | 0.0 | 0.0 | 0.0 |
| Mixed ingest growth, 5 pipelines | 15.6 | 0.0 | 0.0 | 0.0 |
| Mixed source egress, 20 outputs | 5.8 | 0.0 | 1.5 | 0.5 |
| Mixed 720p transcode egress, 20 outputs | 5.7 | 8.3 | 4.3 | 4.6 |
| HEVC bridge, 10 RTMP outputs | 5.8 | 8.2 | 0.0 | 0.0 |

What these numbers say:

- Baseline process cost is about `55-73 MB` depending on whether a pipeline is
  live. The fully empty isolated baseline peaked at `72.8 MB`.
- Ingest fan-in is cheap. Five live ingest pipelines stay under `83 MB` parent
  RSS with single-digit CPU.
- Source egress is cheap. Twenty mixed source outputs stay under `84 MB` parent
  RSS and about `12%` total CPU.
- External 720p transcode is mostly a child-process memory problem. The parent
  grows to `120.3 MB`, but the shared FFmpeg child adds another `166.5 MB`.
- The HEVC bridge is the in-process CPU cliff. It reaches `158.7 MB` and
  `71.82%` CPU without any child FFmpeg process.

## Profiling Workflow

For targeted profiling, avoid a full sweep. Run one scenario with a long sample
window and attach a profiler during the live run:

```sh
WORK_DIR=test/artifacts/profile-external \
RESOURCE_SWEEP_SCENARIOS=egress-growth-transcode-mixed \
RESOURCE_SWEEP_EGRESS_COUNTS=10 \
RESOURCE_SWEEP_SAMPLE_SECS=30 \
./target/release/test_harness resource-sweep
```

On Linux, attach `perf` to the live pid:

```sh
perf stat -e task-clock,context-switches,cpu-migrations,page-faults,minor-faults,major-faults -p <pid> -- sleep 10
perf record -F 99 -e cpu-clock -g -p <pid> -- sleep 10
perf report --stdio --no-children
```

On this WSL setup the `/usr/bin/perf` wrapper did not match the running kernel,
but `/usr/lib/linux-tools/6.8.0-124-generic/perf` worked for software-counter
sampling.

Current profiling takeaways from June 28, 2026:

- External `720p` mixed egress: `restream` itself is light (`282 ms`
  task-clock over a 10 second sample), while child `ffmpeg` carried the load
  (`4.65 s` task-clock). Hot symbols were `x264_8_trellis_coefn`,
  `__memmove_avx_unaligned_erms`, and heavy `pthread_cond_broadcast` wakeups.
- Internal `720p` mixed egress: hot symbols moved into `restream` and included
  `x264_8_encoder_encode`, `slicetype_frame_cost`, `ff_hscale8to15_4_avx2`,
  and `ff_h264_decode_mb_cavlc`.
- HEVC bridge: hot symbols were `x264_8_encoder_encode`,
  `ff_hevc_deblocking_boundary_strengths`, `ff_hevc_hls_filter`,
  `ff_hevc_hls_residual_coding`, plus the same futex/condvar wakeup signature.

Optimization direction from those profiles:

- For external `720p`, optimizing parent Rust code will not move the main CPU
  needle much; the child encoder dominates.
- For internal `720p` and the HEVC bridge, the codec work is real, but the
  wakeup/futex footprint suggests there is still room to reduce handoff churn
  between queueing stages and worker threads.
- Avoiding transcode remains the largest operational win. Source passthrough
  scales cleanly in both CPU and memory.

Then watch:

- Restream UI: `http://127.0.0.1:3030`
- mediamtx API: `http://127.0.0.1:9997/v3/paths/list`

Manual shutdown:

```sh
pkill -9 -x ffmpeg
pkill -9 -x restream
pkill -9 -x mediamtx
```
