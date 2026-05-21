# Architecture

This is the short map of how Restream fits together. The code is the source of truth for implementation details; this document is meant to stay high-level and durable.

## System Shape

```text
Publisher (RTMP/SRT)  ─┐
Video file (/media/)  ─┤─> MediaMTX ingest -> FFmpeg output jobs -> External destinations
                        │
Browser dashboard ──────┤-> Restream Node API -> SQLite data.db
                             └-> MediaMTX control/health API
```

MediaMTX handles media transport. Restream handles control-plane state, dashboard APIs, FFmpeg job orchestration, health aggregation, and recovery decisions.

Input sources:
- **Live ingest** — publisher sends RTMP or SRT to MediaMTX
- **Pulled source** — MediaMTX actively pulls a pipeline input from a configured source URL
- **Video ingest** — a pre-recorded video from the `/media` folder is streamed into a pipeline via MediaMTX (loop and start-time configurable)
- **Recording** — any live pipeline can be recorded to an MP4 file in `/media`

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
| `meta` | Small persisted metadata (server name, custom encodings) |
| `ingests` | Video ingest configurations (file, stream key, loop, start time) |

Media files (recordings and video ingest sources) live in the `media/` directory on disk and are not tracked in SQLite beyond ingest configuration rows.

The app also keeps short-lived in-memory state for live child process handles, FFmpeg progress, output media details, stop requests, probe cache, health snapshots, and recovery timers. SQLite is the durable state; process maps are rebuilt naturally as jobs start and stop.

## Main Flows

### Ingest Authorization

MediaMTX calls Restream's local `/internal/mediamtx/auth` endpoint for publish/read/playback
authorization. Publish attempts are allowed only when the path is a configured
`live/<streamKey>`. Unknown-key attempts are counted per publisher IP in a rolling window; once the
limit is reached, that IP is temporarily banned from ingest attempts. Internal read/playback calls
from FFmpeg, ffprobe, and the HLS preview proxy remain localhost-only.

### Pipeline Input Source

Pipeline inputs default to passive publisher ingest through the permanent `live/<streamKey>` MediaMTX
paths. A pipeline can also set an `inputSource` URL. In that mode Restream stores the source in
SQLite and patches the matching MediaMTX path `source` option so MediaMTX actively pulls the stream.

Effective MediaMTX paths use:

```text
live/<streamKey>
```

### Output Start

Starting an output:

1. Checks that the pipeline and output exist.
2. Rejects duplicate starts for the same output.
3. Uses the latest MediaMTX publisher protocol from health state to choose the local pull URL.
4. Spawns FFmpeg pulling via RTMP for RTMP ingest or SRT for SRT ingest, then pushes to the output URL.
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

### Config

`/config` returns pipelines, outputs, jobs, ingest URLs, and app config. The response is read directly from SQLite on every request — no caching layer — which is fast enough given SQLite's in-process read cost and the low poll rate.

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
- `public/output.css`: generated CSS (generated by `npm run build:frontend`)

The frontend talks only to Restream APIs. It does not call MediaMTX directly.

**Polling intervals:**

| Data | Interval | Notes |
|---|---|---|
| `/health` + `/metrics/system` | 5 s | Every poll tick |
| `/config` | ~10 s | Every other tick via toggle; reset to immediate after any mutation or tab focus |
| `/stream-keys` | once on load | Prefetched at module init; cached for the session |
| History logs | on demand | Fetched only when the user opens a history tab |

When the tab is hidden the poll interval drops to 30 s for all endpoints.

## Known Constraints

- The app assumes MediaMTX is reachable on localhost with its configured ports.
- `data.db` is local SQLite; there is no built-in replication or backup scheduler.
- FFmpeg and ffprobe must be available to start and validate outputs.
- Output health depends on FFmpeg emitting progress data via fd3; a running job with no progress yet shows as `warning` until the first progress report arrives.
