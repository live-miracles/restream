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

> Note: If MediaMTX path registration succeeds but the SQLite insert fails, the API now attempts a
> compensating MediaMTX path delete before returning `500`.

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

**Errors:** `409` key already exists; `500` MediaMTX registration failed, SQLite insert failed, or rollback failed.

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

> If MediaMTX deletion succeeds but the SQLite delete fails, the API attempts a compensating path re-add before returning `500`.

**Response 200:**
```json
{ "message": "Stream key deleted" }
```

**Errors:** `404` key not found; `500` MediaMTX deletion failed, SQLite delete failed, or rollback failed.

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
- typed `eventType` values such as `pipeline.config.stream_key_changed` and `pipeline.input_state.transitioned`

**Response 200:**
```json
{
  "pipelineId": "a1b2c3d4e5f6a7b8",
  "logs": [
    {
      "ts": "2026-04-16T05:30:00.000Z",
      "eventType": "pipeline.input_state.transitioned",
      "eventData": { "from": "off", "to": "on" },
      "message": "[input_state] off -> on"
    },
    {
      "ts": "2026-04-16T05:25:00.000Z",
      "eventType": "pipeline.config.stream_key_changed",
      "eventData": { "fromMasked": "ab...cd", "toMasked": "ef...12" },
      "message": "[config] stream_key changed from ab...cd to ef...12"
    }
  ]
}
```

**Errors:** `404` pipeline not found.

---

### `DELETE /pipelines/:id`

Deletes a pipeline. All running output jobs are stopped and the API waits for those processes to exit before deletion. Outputs and jobs cascade-delete via SQLite FK only after teardown completes.

**Response 200:**
```json
{ "message": "Pipeline <id> deleted" }
```

**Errors:** `404` pipeline not found; `409` one or more running outputs failed to stop before delete.

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
    "desiredState": "stopped",
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

Deletes an output. If a job is running, the API sends a stop signal and waits for the process to exit before removing the DB row.

**Response 200:**
```json
{ "message": "Output <outputId> from pipeline <pipelineId> deleted" }
```

**Errors:** `404` output or pipeline not found; `409` running process did not stop before delete.

---

## 4. Output Runtime Control

### `POST /pipelines/:pipelineId/outputs/:outputId/start`

Sets this output's desired state to `running` and reconciles runtime toward that state. The full call flow is:

1. Persist `outputs.desiredState='running'`.
2. Acquire an in-memory start lock for `(pipelineId, outputId)` if a start is needed.
3. Validate pipeline + output exist.
4. Check for an existing running job.
5. Require `pipeline.streamKey`; resolve probe URL `rtsp://localhost:8554/<streamKey>`.
6. Run `ffprobe -rtsp_transport tcp <probeUrl>` with 8 s timeout.
7. Build tagged pull URL: `rtsp://localhost:8554/<streamKey>?reader_id=reader_<pipelineId>_<outputId>`.
8. Spawn FFmpeg for the selected output encoding:
  - `source`: codec copy (`-c:v copy -c:a copy`)
  - `vertical-crop`: `-vf scale=720:1280:force_original_aspect_ratio=increase,crop=720:1280` + H.264/AAC encode
  - `vertical-rotate`: `-vf transpose=1,scale=720:1280:force_original_aspect_ratio=increase,crop=720:1280` + H.264/AAC encode
  - `720p`: `-vf scale=-2:720` + H.264/AAC encode
  - `1080p`: `-vf scale=-2:1080` + H.264/AAC encode
9. Persist job row in DB, return after 250 ms stability check.

**Request body:** none

**Response 201:**
```json
{
  "message": "Output started",
  "desiredState": "running",
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

**Response 409:**
```json
{
  "error": "Pipeline input is not available yet",
  "message": "Output desired state set to running; waiting for input",
  "desiredState": "running",
  "detail": "Publisher connected, stream not ready yet"
}
```

**Errors:**
- `400` no input URL available
- `404` pipeline or output not found
- `409` start already in progress for this output
- `409` pipeline input not available yet; desired state is still updated to `running`
- `500` FFmpeg failed to start (includes last 100 log lines)

---

### `POST /pipelines/:pipelineId/outputs/:outputId/stop`

Sets this output's desired state to `stopped`, clears pending retry timers, and reconciles runtime toward that state. If a process is running, it is stopped via SIGTERM with a 5 s SIGKILL escalation.

**Response 200:**
```json
{
  "message": "Output desired state set to stopped",
  "desiredState": "stopped",
  "previousState": "running",
  "jobId": "3f2e1d0c9b8a7f6e",
  "result": {
    "stopped": true,
    "reason": "signal-sent"   // "signal-sent" | "marked-stopped" | "signal-failed"
  }
}
```

If no process is running, the output intent is still updated and the response remains `200`:

```json
{
  "message": "Output desired state set to stopped",
  "desiredState": "stopped",
  "previousState": "running",
  "jobId": null,
  "result": {
    "stopped": false,
    "reason": "already_stopped"
  }
}
```

**Errors:** `404` output or pipeline not found.

---

### `GET /pipelines/:pipelineId/outputs/:outputId/history?limit=200&filter=lifecycle`

Returns recent job logs for a specific output. This endpoint is intended for diagnostics and output history UI.

**Query params:**
- `limit` optional; default `200`; min `1`; max `1000`.
- `filter` optional; set to `lifecycle` to return only lifecycle events.
- `since` optional; inclusive lower timestamp bound (ISO 8601).
- `until` optional; exclusive upper timestamp bound (ISO 8601).
- `order` optional; `asc` or `desc`.
- `prefix` optional; comma-separated or repeated list of message families: `lifecycle`, `stderr`, `exit`, `control`, `config`, `input_state`.

Ordering behavior:
- Default path (`filter` omitted): newest first.
- Lifecycle path (`filter=lifecycle`): oldest first, full lifecycle sequence for timeline rendering.

Filtering behavior:
- `since` and `until` constrain the returned time window.
- `prefix` filters by message prefix when `filter` is omitted.
- `filter=lifecycle` is equivalent to a lifecycle-only prefix filter and ignores `prefix`.

Guardrails:
- Any requested history window is capped server-side to 24 hours.
- Requests that include high-volume families (`stderr`, `exit`, `control`) are capped to a 10 minute window when both `since` and `until` are provided.

**Response 200:**
```json
{
  "pipelineId": "a1b2c3d4e5f6a7b8",
  "outputId": "f8e7d6c5b4a3f2e1",
  "logs": [
    {
      "ts": "2026-04-15T12:20:57.098Z",
      "eventType": "lifecycle.exited",
      "eventData": {
        "status": "failed",
        "requestedStop": false,
        "exitCode": 255,
        "exitSignal": null
      },
      "message": "[lifecycle] exited status=failed requestedStop=false exitCode=255 exitSignal=null"
    },
    {
      "ts": "2026-04-15T12:20:57.098Z",
      "eventType": "output.exit",
      "eventData": { "code": 255, "signal": null },
      "message": "[exit] code=255 signal=null"
    },
    {
      "ts": "2026-04-15T12:20:56.971Z",
      "eventType": "output.stderr",
      "eventData": null,
      "message": "[stderr] [flv @ ...] Non-monotonic DTS ..."
    }
  ]
}
```

**Errors:** `400` invalid `limit`, `since`, `until`, `order`, or `prefix`, or requested window too large; `404` output or pipeline not found.

> History enrichment: timeline-relevant rows now include stable `eventType` codes and structured
> `eventData` payloads. The human-readable `message` is still stored and returned for raw log
> inspection, but clients should key timeline behavior off the typed fields instead of parsing the
> prose message.

---

## 5. Config Snapshot

### `GET /config`

Returns the full state snapshot used by the dashboard. Supports conditional GET via `If-None-Match` / ETag.

Response headers now include a shared `X-Snapshot-Version` token that identifies the config/jobs
state version represented by this snapshot. The dashboard compares that token with the health
response before committing a render.

**Request headers (optional):**
```
If-None-Match: "abc123..."
```

**Response headers:**
```
ETag: "..."                 // config + jobs snapshot hash
X-Config-ETag: "..."        // config-only snapshot hash
X-Snapshot-Version: "..."   // shared config/jobs state version used to align /config and /health
```

**Response 200:**
```json
{
  "serverName": "My Server",
  "pipelinesLimit": 25,
  "outLimit": 95,
  "outputRecovery": {
    "enabled": true,
    "immediateRetries": 3,
    "immediateDelayMs": 1000,
    "backoffRetries": 5,
    "backoffBaseDelayMs": 2000,
    "backoffMaxDelayMs": 60000,
    "resetFailureCountAfterMs": 30000,
    "restartOnInputRecovery": true,
    "inputRecoveryRestartMode": "inputUnavailableOnly",
    "inputRecoveryRestartDelayMs": 1000,
    "inputRecoveryRestartStaggerMs": 250
  },
  "ingest": {
    "host": null,
    "rtmpPort": "1935",
    "rtspPort": "8554",
    "srtPort": "8890"
  },
  "pipelines": [ ... ],
  "outputs": [ ... ],
  "jobs": [ ... ]
}
```

Each output now includes `desiredState`, which is the persistent operator intent (`running` or `stopped`).

**Response headers:**
```
ETag: "abc123def456..."
X-Config-ETag: "7890abcd1234..."
```

**Response 304:** ETag matches — no body, no change.

> The ETag is a SHA-256 hash of a deterministic state snapshot, persisted in the `meta` table.

---

### `HEAD /config`

Returns the current ETag without a response body. Used to poll for changes without downloading the full config.

**Response 200** (no body) + `ETag` and `X-Config-ETag` headers.

---

## 6. Health and Metrics

### `GET /health`

Returns the latest server-side health snapshot. A periodic collector refreshes this snapshot in the background by calling MediaMTX endpoints in parallel: `/v3/paths/list`, `/v3/rtspconns/list`, `/v3/rtspsessions/list`, `/v3/rtmpconns/list`, `/v3/srtconns/list`, and `/v3/webrtcsessions/list`, then merging that runtime state with DB job state and input lifecycle bookkeeping. The collector interval defaults to 2000 ms and can be overridden with `HEALTH_SNAPSHOT_INTERVAL_MS`.

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
    "rtmpConnCount": 1,
    "srtConnCount": 0,
    "webrtcSessionCount": 0,
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
    "rtmpConnCount": 0,
    "srtConnCount": 0,
    "webrtcSessionCount": 0,
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

Returns host system metrics from a fixed background sampler. Throughput and CPU values are computed against the previous timer sample, not against the previous HTTP request, so concurrent clients see the same rates for the same sample window.

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
