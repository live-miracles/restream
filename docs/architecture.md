# Restream Architecture

This document describes the current implemented architecture.

---

## 1. System Overview

Restream is a control plane that sits on top of [MediaMTX](https://github.com/bluenviron/mediamtx). It manages stream ingest keys, pipelines, and outbound FFmpeg jobs via a REST API and a browser-based dashboard. MediaMTX handles all media routing; Restream handles all orchestration.

```
RTMP Publisher ──► MediaMTX (RTMP :1935)
                       │
                       │ RTSP :8554 (pulled by FFmpeg)
                       ▼
                   FFmpeg Job(s) ──► External Platforms (YouTube, Facebook, etc.)

Browser ──► Node API :3030 ──► SQLite (data/data.db)
                    │
                    └──► MediaMTX API :9997 (health, path config, connection stats)
```

Stream-key create/delete is a cross-system operation touching both SQLite and MediaMTX path
configuration. The API now treats the SQLite write as the second phase and compensates by rolling
back the MediaMTX mutation if that DB phase fails, so the two control-plane stores do not drift on
single-request errors.

Pipeline and output deletion now also avoid split-brain windows between DB state and live FFmpeg
processes. If a delete targets a running job, the API waits for teardown to complete before
removing rows; if teardown times out or fails, the delete returns `409` and leaves the resources in
place.

### Runtime Components

| Component          | Role                                      | Default port(s)         |
|--------------------|-------------------------------------------|-------------------------|
| Node API server    | REST control plane + static UI            | `3030`                  |
| SQLite (`data/data.db`) | Persistent state (keys, pipelines, jobs)  | —                       |
| MediaMTX           | RTMP ingest, RTSP relay, path management  | `1935`, `8554`, `9997`  |
| FFmpeg             | Per-output RTSP→RTMP push process         | spawned on demand       |
| nginx-rtmp         | Local test RTMP sink for Docker/test workflows | `1936` RTMP, `8081` HTTP|

### Code Layers And Ownership

The refactor intentionally pushed the backend into a small number of layers.

1. Composition root: `src/index.js` wires services together and should stay light on business
  logic.
2. Route modules: `src/routes-pipeline.js`, `src/routes-output.js`, and `src/preview.js`
  validate request shape, translate HTTP concerns, and call services.
3. Service modules: `src/pipeline-runtime-state.js`, `src/health.js`, `src/outputs.js`,
  `src/recovery.js`, and `src/bootstrap.js` own orchestration, shared runtime state, retry policy,
  health aggregation, and process lifecycle behavior.
4. Persistence and pure helpers: `src/db.js` owns SQLite access, while `src/utils.js`,
  `src/health-compute.js`, and `src/recovery-helpers.js` own shared calculations and protocol-
  specific logic.
5. Frontend modules: `public/js/client.js` and `public/js/pipeline.js` own fetch/state primitives,
  `public/js/features/` owns dashboard behavior, and `public/js/history.js` owns the history modal flows.

Two backend snapshots are especially important to keep mentally separate:

- `/config` is the durable control-plane snapshot, built from SQLite plus public config.
- `/health` is the live runtime snapshot, built from MediaMTX, process Maps, and selected DB
  metadata.

When debugging UI drift, decide which snapshot is wrong before changing any implementation.

For a code-oriented breakdown of the backend service boundaries, see
[backend-services.md](./backend-services.md). For the recommended reading order for new
contributors, see [onboarding.md](./onboarding.md).

---

## 2. Data Model

### 2.1 Entities (SQLite)

```
stream_keys
  key         TEXT PK        -- random hex or user-supplied
  label       TEXT           -- human-readable name
  created_at  TEXT

pipelines
  id          TEXT PK
  name        TEXT NOT NULL
  stream_key  TEXT FK → stream_keys.key ON DELETE SET NULL
  encoding    TEXT           -- reserved; not used for runtime routing
  created_at  TEXT
  updated_at  TEXT

outputs
  id          TEXT PK
  pipeline_id TEXT FK → pipelines.id ON DELETE CASCADE
  name        TEXT NOT NULL
  url         TEXT NOT NULL  -- destination RTMP URL
  encoding    TEXT           -- 'source' | 'vertical-crop' | 'vertical-rotate' | '720p' | '1080p'
  created_at  TEXT

jobs
  id          TEXT PK
  pipeline_id TEXT FK → pipelines.id ON DELETE CASCADE
  output_id   TEXT FK → outputs.id ON DELETE CASCADE
  UNIQUE(pipeline_id, output_id)   -- enforced by unique index; one row per output
  pid         INTEGER        -- OS process ID of FFmpeg (null if proc died before record)
  status      TEXT           -- 'running' | 'stopped' | 'failed'
  started_at  TEXT
  ended_at    TEXT
  exit_code   INTEGER
  exit_signal TEXT

job_logs
  id          INTEGER PK AUTOINCREMENT
  job_id      TEXT           -- optional reference to current/previous job id
  pipeline_id TEXT           -- direct lookup key for history by pipeline
  output_id   TEXT           -- direct lookup key for history by output
  event_type  TEXT           -- stable event code, e.g. lifecycle.started or pipeline.input_state.transitioned
  event_data  TEXT           -- optional JSON payload for structured event details
  ts          TEXT
  message     TEXT           -- human-readable line kept for raw logs and operator inspection

meta
  key         TEXT PK
  value       TEXT           -- used for ETag persistence across restarts
```

### 2.2 In-Memory State

`processes` (`Map<jobId, ChildProcess>`) — runtime-only reference to live FFmpeg processes. Lost on server restart; DB `status` is authoritative.

`ffmpegProgressByJobId` (`Map<jobId, Record<string,string>>`) — latest parsed FFmpeg `-progress pipe:3` key/value block per running job.

`ffmpegOutputMediaByJobId` (`Map<jobId, { video, audio }>` ) — parsed output media details extracted from FFmpeg stderr `Output #0` section.

`stopRequestedJobIds` (`Set<jobId>`) — tracks IDs of jobs for which the user explicitly called `POST /stop`. Used by the exit handler to classify exit status as `stopped` vs `failed`. An entry is added on `/stop` and removed after the exit event fires.

---

## 3. Configuration

Runtime config is loaded from `src/config/restream.json` (path overridable via `RESTREAM_CONFIG_PATH`). It is read fresh on every `/config` request and merged into the snapshot. See [configuration.md](./configuration.md) for all options.

---

## 4. API Server Call Flows

### 4.1 Output Start (`POST /pipelines/:pipelineId/outputs/:outputId/start`)

```
Client
  │
  ▼
Acquire in-memory start lock for (pipelineId, outputId)
  │ already locked → 409 "Start already in progress for this output"
  ▼
Validate pipeline + output exist in DB
  │
  ▼
Check for existing running job (409 if found)
  │
  ▼
Resolve probe URL: rtsp://localhost:8554/<streamKey>
  │
  ▼
ffprobe -rtsp_transport tcp <probeUrl>   (8 s timeout)
  │ fail → 409 "Pipeline input is not available yet"
  │ ok  → cache probe result in streamProbeCache (TTL: PROBE_CACHE_TTL_MS)
  ▼
Build tagged RTSP URL:
  rtsp://localhost:8554/<streamKey>?reader_id=reader_<pipelineId>_<outputId>
  │
  ▼
Build FFmpeg args:
  ffmpeg -nostdin -hide_banner -loglevel info
         -nostats -stats_period 1 -progress pipe:3
         -rtsp_transport tcp
         -i <taggedRtspUrl>
         (profile by output encoding)
           source: -c:v copy -c:a copy
           vertical-crop: -vf scale=720:1280:force_original_aspect_ratio=increase,crop=720:1280 + libx264/aac
           vertical-rotate: -vf transpose=1,scale=720:1280:force_original_aspect_ratio=increase,crop=720:1280 + libx264/aac
           720p: -vf scale=-2:720 + libx264/aac
           1080p: -vf scale=-2:1080 + libx264/aac
         protocol-specific output flags:
           rtmp/rtmps: -flvflags no_duration_filesize -rtmp_live live -f flv <outputUrl>
           srt: -f mpegts <outputUrl>
           rtsp/rtsps: -f rtsp -rtsp_transport tcp <outputUrl>
  │
  ▼
spawn(ffmpegCmd, args)
  │
  ▼
db.createJob({ status: 'running', pid, startedAt })
db.appendJobLog(jobId, '[lifecycle] started status=running pid=<pid|null>')
recomputeEtag()
processes.set(jobId, child)
  │
  └─ ON CONFLICT(pipeline_id, output_id): updates existing row (no jobs-table growth)
  │
  ▼
Wait 250 ms → check if job still 'running'
  │ failed immediately → 500 + last 100 log lines
  │ still running      → 201 { job }
  ▼
[Background] child stdout/stderr → db.appendJobLog()
[Background] child 'exit' → db.updateJob({ status, exitCode, exitSignal })
              → if unexpected terminal exit and not user-stop:
                register failureCount
                schedule auto-retry based on outputRecovery config
                append [lifecycle] retry_decision failureCount=<n> scheduled=<true|false>
                input-unavailable clean stops append [lifecycle] retry_suppressed ... instead
                if scheduled=false append [lifecycle] retry_exhausted ... action=give_up
                           → recomputeEtag()
                           → processes.delete(jobId)
                           → stopRequestedJobIds.delete(jobId)
```

### 4.2 Output Stop (`POST /pipelines/:pipelineId/outputs/:outputId/stop`)

```
Client
  │
  ▼
db.getRunningJobFor(pipelineId, outputId)
  │ none → 404
  ▼
stopRequestedJobIds.add(jobId)
db.appendJobLog(jobId, '[lifecycle] stop_requested signal=<signal> status=running')
  │
  ▼
processes.get(jobId) → send SIGTERM
  │ if process still alive after 5 s → SIGKILL (setTimeout)
  │ if process ref missing → mark job 'stopped' in DB directly
  ▼
recomputeEtag()
200 { jobId, result }
```

### 4.3 Health Aggregation (`GET /health`)

```
Client
  │
  ▼
Parallel fetch from MediaMTX API:
  ├── GET /v3/paths/list         → pathByName Map
  ├── GET /v3/rtspconns/list     → rtspConnectionRecords
  ├── GET /v3/rtspsessions/list  → rtspSessionById Map
  ├── GET /v3/rtmpconns/list     → RTMP publishers by path
  ├── GET /v3/srtconns/list      → SRT publishers by path

Build rtspByReaderTag Map:
  for each RTSP connection:
    parse ?reader_id=<tag> from conn.query
    rtspByReaderTag.set(tag, [conn, ...])

Load from DB:
  ├── listPipelines()
  ├── listOutputs()
  └── listJobs() → jobByOutputId Map (one row per output due to upsert)

For each pipeline:
  ├── Input health:
  │     pathInfo = pathByName.get(streamKey)
  │     status = 'on'      if pathInfo.available (fallback: deprecated pathInfo.ready)
  │            = 'warning' if pathInfo.online && !pathInfo.available
  │            = 'off'     otherwise
  │     publishStartedAt = pathInfo.availableTime || pathInfo.readyTime   (protocol-agnostic)
  │     video/audio = from pathInfo.tracks2 + ffprobe cache (if available)
  │
  └── For each output:
      latestJob = jobByOutputId.get(outputId)
        status = 'error'   if latestJob.status === 'failed'
               = 'off'     if no running job
               = 'on'      if running AND rtspByReaderTag has reader_<pid>_<oid>
               = 'warning' if running AND no reader tag match

200 { generatedAt, status: 'ready', mediamtx: { pathCount, rtspConnCount, rtmpConnCount, srtConnCount, ready }, pipelines: {...} }
When MediaMTX is unavailable:
{ generatedAt, status: 'degraded', mediamtx: { ...counts, ready }, pipelines: {} }
```

### 4.4 Output History (`GET /pipelines/:pipelineId/outputs/:outputId/history`)

```
Client
  │
  ▼
Validate pipeline + output exist in DB
  │
  ▼
Parse and clamp query limit (default 200, range 1..1000)
  │
  ▼
Parse optional history filters:
  - since / until timestamps
  - order (asc|desc)
  - prefix families (stderr, exit, control, ...)
  - filter=lifecycle shortcut
  │
  ▼
db.listJobLogsByOutputFiltered(pipelineId, outputId, filters)
  │
  ▼
Return filtered logs:
  { pipelineId, outputId, logs: [{ ts, message }, ...] }
```

`job_logs.message` includes `[lifecycle] ...` lines for key job-table transitions (`started`, `stop_requested`, `failed_on_error`, `exited`, `retry_decision`, `retry_exhausted`, `marked_stopped_no_process`) so UI can render a structured timeline while preserving raw logs. The `exited` line includes `requestedStop=<true|false>` to distinguish intentional stops from failures.

### 4.5 Config Snapshot (`GET /config`)

```
Client (browser dashboard polls this)
  │
  ├── sends If-None-Match: "<etag>" (if known)
  │
  ▼
Server reads ETag from db.getEtag()
  │ If-None-Match matches current ETag → 304 Not Modified (no body)
  │
  ▼
Build snapshot:
  { ...runtimeConfig,       ← from src/config/restream.json
    streamKeys,             ← db.listStreamKeys()
    pipelines,              ← db.listPipelines() + per-pipeline ingestUrls
    outputs,                ← db.listOutputs()
    jobs }                  ← db.listJobs()

200 { snapshot }  +  ETag: "<hash>"
```

ETag is recomputed (SHA-256 of deterministic JSON snapshot) on every job/pipeline/output state change and persisted in the `meta` table so it survives restarts.

### 4.6 Stream Key Creation (`POST /stream-keys`)

```
Client
  │
  ▼
Generate key (random 24-char hex) or use provided value
  │
  ▼
Check for duplicate in DB (409 if exists)
  │
  ▼
POST MediaMTX /v3/config/paths/add/<key>
  │ error → 500
  ▼
db.createStreamKey({ key, label, createdAt })
201 { streamKey }
```

`<key>` above is provisioned as effective path `live/<streamKey>` in MediaMTX.

### 4.7 Stream Key Deletion (`DELETE /stream-keys/:key`)

```
Client
  │
  ▼
Check key exists in DB (404 if not)
  │
  ▼
DELETE MediaMTX /v3/config/paths/delete/<key>
  │ error → 500 (DB row is not deleted)
  ▼
db.deleteStreamKey(key)
200 { message }
```

Deletion targets the same effective path (`live/<streamKey>`).

### 4.8 Pipeline/Output Deletion with Cascade Stop

```
DELETE /pipelines/:id
  ├── Find all outputs for pipeline
  ├── For each output with a running job → stopRunningJob() (SIGTERM → SIGKILL)
  └── db.deletePipeline(id)  [CASCADE deletes outputs + jobs]

DELETE /pipelines/:pipelineId/outputs/:outputId
  ├── Find running job for output → stopRunningJob()
  └── db.deleteOutput(pipelineId, outputId)
```

### 4.9 Backend Module Boundaries

Recent backend refactors moved reusable runtime helpers out of bootstrap wiring and into dedicated utility modules.

| File | Role |
|---|---|
| `src/index.js` | Composes services/routes and wires shared runtime maps and dependencies |
| `src/routes-pipeline.js` | Config snapshot, pipeline CRUD, and stream-key routes |
| `src/routes-output.js` | Output lifecycle/history routes |
| `src/pipeline-runtime-state.js` | Shared input transition history and recovery coordination state |
| `src/health.js` + `src/health-compute.js` | Live health monitor plus pure health/media helpers |
| `src/outputs.js` + `src/recovery.js` + `src/recovery-helpers.js` | Output lifecycle, retry policy, and process/restart helper seams |
| `src/db.js` | Schema, migrations, and durable query layer |
| `src/utils.js` | Shared validation, logging, MediaMTX, FFmpeg, and redaction helpers |

This was a structure/maintainability change only; API behavior and external contracts remained the same.

### 4.10 Internal Backend APIs And Contracts

The backend now relies on a small number of internal service contracts. These are not public API
surfaces, but they are stable enough that new code should treat them as explicit seams rather than
reaching through neighboring modules.

#### 4.10.1 Composition-Root Wiring Contract

`src/index.js` is the only place that should wire long-lived service instances together.

```
src/index.js
  |
  +-- createRuntimeRegistries()
  |     -> processes
  |     -> ffmpegProgressByJobId
  |     -> ffmpegOutputMediaByJobId
  |
  +-- createPipelineRuntimeStateService({ db })
  |     -> pipelineRuntimeState
  |
  +-- createHealthMonitorService({
  |       db,
  |       fetch,
  |       normalizeEtag,
  |       ffmpegProgressByJobId,
  |       ffmpegOutputMediaByJobId,
  |       pipelineRuntimeState,
  |       spawn,
  |     })
  |
  +-- createOutputLifecycleService({
  |       db,
  |       getConfig,
  |       spawn,
  |       processes,
  |       ffmpegProgressByJobId,
  |       ffmpegOutputMediaByJobId,
  |       recomputeEtag,
  |       isLatestJobLikelyInputUnavailableStop:
  |         pipelineRuntimeState.isLatestJobLikelyInputUnavailableStop,
  |     })
  |
  +-- registerPipelineApi({ ..., pipelineRuntimeState, ... })
  +-- registerOutputApi({ ..., reconcileOutput, stopRunningJobAndWait, ... })
  |
  \-- pipelineRuntimeState.setInputRecoveryHandler(
        outputLifecycle.restartPipelineOutputsOnInputRecovery
      )
```

Contract rules:

- `src/index.js` owns callback registration and concrete singleton creation.
- `src/health.js` and `src/outputs.js` must not import each other directly.
- Shared mutable Maps for live FFmpeg state are created once in the composition root and passed
  down; route modules do not own them.

#### 4.10.2 Pipeline Runtime-State Contract

`src/pipeline-runtime-state.js` is the shared coordinator for pipeline input transitions. It exists
so routes, health collection, and output recovery can share one state model without reaching into
each other's implementation details.

```
pipeline-runtime-state.js
  |
  +-- bootstrap()
  |     seeds in-memory input-status history from DB + MediaMTX
  |
  +-- resolveInputState(streamKey, existingEverSeenLive)
  |     used by routes when pipeline stream-key assignments change
  |
  +-- seedPipelineState(pipelineId, status)
  +-- clearPipelineState(pipelineId)
  |     used by routes after create/update/delete
  |
  +-- recordPipelineInputStatus(pipelineId, inputStatus, { publisher })
  |     used by health.js while building live snapshots
  |
  +-- isLatestJobLikelyInputUnavailableStop(pipelineId, latestJob)
  |     used by outputs.js recovery logic
  |
  \-- setInputRecoveryHandler(fn)
        called once by src/index.js
```

Method-level contract:

| Method | Called by | Contract |
|---|---|---|
| `bootstrap()` | `health.start()` | Seed in-memory input status before periodic collectors start. |
| `resolveInputState(streamKey, existingEverSeenLive)` | `routes-pipeline.js` | Returns `{ status, inputEverSeenLive }` from current MediaMTX state; routes use this to initialize persisted pipeline fields. |
| `seedPipelineState(pipelineId, status)` | `routes-pipeline.js` | Seeds a new or updated pipeline's in-memory state after config changes. |
| `clearPipelineState(pipelineId)` | `routes-pipeline.js` | Removes transient state after pipeline deletion. |
| `recordPipelineInputStatus(pipelineId, inputStatus, { publisher })` | `health.js` | Logs transitions, updates the last input-unavailable timestamp, and fires the recovery callback on a non-`on` to `on` transition. |
| `isLatestJobLikelyInputUnavailableStop(pipelineId, latestJob)` | `outputs.js` | Classifies a clean stop near an input-off transition so recovery can suppress noisy retries. |
| `setInputRecoveryHandler(fn)` | `index.js` | Registers a fire-and-forget callback with signature `fn(pipelineId)`. The runtime-state service does not await or catch this callback. |

The important boundary is that `routes-pipeline.js` only uses the route-facing subset
(`resolveInputState`, `seedPipelineState`, `clearPipelineState`), while `src/health.js` owns live
transition recording and `src/outputs.js` owns recovery decisions.

#### 4.10.3 Health Monitor Contract

`createHealthMonitorService()` returns exactly three operational entrypoints:

```
healthMonitor
  |
  +-- start()
  |     1. start MediaMTX readiness checks
  |     2. await pipelineRuntimeState.bootstrap()
  |     3. start periodic health snapshot collection
  |
  +-- stop()
  |     clears readiness, collector, and probe-eviction timers
  |
  \-- registerRoutes(app)
        GET /health
        GET /healthz
```

Contract rules:

- `start()` must run before the app is considered ready.
- `registerRoutes(app)` is pure route registration; it does not start background work by itself.
- `src/health.js` owns probe cache, readiness polling, and live snapshot assembly, but it does not
  own desired-state or retry policy.
- `src/health.js` may receive a `pipelineRuntimeState` instance from `src/index.js`; that shared
  instance is the normal app path. The fallback local creation path exists only so the module can
  still be exercised in isolation.

#### 4.10.4 Output Lifecycle And Recovery Contract

`src/outputs.js` owns concrete FFmpeg worker lifecycle. `src/recovery.js` owns desired-state,
retry-budget, and stop/restart coordination policy.

```
routes-output.js
  |
  +-- setOutputDesiredState(...)
  +-- resetOutputFailureCount(...)
  \-- reconcileOutput(...)
          |
          +-- desiredState == stopped
          |     -> stopRunningJob() when a job is active
          |
          +-- desiredState == running and job exists
          |     -> already_running
          |
          \-- desiredState == running and no job exists
                -> startOutputJob()
                   -> spawn ffmpeg
                   -> create/update job row
                   -> register process + progress maps
```

Internal contract split:

| Provider | Owns | Consumers |
|---|---|---|
| `src/outputs.js` | FFmpeg spawn/exit wiring, progress parsing, job start stability check, graceful shutdown | `routes-output.js`, `src/index.js` |
| `src/recovery.js` | desiredState, failure counts, start locks, restart timers, stop escalation, input-recovery restart policy | `src/outputs.js` |

Contract rules:

- Route handlers should not spawn or kill FFmpeg directly; they must go through
  `reconcileOutput()`, `stopRunningJob()`, or `stopRunningJobAndWait()`.
- `desiredState` is operator or system intent. `jobs.status` is actual process outcome. The two are
  allowed to diverge during retries and input outages.
- `restartPipelineOutputsOnInputRecovery(pipelineId)` is the recovery callback installed by the
  composition root; it is triggered by pipeline input transitions, not by routes.

#### 4.10.5 Route Registration Contract

The route modules are intentionally narrow adapters around services and DB operations.

```
registerConfigApi(app, db, getConfig, toPublicConfig)
  -> owns /config and /config/version
  -> owns ETag recomputation helpers returned to index.js

registerPipelineApi(app, ..., pipelineRuntimeState, ...)
  -> owns stream-key CRUD and pipeline CRUD
  -> may mutate MediaMTX path config and DB in one request
  -> may seed or clear runtime state after config mutations

registerOutputApi(app, ..., reconcileOutput, stopRunningJobAndWait, ...)
  -> owns output CRUD, start/stop endpoints, and history endpoints
  -> delegates output state transitions to the lifecycle/recovery seam
```

The route modules may coordinate multiple subsystems, but they should stop at orchestration. The
long-lived runtime Maps, timers, and retry policy all belong to services, not to route handlers.

---

## 5. Frontend Architecture

### 5.1 Files

| File                  | Role                                                          |
|-----------------------|---------------------------------------------------------------|
| `public/index.html`   | Dashboard SPA shell                                           |
| `public/stream-keys.html` | Stream key management page                               |
| `public/js/client.js` | Shared mutable UI state plus API/ETag helpers |
| `public/js/pipeline.js` | `parsePipelinesInfo()` and throughput helpers |
| `public/js/features/dashboard.js` | Polling orchestration and config/version drift detection |
| `public/js/features/dashboard-view.js` | Dashboard DOM rendering, metrics, and health banner state |
| `public/js/features/editor.js`    | Pipeline/output edit flows and start/stop controls             |
| `public/js/features/view.js` | Selected-pipeline detail, ingest details, preview, and output-list wiring |
| `public/js/features/output-list-view.js` | Output row DOM builder, metrics badges, and per-output action buttons |
| `public/js/history.js` | Output/pipeline history modal control, polling, and rendering |
| `public/js/history/classify.mjs` | Pure history classification helpers |
| `public/js/utils.js`     | `setServerConfig()`, masking, copy, formatting, and DOM helpers |
| `public/output.css`   | Compiled Tailwind + DaisyUI output (do not edit manually)     |
| `input.css`           | Tailwind source (project root, compiled with `make css`)      |

### 5.2 Dashboard Polling Call Flow

```
Page load
  │
  ▼
fetchConfig()          GET /config (with If-None-Match ETag)
fetchHealth()          GET /health
fetchSystemMetrics()   GET /metrics/system (latest fixed background sample)
  │
  ▼
parsePipelinesInfo()   merges config.pipelines + config.outputs + config.jobs
                       + health.pipelines into pipeline view-model array
  │
  ▼
renderPipelines()      DOM update for pipeline cards and output tables
renderMetrics()        DOM update for system metrics (CPU, mem, disk, net)
  │
  ▼
guarded poll loop schedules the next refresh after the current one finishes
visibility changes retune that loop between visible/hidden intervals
setInterval(checkStreamingConfigs, 30000)       external-change detection (see below)
```

`GET /metrics/system` no longer advances the rate-calculation baseline on every request. A server-side timer samples CPU and network counters on a fixed cadence, and dashboard polls just read the latest completed sample.

Dashboard refresh triggers are also coalesced client-side. Poll ticks, visibility refreshes, and mutation-driven refreshes all funnel through a single in-flight gate, and the dashboard/history pollers now share the same guarded timeout-loop behavior so a slow refresh cannot overlap with another full fetch pass and later overwrite fresher state.

`public/index.html` and `public/stream-keys.html` load frontend entry modules as ES modules (`<script type="module">`). The dashboard page now boots through `public/js/features/dashboard-entry.js`, which imports the dashboard/history/editor feature graph and registers the few cross-feature callbacks that would otherwise create circular dependencies. HTML-bound handlers used by inline attributes remain the only frontend functions intentionally exposed on `window`.

### 5.5 Frontend Module Conventions

See [frontend-modules.md](./frontend-modules.md) for implementation-level rules and examples. In short:

- Import dependencies explicitly; do not rely on implicit globals.
- Keep shared mutable dashboard state in `public/js/client.js`.
- Expose `window.*` only for handlers invoked directly by HTML attributes or legacy hooks.
- Prefer normal module exports/imports for all other cross-file calls.

#### External-change detection (`checkStreamingConfigs`)

The dashboard tracks two ETag variables:

| Variable | Updated by | Purpose |
|---|---|---|
| `etag` | every `fetchConfig()` (background poll) | keeps conditional GET efficient |
| `userConfigEtag` | only after this tab successfully mutates config | marks last change *this user* made |

Every 30 s, `checkStreamingConfigs` calls `GET /config` with `If-None-Match: userConfigEtag`. If the server returns 200 (current ETag differs from `userConfigEtag`), a mutation happened that this tab did not initiate. After a 5 s confirmation re-check, the `#streaming-config-changed-alert` warning banner is shown, prompting the user to refresh. A 304 keeps the banner hidden. Delayed re-checks are canceled when this tab updates its own baseline ETag, and stale re-checks (queued with an old baseline) are ignored so local edits do not trigger false warnings. After successful local mutations, the client re-syncs baseline from `HEAD /config/version` before storing `userConfigEtag`.

The dashboard now also checks a shared `X-Snapshot-Version` token returned by both `/config` and
`/health`. That token represents the config/jobs version seen by each endpoint. Health snapshots
refresh themselves when their cached token is behind the current config/jobs state, and the client
retries a refresh if the two responses still disagree, preventing mixed-moment merges after rapid
mutations.

### 5.3 Throughput Computation (client-side)

`public/js/pipeline.js` maintains the input-throughput baseline. On each poll cycle,
`computeKbps(stateMap, key, totalBytes, nowMs)` computes input bitrate from the delta of
`input.bytesReceived` between cycles:

```
kbps = (deltaBytes × 8) / (deltaMs / 1000) / 1000
```

These values are stored as numeric Kbps in the dashboard model. At render time, the UI formats bitrate display with adaptive units (`kb/s`, `mb/s`, `gb/s`) while preserving Kbps as the transport unit in API/model fields.

For outputs, progress is server-provided from ffmpeg and is not delta-computed in the browser.

The backend emits `outputs[*].bitrate` (raw ffmpeg string), `bitrateKbps` (numeric Kbps),
`totalSize` (numeric bytes), `progressFrame`, and `progressFps` by parsing ffmpeg progress once on
the server. The frontend uses `bitrateKbps` for per-output and aggregate bitrate, and uses
`totalSize`, `progressFrame`, and `progressFps` directly for output badges, keeping UI logic
agnostic of ffmpeg-specific string formats. When ffmpeg reports `N/A` (common with HLS uploads),
the backend normalizes those values to `null` before they reach the dashboard.

### 5.4 Output History Modal

Each output card exposes a history modal with two modes:

**Timeline mode** (default)
- On open, fetches only `filter=lifecycle` logs (lifecycle events only, oldest-first).
- Polls on the same interval as the main dashboard poll, but uses a guarded timeout loop so only one history request is in flight at a time.
- `retry_exhausted` is rendered as a terminal error badge (`Retry exhausted`) so operators can distinguish "will retry" from "gave up".
- Each lifecycle event row has a collapsible context section that loads surrounding `stderr`/`exit`/`control` logs on demand when expanded.
- Context fetch is bounded: at most 50 rows, at most 5 minutes before the event, floored to the previous lifecycle event's timestamp. Result is cached per event key for the modal session.

**Raw mode**
- Fetches up to 1000 rows ordered newest/oldest (user-selectable).
- Find-in-page search: all rows render regardless of query; matching rows are highlighted inline and assigned a sequential match index. The `n/m` counter and up/down navigation (Enter / Shift+Enter) move between matches only. Scrolls active match into view. Non-matching rows remain visible for context.

**Frontend constants** (in `public/js/history.js`):

| Constant | Value | Purpose |
|---|---|---|
| `OUTPUT_HISTORY_RAW_LIMIT` | 1000 | Max rows fetched in raw mode |
| `OUTPUT_HISTORY_CONTEXT_LIMIT` | 50 | Max rows per context on-demand fetch |
| `OUTPUT_HISTORY_CONTEXT_WINDOW_MS` | 5 min | Look-back window for context fetch |

The pipeline-history modal uses the same single-flight polling rule in live mode: if one poll is still running, another is not started on top of it.

### 5.5.1 Module Migration Troubleshooting

If a page renders partially after a refactor, check these first:

1. `ReferenceError` in browser console (usually an implicit-global access that should be an import or `window.*`).
2. HTML handlers (`onclick`, `data-*`) still pointing to a function that is no longer exported to `window`.
3. Shared state reads/writes still referencing old globals instead of `public/js/client.js`.
4. Stale browser JS from upstream cache/proxy; normal reload should revalidate, hard-refresh only if intermediaries ignore cache headers.

### 5.6 Frontend Internal APIs And Contracts

The frontend is flatter now, but it still relies on a few internal contracts that are worth making
explicit.

#### 5.6.1 Shared State Ownership Contract

`public/js/client.js` exports the shared singleton `state`. The contract is by ownership, not by
immutability.

```
client.js
  |
  +-- export const state = {
  |     config,
  |     health,
  |     pipelines,
  |     metrics,
  |   }
  |
  +-- export fetch helpers
  |     getConfig()
  |     getConfigVersion()
  |     getHealth()
  |     getSystemMetrics()
  |     apiRequest(...)
  |
  \-- export createAdaptivePollLoop(...)

dashboard.js
  |
  +-- writes state.config
  +-- writes state.health
  +-- writes state.metrics
  \-- writes state.pipelines

dashboard-view.js / editor.js / view.js / history.js
  \-- read shared state, but do not own it
```

Current ownership rule:

- `public/js/features/dashboard.js` is the only writer of `state.config`, `state.health`,
  `state.metrics`, and `state.pipelines`.
- Other frontend modules should treat `state` as read-mostly shared data and keep their own modal
  or UI-local state separately.
- `parsePipelinesInfo(config, health)` in `public/js/pipeline.js` is the only supported merge point
  from API snapshots into the dashboard pipeline model.

#### 5.6.2 Dashboard Refresh Coordination Contract

The dashboard controller is the only module allowed to decide when a full config/health/metrics
refresh happens.

```
initDashboard()
  |
  +-- requestDashboardRefresh()
  |     -> fetchConfig()
  |     -> fetchHealth()
  |     -> fetchSystemMetrics()
  |     -> compare snapshot versions
  |     -> state.pipelines = parsePipelinesInfo(state.config, state.health)
  |     -> renderPipelines()
  |     -> renderMetrics()
  |
  +-- dashboardPollLoop.start()
  |
  \-- document.visibilitychange
        -> onVisibilityChange()
           -> dashboardPollLoop.syncWithVisibility(...)
           -> syncDashboardVisibilityDependents()
```

Contract rules:

- `requestDashboardRefresh()` is single-flight; overlapping callers queue behind the active refresh.
- Config and health slices are considered mergeable only when their snapshot-version tokens agree.
- `renderPipelines()` and `renderMetrics()` run after state replacement, not during incremental
  fetch callbacks.

#### 5.6.3 Dashboard Action-Coordinator Contract

`public/js/features/dashboard-actions.js` exists so feature modules can request dashboard-level work
without importing the dashboard controller directly.

```
dashboard.js
  |
  \-- setDashboardActionHandlers({
        refreshDashboard: requestDashboardRefresh,
        syncUserConfigBaseline,
      })

editor.js / history.js / other feature modules
  |
  +-- refreshDashboard()
  +-- syncUserConfigBaseline()
  +-- registerDashboardVisibilitySync(handler)
  \-- rely on dashboard.js to call syncDashboardVisibilityDependents()
```

Contract rules:

- `setDashboardActionHandlers()` must run during dashboard startup before other modules rely on the
  forwarded callbacks.
- Registered visibility handlers are called sequentially by
  `syncDashboardVisibilityDependents()`.
- `dashboard-actions.js` is a coordinator seam, not a second state store.

#### 5.6.4 View Action Adapter Contract

`public/js/features/view.js` owns selected-pipeline state lookup and action-adapter wiring.
`public/js/features/output-list-view.js` owns the DOM builder for per-output rows. Together they
render the selected pipeline without coupling output DOM code directly to editor/history modules.

```
view.js
  |
  +-- reads selected pipe from state.pipelines
  +-- imports stable wrappers from pipeline-view-actions.js
  |     |
  |     +-- default action providers
  |     |     -> editor.js
  |     |     -> history.js
  |     |
  |     \-- test overrides
  |           -> setPipelineViewActionOverrides(...)
  |           -> resetPipelineViewActionOverrides()
  |
  \-- renderOutputsList(outputsList, pipe, actionAdapters)
        from output-list-view.js
```

Contract rules:

- `view.js` should call only the action adapter functions, not import editor/history internals
  directly.
- `output-list-view.js` should stay a DOM-builder seam: it receives the selected pipeline and the
  narrow action callbacks from `view.js`, and it should not read shared state or import
  editor/history modules itself.
- Tests may override adapter functions, but production wiring should continue using the default
  providers; `view.js` remains the only module that bridges those overrides into output rendering.
- This adapter is intentionally narrow: output start/stop/edit/delete, history openers, publisher
  quality opener, and the output-toggle busy check.

#### 5.6.5 History Modal Contract

`public/js/history.js` owns modal-local history state and polling. It does not own the dashboard's
global pipeline snapshot.

```
openOutputHistoryModal(pipeId, outId, outName)
  |
  +-- reset outputHistoryState
  +-- pollHistoryOnce(true)
  +-- start or stop modal-local poll loop
  \-- render output-history view

openPipelineHistoryModal(pipeId, pipeName)
  |
  +-- reset pipelineHistoryState
  +-- pollPipelineHistoryOnce(true)
  \-- render pipeline-history view
```

Contract rules:

- `outputHistoryState` and `pipelineHistoryState` are local to the history feature and are reset on
  every modal open.
- `registerDashboardVisibilitySync(syncHistoryPollingWithVisibility)` is the only intended bridge
  from history polling to dashboard visibility changes.
- Timeline context loading is bounded and cached per modal session; it is not a general-purpose log
  query layer for the rest of the UI.

---

## 6. Reader Correlation: How Output Health Works

Each FFmpeg output is identified in MediaMTX by a `reader_id` query parameter appended to its RTSP pull URL:

```
rtsp://localhost:8554/<streamKey>?reader_id=reader_<pipelineId>_<outputId>
```

MediaMTX surfaces this query string in `/v3/rtspconns/list` as `conn.query`. The health endpoint parses `reader_id` from each connection's query and builds a `rtspByReaderTag` map. An output's status becomes `on` when its expected tag is found in this map.


See [health-mapping.md](./health-mapping.md) for full status derivation diagrams.

---

## 7. Deployment Topologies

### 7.1 App + MediaMTX (separately started)

```
[Host]
  bin/mediamtx/mediamtx mediamtx.yml       :1935 :8554 :8888 :9997
  node src/index.js                          :3030
    │
    └── (connects to)
        localhost:9997 (MediaMTX API)
        localhost:8554 (MediaMTX RTSP)
```

The primary repo-managed edit/test flow remains host mode: start the app with `npm start` or
`npm run dev`, and start MediaMTX separately with your own supervisor or a direct command such as
`bin/mediamtx/mediamtx mediamtx.yml`.

### 7.2 Optional Docker Stack (`make run-docker`)

`make run-docker` is still available when you want the older disposable container stack.

```
[Docker]
  pause      :3030 :1935 :8554 :8890/udp
  app        -> pause network namespace
  mediamtx   -> pause network namespace
  nginx-rtmp :1936 :8081
```

This stack starts the app, MediaMTX, and the `nginx-rtmp` validation sink together. It remains
optional; host mode is still the main path described elsewhere in this guide.

---

## 8. Operations Reference

| Command               | Purpose                                              |
|-----------------------|------------------------------------------------------|
| `make deps`           | Run Linux preflight, install Node deps, and download MediaMTX; use `DEV=1 make deps` for dev/test dependencies |
| `npm start`           | Start the Node control plane from `src/index.js` |
| `npm run dev`         | Start the Node control plane with nodemon hot reload |
| `make run-docker`     | Start the optional app + MediaMTX + `nginx-rtmp` container stack |
| `docker compose down` | Stop the optional Docker stack |
| `make run-4x3`        | Start `nginx-rtmp` and run the artifact suite against an already-running app plus MediaMTX stack |
| `make css`            | Rebuild `public/output.css` from `input.css`         |
| `make format`         | Run prettier over all files                          |
| `make security`       | npm audit + outdated packages                        |
| `make security-strict`| npm audit --audit-level=low (fails on any vuln)      |
| `make start-input`    | Push a test colorbar loop via RTMP (uses ffmpeg)      |
| `make probe-output`   | ffprobe a test output URL                            |

---

## 9. Known Limitations

- Output reader identification relies on MediaMTX exposing the RTSP connection query string. If a future MediaMTX version strips query params from connection records, reader correlation falls back to `warning` state for all running outputs.
- `ffprobe` is run against the RTSP input before each output start. On intermittent networks or MediaMTX restarts this may produce false 409 "input not available" errors.
- The probe cache (`streamProbeCache`) is in-memory; it is lost on server restart, adding ~1-2 s latency to the first `/health` call after restart.
- Audio metrics may be absent when the RTMP source publishes without audio metadata in the initial announce.
- `data/data.db` is local SQLite. No built-in replication or backup mechanism.
