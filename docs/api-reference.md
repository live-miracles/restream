# Restream API Reference

Base URL: `http://localhost:3030`

All request/response bodies are JSON. All timestamps are ISO 8601 UTC strings.

---

## 1. Stream Keys

### `GET /stream-keys`

Returns all stream keys ordered by creation date descending.

**Response 200:**
```json
[
  {
    "key": "c1518f5ef0d917ef1b6547d7",
    "label": "English Feed",
    "createdAt": "2026-04-10T10:59:00.000Z"
  }
]
```

---

### `POST /stream-keys`

Creates a stream key and registers the corresponding path in MediaMTX.

**Request body:**
```json
{
  "streamKey": "mystream",   // optional — omit for auto-generated 24-char hex
  "label": "English Feed"    // optional
}
```

**Response 201:**
```json
{
  "message": "Stream key created",
  "streamKey": {
    "key": "mystream",
    "label": "English Feed",
    "createdAt": "2026-04-10T10:59:00.000Z"
  }
}
```

**Errors:** `409` key already exists; `500` MediaMTX path registration failed.

---

### `POST /stream-keys/:key`

Updates the label of an existing stream key.

**Request body:**
```json
{ "label": "Spanish Feed" }
```

**Response 200:**
```json
{ "message": "Stream key updated", "streamKey": { ... } }
```

**Errors:** `404` key not found.

---

### `DELETE /stream-keys/:key`

Deletes a stream key and removes its path from MediaMTX.

> Note: If MediaMTX path deletion fails the request returns `500`. The DB row is **not** deleted until MediaMTX confirms success.

**Response 200:**
```json
{ "message": "Stream key deleted" }
```

**Errors:** `404` key not found; `500` MediaMTX path deletion failed.

---

## 2. Pipelines

### `GET /pipelines`

Returns all pipelines.

**Response 200:**
```json
[
  {
    "id": "a1b2c3d4e5f6a7b8",
    "name": "Pipeline 1",
    "streamKey": "c1518f5ef0d917ef1b6547d7",
    "encoding": null,
    "inputEverSeenLive": 1,
    "createdAt": "2026-04-10T11:00:00.000Z",
    "updatedAt": null
  }
]
```

---

### `POST /pipelines`

Creates a pipeline.

**Request body:**
```json
{
  "name": "Pipeline 1",
  "streamKey": "c1518f5ef0d917ef1b6547d7",  // optional
  "encoding": null                            // reserved, not used at runtime
}
```

**Response 201:**
```json
{ "message": "Pipeline created", "pipeline": { ... } }
```

**Errors:** `400` missing name.

---

### `POST /pipelines/:id`

Updates pipeline fields.

**Request body:** same shape as create (all fields optional).

**Response 200:**
```json
{ "message": "Pipeline updated", "pipeline": { ... } }
```

**Errors:** `404` pipeline not found; `409` stream key change blocked while outputs are running.

---

### `GET /pipelines/:pipelineId/history?limit=200`

Returns append-only pipeline history events for UI consumption. This stream includes:

- pipeline config mutations (`[config] ...`)
- input state transitions (`[input_state] off -> on`, `on -> warning`, `warning -> error`, etc.)

Pipeline-level history entries are stored in `job_logs` with:

- `pipeline_id = :pipelineId`
- `output_id IS NULL`
- optional `event_type` values such as `pipeline_config` and `pipeline_state`

**Response 200:**
```json
{
  "pipelineId": "a1b2c3d4e5f6a7b8",
  "logs": [
    {
      "ts": "2026-04-16T05:30:00.000Z",
      "message": "[input_state] off -> on",
      "eventType": "pipeline_state"
    },
    {
      "ts": "2026-04-16T05:25:00.000Z",
      "message": "[config] stream_key changed from ab...cd to ef...12",
      "eventType": "pipeline_config"
    }
  ]
}
```

**Errors:** `404` pipeline not found.

---

### `DELETE /pipelines/:id`

Deletes a pipeline. All running output jobs are stopped (SIGTERM) before deletion. Outputs and jobs cascade-delete via SQLite FK.

**Response 200:**
```json
{ "message": "Pipeline <id> deleted" }
```

**Errors:** `404` pipeline not found.

---

## 3. Outputs

### `POST /pipelines/:pipelineId/outputs`

Creates an output for a pipeline.

**Request body:**
```json
{
  "name": "YouTube",
  "url": "rtmp://a.rtmp.youtube.com/live2/xxxx-xxxx-xxxx",
  "encoding": "source"   // one of: source | vertical-crop | vertical-rotate | 720p | 1080p
}
```

**Response 201:**
```json
{
  "message": "Output created",
  "output": {
    "id": "f8e7d6c5b4a3f2e1",
    "pipelineId": "a1b2c3d4e5f6a7b8",
    "name": "YouTube",
    "url": "rtmp://...",
    "encoding": "source",
    "createdAt": "2026-04-10T11:05:00.000Z"
  }
}
```

**Errors:** `400` missing/invalid name/url/encoding; `404` pipeline not found.

---

### `POST /pipelines/:pipelineId/outputs/:outputId`

Updates an output.

**Request body:** same shape as create.

**Response 200:**
```json
{ "message": "Output updated", "output": { ... } }
```

**Errors:** `400` invalid encoding; `404` output or pipeline not found; `409` cannot change URL/encoding while running.

---

### `DELETE /pipelines/:pipelineId/outputs/:outputId`

Deletes an output. Running job is stopped first.

**Response 200:**
```json
{ "message": "Output <outputId> from pipeline <pipelineId> deleted" }
```

**Errors:** `404` output or pipeline not found.

---

## 4. Output Runtime Control

### `POST /pipelines/:pipelineId/outputs/:outputId/start`

Starts an FFmpeg job for this output. The full call flow is:

1. Acquire in-memory start lock for `(pipelineId, outputId)` — 409 if another start is already in progress.
2. Validate pipeline + output exist.
3. Check for an existing running job — 409 if found.
4. Require `pipeline.streamKey`; resolve probe URL `rtsp://localhost:8554/<streamKey>`.
5. Run `ffprobe -rtsp_transport tcp <probeUrl>` with 8 s timeout.
6. Build tagged pull URL: `rtsp://localhost:8554/<streamKey>?reader_id=reader_<pipelineId>_<outputId>`.
7. Spawn FFmpeg for the selected output encoding:
  - `source`: codec copy (`-c:v copy -c:a copy`)
  - `vertical-crop`: `-vf scale=720:1280:force_original_aspect_ratio=increase,crop=720:1280` + H.264/AAC encode
  - `vertical-rotate`: `-vf transpose=1,scale=720:1280:force_original_aspect_ratio=increase,crop=720:1280` + H.264/AAC encode
  - `720p`: `-vf scale=-2:720` + H.264/AAC encode
  - `1080p`: `-vf scale=-2:1080` + H.264/AAC encode
8. Persist job row in DB, return after 250 ms stability check.

**Request body:** none

**Response 201:**
```json
{
  "message": "Job started",
  "job": {
    "id": "3f2e1d0c9b8a7f6e",
    "pipelineId": "a1b2c3d4e5f6a7b8",
    "outputId": "f8e7d6c5b4a3f2e1",
    "pid": 12345,
    "status": "running",
    "startedAt": "2026-04-10T11:10:00.000Z",
    "endedAt": null,
    "exitCode": null,
    "exitSignal": null
  }
}
```

**Errors:**
- `400` no input URL available
- `404` pipeline or output not found
- `409` start already in progress for this output
- `409` output already has a running job
- `409` RTSP input not available yet (ffprobe failed)
- `500` FFmpeg failed to start (includes last 100 log lines)

---

### `POST /pipelines/:pipelineId/outputs/:outputId/stop`

Stops the running FFmpeg job via SIGTERM with a 5 s SIGKILL escalation.

**Response 200:**
```json
{
  "message": "Stopping job",
  "jobId": "3f2e1d0c9b8a7f6e",
  "result": {
    "stopped": true,
    "reason": "signal-sent"   // "signal-sent" | "marked-stopped" | "signal-failed"
  }
}
```

**Errors:** `404` no running job for this output.

---

### `GET /pipelines/:pipelineId/outputs/:outputId/history?limit=200&filter=lifecycle`

Returns recent job logs for a specific output. This endpoint is intended for diagnostics and output history UI.

**Query params:**
- `limit` optional; default `200`; min `1`; max `1000`.
- `filter` optional; set to `lifecycle` to return only lifecycle events.

Ordering behavior:
- Default path (`filter` omitted): newest first.
- Lifecycle path (`filter=lifecycle`): oldest first, full lifecycle sequence for timeline rendering.

**Response 200:**
```json
{
  "pipelineId": "a1b2c3d4e5f6a7b8",
  "outputId": "f8e7d6c5b4a3f2e1",
  "logs": [
    {
      "ts": "2026-04-15T12:20:57.098Z",
      "message": "[lifecycle] exited status=failed requestedStop=false exitCode=255 exitSignal=null"
    },
    {
      "ts": "2026-04-15T12:20:57.098Z",
      "message": "[exit] code=255 signal=null"
    },
    {
      "ts": "2026-04-15T12:20:56.971Z",
      "message": "[stderr] [flv @ ...] Non-monotonic DTS ..."
    }
  ]
}
```

**Errors:** `404` output or pipeline not found.

> Lifecycle enrichment: start/stop/status transitions now emit `[lifecycle] ...` messages in `job_logs`.
> Current emitted formats:
> - `[lifecycle] started status=running pid=<pid|null>`
> - `[lifecycle] stop_requested signal=<signal> status=running`
> - `[lifecycle] failed_on_error status=failed exitCode=null exitSignal=null`
> - `[lifecycle] exited status=<stopped|failed> requestedStop=<true|false> exitCode=<code|null> exitSignal=<signal|null>`
> - `[lifecycle] marked_stopped_no_process status=stopped`

---

## 5. Config Snapshot

### `GET /config`

Returns the full state snapshot used by the dashboard. Supports conditional GET via `If-None-Match` / ETag.

**Request headers (optional):**
```
If-None-Match: "abc123..."
```

**Response 200:**
```json
{
  "serverName": "My Server",
  "pipelinesLimit": 25,
  "outLimit": 95,
  "streamKeys": [ ... ],
  "pipelines": [ ... ],
  "outputs": [ ... ],
  "jobs": [ ... ]
}
```

**Response headers:**
```
ETag: "abc123def456..."
```

**Response 304:** ETag matches — no body, no change.

> The ETag is a SHA-256 hash of a deterministic state snapshot, persisted in the `meta` table.

---

### `HEAD /config`

Returns the current ETag without a response body. Used to poll for changes without downloading the full config.

**Response 200** (no body) + `ETag` header.

---

## 6. Health and Metrics

### `GET /health`

Returns the latest server-side health snapshot. A periodic collector refreshes this snapshot in the background by calling three MediaMTX endpoints in parallel: `/v3/paths/list`, `/v3/rtspconns/list`, `/v3/rtspsessions/list`, then merging that runtime state with DB job state and input lifecycle bookkeeping. The collector interval defaults to 2000 ms and can be overridden with `HEALTH_SNAPSHOT_INTERVAL_MS`.

`GET /health` itself does not call MediaMTX. It returns the most recent cached snapshot immediately.

Headers:

- `ETag` for the current snapshot content
- Supports `If-None-Match` and returns `304 Not Modified` when unchanged

**Response 200:**
```json
{
  "generatedAt": "2026-04-10T11:31:36.879Z",
  "ageMs": 412,
  "status": "ready",
  "mediamtx": {
    "pathCount": 2,
    "rtspConnCount": 3,
    "ready": true
  },
  "pipelines": {
    "<pipelineId>": {
      "input": {
        "status": "on",
        "publishStartedAt": "2026-04-10T09:00:00.000Z",
        "streamKey": "c1518f5ef0d917ef1b6547d7",
        "readers": 3,
        "bytesReceived": 358000000,
        "bytesSent": 320000000,
        "video": {
          "codec": "H264",
          "width": 1920,
          "height": 1080,
          "profile": "High",
          "level": "4",
          "fps": 30,
          "bw": null
        },
        "audio": {
          "codec": "aac",
          "channels": 2,
          "sample_rate": 48000,
          "profile": "LC",
          "bw": null
        }
      },
      "outputs": {
        "<outputId>": {
          "status": "on",
          "jobId": "3f2e1d0c9b8a7f6e",
          "totalSize": "120000000",
          "bitrate": "1842.5kbits/s",
          "bitrateKbps": 1842.5,
          "mediaSource": "ffmpeg",
          "media": {
            "video": {
              "codec": "h264",
              "width": 1280,
              "height": 720,
              "fps": 30,
              "profile": null,
              "level": null
            },
            "audio": {
              "codec": "aac",
              "sample_rate": 48000,
              "channels": 2
            }
          }
        }
      }
    }
  }
}
```

During startup, before MediaMTX is ready, `/health` returns an initialization snapshot:

```json
{
  "generatedAt": "2026-04-10T11:31:36.879Z",
  "ageMs": 87,
  "status": "initializing",
  "mediamtx": {
    "pathCount": 0,
    "rtspConnCount": 0,
    "ready": false
  },
  "pipelines": {}
}
```

If a collector cycle fails after startup, `/health` returns a degraded snapshot and may retain the last known pipeline data:

```json
{
  "generatedAt": "2026-04-10T11:31:36.879Z",
  "ageMs": 1011,
  "status": "degraded",
  "mediamtx": {
    "pathCount": 2,
    "rtspConnCount": 3,
    "ready": true
  },
  "pipelines": {}
}
```

**Input status values:**
| Status | Meaning                                                          |
|--------|------------------------------------------------------------------|
| `on`   | `pathInfo.available === true` (fallback: deprecated `ready`) |
| `warning` | `pathInfo.online === true` but path is not yet `available` |
| `error` | Stream key is configured, path is neither online nor available, and the pipeline has previously been seen live |
| `off`  | No path info, or path is neither online nor available |

**Output status values:**
| Status    | Meaning                                              |
|-----------|------------------------------------------------------|
| `on`      | Job running + RTSP reader tag matched in MediaMTX    |
| `warning` | Job running but no matching RTSP reader tag          |
| `error`   | Latest job status is `failed`                        |
| `off`     | No running job                                       |

For each output, ffmpeg runtime progress contributes:

- `totalSize` from ffmpeg `total_size` (raw cumulative bytes written)
- `bitrate` from ffmpeg `bitrate` (raw ffmpeg rate string, for example `1842.5kbits/s`, kept for debugging)
- `bitrateKbps` server-normalized numeric bitrate in Kbps for UI consumption

Output media metadata is server-resolved and included as:

- `mediaSource`: `ffmpeg` | `fallback-source` | `fallback-profile` | `unknown`
- `media.video` / `media.audio`: codec/geometry/audio fields used by the dashboard

When FFmpeg has not emitted full `Output #0` stream info yet, backend fallback rules apply:

- `fallback-source`: copies media metadata from pipeline input for `source` outputs
- `fallback-profile`: derives expected media from selected transcode profile

---

### `GET /healthz`

Readiness endpoint used by launch scripts and infra probes.

- `200` with `{ "status": "ok" }` when MediaMTX is reachable.
- `503` with `{ "status": "not_ready" }` when MediaMTX is not ready.

---

### `GET /metrics/system`

Returns host system metrics. Throughput and CPU values are computed against the previous sample.

**Response 200:**
```json
{
  "generatedAt": "2026-04-10T11:35:00.000Z",
  "cpu": {
    "usagePercent": 12.34,
    "cores": 4,
    "load1": 0.85
  },
  "memory": {
    "totalBytes": 8589934592,
    "usedBytes": 3000000000,
    "freeBytes": 5589934592,
    "usedPercent": 34.92
  },
  "disk": {
    "totalBytes": 107374182400,
    "usedBytes": 50000000000,
    "freeBytes": 57374182400,
    "usedPercent": 46.57
  },
  "network": {
    "downloadBytesPerSec": 35000.00,
    "uploadBytesPerSec": 90000.00,
    "downloadKbps": 280.00,
    "uploadKbps": 720.00
  }
}
```

---

## 7. Error Model

All errors return:
```json
{ "error": "Human-readable description" }
```

| Status | Meaning                                           |
|--------|---------------------------------------------------|
| `400`  | Validation or missing required field              |
| `404`  | Resource not found                                |
| `409`  | Conflict — already running, duplicate key, input not ready |
| `500`  | Internal server error or MediaMTX communication failure |

---

## 8. Response ETag Lifecycle

```
POST /pipelines            → recomputeEtag()
POST /pipelines/:id        → recomputeEtag()
DELETE /pipelines/:id      → recomputeEtag()
POST /pipelines/.../outputs         → recomputeEtag()
POST /pipelines/.../outputs/:id     → recomputeEtag()
DELETE /pipelines/.../outputs/:id   → recomputeEtag()
POST .../start             → recomputeEtag() (on create + on every exit transition)
POST .../stop              → recomputeEtag()
```

Clients should save the ETag from `GET /config` and pass it as `If-None-Match` on subsequent calls to avoid unnecessary payload transfers.
