# Architecture

This is the short map of how Restream fits together. The code is the source of truth for implementation details; this document is meant to stay high-level and durable.

## System Shape

```text
Publisher
  -> MediaMTX ingest (RTMP or SRT)
  -> FFmpeg output jobs (pull from MediaMTX via RTMP or SRT)
  -> External streaming destinations

Browser dashboard
  -> Restream Node API
  -> SQLite data.db
  -> MediaMTX control/health API
```

MediaMTX handles media transport. Restream handles control-plane state, dashboard APIs, FFmpeg job orchestration, health aggregation, and recovery decisions.

## Runtime Components

| Component | Role |
|---|---|
| Node API | Serves the dashboard, exposes REST APIs, owns orchestration |
| SQLite `data.db` | Stores stream keys, pipelines, outputs, jobs, logs, and metadata |
| MediaMTX | Accepts ingest, exposes RTMP/SRT relay, serves preview HLS, exposes control APIs |
| FFmpeg | One child process per running output |
| Browser dashboard | Operator UI for pipeline and output control |

## Core Data Model

| Entity | Purpose |
|---|---|
| `stream_keys` | Publisher-facing ingest keys and labels |
| `pipelines` | Logical input streams tied to stream keys |
| `outputs` | Destinations attached to pipelines |
| `jobs` | Current output process state; one row per output |
| `job_logs` | Lifecycle, stderr, control, and history rows |
| `meta` | Small persisted metadata such as ETags |

The app also keeps short-lived in-memory state for live child process handles, FFmpeg progress, output media details, stop requests, probe cache, health snapshots, and recovery timers. SQLite is the durable state; process maps are rebuilt naturally as jobs start and stop.

## Main Flows

### Stream Key Management

Creating or deleting a stream key updates both SQLite and MediaMTX path configuration. If one side fails, the API avoids leaving the two stores intentionally inconsistent.

Effective MediaMTX paths use:

```text
live/<streamKey>
```

### Output Start

Starting an output:

1. Checks that the pipeline and output exist.
2. Rejects duplicate starts for the same output.
3. Probes the MediaMTX path via SRT to confirm the input is available and read codec/format details.
4. Resolves the FFmpeg pull protocol based on the output destination: SRT for SRT and HLS outputs, RTMP for RTMP outputs.
5. Spawns FFmpeg pulling from `srt://localhost:8890?streamid=read:live/<key>` or `rtmp://localhost:1935/live/<key>` and pushing to the output URL.
6. Records the job and lifecycle logs in SQLite.
7. Tracks FFmpeg progress (via fd3) and exit state in the background.

### Output Stop and Delete

Stopping sends a termination signal to the FFmpeg process and records the result. Deleting a pipeline or output first tears down any running output jobs so the database does not remove rows underneath live processes.

### Health

The health service periodically reads MediaMTX runtime state, SQLite job/config state, and FFmpeg progress data. It merges those inputs into the `/health` response used by the dashboard.

Output health status is derived from FFmpeg progress data:

- `on` — job is running and FFmpeg has emitted at least one progress report via fd3
- `warning` — job is running but no progress data has been received yet
- `off` — no running job
- `error` — latest job status is `failed`

See [health-mapping.md](./health-mapping.md) for the detailed status rules.

### Config and ETags

`/config` returns stream keys, pipelines, outputs, jobs, ingest URLs, and app config. The response uses ETags so the browser can poll cheaply. ETags are recomputed after state changes and stored in SQLite so they survive process restarts.

### Recovery

Unexpected output exits can be retried according to the output recovery config. Manual stops set desired state to `stopped` and suppress retries. When an input disappears and later returns, recovery can restart outputs that were likely stopped by input loss.

## Backend Shape

The backend is TypeScript under `src/`, compiled to `dist/` by `npm run build:backend`.

- `src/index.ts`: app composition and route wiring
- `src/types.ts`: shared interfaces (Pipeline, Output, Job, Db, etc.)
- `src/api/`: REST route handlers
- `src/services/`: health monitor, output lifecycle, and startup bootstrap
- `src/db/`: SQLite schema setup and typed query helpers
- `src/utils/`: FFmpeg command building, MediaMTX client, validation, and logging

## Frontend Shape

The dashboard is a TypeScript/ES-module frontend under `public/`.

- `public/ts/`: TypeScript source (compiled to `public/js/` by `npm run build:frontend`)
- `public/ts/core/`: API client, shared state, view-model helpers, utilities
- `public/ts/features/`: dashboard rendering and interaction flows
- `public/ts/history/`: output and pipeline history modals
- `public/input.css`: Tailwind/DaisyUI source
- `public/output.css`: generated CSS

The frontend talks only to Restream APIs. It does not call MediaMTX directly.

## Host Runtime

The current expected runtime is host processes:

```text
MediaMTX :1935 :8890 :8888 :9997
Node app :3030
SQLite   ./data.db
```

Start MediaMTX from the project root with `./mediamtx` or `mediamtx.exe`, then start the app separately with `npm start`. Long-lived Linux deployment is covered in the [Linux VM Deployment](../README.md#linux-vm-deployment-gcp) section in the README.

## Known Constraints

- The app assumes MediaMTX is reachable on localhost with its configured ports.
- `data.db` is local SQLite; there is no built-in replication or backup scheduler.
- FFmpeg and ffprobe must be available to start and validate outputs.
- Output health depends on FFmpeg emitting progress data via fd3; a running job with no progress yet shows as `warning` until the first progress report arrives.
