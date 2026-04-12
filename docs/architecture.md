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

Browser ──► Node API :3030 ──► SQLite (data.db)
                    │
                    └──► MediaMTX API :9997 (health, path config, connection stats)
```

### Runtime Components

| Component          | Role                                      | Default port(s)         |
|--------------------|-------------------------------------------|-------------------------|
| Node API server    | REST control plane + static UI            | `3030`                  |
| SQLite (`data.db`) | Persistent state (keys, pipelines, jobs)  | —                       |
| MediaMTX           | RTMP ingest, RTSP relay, path management  | `1935`, `8554`, `9997`  |
| FFmpeg             | Per-output RTSP→RTMP push process         | spawned on demand       |
| nginx-rtmp         | Local test RTMP sink (docker-compose only)| `1936` RTMP, `8081` HTTP|

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
  encoding    TEXT           -- 'source' | 'copy' (pass-through); future: transcode opts
  created_at  TEXT

jobs
  id          TEXT PK
  pipeline_id TEXT FK → pipelines.id ON DELETE CASCADE
  output_id   TEXT FK → outputs.id ON DELETE CASCADE
  pid         INTEGER        -- OS process ID of FFmpeg (null if proc died before record)
  status      TEXT           -- 'running' | 'stopped' | 'failed'
  started_at  TEXT
  ended_at    TEXT
  exit_code   INTEGER
  exit_signal TEXT

job_logs
  id          INTEGER PK AUTOINCREMENT
  job_id      TEXT FK → jobs.id ON DELETE CASCADE
  ts          TEXT
  message     TEXT           -- one line per stdout/stderr chunk or control event

meta
  key         TEXT PK
  value       TEXT           -- used for ETag persistence across restarts
```

### 2.2 In-Memory State

`processes` (`Map<jobId, ChildProcess>`) — runtime-only reference to live FFmpeg processes. Lost on server restart; DB `status` is authoritative.

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
  rtsp://<host>/<streamKey>?reader_id=reader_<pipelineId>_<outputId>
  │
  ▼
Build FFmpeg args:
  ffmpeg -nostdin
         -rtsp_transport tcp
         -i <taggedRtspUrl>
         -c:v copy -c:a copy
         -flvflags no_duration_filesize
         -rtmp_live live
         -f flv <outputUrl>
  │
  ▼
spawn(ffmpegCmd, args)
  │
  ▼
db.createJob({ status: 'running', pid, startedAt })
recomputeEtag()
processes.set(jobId, child)
  │
  ▼
Wait 250 ms → check if job still 'running'
  │ failed immediately → 500 + last 100 log lines
  │ still running      → 201 { job }
  ▼
[Background] child stdout/stderr → db.appendJobLog()
[Background] child 'exit' → db.updateJob({ status, exitCode, exitSignal })
                           → recomputeEtag()
                           → processes.delete(jobId)
```

### 4.2 Output Stop (`POST /pipelines/:pipelineId/outputs/:outputId/stop`)

```
Client
  │
  ▼
db.getRunningJobFor(pipelineId, outputId)
  │ none → 404
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
  └── GET /v3/rtspsessions/list  → rtspSessionById Map

Build rtspByReaderTag Map:
  for each RTSP connection:
    parse ?reader_id=<tag> from conn.query
    rtspByReaderTag.set(tag, [conn, ...])

Load from DB:
  ├── listPipelines()
  ├── listOutputs()
  └── listJobs() → latestJobByOutputId Map (most recent per output)

For each pipeline:
  ├── Input health:
  │     pathInfo = pathByName.get(streamKey)
  │     status = 'on'  if pathInfo.online || pathInfo.ready
  │            = 'off' otherwise
  │     publishStartedAt = pathInfo.readyTime   (protocol-agnostic)
  │     video/audio = from pathInfo.tracks2 + ffprobe cache (if online)
  │
  └── For each output:
        latestJob = latestJobByOutputId.get(outputId)
        status = 'error'   if latestJob.status === 'failed'
               = 'off'     if no running job
               = 'on'      if running AND rtspByReaderTag has reader_<pid>_<oid>
               = 'warning' if running AND no reader tag match

200 { generatedAt, mediamtx: { pathCount, rtspConnCount }, pipelines: {...} }
```

### 4.4 Config Snapshot (`GET /config`)

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
    pipelines,              ← db.listPipelines()
    outputs,                ← db.listOutputs()
    jobs }                  ← db.listJobs()

200 { snapshot }  +  ETag: "<hash>"
```

ETag is recomputed (SHA-256 of deterministic JSON snapshot) on every job/pipeline/output state change and persisted in the `meta` table so it survives restarts.

### 4.5 Stream Key Creation (`POST /stream-keys`)

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

### 4.6 Stream Key Deletion (`DELETE /stream-keys/:key`)

```
Client
  │
  ▼
Check key exists in DB (404 if not)
  │
  ▼
DELETE MediaMTX /v3/config/paths/delete/<key>
  │ (MediaMTX error logged but not fatal — key still deleted from DB)
  ▼
db.deleteStreamKey(key)
200 { message }
```

### 4.7 Pipeline/Output Deletion with Cascade Stop

```
DELETE /pipelines/:id
  ├── Find all outputs for pipeline
  ├── For each output with a running job → stopRunningJob() (SIGTERM → SIGKILL)
  └── db.deletePipeline(id)  [CASCADE deletes outputs + jobs]

DELETE /pipelines/:pipelineId/outputs/:outputId
  ├── Find running job for output → stopRunningJob()
  └── db.deleteOutput(pipelineId, outputId)
```

---

## 5. Frontend Architecture

### 5.1 Files

| File                  | Role                                                          |
|-----------------------|---------------------------------------------------------------|
| `public/index.html`   | Dashboard SPA shell                                           |
| `public/stream-keys.html` | Stream key management page                               |
| `public/api.js`       | All API calls as relative paths (never direct to MediaMTX)   |
| `public/dashboard.js` | Event handlers, modal logic, polling orchestration           |
| `public/pipeline.js`  | `parsePipelinesInfo()` — merges config + health into view model |
| `public/render.js`    | DOM rendering — pipeline cards, stats tables, output rows     |
| `public/utils.js`     | `setServerConfig()`, `formatTime()`, `copyData()`, etc.       |
| `public/output.css`   | Compiled Tailwind + DaisyUI output (do not edit manually)     |
| `input.css`           | Tailwind source (project root, compiled with `make css`)      |

### 5.2 Dashboard Polling Call Flow

```
Page load
  │
  ▼
fetchConfig()          GET /config (with If-None-Match ETag)
fetchHealth()          GET /health
fetchSystemMetrics()   GET /metrics/system
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
setInterval(fetchAndRerender, <pollInterval>)   repeats above on interval
```

### 5.3 Throughput Computation (client-side)

`pipeline.js` maintains `throughputState.inputBytes` and `throughputState.outputBytes` maps. On each poll cycle, `computeKbps(stateMap, key, totalBytes, nowMs)` computes instantaneous bitrate from the delta of `bytesReceived`/`bytesSent` between cycles:

```
kbps = (deltaBytes × 8) / (deltaMs / 1000) / 1000
```

Values are `.toFixed(1)` Kbps strings displayed in the stats panel.

---

## 6. Reader Correlation: How Output Health Works

Each FFmpeg output is identified in MediaMTX by a `reader_id` query parameter appended to its RTSP pull URL:

```
rtsp://<host>/<streamKey>?reader_id=reader_<pipelineId>_<outputId>
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
| `make down`           | Stop and remove docker containers                    |
| `make verify`         | Clean startup check (container profile)              |
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
- `data.db` is local SQLite. No built-in replication or backup mechanism.
