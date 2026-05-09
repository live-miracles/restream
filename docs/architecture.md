# Architecture

This is the short map of how Restream fits together. The code is the source of truth for implementation details; this document is meant to stay high-level and durable.

## System Shape

```text
Publisher
  -> MediaMTX ingest
  -> MediaMTX RTSP relay
  -> FFmpeg output jobs
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
| MediaMTX | Accepts ingest, exposes RTSP relay, serves preview HLS, exposes control APIs |
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
3. Probes the MediaMTX RTSP input with ffprobe.
4. Spawns FFmpeg with the selected output profile and destination URL.
5. Records the job and lifecycle logs in SQLite.
6. Tracks FFmpeg progress and exit state in the background.

### Output Stop and Delete

Stopping sends a termination signal to the FFmpeg process and records the result. Deleting a pipeline or output first tears down any running output jobs so the database does not remove rows underneath live processes.

### Health

The health service periodically reads MediaMTX runtime state, SQLite job/config state, and FFmpeg progress. It merges those inputs into the `/health` response used by the dashboard.

Output health is correlated by adding a `reader_id` query parameter to each FFmpeg RTSP pull URL:

```text
rtsp://localhost:8554/<streamKey>?reader_id=reader_<pipelineId>_<outputId>
```

When MediaMTX reports a matching RTSP reader, the output is treated as actively connected. See [health-mapping.md](./health-mapping.md) for the detailed status rules.

### Config and ETags

`/config` returns stream keys, pipelines, outputs, jobs, ingest URLs, and app config. The response uses ETags so the browser can poll cheaply. ETags are recomputed after state changes and stored in SQLite so they survive process restarts.

### Recovery

Unexpected output exits can be retried according to the output recovery config. Manual stops set desired state to `stopped` and suppress retries. When an input disappears and later returns, recovery can restart outputs that were likely stopped by input loss.

## Frontend Shape

The dashboard is a static ES-module frontend under `public/`.

- `public/index.html`: main dashboard
- `public/stream-keys.html`: stream-key management
- `public/js/core/`: API, shared state, view-model helpers, utilities
- `public/js/features/`: dashboard rendering and interaction flows
- `public/js/history/`: output and pipeline history modals
- `public/input.css`: Tailwind/DaisyUI source
- `public/output.css`: generated CSS

The frontend talks only to Restream APIs. It does not call MediaMTX directly.

## Host Runtime

The current expected runtime is host processes:

```text
MediaMTX :1935 :8554 :8888 :9997
Node app :3030
SQLite   ./data.db
```

Start MediaMTX from the project root with `./mediamtx` or `mediamtx.exe`, then start the app separately with `npm start`. Long-lived Linux deployment is covered in [deployment-host.md](./deployment-host.md).

## Known Constraints

- The app assumes MediaMTX is reachable on localhost with its configured ports.
- `data.db` is local SQLite; there is no built-in replication or backup scheduler.
- FFmpeg and ffprobe must be available to start and validate outputs.
- Output health depends on MediaMTX exposing RTSP connection query strings.
