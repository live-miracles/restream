# Testing Artifacts (Saved During Conversation)

This folder stores reusable test artifacts captured from ad-hoc testing sessions.

## Saved Artifacts

- setup-4x3-copy.sh
- start-inputs-from-manifest.sh
- start-outputs-from-manifest.sh
- health-snapshot.sh
- run-4x3.sh
- session-2026-04-10-4x3.json

## What Each Script Does

1. setup-4x3-copy.sh
- Creates 4 stream keys and 4 pipelines.
- Creates 3 outputs per pipeline (12 outputs total).
- Forces output encoding to copy.
- Writes a run manifest to test/artifacts/session-4x3-last.json by default.

2. start-inputs-from-manifest.sh
- Starts one ffmpeg tee publisher that pushes to all stream keys from a manifest.
- Uses test/colorbar-timer.mp4 in loop mode.
- Stores logs and PID files in test/artifacts/logs.

3. start-outputs-from-manifest.sh
- Starts all outputs listed in a manifest via REST API.
- Accepts 200, 201, and 409 as non-fatal responses.

4. health-snapshot.sh
- Captures /health payload to test/artifacts/runs.
- Prints quick ON counts for input/output.

5. run-4x3.sh
- Runs full 4x3 test flow end-to-end.
- Waits for all expected inputs and outputs to reach active state.
- Captures health snapshot.
- `CLEAN_START=0` — skip stack teardown/relaunch and reuse a running stack.
- `KEEP_RUNNING=1` — leave the app and input publishers alive after the run
  so the dashboard and logs can be inspected without the EXIT trap tearing
  everything down.

6. wait-all-active.sh
- Polls `/health` until all manifest inputs and outputs are `on`.
- Treats output `warning` as active (running but reader correlation pending), so readiness reflects actual running outputs.
- Fails if readiness timeout is reached.

## Replay Flow

1. Start services:
- make up

2. Create setup + manifest:
- bash test/artifacts/setup-4x3-copy.sh

3. Start input publishers:
- bash test/artifacts/start-inputs-from-manifest.sh test/artifacts/session-4x3-last.json

4. Start outputs:
- bash test/artifacts/start-outputs-from-manifest.sh test/artifacts/session-4x3-last.json

5. Capture health snapshot:
- bash test/artifacts/health-snapshot.sh

## One-Command Make Flow

Full clean run (default):
- make run-4x3

Leave stack running after completion for inspection:
- KEEP_RUNNING=1 make run-4x3

Reuse an already-running stack (skip media-service restart):
- CLEAN_START=0 make run-4x3

## Notes

- session-2026-04-10-4x3.json is the exact pipeline/output mapping captured from the live run in this conversation.
- session-4x3-last.json is generated each time setup-4x3-copy.sh runs.
