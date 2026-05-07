# 4x3 Test Artifacts

The 4x3 workflow is driven by one tracked manifest and one Node runner.

## Files

- run-4x3.mjs
- session-4x3-manifest.json

## What The Runner Does

1. Loads session-4x3-manifest.json as the tracked source of truth.
2. Verifies the target app stack is already running.
3. Verifies the local runner prerequisites (`ffmpeg` and `docker compose`) are available.
4. Ensures missing stream keys, pipelines, and outputs exist without mutating conflicting existing outputs.
5. Starts ffmpeg input publishers for the manifest stream keys.
6. Starts outputs for the resolved pipeline/output IDs.
7. Waits for manifest-scoped inputs and outputs to become `on`; one tracked manifest output is an HLS playlist upload target, so this stage also exercises HTTP PUT HLS egress.
8. Captures a `/health` snapshot into test/artifacts/runs.
9. Verifies output auto-retry by dropping and restoring the RTMP output sink.
10. Verifies output auto-retry after an unexpected FFmpeg `SIGKILL` while desired state remains `running`.
11. Verifies input recovery restarts outputs whose desired state remains `running`.
12. Verifies SRT output loopback by temporarily repointing one output to another pipeline's SRT ingest URL, confirming the target pipeline input turns `on`, then restoring original publisher/output state.
13. Verifies RTSP output loopback by temporarily repointing one output to another pipeline's RTSP ingest URL, confirming the target pipeline input turns `on`, then restoring original publisher/output state.

## Primary Entry Points

Start the app stack first:
- Host mode: `make run-host`
- Or an already-running host/systemd deployment that exposes the same API and MediaMTX ports

Optional disposable container stack:
- make run-docker

Preferred runner entry point (also starts `nginx-rtmp` if needed):
- make run-4x3

Direct runner once `nginx-rtmp` is already running:
- docker compose up -d nginx-rtmp
- npm run test:4x3

Leave input publishers and resources created by the current run in place after completion for inspection:
- KEEP_RUNNING=1 make run-4x3

## Notes

- session-4x3-manifest.json is not rewritten by the runner.
- `make run-4x3` no longer starts the app or MediaMTX; it assumes `make run-host` or another already-running host/systemd stack is already serving the API.
- `make run-docker` starts the optional containerized app + MediaMTX + `nginx-rtmp` stack.
- `make run-4x3` starts `nginx-rtmp` for you; `npm run test:4x3` expects that sink container to already be running.
- `make run-4x3` and `npm run test:4x3` still require host `node`, host `ffmpeg`, and Docker with the compose plugin because the runner starts local publishers and manages the `nginx-rtmp` sink.
- If the prestarted stack is host mode, it also still depends on the `make deps` outputs (`node_modules/` and `bin/mediamtx/mediamtx`).
- `CLEAN_START` is no longer supported.
- If a manifest output name already exists with different settings, the runner stops with a conflict instead of rewriting that output in place.
- When `KEEP_RUNNING` is unset or `0`, shutdown removes only the stream keys, pipelines, and outputs created during the current run.
- SRT/RTSP loopback activation and restore checks use a fixed 30-second window.
- If an output omits `encoding`, the runner assigns a fallback encoding with a safety cap: at most one each of `vertical-crop`, `vertical-rotate`, `720p`, and `1080p`; remaining unspecified outputs default to `source`.
- The tracked manifest includes two HLS outputs targeting a dedicated nginx `/hls-upload/` blackhole endpoint: one `source` output and one transcoded `720p` output, making it clear that missing Frame/FPS badges are tied to encoding mode rather than HLS itself.
- Logs go to test/artifacts/logs.
- Health snapshots go to test/artifacts/runs.
- During loopback verification, the runner logs the exact source output and target pipeline selection as a structured `[srt-loopback] selection ...` or `[rtsp-loopback] selection ...` line for auditability.
