# 4x3 Test Artifacts

The 4x3 workflow is driven by one tracked manifest and one Node runner.

## Files

- run-4x3.mjs
- session-4x3-manifest.json
- mobile-cdp/capture-mobile-layout.mjs

## What The Runner Does

1. Loads session-4x3-manifest.json as the tracked source of truth.
2. Optionally does a clean start of the local stack.
3. Ensures missing stream keys, pipelines, and outputs exist.
4. Starts ffmpeg input publishers for the manifest stream keys.
5. Starts outputs for the resolved pipeline/output IDs.
6. Waits for manifest-scoped inputs and outputs to become `on`.
7. Captures a `/health` snapshot into test/artifacts/runs.
8. Verifies output auto-retry by dropping and restoring the RTMP output sink.
9. Verifies output auto-retry after an unexpected FFmpeg `SIGKILL` while desired state remains `running`.
10. Verifies input recovery restarts outputs whose desired state remains `running`.

## Primary Entry Points

Full clean run (default):
- make run-4x3

Equivalent npm script:
- npm run test:4x3

Leave stack running after completion for inspection:
- KEEP_RUNNING=1 make run-4x3

Reuse an already-running stack (skip media-service restart):
- CLEAN_START=0 make run-4x3

Docker mode (backend in container):
- make run-docker
- CLEAN_START=0 RTMP_OUTPUT_BASE="rtmp://nginx-rtmp/live" make run-4x3

## Mobile CDP Layout Checks

Use Chrome DevTools Protocol device emulation instead of the VS Code integrated browser for mobile layout validation.

Examples:
- `npm run test:mobile:cdp -- --device "iPhone 14 Pro" --url "http://localhost:3030/mobile/dashboard.html?tab=outputs" --wait-for ".output-card"`
- `npm run test:mobile:cdp -- --device "Pixel 7" --url "http://localhost:3030/mobile/keys.html" --wait-for "#mobile-keys-list"`
- `npm run test:mobile:cdp -- --list-devices`

Notes:
- Screenshots are written to `test/artifacts/screenshots/mobile-cdp/` by default.
- Override the Chrome binary with `CHROME_BIN=/path/to/chrome` if `/usr/bin/google-chrome` is not correct on your machine.
- Use `--click <selector>` when a layout state needs an interaction before capture, for example switching to the outputs tab.

## Notes

- session-4x3-manifest.json is not rewritten by the runner.
- If an output omits `encoding`, the runner assigns a fallback encoding with a safety cap: at most one each of `vertical-crop`, `vertical-rotate`, `720p`, and `1080p`; remaining unspecified outputs default to `source`.
- Logs go to test/artifacts/logs.
- Health snapshots go to test/artifacts/runs.
