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
| nginx-rtmp         | Local test RTMP sink (docker-compose only)| `1936` RTMP, `8081` HTTP|

---

## 1.1 Browser Shell Routing

The static UI now has paired desktop and mobile shells for the main operator flows:

- dashboard: `/` or `/index.html` for desktop, `/mobile/dashboard.html` for mobile
- stream keys: `/stream-keys.html` for desktop, `/mobile/keys.html` for mobile

Request routing is handled in two layers:

1. The Node server checks `?view=mobile` or `?view=desktop` first.
2. If no explicit override is present, the server uses `Sec-CH-UA-Mobile` when available and falls back to user-agent matching before redirecting to the preferred shell.
3. The browser-side `device-route.js` module preserves the explicit override across same-origin navigation and performs a client-side shell redirect when the loaded document does not match the current device heuristics.

Legacy aliases `/mobile-dashboard.html` and `/mobile-keys.html` still redirect to the grouped mobile files so older links do not break.

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
         -flvflags no_duration_filesize
         -rtmp_live live
         -f flv <outputUrl>
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
  └── GET /v3/webrtcsessions/list → WebRTC publishers by path

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

200 { generatedAt, status: 'ready', mediamtx: { pathCount, rtspConnCount, rtmpConnCount, srtConnCount, webrtcSessionCount, ready }, pipelines: {...} }
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
| `src/utils/app.js` | Shared app helpers (`log`, `validateName`, `createHttpError`, token masking) |
| `src/utils/ffmpeg.js` | FFmpeg argument/profile construction and progress/media parsing helpers |
| `src/utils/mediamtx.js` | MediaMTX URL/tag helpers for path and reader correlation |
| `src/utils/retry.js` | Retry/backoff decision helpers used by output recovery flows |

This was a structure/maintainability change only; API behavior and external contracts remained the same.

---

## 5. Frontend Architecture

### 5.1 Files

| File                  | Role                                                          |
|-----------------------|---------------------------------------------------------------|
| `public/index.html`   | Dashboard SPA shell                                           |
| `public/stream-keys.html` | Stream key management page                               |
| `public/js/core/state.js` | Shared mutable UI state (`config`, `health`, `pipelines`, `metrics`) |
| `public/js/core/api.js`       | All API calls as relative paths (never direct to MediaMTX)   |
| `public/js/features/dashboard.js` | Polling orchestration and config/version drift detection      |
| `public/js/core/pipeline.js`  | `parsePipelinesInfo()` — merges config + health into view model |
| `public/js/features/render.js`    | DOM rendering — pipeline cards, stats tables, output rows     |
| `public/js/features/editor.js`    | Pipeline/output edit flows and start/stop controls             |
| `public/js/features/pipeline-view.js` | Selected-pipeline detail + outputs column renderers       |
| `public/js/history/controller.js` | Output/pipeline history modal control and polling            |
| `public/js/core/utils.js`     | `setServerConfig()`, `formatTime()`, `copyData()`, etc.       |
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
setInterval(requestDashboardRefresh, <pollInterval>)   repeats above on interval
setInterval(checkStreamingConfigs, 30000)       external-change detection (see below)
```

`GET /metrics/system` no longer advances the rate-calculation baseline on every request. A server-side timer samples CPU and network counters on a fixed cadence, and dashboard polls just read the latest completed sample.

Dashboard refresh triggers are also coalesced client-side. Poll ticks, visibility refreshes, and mutation-driven refreshes all funnel through a single in-flight gate, so a slow refresh cannot overlap with another full `fetchAndRerender()` pass and later overwrite fresher state.

`public/index.html` and `public/stream-keys.html` load frontend entry modules as ES modules (`<script type="module">`). The dashboard page now boots through `public/js/features/dashboard-entry.js`, which imports the dashboard/history/editor feature graph and registers the few cross-feature callbacks that would otherwise create circular dependencies. HTML-bound handlers used by inline attributes remain the only frontend functions intentionally exposed on `window`.

### 5.5 Frontend Module Conventions

See [frontend-modules.md](./frontend-modules.md) for implementation-level rules and examples. In short:

- Import dependencies explicitly; do not rely on implicit globals.
- Keep shared mutable dashboard state in `public/js/core/state.js`.
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

`public/js/core/pipeline.js` maintains `throughputState.inputBytes` for input throughput only. On each poll cycle, `computeKbps(stateMap, key, totalBytes, nowMs)` computes input bitrate from the delta of `input.bytesReceived` between cycles:

```
kbps = (deltaBytes × 8) / (deltaMs / 1000) / 1000
```

These values are stored as numeric Kbps in the dashboard model. At render time, the UI formats bitrate display with adaptive units (`kb/s`, `mb/s`, `gb/s`) while preserving Kbps as the transport unit in API/model fields.

For outputs, bitrate is server-provided from ffmpeg progress (`bitrate`) in raw ffmpeg format (for example `1842.5kbits/s`) and is not delta-computed in the browser.

The backend also emits `outputs[*].bitrateKbps` (numeric Kbps) by parsing ffmpeg progress once on the server. The frontend uses `bitrateKbps` for per-output and aggregate output bitrate, keeping UI logic agnostic of ffmpeg-specific string formats.

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

**Frontend constants** (in `public/js/history/state.js`):

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
3. Shared state reads/writes still referencing old globals instead of `public/js/core/state.js`.
4. Stale browser JS from upstream cache/proxy; normal reload should revalidate, hard-refresh only if intermediaries ignore cache headers.

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

### 7.1 Host Node + Docker MediaMTX (`make run-host`)

```
[Host]
  node src/index.js  :3030
    │
    └── (connects to)
        localhost:9997 (MediaMTX API)
        localhost:8554 (MediaMTX RTSP)

[Docker]
  mediamtx           :1935 :8554 :9997
  nginx-rtmp         :1936 (RTMP test sink)
```

### 7.2 Full Docker (`make run-docker`)

All services are defined in `docker-compose.yml` under the `container` profile.

`pause`, `app`, and `mediamtx-pod` share a single network namespace (`network_mode: service:pause`).
This allows the app to use hardcoded `localhost:9997` (MediaMTX API) and `localhost:8554` (RTSP).

`nginx-rtmp` runs as a separate container with independent host port mappings.

---

## 8. Operations Reference

| Command               | Purpose                                              |
|-----------------------|------------------------------------------------------|
| `make run-host`       | Start docker services + node on host (dev)           |
| `make run-docker`     | Full docker-compose stack                            |
| `make down`           | Stop docker services and clean local database files  |
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
