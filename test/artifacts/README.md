# 4x3 Test Artifacts

The 4x3 workflow is driven by one tracked manifest and one Node runner.

## Files

- run-4x3.mjs
- session-4x3-manifest.json

## What The Runner Does

1. Loads session-4x3-manifest.json as the tracked source of truth.
2. Verifies the target app stack is already running.
3. Ensures missing stream keys, pipelines, and outputs exist.
4. Starts ffmpeg input publishers for the manifest stream keys.
5. Starts outputs for the resolved pipeline/output IDs.
6. Waits for manifest-scoped inputs and outputs to become `on`.
7. Captures a `/health` snapshot into test/artifacts/runs.
8. Verifies output auto-retry by dropping and restoring the RTMP output sink.
9. Verifies output auto-retry after an unexpected FFmpeg `SIGKILL` while desired state remains `running`.
10. Verifies input recovery restarts outputs whose desired state remains `running`.
11. Verifies SRT output loopback by temporarily repointing one output to another pipeline's SRT ingest URL, confirming the target pipeline input turns `on`, then restoring original publisher/output state.
12. Verifies RTSP output loopback by temporarily repointing one output to another pipeline's RTSP ingest URL, confirming the target pipeline input turns `on`, then restoring original publisher/output state.

## Primary Entry Points

Start one supported stack first:
- Host mode: `make run-host`
- Docker mode: `make run-docker`

Preferred runner entry point (also starts `nginx-rtmp` if needed):
- make run-4x3

Equivalent bare runner:
- npm run test:4x3

Leave input publishers running after completion for inspection:
- KEEP_RUNNING=1 make run-4x3

Docker mode output URL normalization:
- RTMP_OUTPUT_BASE="rtmp://nginx-rtmp/live" make run-4x3

## Notes

- session-4x3-manifest.json is not rewritten by the runner.
- `make run-4x3` no longer starts the app or MediaMTX; it assumes `make run-host` or `make run-docker` is already running.
- `CLEAN_START` is no longer supported.
- If an output omits `encoding`, the runner assigns a fallback encoding with a safety cap: at most one each of `vertical-crop`, `vertical-rotate`, `720p`, and `1080p`; remaining unspecified outputs default to `source`.
- Logs go to test/artifacts/logs.
- Health snapshots go to test/artifacts/runs.
- During loopback verification, the runner logs the exact source output and target pipeline selection as a structured `[srt-loopback] selection ...` or `[rtsp-loopback] selection ...` line for auditability.
